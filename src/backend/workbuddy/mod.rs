//! WorkBuddy 私有客户端适配器。
//!
//! 当前协议对照基线为官方 CN 桌面 5.11.3 发布包；历史社区实现不是官方契约：
//! - 登录：`POST /v2/plugin/auth/state` → 浏览器授权 → `GET /v2/plugin/auth/token`
//!   → `GET /v2/plugin/login/account`；
//! - 对话：`POST /v2/chat/completions`，请求与 SSE 响应均为 OpenAI Chat
//!   Completions 形态，但必须 `stream=true`；
//! - 目录：`GET /v3/config`；额度与签到在 `billing/meter/*`。
//!
//! 出站请求需要官方客户端头组（Origin/Referer、X-IDE-*、会话头族），
//! 各接口使用独立作用域；普通元数据一致不能证明官方渠道会接受请求。

mod catalog;
mod checkin;
pub mod oauth;
pub use checkin::WorkBuddyCheckin;

use crate::accounts::{Account, AccountPool, FailureKind};
use crate::backend::openai_compat;
use crate::backend::{Backend, CostTier};
use crate::core::error::AppError;
use crate::core::stream::EventStream;
use crate::core::{
    Capabilities, ChatRequest, ContentPart, Message, ModelInfo, Role, ToolChoice, UpstreamError,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

/// 事件通道容量。
const EVENT_CHANNEL_CAPACITY: usize = 32;

/// 默认客户端版本。与官方 WorkBuddy 桌面端分发包保持一致。
const DEFAULT_CLIENT_VERSION: &str = "5.11.3";
/// 默认内置 CLI 版本。
const DEFAULT_CLI_VERSION: &str = "2.176.0";
/// 默认登录平台标识。
const DEFAULT_LOGIN_PLATFORM: &str = "workbuddy";

/// WorkBuddy 区域。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkBuddyRegion {
    /// 国内站。
    Cn,
    /// 国际站。
    Intl,
}

impl WorkBuddyRegion {
    /// 基础地址。
    pub fn base(self) -> &'static str {
        match self {
            WorkBuddyRegion::Cn => "https://www.workbuddy.cn",
            WorkBuddyRegion::Intl => "https://www.workbuddy.ai",
        }
    }

    /// 声明域，用于 `X-Domain` 缺省值。
    pub fn domain(self) -> &'static str {
        match self {
            WorkBuddyRegion::Cn => "www.workbuddy.cn",
            WorkBuddyRegion::Intl => "www.workbuddy.ai",
        }
    }

    /// 是否国际站。
    pub fn is_global(self) -> bool {
        matches!(self, WorkBuddyRegion::Intl)
    }

    /// 语言标识。
    pub fn accept_language(self) -> &'static str {
        if self.is_global() { "en-US" } else { "zh-CN" }
    }

    /// UA 中的平台品牌段。
    pub fn ua_platform(self) -> &'static str {
        if self.is_global() {
            "WorkBuddy AI"
        } else {
            "WorkBuddy"
        }
    }

    /// 从后端 options 解析区域。缺省为国内站。
    pub fn from_options(options: &str) -> Result<Self, AppError> {
        Ok(profile_from_options(options)?.region)
    }
}

/// 客户端画像。版本可按后端覆盖，避免上游升级后无法对齐。
#[derive(Debug, Clone)]
pub struct WorkBuddyProfile {
    pub region: WorkBuddyRegion,
    pub client_version: String,
    pub cli_version: String,
    /// 登录平台的 query 参数值。
    pub login_platform: String,
    pub deployment_type: String,
    pub private_data: bool,
}

/// 后端选项 JSON。
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
pub(crate) struct BackendOptions {
    #[serde(default)]
    region: String,
    #[serde(default)]
    client_version: String,
    #[serde(default)]
    cli_version: String,
    #[serde(default)]
    login_platform: String,
    #[serde(default)]
    deployment_type: String,
    /// true 表示不允许模型优化使用请求数据，缺省采用此值。
    #[serde(default)]
    private_data: Option<bool>,
}

/// 从 options 解析客户端画像。
pub fn profile_from_options(options: &str) -> Result<WorkBuddyProfile, AppError> {
    let parsed: BackendOptions = if options.trim().is_empty() {
        BackendOptions::default()
    } else {
        serde_json::from_str(options).map_err(|error| {
            AppError::bad_request(format!(
                "workbuddy backend options is not valid JSON: {error}"
            ))
        })?
    };
    Ok(WorkBuddyProfile {
        region: if parsed.region.eq_ignore_ascii_case("intl")
            || parsed.region.eq_ignore_ascii_case("global")
        {
            WorkBuddyRegion::Intl
        } else {
            WorkBuddyRegion::Cn
        },
        client_version: if parsed.client_version.trim().is_empty() {
            DEFAULT_CLIENT_VERSION.to_string()
        } else {
            parsed.client_version.trim().to_string()
        },
        cli_version: if parsed.cli_version.trim().is_empty() {
            DEFAULT_CLI_VERSION.to_string()
        } else {
            parsed.cli_version.trim().to_string()
        },
        login_platform: if parsed.login_platform.trim().is_empty() {
            DEFAULT_LOGIN_PLATFORM.to_string()
        } else {
            parsed.login_platform.trim().to_string()
        },
        deployment_type: if parsed.deployment_type.trim().is_empty() {
            "SaaS".into()
        } else {
            parsed.deployment_type.trim().into()
        },
        private_data: parsed.private_data.unwrap_or(true),
    })
}

/// 账号凭证。字段名与登录响应保持一致，便于排查。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkBuddyCredential {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    /// 访问令牌过期时间（Unix 秒）。0 表示未知。
    #[serde(default)]
    pub expires_at: u64,
    /// 刷新令牌过期时间（Unix 秒）。0 表示未知。
    #[serde(default)]
    pub refresh_expires_at: u64,
    #[serde(default)]
    pub uid: String,
    #[serde(default)]
    pub nickname: String,
    #[serde(default)]
    pub account_type: String,
    #[serde(default)]
    pub enterprise_id: String,
    #[serde(default)]
    pub department: String,
    #[serde(default)]
    pub domain: String,
    /// 可选的设备令牌，官方客户端部分接口会带。
    #[serde(default)]
    pub device_token: String,
}

impl WorkBuddyCredential {
    /// 从账号记录解析凭证。
    pub fn parse(account: &Account) -> Result<Self, AppError> {
        serde_json::from_str(&account.credential).map_err(|error| {
            AppError::bad_request(format!(
                "account `{}` credential is not valid JSON: {error}",
                account.id
            ))
        })
    }

    /// 校验登录必需字段。
    pub fn validate(&self) -> Result<(), AppError> {
        if self.access_token.is_empty() || self.uid.is_empty() {
            return Err(AppError::bad_request(
                "workbuddy credential requires access_token and uid; sign in again",
            ));
        }
        Ok(())
    }

    /// 由登录结果构造凭证。
    pub fn from_login(login: &oauth::LoginResult) -> Self {
        WorkBuddyCredential {
            access_token: login.access_token.clone(),
            refresh_token: login.refresh_token.clone(),
            expires_at: login.expires_at,
            refresh_expires_at: login.refresh_expires_at,
            uid: login.uid.clone(),
            nickname: login.nickname.clone(),
            account_type: login.account_type.clone(),
            enterprise_id: login.enterprise_id.clone(),
            department: login.department.clone(),
            domain: login.domain.clone(),
            device_token: String::new(),
        }
    }

    /// 是否即将过期。
    pub fn expires_soon(&self) -> bool {
        self.expires_at > 0 && self.expires_at <= crate::records::now_seconds() + 60
    }
}

/// 构造公共请求头。
fn common_headers(profile: &WorkBuddyProfile) -> Vec<(String, String)> {
    let origin = profile.region.base();
    vec![
        ("accept".to_string(), "application/json".to_string()),
        ("content-type".to_string(), "application/json".to_string()),
        ("x-requested-with".to_string(), "XMLHttpRequest".to_string()),
        ("origin".to_string(), origin.to_string()),
        ("referer".to_string(), format!("{origin}/")),
        (
            "user-agent".to_string(),
            format!(
                "WorkBuddy/{} {}/{} CLI/{}",
                profile.client_version,
                profile.region.ua_platform(),
                profile.client_version,
                profile.cli_version
            ),
        ),
        ("x-codebuddy-request".to_string(), "1".to_string()),
        (
            "accept-language".to_string(),
            profile.region.accept_language().to_string(),
        ),
    ]
}

/// 构造匿名请求头，用于登录启动与令牌轮询。
fn anonymous_headers(profile: &WorkBuddyProfile) -> Vec<(String, String)> {
    let mut headers = common_headers(profile);
    for name in [
        "x-no-authorization",
        "x-no-user-id",
        "x-no-enterprise-id",
        "x-no-department-info",
    ] {
        headers.push((name.to_string(), "true".to_string()));
    }
    headers.push(("x-domain".into(), profile.region.domain().into()));
    headers
}

/// 构造带账号身份的通用请求头。
fn auth_headers(
    profile: &WorkBuddyProfile,
    credential: &WorkBuddyCredential,
) -> Vec<(String, String)> {
    let mut headers = common_headers(profile);
    headers.push((
        "authorization".to_string(),
        format!("Bearer {}", credential.access_token),
    ));
    headers.push(("x-user-id".to_string(), credential.uid.clone()));
    if credential.enterprise_id.is_empty() {
        headers.push(("x-no-enterprise-id".to_string(), "1".to_string()));
    } else {
        headers.push((
            "x-enterprise-id".to_string(),
            credential.enterprise_id.clone(),
        ));
    }
    let domain = if !credential.domain.is_empty() {
        credential.domain.clone()
    } else {
        profile.region.domain().to_string()
    };
    if domain.is_empty() {
        headers.push(("x-no-department-info".to_string(), "1".to_string()));
    } else {
        headers.push(("x-domain".to_string(), domain));
    }
    headers
}

/// 构造对话请求头。
///
/// 官方原生客户端的对话请求不带浏览器来源头；会话头族按账号与下游会话
/// 稳定轮换，并补齐 agent 身份、ACP 连接和 tracing 上下文。
fn chat_headers(
    profile: &WorkBuddyProfile,
    credential: &WorkBuddyCredential,
    channel: &ChatChannel,
) -> Vec<(String, String)> {
    let mut headers = auth_headers(profile, credential);
    headers.retain(|(name, _)| name != "origin" && name != "referer");
    headers.push((
        "accept".to_string(),
        "application/json, text/event-stream".to_string(),
    ));
    headers.push(("x-agent-intent".to_string(), "craft".to_string()));
    headers.push(("x-agent-purpose".to_string(), "conversation".to_string()));
    headers.push(("x-agent-type".to_string(), "main".to_string()));
    headers.push(("x-ide-name".to_string(), "WorkBuddy".to_string()));
    headers.push(("x-ide-type".to_string(), "WorkBuddy".to_string()));
    headers.push(("x-ide-version".to_string(), profile.client_version.clone()));
    headers.push(("x-product".to_string(), "WorkBuddy".to_string()));
    headers.push(("x-conversation-id".into(), channel.conversation_id.clone()));
    headers.push((
        "x-conversation-request-id".to_string(),
        channel.request_id.clone(),
    ));
    headers.push((
        "x-conversation-message-id".to_string(),
        channel.message_id.clone(),
    ));
    headers.push(("x-request-id".to_string(), channel.message_id.clone()));
    headers.push(("x-root-request-id".to_string(), channel.request_id.clone()));
    headers.push((
        "acp-connection-id".into(),
        channel.acp_connection_id.clone(),
    ));
    headers.push(("x-private-data".into(), profile.private_data.to_string()));
    headers.extend(trace_headers());
    if !credential.device_token.is_empty() {
        headers.push((
            "x-device-token".to_string(),
            credential.device_token.clone(),
        ));
    }
    headers
}

/// 构造单次请求的 tracing 上下文。
///
/// 上游要求 `traceparent`、B3 与 `X-Trace-ID` 共享同一 traceId；这里为每次
/// 请求新建一组自洽的 ID，不冒充下游会话的真实链路。
fn trace_headers() -> Vec<(String, String)> {
    let trace_id = random_hex(16);
    let span_id = random_hex(8);
    vec![
        (
            "traceparent".to_string(),
            format!("00-{trace_id}-{span_id}-01"),
        ),
        ("x-b3-traceid".to_string(), trace_id.clone()),
        ("x-b3-spanid".to_string(), span_id),
        ("x-b3-sampled".to_string(), "1".to_string()),
        ("x-trace-id".to_string(), trace_id),
    ]
}

/// 构造计费/签到请求头。
fn billing_headers(
    profile: &WorkBuddyProfile,
    credential: &WorkBuddyCredential,
) -> Vec<(String, String)> {
    let mut headers = vec![
        (
            "authorization".to_string(),
            format!("Bearer {}", credential.access_token),
        ),
        ("accept".to_string(), "application/json".to_string()),
        ("content-type".to_string(), "application/json".to_string()),
        ("x-codebuddy-request".to_string(), "1".to_string()),
        (
            "accept-language".to_string(),
            profile.region.accept_language().to_string(),
        ),
        (
            "user-agent".to_string(),
            format!("WorkBuddy/{}", profile.client_version),
        ),
        ("x-user-id".to_string(), credential.uid.clone()),
    ];
    if !credential.enterprise_id.is_empty() {
        headers.push((
            "x-enterprise-id".to_string(),
            credential.enterprise_id.clone(),
        ));
        headers.push(("x-tenant-id".to_string(), credential.enterprise_id.clone()));
    }
    if !credential.domain.is_empty() {
        headers.push(("x-domain".to_string(), credential.domain.clone()));
    }
    if !credential.device_token.is_empty() {
        headers.push((
            "x-device-token".to_string(),
            credential.device_token.clone(),
        ));
    }
    headers
}

/// 编码单条消息为 OpenAI 形态。
fn encode_message(message: &Message) -> Value {
    let role = match message.role.normalized() {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
        Role::Developer => "system",
    };
    let has_image = message.has_image();
    let content = if has_image {
        json!(
            message
                .content
                .iter()
                .map(|part| match part {
                    ContentPart::Text { text } => json!({"type": "text", "text": text}),
                    ContentPart::ImageUrl { url, detail } => json!({
                        "type": "image_url",
                        "image_url": {
                            "url": url,
                            "detail": detail.clone().unwrap_or_else(|| "auto".to_string()),
                        }
                    }),
                })
                .collect::<Vec<_>>()
        )
    } else {
        json!(message.text_content())
    };
    let mut encoded = json!({"role": role, "content": content});
    if !message.tool_calls.is_empty() {
        encoded["tool_calls"] = json!(
            message
                .tool_calls
                .iter()
                .map(|call| json!({
                    "id": call.id,
                    "type": "function",
                    "function": {"name": call.name, "arguments": call.arguments}
                }))
                .collect::<Vec<_>>()
        );
    }
    if let Some(tool_call_id) = &message.tool_call_id {
        encoded["tool_call_id"] = json!(tool_call_id);
    }
    if let Some(name) = &message.name {
        encoded["name"] = json!(name);
    }
    encoded
}

/// 构造 OpenAI 兼容请求体。上游要求始终流式，非流式由入站层聚合。
fn build_body(request: &ChatRequest) -> Value {
    let messages = request
        .messages
        .iter()
        .map(encode_message)
        .collect::<Vec<_>>();
    let mut body = json!({
        "model": request.model,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if !request.tools.is_empty() {
        body["tools"] = json!(
            request
                .tools
                .iter()
                .map(|tool| json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    }
                }))
                .collect::<Vec<_>>()
        );
    }
    if let Some(choice) = &request.tool_choice {
        body["tool_choice"] = match choice {
            ToolChoice::Auto => json!("auto"),
            ToolChoice::None => json!("none"),
            ToolChoice::Required => json!("required"),
            ToolChoice::Named(name) => {
                json!({"type": "function", "function": {"name": name}})
            }
        };
    }
    if let Some(max_tokens) = request.max_tokens {
        body["max_tokens"] = json!(max_tokens);
    }
    if let Some(temperature) = request.temperature {
        body["temperature"] = json!(temperature);
    }
    if let Some(top_p) = request.top_p {
        body["top_p"] = json!(top_p);
    }
    if !request.stop.is_empty() {
        body["stop"] = json!(request.stop);
    }
    if let Some(effort) = &request.reasoning_effort {
        body["reasoning_effort"] = json!(effort);
    }
    body
}

/// 取目录主体。上游把字段包在 `data` 下，兼容无包装形态。
fn catalog_data(value: &Value) -> &Value {
    value.get("data").unwrap_or(value)
}

/// 从模型目录取可见模型。
///
/// 可见性规则与官方客户端一致：agent（craft / default / cli）声明的模型，
/// 再与 availableModels 求交集，剔除本地、自定义 URL、禁用与隐藏项。
fn parse_models(root: &Value) -> Vec<ModelInfo> {
    parse_models_at(root, crate::records::now_seconds())
}

/// 同一目录采用同一时刻计算报价，避免解析过程中跨越活动边界。
fn parse_models_at(root: &Value, now: u64) -> Vec<ModelInfo> {
    let agents = root.get("agents").and_then(Value::as_array);
    let agent = agents.and_then(|items| {
        items
            .iter()
            .find(|agent| agent.get("name").and_then(Value::as_str) == Some("craft"))
            .or_else(|| {
                items.iter().find(|agent| {
                    agent
                        .get("tags")
                        .and_then(Value::as_array)
                        .map(|tags| tags.iter().any(|tag| tag.as_str() == Some("default")))
                        .unwrap_or(false)
                })
            })
            .or_else(|| {
                items
                    .iter()
                    .find(|agent| agent.get("name").and_then(Value::as_str) == Some("cli"))
            })
    });
    let available = root
        .get("availableModels")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect::<HashSet<_>>()
        })
        .unwrap_or_default();
    let ids: HashSet<String> = agent
        .and_then(|agent| agent.get("models").and_then(Value::as_array))
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect::<HashSet<_>>()
        })
        .filter(|ids| !ids.is_empty())
        .unwrap_or_else(|| available.clone());
    let excluded = ["text-to-image", "text-to-video", "image-to-video"];
    let mut models = Vec::new();
    let Some(items) = root.get("models").and_then(Value::as_array) else {
        return models;
    };
    let mut seen = HashSet::new();
    for item in items {
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            continue;
        };
        if !ids.contains(id) || (!available.is_empty() && !available.contains(id)) {
            continue;
        }
        let hidden = item.get("hidden").and_then(Value::as_bool).unwrap_or(false)
            || item
                .get("disabled")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            || item.get("enabled").and_then(Value::as_bool) == Some(false)
            || item.get("local").and_then(Value::as_bool).unwrap_or(false)
            || item.get("url").and_then(Value::as_str).is_some();
        if hidden {
            continue;
        }
        let excluded_tag = item
            .get("tags")
            .and_then(Value::as_array)
            .map(|tags| {
                tags.iter().any(|tag| {
                    tag.as_str()
                        .map(|value| excluded.contains(&value))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);
        if excluded_tag || !seen.insert(id.to_string()) {
            continue;
        }
        let quote = catalog::quote(
            item,
            root.get("modelPromotions")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
            now,
        );
        let token_limits = crate::backend::catalog::token_limits(item);
        models.push(ModelInfo {
            id: id.to_string(),
            display_name: item
                .get("name")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            price_factor: quote.factor,
            original_price_factor: quote.original,
            price_valid_until: quote.valid_until,
            capabilities: crate::backend::catalog::capabilities(
                item,
                &Capabilities {
                    tools: true,
                    parallel_tools: false,
                    images: false,
                    reasoning: false,
                    context_window: token_limits
                        .context_window
                        .or(token_limits.max_input_tokens),
                },
            ),
            token_limits,
        });
    }
    models
}

/// WorkBuddy 后端。
pub struct WorkBuddyBackend {
    id: String,
    name: String,
    profile: WorkBuddyProfile,
    models: Vec<String>,
    pool: AccountPool,
    client: reqwest::Client,
    channels: ChatChannels,
}

impl WorkBuddyBackend {
    /// 构造后端。
    pub fn new(
        id: String,
        name: String,
        profile: WorkBuddyProfile,
        models: Vec<String>,
        accounts: Vec<Account>,
    ) -> Result<Self, AppError> {
        Self::new_with_timeout(
            id,
            name,
            profile,
            models,
            accounts,
            std::time::Duration::from_secs(10),
        )
    }

    /// 生成请求与会话建立共用配置的连接时限。
    pub fn new_with_timeout(
        id: String,
        name: String,
        profile: WorkBuddyProfile,
        models: Vec<String>,
        accounts: Vec<Account>,
        timeout: std::time::Duration,
    ) -> Result<Self, AppError> {
        let client = reqwest::Client::builder()
            .connect_timeout(timeout)
            .build()
            .map_err(|error| AppError::Internal {
                message: format!("build http client failed: {error}"),
            })?;
        Ok(WorkBuddyBackend {
            id,
            name,
            profile,
            models,
            pool: AccountPool::new(accounts),
            client,
            channels: ChatChannels::new(),
        })
    }

    /// 发起一次带鉴权头的 GET。
    async fn get_json(
        &self,
        credential: &WorkBuddyCredential,
        path: &str,
    ) -> Result<Value, AppError> {
        let url = format!("{}{path}", self.profile.region.base());
        let mut builder = self.client.get(&url);
        for (name, value) in auth_headers(&self.profile, credential) {
            builder = builder.header(name, value);
        }
        if path == "/v3/config" {
            builder = builder.header("X-Product", &self.profile.deployment_type);
        }
        let response = builder.send().await?;
        self.decode(response).await
    }

    /// 发起一次带计费头的 POST。
    async fn post_billing(
        &self,
        credential: &WorkBuddyCredential,
        path: &str,
    ) -> Result<Value, AppError> {
        let url = format!("{}{path}", self.profile.region.base());
        let mut builder = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .body("{}");
        for (name, value) in billing_headers(&self.profile, credential) {
            builder = builder.header(name, value);
        }
        let response = builder.send().await?;
        self.decode(response).await
    }

    /// 统一解析 JSON 响应。业务错误码原样上抛。
    async fn decode(&self, response: reqwest::Response) -> Result<Value, AppError> {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(AppError::Upstream(UpstreamError::status(
                status.as_u16(),
                body,
            )));
        }
        let parsed: Value = serde_json::from_str(&body).map_err(|error| {
            AppError::Upstream(UpstreamError {
                status: None,
                code: "invalid_upstream_response".to_string(),
                message: format!("invalid workbuddy JSON: {error}"),
                retryable: false,
                execution_unknown: false,
            })
        })?;
        if let Some(code) = parsed.get("code") {
            let text = match code {
                Value::Number(number) => number.to_string(),
                Value::String(value) => value.clone(),
                _ => String::new(),
            };
            if !text.is_empty() && text != "0" && text != "200" {
                let message = parsed
                    .get("message")
                    .or_else(|| parsed.get("msg"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                return Err(AppError::Upstream(UpstreamError {
                    status: Some(status.as_u16()),
                    code: text,
                    message: message.to_string(),
                    retryable: false,
                    execution_unknown: false,
                }));
            }
        }
        Ok(parsed)
    }

    /// 拉取真实模型目录。
    async fn fetch_models(
        &self,
        credential: &WorkBuddyCredential,
    ) -> Result<Vec<ModelInfo>, AppError> {
        let config = self.get_json(credential, "/v3/config").await?;
        Ok(parse_models(catalog_data(&config)))
    }

    /// 读取额度：企业账号走企业用量，个人账号汇总资源包。
    async fn read_quota(
        &self,
        credential: &WorkBuddyCredential,
        account_id: &str,
    ) -> Result<crate::accounts::remote::QuotaInfo, AppError> {
        use crate::accounts::remote::{QuotaConfidence, QuotaInfo};
        if !credential.enterprise_id.is_empty() {
            let value = self
                .post_billing(credential, "/v2/billing/meter/get-enterprise-user-usage")
                .await?;
            let data = value.get("data").unwrap_or(&value);
            let used = data.get("credit").and_then(Value::as_f64).map(|v| v as i64);
            let limit = data
                .get("limitNum")
                .and_then(Value::as_f64)
                .map(|v| v as i64);
            let unlimited = limit == Some(-1);
            return Ok(QuotaInfo {
                routing: {
                    use crate::accounts::quota::{QuotaSnapshot, decimal_string, value_units};
                    let amount = value_units(&data["limitNum"])
                        .zip(value_units(&data["credit"]))
                        .and_then(|(total, used)| total.checked_sub(used));
                    Some(QuotaSnapshot {
                        remaining: (!unlimited).then(|| amount.map(decimal_string)).flatten(),
                        unit: "credits".into(),
                        quota_scope_id: format!(
                            "workbuddy:{:?}:{}:{}",
                            self.profile.region, credential.uid, credential.enterprise_id
                        ),
                        valid_until: None,
                        unlimited,
                        complete: unlimited || amount.is_some(),
                    })
                },
                backend_id: self.id.clone(),
                account_id: account_id.to_string(),
                used,
                total: if unlimited { None } else { limit },
                remaining: None,
                unit: Some("credits".to_string()),
                reset_at: None,
                confidence: if used.is_some() {
                    QuotaConfidence::Verified
                } else {
                    QuotaConfidence::Unknown
                },
                note: unlimited.then(|| "unlimited".to_string()),
                checked_at: crate::records::now_seconds(),
            });
        }
        let value = self
            .post_billing(credential, "/billing/meter/get-user-resource-summary")
            .await?;
        let data = value.get("data").unwrap_or(&value);
        let Some(packages) = data.get("Packages").and_then(Value::as_array) else {
            return Ok(QuotaInfo::unknown(
                &self.id,
                account_id,
                "resource summary has no Packages",
            ));
        };
        let read = |item: &Value, key: &str| -> Option<f64> {
            item.get(key).and_then(|value| match value {
                Value::Number(number) => number.as_f64(),
                Value::String(text) => text.parse().ok(),
                _ => None,
            })
        };
        let mut used = 0.0;
        let mut total = 0.0;
        let mut complete = true;
        for package in packages {
            match (
                read(package, "CycleUsedCapacity"),
                read(package, "CycleTotalCapacity"),
            ) {
                (Some(package_used), Some(package_total)) => {
                    used += package_used.max(0.0);
                    total += package_total.max(0.0);
                }
                _ => complete = false,
            }
        }
        if !complete {
            return Ok(QuotaInfo::unknown(
                &self.id,
                account_id,
                "incomplete credit summary",
            ));
        }
        Ok(QuotaInfo {
            routing: {
                use crate::accounts::quota::{QuotaSnapshot, decimal_string};
                let precise = routing_credit_balance(packages);
                Some(QuotaSnapshot {
                    remaining: precise.map(decimal_string),
                    unit: "credits".into(),
                    quota_scope_id: format!(
                        "workbuddy:{:?}:{}",
                        self.profile.region, credential.uid
                    ),
                    valid_until: None,
                    unlimited: false,
                    complete: precise.is_some(),
                })
            },
            backend_id: self.id.clone(),
            account_id: account_id.to_string(),
            used: Some(used.round() as i64),
            total: Some(total.round() as i64),
            remaining: None,
            unit: Some("credits".to_string()),
            reset_at: None,
            confidence: QuotaConfidence::Verified,
            note: None,
            checked_at: crate::records::now_seconds(),
        })
    }
}

/// 只汇总同单位的当前资源包，优先保留上游明确的余额及其全部小数。
fn routing_credit_balance(packages: &[Value]) -> Option<i128> {
    use crate::accounts::quota::value_units;
    if packages.is_empty() {
        return None;
    }
    packages.iter().try_fold(0_i128, |sum, package| {
        if package["CapacityUnit"].as_str() != Some("credits") {
            return None;
        }
        let remaining = match package.get("CycleRemainCapacity") {
            Some(value) => value_units(value)?,
            None => {
                let used = value_units(&package["CycleUsedCapacity"])?;
                let total = value_units(&package["CycleTotalCapacity"])?;
                if used < 0 || total < 0 {
                    return None;
                }
                total.checked_sub(used)?
            }
        };
        sum.checked_add(remaining)
    })
}

#[async_trait]
impl Backend for WorkBuddyBackend {
    /// 旧目录的 context_window 实际表示输入容量，不能再减去输出预留。
    fn model_limits(&self, model: &ModelInfo) -> crate::core::TokenLimits {
        let mut limits = model.token_limits.clone();
        if limits == crate::core::TokenLimits::default() {
            limits.max_input_tokens = model.capabilities.context_window;
        }
        limits
    }

    /// 返回账号可用性，实际调用通过同一个账号池完成。
    fn routing_accounts(&self) -> Option<Vec<crate::accounts::AccountState>> {
        Some(self.pool.snapshot())
    }

    /// 使用与管理控制台相同的余额来源。
    async fn routing_quota(
        &self,
        id: &str,
    ) -> Result<crate::accounts::remote::QuotaInfo, AppError> {
        let account = self
            .pool
            .get_available(id)
            .ok_or_else(|| AppError::bad_request("account unavailable"))?;
        let credential = WorkBuddyCredential::parse(&account)?;
        self.read_quota(&credential, id).await
    }

    /// 返回标识。
    fn id(&self) -> &str {
        &self.id
    }

    /// 返回名称。
    fn name(&self) -> &str {
        &self.name
    }

    /// 返回类型。
    fn kind(&self) -> &str {
        "workbuddy"
    }

    /// 官方接口支持工具与图片；具体模型能力以目录为准。
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tools: true,
            parallel_tools: false,
            images: true,
            reasoning: true,
            context_window: None,
        }
    }

    /// WorkBuddy 额度属于订阅额度。
    fn cost_tier(&self) -> CostTier {
        CostTier::Subscription
    }

    /// 私有客户端后端优先级低于官方网关。
    fn priority(&self) -> u8 {
        200
    }

    /// 返回配置模型，同时作为模型寻址白名单。
    fn static_models(&self) -> Vec<String> {
        self.models.clone()
    }

    /// 返回模型清单：有账号时读上游目录，无账号时回落到配置。
    async fn list_models(&self) -> Result<Vec<ModelInfo>, AppError> {
        if self.pool.is_empty() {
            return Ok(self
                .models
                .iter()
                .map(|model| ModelInfo {
                    id: model.clone(),
                    display_name: Some(model.clone()),
                    price_factor: None,
                    original_price_factor: None,
                    price_valid_until: None,
                    capabilities: self.capabilities(),
                    token_limits: Default::default(),
                })
                .collect());
        }
        let account = self.pool.select().ok_or_else(|| {
            AppError::Upstream(UpstreamError {
                status: Some(503),
                code: "no_available_account".to_string(),
                message: "all workbuddy accounts are cooling or disabled".to_string(),
                retryable: true,
                execution_unknown: false,
            })
        })?;
        let credential = WorkBuddyCredential::parse(&account)?;
        credential.validate()?;
        match self.fetch_models(&credential).await {
            Ok(models) if !models.is_empty() => {
                self.pool.report_success(&account.id);
                Ok(models)
            }
            Ok(_) => Err(AppError::Upstream(UpstreamError {
                status: None,
                code: "empty_model_list".to_string(),
                message: "upstream model catalog is empty".to_string(),
                retryable: false,
                execution_unknown: false,
            })),
            Err(error) => {
                let status = match &error {
                    AppError::Upstream(upstream) => upstream.status,
                    _ => None,
                };
                self.pool
                    .report_failure(&account.id, FailureKind::classify(status, ""));
                Err(error)
            }
        }
    }

    /// 发起生成。
    async fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, AppError> {
        let account = self.pool.select().ok_or_else(|| {
            AppError::Upstream(UpstreamError {
                status: Some(503),
                code: "no_available_account".to_string(),
                message: "all workbuddy accounts are cooling or disabled".to_string(),
                retryable: true,
                execution_unknown: false,
            })
        })?;
        let credential = WorkBuddyCredential::parse(&account)?;
        credential.validate()?;
        let channel = self.channels.claim(
            &format!(
                "{}\u{0}{}",
                account.id,
                request.session.as_deref().unwrap_or_default()
            ),
            now_ms(),
        );
        let body = build_body(&request);
        let url = format!("{}/v2/chat/completions", self.profile.region.base());
        let mut builder = self.client.post(&url).json(&body);
        for (name, value) in chat_headers(&self.profile, &credential, &channel) {
            builder = builder.header(name, value);
        }
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return Err(AppError::BadRequest {
                    code: "request_cancelled".to_string(),
                    message: "client cancelled before upstream response".to_string(),
                });
            }
            result = builder.send() => result?,
        };
        let status = response.status();
        if !status.is_success() {
            crate::gateway::retry::capture_retry_after(&response);
            let text = response.text().await.unwrap_or_default();
            self.pool.report_failure(
                &account.id,
                FailureKind::classify(Some(status.as_u16()), &text),
            );
            return Err(AppError::Upstream(UpstreamError::status(
                status.as_u16(),
                text,
            )));
        }
        self.pool.report_success(&account.id);
        let (sender, receiver) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        tokio::spawn(async move {
            openai_compat::pump_sse(response, sender, cancel).await;
        });
        Ok(Box::pin(ReceiverStream::new(receiver)))
    }
}

/// 一次对话的空闲上限；超过后开启新会话，与官方客户端一致。
const CONVERSATION_IDLE_MS: u64 = 30 * 60 * 1000;
/// 一轮 user turn 的空闲上限；超过后换新的请求 ID。
const TURN_IDLE_MS: u64 = 10 * 60 * 1000;
/// 每个后端最多跟踪的会话数量，超出后淘汰最久未用的一项。
const MAX_TRACKED_CHANNELS: usize = 64;

/// 单个对话通道的头族取值。
struct ChatChannel {
    /// 会话级 ID，36 位带连字符 UUID，一次会话内稳定。
    conversation_id: String,
    /// 轮次级 ID，32 位十六进制，一次 user turn 内稳定。
    request_id: String,
    /// 消息级 ID，32 位十六进制 UUIDv7，每次请求新建。
    message_id: String,
    /// 声称所属的 ACP 连接，按会话稳定。
    acp_connection_id: String,
}

/// 通道在两次请求之间的状态。
struct ChatChannelState {
    conversation_id: String,
    request_id: String,
    acp_connection_id: String,
    last_at_ms: u64,
}

/// 按账号与下游会话维护对话头族。
///
/// 上游网关按会话计数，并在渠道校验中检查头族形状；因此会话内必须稳定，
/// 空闲后再轮换，不能每个请求都造一组新 ID。
struct ChatChannels {
    states: std::sync::Mutex<std::collections::HashMap<String, ChatChannelState>>,
}

impl ChatChannels {
    /// 创建空的状态表。
    fn new() -> Self {
        Self {
            states: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// 取本请求应使用的通道；按休眠时长轮换会话与轮次 ID。
    fn claim(&self, key: &str, now: u64) -> ChatChannel {
        let mut states = self
            .states
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let message_id = uuid_v7_hex(now);
        let current = states.get(key);
        let state = match current {
            Some(current) if now.saturating_sub(current.last_at_ms) <= CONVERSATION_IDLE_MS => {
                ChatChannelState {
                    conversation_id: current.conversation_id.clone(),
                    request_id: if now.saturating_sub(current.last_at_ms) > TURN_IDLE_MS {
                        random_hex(16)
                    } else {
                        current.request_id.clone()
                    },
                    acp_connection_id: current.acp_connection_id.clone(),
                    last_at_ms: now,
                }
            }
            _ => ChatChannelState {
                conversation_id: uuid::Uuid::new_v4().to_string(),
                request_id: random_hex(16),
                acp_connection_id: uuid::Uuid::new_v4().to_string(),
                last_at_ms: now,
            },
        };
        let channel = ChatChannel {
            conversation_id: state.conversation_id.clone(),
            request_id: state.request_id.clone(),
            message_id,
            acp_connection_id: state.acp_connection_id.clone(),
        };
        states.remove(key);
        states.insert(key.to_string(), state);
        while states.len() > MAX_TRACKED_CHANNELS {
            let oldest = states.keys().next().cloned();
            match oldest {
                Some(oldest) => {
                    states.remove(&oldest);
                }
                None => break,
            }
        }
        channel
    }
}

/// 当前毫秒时间戳。
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

/// 生成指定字节数的十六进制 ID。
fn random_hex(bytes: usize) -> String {
    use rand_core::{OsRng, RngCore};
    let mut buffer = vec![0u8; bytes];
    OsRng.fill_bytes(&mut buffer);
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 生成 32 位十六进制 UUIDv7，保留时间有序性，与官方消息 ID 形状一致。
fn uuid_v7_hex(now: u64) -> String {
    use rand_core::{OsRng, RngCore};
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    for (index, byte) in bytes.iter_mut().take(6).enumerate() {
        *byte = (now >> (8 * (5 - index))) as u8;
    }
    bytes[6] = 0x70 | (bytes[6] & 0x0f);
    bytes[8] = 0x80 | (bytes[8] & 0x3f);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// WorkBuddy 凭证续期器。
pub struct WorkBuddyRefresher {
    backend_id: String,
    profile: WorkBuddyProfile,
    client: reqwest::Client,
}

impl WorkBuddyRefresher {
    /// 构造续期器。
    pub fn new(backend_id: String, profile: WorkBuddyProfile) -> Result<Self, AppError> {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|error| AppError::Internal {
                message: format!("build http client failed: {error}"),
            })?;
        Ok(WorkBuddyRefresher {
            backend_id,
            profile,
            client,
        })
    }
}

#[async_trait]
impl crate::accounts::remote::CredentialRefresher for WorkBuddyRefresher {
    /// 返回后端标识。
    fn backend_id(&self) -> &str {
        &self.backend_id
    }

    /// 用 refresh_token 换新令牌，并写回凭证 JSON。
    async fn refresh(&self, account: &Account) -> Result<Account, AppError> {
        let mut credential = WorkBuddyCredential::parse(account)?;
        if credential.refresh_token.is_empty() {
            return Err(AppError::bad_request(
                "workbuddy credential has no refresh_token; sign in again",
            ));
        }
        let url = format!(
            "{}/v2/plugin/auth/token/refresh",
            self.profile.region.base()
        );
        let mut builder = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .header("x-refresh-token", &credential.refresh_token)
            .header("x-auth-refresh-source", "plugin")
            .body("{}");
        for (name, value) in common_headers(&self.profile) {
            builder = builder.header(name, value);
        }
        if !credential.enterprise_id.is_empty() {
            builder = builder.header("x-enterprise-id", &credential.enterprise_id);
        }
        let response = builder.send().await?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(AppError::Upstream(UpstreamError::status(
                status.as_u16(),
                body,
            )));
        }
        let parsed: Value = serde_json::from_str(&body).map_err(|error| {
            AppError::Upstream(UpstreamError {
                status: None,
                code: "invalid_refresh_response".to_string(),
                message: format!("invalid refresh JSON: {error}"),
                retryable: false,
                execution_unknown: false,
            })
        })?;
        let data = parsed.get("data").unwrap_or(&parsed);
        let access = data
            .get("accessToken")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                AppError::Upstream(UpstreamError {
                    status: None,
                    code: "missing_access_token".to_string(),
                    message: "refresh response did not contain accessToken".to_string(),
                    retryable: false,
                    execution_unknown: false,
                })
            })?;
        credential.access_token = access.to_string();
        if let Some(refresh) = data.get("refreshToken").and_then(Value::as_str)
            && !refresh.is_empty()
        {
            credential.refresh_token = refresh.to_string();
        }
        if let Some(domain) = data.get("domain").and_then(Value::as_str)
            && !domain.is_empty()
        {
            credential.domain = domain.to_string();
        }
        apply_expiry(&mut credential, data);
        let mut updated = account.clone();
        updated.credential =
            serde_json::to_string(&credential).map_err(|error| AppError::Internal {
                message: format!("serialize credential failed: {error}"),
            })?;
        Ok(updated)
    }
}

/// 把 `expiresIn` / `refreshExpiresIn` 换算成绝对时间。
fn apply_expiry(credential: &mut WorkBuddyCredential, data: &Value) {
    let now = crate::records::now_seconds();
    if let Some(seconds) = data.get("expiresIn").and_then(Value::as_u64) {
        credential.expires_at = now + seconds;
    }
    if let Some(seconds) = data.get("refreshExpiresIn").and_then(Value::as_u64) {
        credential.refresh_expires_at = now + seconds;
    }
}

/// WorkBuddy 额度读取器。
pub struct WorkBuddyQuota {
    backend: WorkBuddyBackend,
}

impl WorkBuddyQuota {
    /// 构造读取器。复用后端实例以共享请求头与解析逻辑。
    pub fn new(
        backend_id: String,
        name: String,
        profile: WorkBuddyProfile,
    ) -> Result<Self, AppError> {
        Ok(WorkBuddyQuota {
            backend: WorkBuddyBackend::new(backend_id, name, profile, Vec::new(), Vec::new())?,
        })
    }
}

#[async_trait]
impl crate::accounts::remote::QuotaReader for WorkBuddyQuota {
    /// 返回后端标识。
    fn backend_id(&self) -> &str {
        &self.backend.id
    }

    /// 读取额度。
    async fn read(
        &self,
        account: &Account,
    ) -> Result<crate::accounts::remote::QuotaInfo, AppError> {
        let credential = WorkBuddyCredential::parse(account)?;
        credential.validate()?;
        self.backend.read_quota(&credential, &account.id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 实际账本八位小数不能取整，混合单位或缺失金额不参与余额排序。
    #[test]
    fn routing_balance_preserves_real_credit_precision_and_checks_units() {
        use crate::accounts::quota::decimal_string;
        let packages = vec![
            json!({"CapacityUnit":"credits","CycleRemainCapacity":"300"}),
            json!({"CapacityUnit":"credits","CycleRemainCapacity":"318.80000012"}),
            json!({"CapacityUnit":"credits","CycleRemainCapacity":"1500"}),
        ];
        assert_eq!(
            decimal_string(routing_credit_balance(&packages).unwrap()),
            "2118.80000012"
        );
        let mut mixed = packages.clone();
        mixed[1]["CapacityUnit"] = json!("requests");
        assert!(routing_credit_balance(&mixed).is_none());
        mixed = packages;
        mixed[1]["CycleRemainCapacity"] = Value::Null;
        assert!(routing_credit_balance(&mixed).is_none());
    }

    /// 区域默认值与端点必须正确。
    #[test]
    fn region_endpoints() {
        assert_eq!(WorkBuddyRegion::Cn.base(), "https://www.workbuddy.cn");
        assert_eq!(WorkBuddyRegion::Intl.base(), "https://www.workbuddy.ai");
        assert_eq!(
            WorkBuddyRegion::from_options(r#"{"region":"intl"}"#).expect("region"),
            WorkBuddyRegion::Intl
        );
        assert_eq!(
            WorkBuddyRegion::from_options("").expect("region"),
            WorkBuddyRegion::Cn
        );
    }

    /// 凭证缺失必需字段要报错。
    #[test]
    fn validates_credential() {
        assert!(WorkBuddyCredential::default().validate().is_err());
        let credential = WorkBuddyCredential {
            access_token: "at".into(),
            uid: "u".into(),
            ..WorkBuddyCredential::default()
        };
        assert!(credential.validate().is_ok());
    }

    /// 请求头必须带官方客户端标识、会话头族与通道校验所需的身份字段。
    #[test]
    fn chat_headers_include_client_identity() {
        let profile = profile_from_options("").expect("profile");
        let credential = WorkBuddyCredential {
            access_token: "at".into(),
            uid: "u1".into(),
            ..WorkBuddyCredential::default()
        };
        let channel = ChatChannel {
            conversation_id: uuid::Uuid::new_v4().to_string(),
            request_id: random_hex(16),
            message_id: uuid_v7_hex(1_700_000_000_000),
            acp_connection_id: uuid::Uuid::new_v4().to_string(),
        };
        let headers = chat_headers(&profile, &credential, &channel);
        let find = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(find("authorization"), Some("Bearer at"));
        assert_eq!(find("x-user-id"), Some("u1"));
        assert_eq!(find("x-product"), Some("WorkBuddy"));
        assert_eq!(find("x-no-enterprise-id"), Some("1"));
        assert_eq!(
            find("x-conversation-id"),
            Some(channel.conversation_id.as_str())
        );
        assert_eq!(
            find("x-conversation-request-id"),
            Some(channel.request_id.as_str())
        );
        assert_eq!(
            find("x-conversation-message-id"),
            Some(channel.message_id.as_str())
        );
        assert_eq!(find("x-request-id"), Some(channel.message_id.as_str()));
        assert_eq!(
            find("acp-connection-id"),
            Some(channel.acp_connection_id.as_str())
        );
        assert_eq!(find("x-agent-intent"), Some("craft"));
        assert_eq!(find("x-agent-purpose"), Some("conversation"));
        assert_eq!(find("x-agent-type"), Some("main"));
        assert_eq!(find("x-private-data"), Some("true"));
        assert!(find("origin").is_none());
        assert!(find("referer").is_none());
        let trace_id = find("x-b3-traceid").expect("b3 trace id");
        assert_eq!(find("x-trace-id"), Some(trace_id));
        assert_eq!(find("x-b3-sampled"), Some("1"));
        assert_eq!(
            find("traceparent"),
            Some(format!("00-{trace_id}-{}-01", find("x-b3-spanid").expect("span")).as_str())
        );
        assert_eq!(profile.login_platform, "workbuddy");
        assert_eq!(profile.cli_version, "2.176.0");
        assert_eq!(profile.deployment_type, "SaaS");
        assert!(find("user-agent").expect("ua").starts_with("WorkBuddy/"));
    }

    /// 会话头族按空闲时长轮换：会话内稳定、轮次超时换请求 ID。
    #[test]
    fn chat_channels_rotate_by_idle() {
        let channels = ChatChannels::new();
        let start = 1_700_000_000_000;
        let first = channels.claim("account-a", start);
        assert_eq!(first.conversation_id.len(), 36);
        assert_eq!(first.acp_connection_id.len(), 36);
        assert_eq!(first.request_id.len(), 32);
        assert_eq!(first.message_id.len(), 32);
        assert_eq!(first.message_id.chars().nth(12), Some('7'));

        let same_turn = channels.claim("account-a", start + 5 * 60 * 1000);
        assert_eq!(same_turn.conversation_id, first.conversation_id);
        assert_eq!(same_turn.request_id, first.request_id);
        assert_ne!(same_turn.message_id, first.message_id);

        let next_turn = channels.claim("account-a", start + 16 * 60 * 1000);
        assert_eq!(next_turn.conversation_id, first.conversation_id);
        assert_ne!(next_turn.request_id, first.request_id);

        let next_conversation = channels.claim("account-a", start + 47 * 60 * 1000);
        assert_ne!(next_conversation.conversation_id, first.conversation_id);
        assert_ne!(next_conversation.acp_connection_id, first.acp_connection_id);

        let other = channels.claim("account-b", start);
        assert_ne!(other.conversation_id, first.conversation_id);
    }

    /// 模型目录只保留 agent 声明且未禁用的模型。
    #[test]
    fn parses_visible_models() {
        let root = json!({
            "agents": [{"name": "craft", "models": ["m1", "m2"]}],
            "availableModels": ["m1", "m2", "m3"],
            "models": [
                {"id": "m1", "name": "Model One", "credits": "x0.69", "supportsToolCall": true, "maxInputTokens": 200000},
                {"id": "m2", "name": "Model Two", "hidden": true},
                {"id": "m3", "name": "Model Three"},
                {"id": "m4", "name": "Local", "local": true}
            ]
        });
        let models = parse_models(&root);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "m1");
        assert_eq!(models[0].display_name.as_deref(), Some("Model One"));
        assert_eq!(models[0].price_factor, Some(0.69));
        assert_eq!(models[0].capabilities.context_window, Some(200000));
    }

    /// 目录报价携带活动截止时间，未被 agent 允许的便宜模型不进入白名单。
    #[test]
    fn visible_catalog_keeps_promotional_prices_and_expiry() {
        let until = "2026-11-20T00:00:00Z";
        let expires =
            time::OffsetDateTime::parse(until, &time::format_description::well_known::Rfc3339)
                .unwrap()
                .unix_timestamp() as u64;
        let root = json!({
            "agents":[{"name":"craft","models":["m1","m2"]}],
            "availableModels":["m1","m2","m3"],
            "models":[
                {"id":"m1","name":"GLM-5.3","credits":"x0.69"},
                {"id":"m2","name":"Hidden","hidden":true,"credits":"0x"},
                {"id":"m3","name":"Other","credits":"0x"}
            ],
            "modelPromotions":[{"modelIds":["m1","m2","m3"],"discount":{"discountedCredits":"0.08x"},
                "schedule":{"validUntil":until}}]
        });
        let models = parse_models_at(&root, expires - 3600);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "m1");
        assert_eq!(models[0].price_factor, Some(0.08));
        assert_eq!(models[0].original_price_factor, Some(0.69));
        assert_eq!(models[0].price_valid_until, Some(expires));
    }

    /// 模型目录可能带 data 包装。
    #[test]
    fn unwraps_catalog_data() {
        let wrapped = json!({"code": 0, "data": {"agents": []}});
        assert!(catalog_data(&wrapped).get("agents").is_some());
        let plain = json!({"agents": []});
        assert!(catalog_data(&plain).get("agents").is_some());
    }

    /// 请求体必须强制流式并带用量统计。
    #[test]
    fn body_forces_stream_and_usage() {
        let request = ChatRequest {
            protocol: crate::core::Protocol::Chat,
            model: "m1".into(),
            messages: vec![Message::text(Role::User, "hi")],
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: Some(64),
            temperature: Some(0.2),
            top_p: None,
            stop: Vec::new(),
            reasoning_effort: None,
            stream: false,
            session: None,
        };
        let body = build_body(&request);
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["messages"][0]["content"], "hi");
    }

    /// 过期时间换算必须基于当前时间。
    #[test]
    fn applies_expiry() {
        let mut credential = WorkBuddyCredential::default();
        apply_expiry(&mut credential, &json!({"expiresIn": 3600}));
        assert!(credential.expires_at > crate::records::now_seconds());
        assert!(!credential.expires_soon());
    }
}
