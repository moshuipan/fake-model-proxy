//! Qoder 私有协议适配器。
//!
//! 协议事实来自对 qoder2api 的只读审计（commit `5bd4083`）：
//! COSY 会话签名 + 自定义字母表编码的请求体 + 双层 SSE 信封。
//! 真实可用性由本机账号的设备码登录验证。

pub mod cosy;
pub mod oauth;

use crate::accounts::{Account, AccountPool, FailureKind};
use crate::backend::{Backend, CostTier};
use crate::core::error::AppError;
use crate::core::stream::EventStream;
use crate::core::{
    Capabilities, ChatRequest, ContentPart, Event, FinishReason, Message, ModelInfo, Role,
    ToolCallDelta, UpstreamError, Usage,
};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

/// 事件通道容量。
const EVENT_CHANNEL_CAPACITY: usize = 32;

/// 旧协议默认版本；与上游下发的模型清单范围绑定。
const LEGACY_COSY_VERSION: &str = "1.0.10";
/// 旧协议默认客户端类型。
const LEGACY_CLIENT_TYPE: &str = "5";

/// Qoder 区域端点。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum QoderRegion {
    /// 国际站。
    Global,
    /// 国内站。
    Cn,
}

impl QoderRegion {
    /// 设备授权页。
    pub fn device_login_base(self) -> &'static str {
        match self {
            QoderRegion::Global => "https://qoder.com/device/selectAccounts",
            QoderRegion::Cn => "https://qoder.com.cn/device/selectAccounts",
        }
    }

    /// 设备令牌轮询端点。
    pub fn poll_endpoint(self) -> &'static str {
        match self {
            QoderRegion::Global => "https://openapi.qoder.sh/api/v1/deviceToken/poll",
            QoderRegion::Cn => "https://openapi.qoder.com.cn/api/v1/deviceToken/poll",
        }
    }

    /// 用户信息端点。
    pub fn userinfo_endpoint(self) -> &'static str {
        match self {
            QoderRegion::Global => "https://openapi.qoder.sh/api/v1/userinfo",
            QoderRegion::Cn => "https://openapi.qoder.com.cn/api/v1/userinfo",
        }
    }

    /// 额度端点。
    pub fn quota_endpoint(self) -> &'static str {
        match self {
            QoderRegion::Global => "https://openapi.qoder.sh/api/v2/quota/usage",
            QoderRegion::Cn => "https://openapi.qoder.com.cn/api/v2/quota/usage",
        }
    }

    /// 令牌续期端点（jobToken）。
    pub fn refresh_endpoint(self) -> &'static str {
        match self {
            QoderRegion::Global => "https://center.qoder.sh/algo/api/v3/user/jobToken?Encode=1",
            QoderRegion::Cn => "https://gateway.qoder.com.cn/algo/api/v3/user/jobToken?Encode=1",
        }
    }

    /// 返回对话端点候选。官方客户端会在多个推理集群间切换，写死单点会稳定踩雷。
    pub fn chat_endpoints(self) -> &'static [&'static str] {
        match self {
            QoderRegion::Global => &[
                "https://api1.qoder.sh/algo/api/v2/service/pro/sse/agent_chat_generation?FetchKeys=llm_model_result&AgentId=agent_common&Encode=1",
                "https://api3.qoder.sh/algo/api/v2/service/pro/sse/agent_chat_generation?FetchKeys=llm_model_result&AgentId=agent_common&Encode=1",
                "https://api2.qoder.sh/algo/api/v2/service/pro/sse/agent_chat_generation?FetchKeys=llm_model_result&AgentId=agent_common&Encode=1",
            ],
            QoderRegion::Cn => &[
                "https://gateway.qoder.com.cn/algo/api/v2/service/pro/sse/agent_chat_generation?FetchKeys=llm_model_result&AgentId=agent_common&Encode=1",
            ],
        }
    }

    /// 返回默认对话端点。查询参数是协议的一部分，不能省略。
    pub fn chat_endpoint(self) -> &'static str {
        self.chat_endpoints()[0]
    }

    /// 返回模型目录端点候选。
    pub fn model_list_endpoints(self) -> &'static [&'static str] {
        match self {
            QoderRegion::Global => &[
                "https://api2.qoder.sh/algo/api/v2/model/list?Encode=1",
                "https://api1.qoder.sh/algo/api/v2/model/list?Encode=1",
            ],
            QoderRegion::Cn => &["https://gateway.qoder.com.cn/algo/api/v2/model/list?Encode=1"],
        }
    }

    /// 返回模型目录端点。
    pub fn model_list_endpoint(self) -> &'static str {
        self.model_list_endpoints()[0]
    }

    /// 返回 machine os 头取值。固定报 linux，避免随运行平台漂移。
    pub fn machine_os(self) -> &'static str {
        "x86_64_linux"
    }

    /// 从后端 options 解析区域。缺省为 global。
    pub fn from_options(options: &str) -> Result<Self, AppError> {
        if options.trim().is_empty() {
            return Ok(QoderRegion::Global);
        }
        let parsed: Value = serde_json::from_str(options).map_err(|error| {
            AppError::bad_request(format!("qoder backend options is not valid JSON: {error}"))
        })?;
        Ok(match parsed.get("region").and_then(Value::as_str) {
            Some("cn") => QoderRegion::Cn,
            _ => QoderRegion::Global,
        })
    }
}

/// 账号凭证。字段缺失时由后端报错，不做猜测。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QoderCredential {
    /// 用户标识，同时是设备指纹的派生种子。
    #[serde(default)]
    pub uid: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub aid: String,
    #[serde(default)]
    pub yx_uid: String,
    #[serde(default)]
    pub organization_id: String,
    #[serde(default)]
    pub organization_name: String,
    #[serde(default)]
    pub user_type: String,
    /// 设备码登录得到的长效令牌。
    #[serde(default)]
    pub device_token: String,
    /// 续期令牌。缺失时只能重新登录。
    #[serde(default)]
    pub refresh_token: String,
    /// 设备标识。缺失时按 uid 派生并保持稳定。
    #[serde(default)]
    pub machine_id: String,
    #[serde(default)]
    pub machine_token: String,
    #[serde(default)]
    pub machine_type: String,
    /// 可选的端点覆盖，便于区域或灰度环境。
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub refresh_url: Option<String>,
    #[serde(default)]
    pub quota_url: Option<String>,
    #[serde(default)]
    pub model_list_url: Option<String>,
}

impl QoderCredential {
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
        if self.device_token.is_empty() || self.uid.is_empty() {
            return Err(AppError::bad_request(
                "qoder credential requires device_token and uid; sign in again",
            ));
        }
        Ok(())
    }

    /// 由登录结果构造凭证。设备标识在此处一次算定，之后保持不变。
    pub fn from_login(login: &oauth::LoginResult) -> Self {
        QoderCredential {
            uid: login.uid.clone(),
            name: login.name.clone(),
            aid: login.uid.clone(),
            yx_uid: String::new(),
            organization_id: login.organization_id.clone(),
            organization_name: login.organization_name.clone(),
            user_type: if login.user_type.is_empty() {
                "personal_standard".to_string()
            } else {
                login.user_type.clone()
            },
            device_token: login.device_token.clone(),
            refresh_token: login.refresh_token.clone(),
            machine_id: cosy::derive_machine_id(&login.uid),
            machine_token: cosy::derive_machine_token(&login.uid),
            machine_type: cosy::derive_machine_type(&login.uid),
            ..QoderCredential::default()
        }
    }

    /// 返回签名用的身份视图。
    fn identity(&self) -> cosy::AuthIdentity<'_> {
        cosy::AuthIdentity {
            name: &self.name,
            aid: &self.aid,
            uid: &self.uid,
            yx_uid: &self.yx_uid,
            organization_id: &self.organization_id,
            organization_name: &self.organization_name,
            user_type: if self.user_type.is_empty() {
                "personal_standard"
            } else {
                &self.user_type
            },
            security_oauth_token: &self.device_token,
            refresh_token: &self.refresh_token,
        }
    }

    /// 返回设备三元组，缺失字段按 uid 派生。
    fn machine(&self) -> (String, String, String) {
        let seed = if self.uid.is_empty() {
            self.device_token.as_str()
        } else {
            self.uid.as_str()
        };
        (
            if self.machine_id.is_empty() {
                cosy::derive_machine_id(seed)
            } else {
                self.machine_id.clone()
            },
            if self.machine_token.is_empty() {
                cosy::derive_machine_token(seed)
            } else {
                self.machine_token.clone()
            },
            if self.machine_type.is_empty() {
                cosy::derive_machine_type(seed)
            } else {
                self.machine_type.clone()
            },
        )
    }
}

/// Qoder 后端。
pub struct QoderBackend {
    id: String,
    name: String,
    region: QoderRegion,
    models: Vec<String>,
    pool: AccountPool,
    client: reqwest::Client,
}

impl QoderBackend {
    /// 构造后端。accounts 由调用方从存储加载。
    pub fn new(
        id: String,
        name: String,
        region: QoderRegion,
        models: Vec<String>,
        accounts: Vec<Account>,
    ) -> Result<Self, AppError> {
        Self::new_with_timeout(
            id,
            name,
            region,
            models,
            accounts,
            std::time::Duration::from_secs(10),
        )
    }

    /// 对聊天实例固定连接预算，端点恢复继续共享总生成预算。
    pub fn new_with_timeout(
        id: String,
        name: String,
        region: QoderRegion,
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
        Ok(QoderBackend {
            id,
            name,
            region,
            models,
            pool: AccountPool::new(accounts),
            client,
        })
    }

    /// 仅对确认未执行的非限流错误换端点；限流交给网关冷却及换账号。
    async fn send_chat_endpoints(
        &self,
        credential: &QoderCredential,
        encoded: &str,
        endpoints: Vec<String>,
        model: &str,
        cancel: &CancellationToken,
        account_id: &str,
    ) -> Result<reqwest::Response, AppError> {
        let mut last_error: Option<AppError> = None;
        for (endpoint_index, endpoint) in endpoints.into_iter().enumerate() {
            if endpoint_index > 0 {
                crate::gateway::retry::additional_submission().await?;
            }
            match send_chat_request(
                &self.client,
                self.region,
                credential,
                encoded,
                &endpoint,
                model,
                cancel,
            )
            .await
            {
                Ok(response) => return Ok(response),
                Err(error) => {
                    // 同账号换端点会放大限流；保留 Retry-After，由网关决定冷却范围。
                    if error.status().as_u16() == 429 {
                        return Err(error);
                    }
                    let retryable_status = match &error {
                        AppError::Upstream(upstream) => {
                            upstream.retryable && !upstream.execution_unknown
                        }
                        _ => false,
                    };
                    if retryable_status {
                        last_error = Some(error);
                        continue;
                    }
                    let status = match &error {
                        AppError::Upstream(upstream) => upstream.status,
                        _ => None,
                    };
                    self.pool
                        .report_failure(account_id, FailureKind::classify(status, ""));
                    return Err(error);
                }
            }
        }
        self.pool.report_failure(account_id, FailureKind::Transient);
        Err(last_error.unwrap_or_else(|| {
            AppError::Upstream(UpstreamError {
                status: Some(503),
                code: "no_chat_endpoint".to_string(),
                message: "no qoder chat endpoint available".to_string(),
                retryable: true,
                execution_unknown: false,
            })
        }))
    }

    /// 构造请求体。字段与官方客户端模板对齐，未使用的可选项不写入。
    fn build_body(&self, request: &ChatRequest, credential: &QoderCredential) -> Value {
        let prompt = request.last_user_text().unwrap_or_default();
        let is_reasoning = request.reasoning_effort.is_some();
        let is_vl = request.has_image();
        let request_id = uuid::Uuid::new_v4().to_string();
        let messages = request
            .messages
            .iter()
            .filter_map(encode_message)
            .collect::<Vec<_>>();
        let messages = if messages.is_empty() {
            vec![encode_user_message(&prompt)]
        } else {
            messages
        };

        let mut parameters = json!({
            "max_tokens": request.max_tokens.unwrap_or(32768),
        });
        if let Some(temperature) = request.temperature {
            parameters["temperature"] = json!(temperature);
        }
        if let Some(top_p) = request.top_p {
            parameters["top_p"] = json!(top_p);
        }
        if !request.stop.is_empty() {
            parameters["stop"] = json!(request.stop);
        }

        let mut body = json!({
            "request_id": request_id,
            "request_set_id": uuid::Uuid::new_v4().to_string(),
            "chat_record_id": request_id,
            "stream": true,
            "chat_task": "FREE_INPUT",
            "chat_context": {
                "chatPrompt": prompt,
                "extra": {
                    "context": [],
                    "modelConfig": {
                        "is_reasoning": is_reasoning,
                        "is_vl": is_vl,
                        "key": request.model,
                    },
                    "originalContent": {"type": "text", "text": prompt},
                },
                "features": [],
                "imageUrls": null,
                "text": {"type": "text", "text": prompt},
            },
            "image_urls": null,
            "is_reply": true,
            "is_retry": false,
            "session_id": request
                .session
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            "code_language": "",
            "source": 1,
            "version": "3",
            "chat_prompt": prompt,
            "parameters": parameters,
            "aliyun_user_type": if credential.user_type.is_empty() {
                "personal_standard"
            } else {
                credential.user_type.as_str()
            },
            "session_type": "qoder",
            "agent_id": "agent_common",
            "task_id": "common",
            "model_config": {
                "key": request.model,
                "display_name": request.model,
                "model": "",
                "format": "openai",
                "is_vl": is_vl,
                "is_reasoning": is_reasoning,
                "api_key": "",
                "url": "",
                "source": "system",
                "max_input_tokens": 180000,
            },
            "messages": messages,
            "business": {
                "product": "ide",
                "version": "1.1.3",
                "type": "agent",
                "id": uuid::Uuid::new_v4().to_string(),
                "name": truncate_chars(&prompt, 30),
                "begin_at": crate::records::now_seconds() * 1000,
                "stage": "start",
            },
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
                crate::core::ToolChoice::Auto => json!("auto"),
                crate::core::ToolChoice::None => json!("none"),
                crate::core::ToolChoice::Required => json!("required"),
                crate::core::ToolChoice::Named(name) => {
                    json!({"type": "function", "function": {"name": name}})
                }
            };
        }
        body
    }

    /// 构造 COSY 鉴权头。签名基于已编码的 body 原文。
    fn build_headers(
        &self,
        credential: &QoderCredential,
        body: &str,
        endpoint: &str,
        accept: &str,
        model: Option<&str>,
    ) -> Result<Vec<(String, String)>, AppError> {
        build_cosy_headers(self.region, credential, body, endpoint, accept, model)
    }

    /// 拉取上游模型目录。失败时向调用方报错，不伪造清单。
    async fn fetch_models(&self, credential: &QoderCredential) -> Result<Vec<ModelInfo>, AppError> {
        let urls = match &credential.model_list_url {
            Some(url) => vec![url.clone()],
            None => self
                .region
                .model_list_endpoints()
                .iter()
                .map(|url| (*url).to_string())
                .collect(),
        };
        let mut last_error: Option<AppError> = None;
        for url in urls {
            let headers = self.build_headers(credential, "", &url, "application/json", None)?;
            let mut builder = self.client.get(&url);
            for (name, value) in headers {
                builder = builder.header(name, value);
            }
            let response = match builder.send().await {
                Ok(response) => response,
                Err(error) => {
                    last_error = Some(AppError::from(error));
                    continue;
                }
            };
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            if !status.is_success() {
                last_error = Some(AppError::Upstream(UpstreamError::status(
                    status.as_u16(),
                    body,
                )));
                if status.is_server_error() {
                    continue;
                }
                return Err(last_error.expect("just assigned"));
            }
            let parsed: Value = serde_json::from_str(&body).map_err(|error| {
                AppError::Upstream(UpstreamError {
                    status: None,
                    code: "invalid_model_list".to_string(),
                    message: format!("invalid model list JSON: {error}"),
                    retryable: false,
                    execution_unknown: false,
                })
            })?;
            return Ok(parse_models(&parsed));
        }
        Err(last_error.unwrap_or_else(|| {
            AppError::Upstream(UpstreamError {
                status: None,
                code: "no_model_list_endpoint".to_string(),
                message: "no qoder model list endpoint available".to_string(),
                retryable: true,
                execution_unknown: false,
            })
        }))
    }
}

#[async_trait]
impl Backend for QoderBackend {
    /// 当前私有封包只编码 user 图文，其他角色的图片必须拒收，避免静默丢失。
    fn validate_request(&self, request: &ChatRequest) -> Result<(), String> {
        if request
            .messages
            .iter()
            .any(|message| message.role != Role::User && message.has_image())
        {
            return Err(
                "image_role_unsupported: Qoder can encode images only in user messages".into(),
            );
        }
        Ok(())
    }

    /// Qoder 私有请求封包限制输入为十八万 token，与目录上下文分别计算。
    fn model_limits(&self, model: &ModelInfo) -> crate::core::TokenLimits {
        let mut limits = model.token_limits.clone();
        if limits == crate::core::TokenLimits::default() {
            limits.context_window = model.capabilities.context_window;
        }
        limits.max_input_tokens = Some(limits.max_input_tokens.unwrap_or(180_000).min(180_000));
        limits
    }

    /// 路由使用账号池当前状态，排除禁用和冷却账号。
    fn routing_accounts(&self) -> Option<Vec<crate::accounts::AccountState>> {
        Some(self.pool.snapshot())
    }

    /// 复用 Qoder 额度读取器，保留其原始资源单位。
    async fn routing_quota(
        &self,
        id: &str,
    ) -> Result<crate::accounts::remote::QuotaInfo, AppError> {
        use crate::accounts::remote::QuotaReader;
        let account = self
            .pool
            .get_available(id)
            .ok_or_else(|| AppError::bad_request("account unavailable"))?;
        QoderQuota::new(self.id.clone(), self.region)?
            .read(&account)
            .await
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
        "qoder"
    }

    /// Qoder 支持工具调用与推理输出，图片能力保守关闭。
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tools: true,
            parallel_tools: false,
            images: false,
            reasoning: true,
            context_window: None,
        }
    }

    /// Qoder 额度属于订阅额度。
    fn cost_tier(&self) -> CostTier {
        CostTier::Subscription
    }

    /// 账号后端默认优先级低于官方网关。
    fn priority(&self) -> u8 {
        200
    }

    /// 返回配置模型。该清单同时充当模型寻址白名单。
    fn static_models(&self) -> Vec<String> {
        self.models.clone()
    }

    /// 返回模型清单：有账号时读取上游目录，无账号时回落到配置。
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
                message: "all qoder accounts are cooling or disabled".to_string(),
                retryable: true,
                execution_unknown: false,
            })
        })?;
        let credential = QoderCredential::parse(&account)?;
        credential.validate()?;
        match self.fetch_models(&credential).await {
            Ok(models) if !models.is_empty() => {
                self.pool.report_success(&account.id);
                Ok(models)
            }
            Ok(_) => Err(AppError::Upstream(UpstreamError {
                status: None,
                code: "empty_model_list".to_string(),
                message: "upstream model list is empty".to_string(),
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

    /// 发起生成，按账号池选择凭证。
    async fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, AppError> {
        self.validate_request(&request)
            .map_err(AppError::bad_request)?;
        let account = self.pool.select().ok_or_else(|| {
            AppError::Upstream(UpstreamError {
                status: Some(503),
                code: "no_available_account".to_string(),
                message: "all qoder accounts are cooling or disabled".to_string(),
                retryable: true,
                execution_unknown: false,
            })
        })?;
        let credential = QoderCredential::parse(&account)?;
        credential.validate()?;
        let body = self.build_body(&request, &credential);
        let body_text = serde_json::to_string(&body).map_err(|error| AppError::Internal {
            message: format!("serialize qoder body failed: {error}"),
        })?;
        let encoded = cosy::encode_body(body_text.as_bytes());
        let endpoints = match &credential.endpoint {
            Some(endpoint) => vec![endpoint.clone()],
            None => self
                .region
                .chat_endpoints()
                .iter()
                .map(|endpoint| (*endpoint).to_string())
                .collect(),
        };

        let response = self
            .send_chat_endpoints(
                &credential,
                &encoded,
                endpoints,
                &request.model,
                &cancel,
                &account.id,
            )
            .await?;
        self.pool.report_success(&account.id);

        let (sender, receiver) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        tokio::spawn(async move {
            pump_stream(response, sender, cancel).await;
        });
        Ok(Box::pin(ReceiverStream::new(receiver)))
    }
}

/// 发送一次推理请求，返回已建流的响应。
async fn send_chat_request(
    client: &reqwest::Client,
    region: QoderRegion,
    credential: &QoderCredential,
    encoded: &str,
    endpoint: &str,
    model: &str,
    cancel: &CancellationToken,
) -> Result<reqwest::Response, AppError> {
    let headers = build_cosy_headers(
        region,
        credential,
        encoded,
        endpoint,
        "text/event-stream",
        Some(model),
    )?;
    let mut builder = client.post(endpoint).body(encoded.to_string());
    for (name, value) in headers {
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
        return Err(decode_upstream_error(status.as_u16(), &text));
    }
    Ok(response)
}

/// 构造 COSY 鉴权头。签名基于已编码的 body 原文。
///
/// 自由函数形态供流式重试任务复用，不依赖后端实例。
fn build_cosy_headers(
    region: QoderRegion,
    credential: &QoderCredential,
    body: &str,
    endpoint: &str,
    accept: &str,
    model: Option<&str>,
) -> Result<Vec<(String, String)>, AppError> {
    let session = cosy::new_session(&credential.identity())?;
    let payload_b64 = cosy::build_payload_b64(&session.info);
    let date = cosy::unix_seconds().to_string();
    let path = cosy::path_signature(endpoint);
    let signature = cosy::sign_request(&payload_b64, &session.cosy_key, &date, body, &path);
    let bearer = cosy::compose_bearer(&payload_b64, &signature);
    let (machine_id, machine_token, machine_type) = credential.machine();
    let mut headers = vec![
        ("authorization".to_string(), bearer),
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), accept.to_string()),
        ("cache-control".to_string(), "no-cache".to_string()),
        ("cosy-key".to_string(), session.cosy_key),
        ("cosy-user".to_string(), credential.uid.clone()),
        ("cosy-date".to_string(), date),
        ("cosy-version".to_string(), LEGACY_COSY_VERSION.to_string()),
        (
            "cosy-clienttype".to_string(),
            LEGACY_CLIENT_TYPE.to_string(),
        ),
        ("cosy-machinetype".to_string(), machine_type),
        ("cosy-machineid".to_string(), machine_id),
        ("cosy-machinetoken".to_string(), machine_token),
        (
            "cosy-machineos".to_string(),
            region.machine_os().to_string(),
        ),
        ("cosy-scene".to_string(), "assistant".to_string()),
        ("cosy-business-product".to_string(), "ide".to_string()),
        ("cosy-business-type".to_string(), "agent".to_string()),
        ("cosy-data-policy".to_string(), "agree".to_string()),
        ("login-version".to_string(), "v2".to_string()),
        ("user-agent".to_string(), "Go-http-client/2.0".to_string()),
    ];
    // 官方客户端只在推理流里带主机名，其他接口不带。
    if accept == "text/event-stream" {
        headers.push((
            "cosy-machinehostname".to_string(),
            cosy::derive_machine_hostname(&credential.uid),
        ));
    }
    if let Some(model) = model {
        headers.push(("x-model-key".to_string(), model.to_string()));
        headers.push(("x-model-source".to_string(), "system".to_string()));
    }
    Ok(headers)
}

/// 编码单条消息。user 走 contents 数组，其余角色走 content 字符串。
fn encode_message(message: &Message) -> Option<Value> {
    let role = match message.role.normalized() {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
        Role::Developer => "system",
    };
    let texts = message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.clone()),
            ContentPart::ImageUrl { .. } => None,
        })
        .collect::<Vec<_>>();

    if role == "user" {
        let contents = message
            .content
            .iter()
            .map(|part| match part {
                ContentPart::Text { text } => json!({"type": "text", "text": text}),
                ContentPart::ImageUrl { url, .. } => {
                    json!({"type": "image_url", "image_url": {"url": url}})
                }
            })
            .collect::<Vec<_>>();
        let contents = if contents.is_empty() {
            vec![json!({"type": "text", "text": ""})]
        } else {
            contents
        };
        return Some(json!({
            "role": "user",
            "content": "",
            "contents": contents,
            "response_meta": blank_response_meta(),
            "reasoning_content_signature": "",
        }));
    }

    let text = texts.join("");
    let mut encoded = json!({
        "role": role,
        "content": text,
        "response_meta": blank_response_meta(),
        "reasoning_content_signature": "",
    });
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
    Some(encoded)
}

/// 构造一条纯文本 user 消息。
fn encode_user_message(text: &str) -> Value {
    json!({
        "role": "user",
        "content": "",
        "contents": [{"type": "text", "text": text}],
        "response_meta": blank_response_meta(),
        "reasoning_content_signature": "",
    })
}

/// 上游要求的空 usage 占位。
fn blank_response_meta() -> Value {
    json!({
        "id": "",
        "usage": {
            "prompt_tokens": 0,
            "completion_tokens": 0,
            "total_tokens": 0,
            "completion_tokens_details": {"reasoning_tokens": 0},
            "prompt_tokens_details": {"cached_tokens": 0},
        },
    })
}

/// 截断到指定字符数，避免多字节截断。
fn truncate_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// 解析模型目录。按 assistant / developer / chat 顺序取第一个非空分类。
fn parse_models(root: &Value) -> Vec<ModelInfo> {
    for category in ["assistant", "developer", "chat"] {
        let Some(items) = root.get(category).and_then(Value::as_array) else {
            continue;
        };
        let models = items
            .iter()
            .filter_map(|item| {
                let key = item.get("key").and_then(Value::as_str)?;
                if key.is_empty() {
                    return None;
                }
                let enabled = item.get("enable").and_then(Value::as_bool).unwrap_or(true);
                if !enabled {
                    return None;
                }
                let token_limits = qoder_token_limits(item);
                let default_window = token_limits
                    .context_tiers
                    .iter()
                    .find(|tier| tier.is_default)
                    .map(|tier| tier.tokens);
                Some(ModelInfo {
                    id: key.to_string(),
                    display_name: item
                        .get("display_name")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                    price_factor: read_price_factor(item, "price_factor"),
                    original_price_factor: read_price_factor(item, "original_price_factor"),
                    price_valid_until: None,
                    capabilities: crate::backend::catalog::capabilities(
                        item,
                        &Capabilities {
                            tools: true,
                            parallel_tools: false,
                            images: false,
                            reasoning: false,
                            context_window: default_window
                                .or(token_limits.context_window)
                                .or(token_limits.max_input_tokens),
                        },
                    ),
                    token_limits,
                })
            })
            .collect::<Vec<_>>();
        if !models.is_empty() {
            return models;
        }
    }
    Vec::new()
}

/// 读取数值或数字字符串倍率；非法、负值与非有限值保持未知。
fn read_price_factor(item: &Value, key: &str) -> Option<f64> {
    item.get(key)
        .and_then(crate::backend::catalog::numeric_factor)
}

/// 保留全部有效上下文档位；多个不同默认值表示未知默认，不任意选择。
fn qoder_token_limits(item: &Value) -> crate::core::TokenLimits {
    let mut limits = crate::backend::catalog::token_limits(item);
    let mut tiers = item
        .get("context_config")
        .and_then(Value::as_object)
        .map(|config| {
            config
                .values()
                .filter_map(|tier| {
                    let tokens = tier
                        .get("token_count")
                        .and_then(crate::backend::catalog::token_count)?;
                    let is_default = tier
                        .get("is_default")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    Some(crate::core::ContextTier { tokens, is_default })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    tiers.sort_unstable_by_key(|tier| tier.tokens);
    let default_tokens = tiers
        .iter()
        .filter(|tier| tier.is_default)
        .map(|tier| tier.tokens)
        .collect::<std::collections::BTreeSet<_>>();
    let mut unique: Vec<crate::core::ContextTier> = Vec::new();
    for tier in tiers {
        if let Some(previous) = unique
            .last_mut()
            .filter(|previous| previous.tokens == tier.tokens)
        {
            previous.is_default |= tier.is_default;
        } else {
            unique.push(tier);
        }
    }
    if default_tokens.len() > 1 {
        for tier in &mut unique {
            tier.is_default = false;
        }
    }
    limits.context_window = unique
        .last()
        .map(|tier| tier.tokens)
        .or(limits.context_window);
    limits.context_tiers = unique;
    limits
}

/// 单次读取的结果。错误暂不发给客户端，由重试层决定是否换流。
struct PumpOutcome {
    emitted: bool,
    finished: bool,
    cancelled: bool,
    error: Option<AppError>,
}

/// 保留完成原因，先交付同帧及尾随用量；完成后最多再等一秒，取消仍覆盖读取。
async fn pump_once(
    response: reqwest::Response,
    sender: &mpsc::Sender<Result<Event, AppError>>,
    cancel: &CancellationToken,
) -> PumpOutcome {
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::<u8>::new();
    let mut emitted = false;
    let mut finish = None;
    let mut deadline = None;
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return PumpOutcome { emitted, finished: false, cancelled: true, error: None };
            }
            _ = async {
                if let Some(deadline) = deadline {
                    tokio::time::sleep_until(deadline).await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => break,
            item = stream.next() => item,
        };
        let Some(item) = chunk else { break };
        let bytes = match item {
            Ok(bytes) => bytes,
            Err(error) => {
                return PumpOutcome {
                    emitted,
                    finished: false,
                    cancelled: false,
                    error: Some(AppError::Upstream(UpstreamError::transport(
                        error.to_string(),
                    ))),
                };
            }
        };
        buffer.extend_from_slice(&bytes);
        while let Some(index) = find_frame_end(&buffer) {
            let frame = buffer.drain(..index).collect::<Vec<u8>>();
            let text = String::from_utf8_lossy(&frame);
            if text.trim().is_empty() {
                continue;
            }
            let decoded = match decode_frame(&text) {
                Ok(decoded) => decoded,
                Err(error) => {
                    return PumpOutcome {
                        emitted,
                        finished: false,
                        cancelled: false,
                        error: Some(error),
                    };
                }
            };
            for event in decoded.events {
                if let Event::Finish(reason) = event {
                    finish.get_or_insert(reason);
                    deadline.get_or_insert(
                        tokio::time::Instant::now() + std::time::Duration::from_secs(1),
                    );
                    continue;
                }
                if matches!(
                    event,
                    Event::Text { .. } | Event::Reasoning { .. } | Event::ToolCall(_)
                ) {
                    emitted = true;
                }
                let sent = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => false,
                    result = sender.send(Ok(event)) => result.is_ok(),
                };
                if !sent {
                    return PumpOutcome {
                        emitted,
                        finished: false,
                        cancelled: true,
                        error: None,
                    };
                }
            }
            if decoded.done {
                finish.get_or_insert(FinishReason::Stop);
                return finish_pump(sender, cancel, emitted, finish).await;
            }
        }
    }
    finish_pump(sender, cancel, emitted, finish).await
}

/// 终态只发送一次，发送前仍可取消；缺少终态交由调用方报告断流。
async fn finish_pump(
    sender: &mpsc::Sender<Result<Event, AppError>>,
    cancel: &CancellationToken,
    emitted: bool,
    finish: Option<FinishReason>,
) -> PumpOutcome {
    if let Some(reason) = finish {
        let sent = tokio::select! {
            biased;
            _ = cancel.cancelled() => false,
            result = sender.send(Ok(Event::Finish(reason))) => result.is_ok(),
        };
        return PumpOutcome {
            emitted,
            finished: sent,
            cancelled: !sent,
            error: None,
        };
    }
    PumpOutcome {
        emitted,
        finished: false,
        cancelled: false,
        error: None,
    }
}

/// 将单次上游流送给调用方，排队错误立即返回，不自动重放请求。
async fn pump_stream(
    response: reqwest::Response,
    sender: mpsc::Sender<Result<Event, AppError>>,
    cancel: CancellationToken,
) {
    let outcome = pump_once(response, &sender, &cancel).await;
    tracing::debug!(
        emitted = outcome.emitted,
        finished = outcome.finished,
        cancelled = outcome.cancelled,
        "qoder upstream read finished"
    );
    if outcome.cancelled {
        return;
    }
    if let Some(error) = outcome.error {
        tracing::debug!(
            code = error.code(),
            retry_after_seconds = queue_retry_after(&error),
            "qoder upstream returned an error"
        );
        let _ = sender.send(Err(error)).await;
        return;
    }
    if !outcome.finished {
        let _ = sender
            .send(Err(AppError::Upstream(UpstreamError {
                status: None,
                code: "incomplete_stream".into(),
                message: "upstream stream ended before completion".into(),
                retryable: false,
                execution_unknown: true,
            })))
            .await;
    }
}

/// 读取已归一排队错误的建议时间，旧格式仅用于观测原始上游。
pub(crate) fn queue_retry_after(error: &AppError) -> Option<u64> {
    let AppError::Upstream(upstream) = error else {
        return None;
    };
    if upstream.code == "qoder_queued" {
        return numeric_field(&upstream.message, "建议");
    }
    if !upstream.message.contains("10605") && !upstream.message.contains("isQueued") {
        return None;
    }
    let wait = numeric_field(&upstream.message, "retryAfterSeconds")
        .or_else(|| numeric_field(&upstream.message, "waitTime"))
        .unwrap_or(15);
    Some(wait)
}

/// 在可能多层转义的文本里取某个键后的第一个数字。
fn numeric_field(text: &str, key: &str) -> Option<u64> {
    let start = text.find(key)? + key.len();
    let mut digits = String::new();
    for ch in text[start..].chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
            if digits.len() >= 6 {
                break;
            }
        } else if !digits.is_empty() {
            break;
        }
    }
    digits.parse().ok()
}

/// 找到 SSE 帧尾。
fn find_frame_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| index + 2)
        .into_iter()
        .chain(
            buffer
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|index| index + 4),
        )
        .min()
}

struct DecodedFrame {
    events: Vec<Event>,
    done: bool,
}

/// 解析单帧，兼容双层信封。
fn decode_frame(frame: &str) -> Result<DecodedFrame, AppError> {
    let mut events = Vec::new();
    let mut done = frame.lines().any(|line| line.trim() == "event: finish");
    for line in frame.lines() {
        let line = line.trim_start();
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[NOT_EXCEED_QUOTA]" {
            continue;
        }
        if data == "[DONE]" {
            done = true;
            continue;
        }
        if data == "[EXCEED_QUOTA]" {
            return Err(AppError::Upstream(UpstreamError {
                status: Some(402),
                code: "qoder_quota_exhausted".into(),
                message: "Qoder 上游确认额度已耗尽".into(),
                retryable: false,
                execution_unknown: false,
            }));
        }
        if data.starts_with("[NOTIFICATIONS]#") {
            continue;
        }
        let parsed: Value = serde_json::from_str(data).map_err(|error| {
            AppError::Upstream(UpstreamError {
                status: None,
                code: "invalid_upstream_chunk".to_string(),
                message: format!("invalid qoder chunk: {error}"),
                retryable: false,
                execution_unknown: false,
            })
        })?;
        events.extend(decode_envelope(&parsed)?);
    }
    Ok(DecodedFrame { events, done })
}

/// 解码信封。外层可能带 statusCodeValue 与字符串 body。
fn decode_envelope(value: &Value) -> Result<Vec<Event>, AppError> {
    if let Some(raw) = value.get("statusCodeValue") {
        let status = raw
            .as_i64()
            .or_else(|| raw.as_str().and_then(|text| text.parse().ok()))
            .unwrap_or(502);
        if status != 200 {
            let detail = value
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            return Err(decode_upstream_error(status.clamp(0, 599) as u16, &detail));
        }
    }
    let inner = match value.get("body").and_then(Value::as_str) {
        Some(body) => match serde_json::from_str::<Value>(body) {
            Ok(parsed) => parsed,
            Err(_) => {
                return Err(AppError::Upstream(UpstreamError {
                    status: None,
                    code: "invalid_upstream_chunk".into(),
                    message: "Qoder 内层响应不是有效 JSON".into(),
                    retryable: false,
                    execution_unknown: true,
                }));
            }
        },
        None => value.clone(),
    };
    decode_delta(&inner)
}

/// 解开最多五层错误 JSON，只将明确的排队或资源限制转换为可操作提示。
fn normalize_upstream_error(detail: &str) -> Option<AppError> {
    let mut value: Value = serde_json::from_str(detail).ok()?;
    let mut queue_code = false;
    let mut queued = None;
    let mut retry_after = None;
    let mut wait_time = None;
    let mut queue_count = None;
    let mut resource_limit = false;
    let mut service_available = None;
    for _ in 0..5 {
        queue_code |= value
            .get("code")
            .is_some_and(|code| code.as_str() == Some("10605") || code.as_u64() == Some(10605));
        resource_limit |= value.get("subCode").and_then(Value::as_str) == Some("RESOURCE_LIMIT");
        queued = value.get("isQueued").and_then(Value::as_bool).or(queued);
        retry_after = value
            .get("retryAfterSeconds")
            .and_then(Value::as_u64)
            .or(retry_after);
        wait_time = value.get("waitTime").and_then(Value::as_u64).or(wait_time);
        queue_count = value
            .get("queueCount")
            .and_then(Value::as_u64)
            .or(queue_count);
        service_available = value
            .get("serviceAvailable")
            .and_then(Value::as_bool)
            .or(service_available);
        let next = value
            .get("message")
            .or_else(|| value.get("body"))
            .unwrap_or(&value);
        match next
            .as_str()
            .and_then(|text| serde_json::from_str(text).ok())
        {
            Some(parsed) => value = parsed,
            None => break,
        }
    }
    let is_queue = queued == Some(true)
        || retry_after.is_some_and(|seconds| seconds > 0)
        || (queue_code && queued.is_none() && !resource_limit);
    if is_queue {
        let seconds = retry_after
            .filter(|seconds| *seconds > 0)
            .or_else(|| wait_time.filter(|seconds| *seconds > 0))
            .unwrap_or(15);
        let position = queue_count
            .filter(|count| *count > 0)
            .map(|count| format!("，前方约 {count} 个请求"))
            .unwrap_or_default();
        return Some(AppError::Upstream(UpstreamError {
            status: Some(429),
            code: "qoder_queued".into(),
            message: format!(
                "Qoder 当前模型正在排队{position}，上游建议 {seconds} 秒后重试。可稍后重试或自行选择其他模型。"
            ),
            retryable: true,
            execution_unknown: false,
        }));
    }
    if resource_limit {
        return Some(AppError::Upstream(UpstreamError {
            status: Some(403),
            code: "qoder_resource_limit".into(),
            message: if service_available == Some(false) {
                "Qoder 当前模型对账号不可用，请在官方客户端检查套餐权限或额度。".into()
            } else {
                "Qoder 当前模型资源受限，请稍后重试或自行选择其他模型。".into()
            },
            retryable: false,
            execution_unknown: false,
        }));
    }
    None
}

/// HTTP 及流内错误共用同一队列识别，普通错误保留原状态和详情。
fn decode_upstream_error(status: u16, detail: &str) -> AppError {
    normalize_upstream_error(detail).unwrap_or_else(|| {
        AppError::Upstream(UpstreamError {
            status: Some(status),
            code: "qoder_envelope_error".into(),
            message: detail.into(),
            retryable: status == 429 || status == 418,
            execution_unknown: status >= 500 || status == 408,
        })
    })
}

/// 解码内层 delta。业务错误码必须终止请求而不是静默丢弃。
fn decode_delta(value: &Value) -> Result<Vec<Event>, AppError> {
    if let Some(code) = value.get("code").and_then(|value| {
        value
            .as_str()
            .map(ToOwned::to_owned)
            .or_else(|| value.as_i64().map(|code| code.to_string()))
    }) && !code.is_empty()
        && code != "0"
    {
        let message = value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        return Err(
            normalize_upstream_error(&value.to_string()).unwrap_or_else(|| {
                AppError::Upstream(UpstreamError {
                    status: None,
                    code,
                    message: message.into(),
                    retryable: false,
                    execution_unknown: false,
                })
            }),
        );
    }
    let mut events = Vec::new();
    if let Some(choices) = value.get("choices").and_then(Value::as_array) {
        for choice in choices {
            if let Some(delta) = choice.get("delta") {
                if let Some(text) = delta.get("content").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    events.push(Event::Text {
                        text: text.to_string(),
                    });
                }
                if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str)
                    && !reasoning.is_empty()
                {
                    events.push(Event::Reasoning {
                        text: reasoning.to_string(),
                    });
                }
                if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                    for (position, call) in calls.iter().enumerate() {
                        let function = call.get("function");
                        let index = call
                            .get("index")
                            .and_then(Value::as_u64)
                            .map(|value| value as u32)
                            .unwrap_or(position as u32);
                        events.push(Event::ToolCall(ToolCallDelta {
                            index,
                            id: call
                                .get("id")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned),
                            name: function
                                .and_then(|value| value.get("name"))
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned),
                            arguments: function
                                .and_then(|value| value.get("arguments"))
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned),
                        }));
                    }
                }
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                events.push(Event::Finish(match reason {
                    "tool_calls" => FinishReason::ToolCalls,
                    "length" => FinishReason::Length,
                    "content_filter" => FinishReason::ContentFilter,
                    _ => FinishReason::Stop,
                }));
            }
        }
    }
    if let Some(usage) = value.get("usage").filter(|value| !value.is_null()) {
        events.push(Event::Usage(Usage {
            input_tokens: usage
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .map(|value| value as u32),
            output_tokens: usage
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .map(|value| value as u32),
            total_tokens: usage
                .get("total_tokens")
                .and_then(Value::as_u64)
                .map(|value| value as u32),
        }));
    }
    Ok(events)
}

/// 把 Unix 秒转成 `Mon, 02 Jan 2006 15:04:05 GMT`。供旧版 jobToken 签名使用。
fn format_http_date(seconds: u64) -> String {
    const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = seconds / 86_400;
    let remainder = seconds % 86_400;
    let hour = remainder / 3_600;
    let minute = (remainder % 3_600) / 60;
    let second = remainder % 60;
    let (year, month, day) = civil_from_days(days as i64);
    let weekday = WEEKDAYS[(days % 7) as usize];
    format!(
        "{weekday}, {day:02} {} {year} {hour:02}:{minute:02}:{second:02} GMT",
        MONTHS[(month - 1) as usize]
    )
}

/// 生成当前 HTTP 日期。
fn http_date_now() -> String {
    format_http_date(cosy::unix_seconds())
}

/// 把 Unix 天数转成公历年月日（Howard Hinnant 算法）。供任务周期计算复用。
pub fn civil_from_days_public(days: i64) -> (i64, u32, u32) {
    civil_from_days(days)
}

/// 把 Unix 天数转成公历年月日（Howard Hinnant 算法）。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Qoder 凭证续期器。
///
/// 续期走官方 jobToken 端点：refresh token 以 `drt-` 前缀标识，使用
/// 旧版 `MD5(appcode&secret&date)` 签名，与对话的 COSY 签名相互独立。
pub struct QoderRefresher {
    backend_id: String,
    region: QoderRegion,
    client: reqwest::Client,
}

impl QoderRefresher {
    /// 构造续期器。
    pub fn new(backend_id: String, region: QoderRegion) -> Result<Self, AppError> {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|error| AppError::Internal {
                message: format!("build http client failed: {error}"),
            })?;
        Ok(QoderRefresher {
            backend_id,
            region,
            client,
        })
    }
}

#[async_trait]
impl crate::accounts::remote::CredentialRefresher for QoderRefresher {
    /// 返回后端标识。
    fn backend_id(&self) -> &str {
        &self.backend_id
    }

    /// 用 refresh_token（或 device_token）换取新令牌，并写回凭证 JSON。
    async fn refresh(&self, account: &Account) -> Result<Account, AppError> {
        let mut credential = QoderCredential::parse(account)?;
        let token = if !credential.refresh_token.is_empty() {
            credential.refresh_token.clone()
        } else {
            credential.device_token.clone()
        };
        if token.is_empty() {
            return Err(AppError::bad_request(
                "qoder credential has neither refresh_token nor device_token; sign in again",
            ));
        }
        let url = credential
            .refresh_url
            .clone()
            .unwrap_or_else(|| self.region.refresh_endpoint().to_string());
        let date = http_date_now();
        let signature = format!(
            "{:x}",
            md5::compute(format!("cosy&d2FyLCB3YXIgbmV2ZXIgY2hhbmdlcw==&{date}"))
        );
        let (machine_id, machine_token, machine_type) = credential.machine();
        // drt- 前缀是 refresh token 的形态；其余按 personal token 传递。
        let (personal_token, refresh_token, need_refresh) = if token.starts_with("drt-") {
            ("", token.as_str(), true)
        } else {
            (token.as_str(), "", false)
        };
        let inner = json!({
            "personalToken": personal_token,
            "securityOauthToken": "",
            "refreshToken": refresh_token,
            "needRefresh": need_refresh,
            "authInfo": {},
        });
        let outer = json!({
            "payload": serde_json::to_string(&inner).map_err(|error| AppError::Internal {
                message: format!("serialize refresh payload failed: {error}"),
            })?,
            "encodeVersion": "1",
        });
        let body = cosy::encode_body(
            serde_json::to_string(&outer)
                .map_err(|error| AppError::Internal {
                    message: format!("serialize refresh envelope failed: {error}"),
                })?
                .as_bytes(),
        );

        let response = self
            .client
            .post(&url)
            .header("cosy-machinetoken", machine_token)
            .header("cosy-machinetype", machine_type)
            .header("cosy-machineid", machine_id)
            .header("login-version", "v2")
            .header("appcode", "cosy")
            .header("accept", "application/json")
            .header("accept-encoding", "identity")
            .header("cosy-version", LEGACY_COSY_VERSION)
            .header("cosy-clienttype", LEGACY_CLIENT_TYPE)
            .header("date", &date)
            .header("signature", signature)
            .header("content-type", "application/json")
            .header("user-agent", "Go-http-client/2.0")
            .body(body)
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(AppError::Upstream(UpstreamError::status(
                status.as_u16(),
                text,
            )));
        }
        let parsed: Value = serde_json::from_str(&text).map_err(|error| {
            AppError::Upstream(UpstreamError {
                status: None,
                code: "invalid_refresh_response".to_string(),
                message: format!("invalid refresh JSON: {error}"),
                retryable: false,
                execution_unknown: false,
            })
        })?;
        let new_token = pick_string(
            &parsed,
            &[
                "device_token",
                "deviceToken",
                "access_token",
                "accessToken",
                "token",
            ],
        )
        .ok_or_else(|| {
            AppError::Upstream(UpstreamError {
                status: None,
                code: "missing_device_token".to_string(),
                message: "refresh response did not contain a token".to_string(),
                retryable: false,
                execution_unknown: false,
            })
        })?;
        credential.device_token = new_token;
        if let Some(next_refresh) = pick_string(&parsed, &["refresh_token", "refreshToken"]) {
            credential.refresh_token = next_refresh;
        }
        let mut updated = account.clone();
        updated.credential =
            serde_json::to_string(&credential).map_err(|error| AppError::Internal {
                message: format!("serialize credential failed: {error}"),
            })?;
        Ok(updated)
    }
}

/// 从顶层或 `data` 子对象里取第一个非空字符串字段。
fn pick_string(value: &Value, keys: &[&str]) -> Option<String> {
    let read = |target: &Value| -> Option<String> {
        keys.iter().find_map(|key| {
            target
                .get(*key)
                .and_then(Value::as_str)
                .filter(|found| !found.is_empty())
                .map(ToOwned::to_owned)
        })
    };
    read(value).or_else(|| value.get("data").and_then(read))
}

/// Qoder 额度读取器。
pub struct QoderQuota {
    backend_id: String,
    region: QoderRegion,
    client: reqwest::Client,
}

impl QoderQuota {
    /// 构造读取器。
    pub fn new(backend_id: String, region: QoderRegion) -> Result<Self, AppError> {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|error| AppError::Internal {
                message: format!("build http client failed: {error}"),
            })?;
        Ok(QoderQuota {
            backend_id,
            region,
            client,
        })
    }
}

#[async_trait]
impl crate::accounts::remote::QuotaReader for QoderQuota {
    /// 返回后端标识。
    fn backend_id(&self) -> &str {
        &self.backend_id
    }

    /// 读取额度。字段缺失时返回 Unknown，不伪造数字。
    async fn read(
        &self,
        account: &Account,
    ) -> Result<crate::accounts::remote::QuotaInfo, AppError> {
        use crate::accounts::remote::{QuotaConfidence, QuotaInfo};
        let credential = QoderCredential::parse(account)?;
        let url = credential
            .quota_url
            .clone()
            .unwrap_or_else(|| self.region.quota_endpoint().to_string());
        let response = self
            .client
            .get(&url)
            .header(
                "Authorization",
                format!("Bearer {}", credential.device_token),
            )
            .header("accept", "application/json")
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Ok(QuotaInfo::unknown(
                &self.backend_id,
                &account.id,
                format!("quota endpoint returned {}", status.as_u16()),
            ));
        }
        let Ok(parsed) = serde_json::from_str::<Value>(&body) else {
            return Ok(QuotaInfo::unknown(
                &self.backend_id,
                &account.id,
                "quota response is not JSON",
            ));
        };
        let bucket = parsed
            .get("userQuota")
            .filter(|value| value.is_object())
            .or_else(|| parsed.get("addonQuota").filter(|value| value.is_object()));
        let Some(bucket) = bucket else {
            return Ok(QuotaInfo::unknown(
                &self.backend_id,
                &account.id,
                "quota response has no userQuota or addonQuota",
            ));
        };
        let used = bucket
            .get("used")
            .and_then(Value::as_f64)
            .map(|value| value as i64);
        let total = bucket
            .get("total")
            .and_then(Value::as_f64)
            .map(|value| value as i64);
        let exceeded = parsed
            .get("isQuotaExceeded")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok(QuotaInfo {
            routing: {
                use crate::accounts::quota::{QuotaSnapshot, decimal_string, value_units};
                let remaining = value_units(&bucket["total"])
                    .zip(value_units(&bucket["used"]))
                    .and_then(|(total, used)| total.checked_sub(used));
                Some(QuotaSnapshot {
                    remaining: if exceeded {
                        Some("0".into())
                    } else {
                        remaining.map(decimal_string)
                    },
                    unit: bucket["unit"].as_str().unwrap_or("").into(),
                    quota_scope_id: format!(
                        "qoder:{:?}:{}:{}",
                        self.region, credential.uid, credential.organization_id
                    ),
                    valid_until: bucket["resetTime"]
                        .as_str()
                        .or(bucket["reset_time"].as_str())
                        .and_then(|text| {
                            time::OffsetDateTime::parse(
                                text,
                                &time::format_description::well_known::Rfc3339,
                            )
                            .ok()
                        })
                        .and_then(|date| u64::try_from(date.unix_timestamp()).ok()),
                    unlimited: false,
                    complete: exceeded || remaining.is_some(),
                })
            },
            backend_id: self.backend_id.clone(),
            account_id: account.id.clone(),
            used,
            total,
            remaining: None,
            unit: bucket
                .get("unit")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            reset_at: bucket
                .get("resetTime")
                .or_else(|| bucket.get("reset_time"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            confidence: if used.is_some() || total.is_some() {
                QuotaConfidence::Verified
            } else {
                QuotaConfidence::Unknown
            },
            note: exceeded.then(|| "quota exceeded".to_string()),
            checked_at: crate::records::now_seconds(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 三个真实 HTTP 假端点验证限流不会在适配器内用同一账号重复提交。
    #[tokio::test]
    async fn http_rate_limit_does_not_resubmit_same_account_to_other_endpoints() {
        use crate::gateway::retry::{ATTEMPT, AttemptContext};
        use crate::gateway::{Gateway, GatewaySettings};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        let requests = Arc::new(AtomicUsize::new(0));
        let observed = requests.clone();
        let app = axum::Router::new().route(
            "/{endpoint}",
            axum::routing::post(move || {
                let observed = observed.clone();
                async move {
                    observed.fetch_add(1, Ordering::SeqCst);
                    (
                        axum::http::StatusCode::TOO_MANY_REQUESTS,
                        [("retry-after", "37")],
                        "account rate limit",
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let credential = QoderCredential {
            uid: "fake-user".into(),
            device_token: "fake-token".into(),
            ..Default::default()
        };
        let backend = QoderBackend::new(
            "qoder".into(),
            "Qoder".into(),
            QoderRegion::Global,
            vec!["auto".into()],
            vec![Account {
                id: "fake-account".into(),
                name: "Fake".into(),
                backend_id: "qoder".into(),
                enabled: true,
                credential: serde_json::to_string(&credential).unwrap(),
            }],
        )
        .unwrap();
        let context =
            AttemptContext::new(&GatewaySettings::default(), 3, Arc::new(Gateway::default()));
        context.spend().unwrap();
        let error = ATTEMPT
            .scope(
                context.clone(),
                backend.send_chat_endpoints(
                    &credential,
                    &cosy::encode_body(b"{}"),
                    ["one", "two", "three"]
                        .map(|path| format!("{base}/{path}"))
                        .to_vec(),
                    "auto",
                    &CancellationToken::new(),
                    "fake-account",
                ),
            )
            .await
            .unwrap_err();
        server.abort();
        assert_eq!(error.status().as_u16(), 429);
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "限流必须交给网关换账号，不能连续 POST 到三个端点"
        );
        assert_eq!(context.used(), 1);
        assert_eq!(context.take_retry_after(), Some(Duration::from_secs(37)));
        assert!(backend.pool.get_available("fake-account").is_some());
    }

    /// 连接前失败及安全拒绝仍可换端点，但服务端未知执行不得重放。
    #[tokio::test]
    async fn endpoint_recovery_preserves_safe_retries_and_unknown_execution() {
        use crate::gateway::retry::{ATTEMPT, AttemptContext};
        use crate::gateway::{Gateway, GatewaySettings};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        for status in [0, 418, 500] {
            let requests = Arc::new(AtomicUsize::new(0));
            let observed = requests.clone();
            let app = axum::Router::new().route(
                "/{endpoint}",
                axum::routing::post(
                    move |axum::extract::Path(path): axum::extract::Path<String>| {
                        let observed = observed.clone();
                        async move {
                            observed.fetch_add(1, Ordering::SeqCst);
                            (
                                axum::http::StatusCode::from_u16(if path == "reject" {
                                    status
                                } else {
                                    200
                                })
                                .unwrap(),
                                "fake response",
                            )
                        }
                    },
                ),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let first_endpoint = if status == 0 {
                let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let endpoint = format!("http://{}/reject", closed.local_addr().unwrap());
                drop(closed);
                endpoint
            } else {
                format!("{base}/reject")
            };
            let credential = QoderCredential {
                uid: "fake-user".into(),
                device_token: "fake-token".into(),
                ..Default::default()
            };
            let backend = QoderBackend::new(
                "qoder".into(),
                "Qoder".into(),
                QoderRegion::Global,
                vec!["auto".into()],
                vec![],
            )
            .unwrap();
            let context =
                AttemptContext::new(&GatewaySettings::default(), 3, Arc::new(Gateway::default()));
            context.spend().unwrap();
            let result = ATTEMPT
                .scope(
                    context.clone(),
                    backend.send_chat_endpoints(
                        &credential,
                        &cosy::encode_body(b"{}"),
                        vec![
                            first_endpoint,
                            format!("{base}/success"),
                            format!("{base}/unused"),
                        ],
                        "auto",
                        &CancellationToken::new(),
                        "fake-account",
                    ),
                )
                .await;
            server.abort();
            if status != 500 {
                assert_eq!(result.unwrap().status().as_u16(), 200);
                assert_eq!(
                    requests.load(Ordering::SeqCst),
                    if status == 0 { 1 } else { 2 }
                );
                assert_eq!(context.used(), 2);
            } else {
                let AppError::Upstream(error) = result.unwrap_err() else {
                    panic!("expected upstream error")
                };
                assert_eq!(error.status, Some(500));
                assert!(error.execution_unknown);
                assert_eq!(requests.load(Ordering::SeqCst), 1);
                assert_eq!(context.used(), 1);
            }
        }
    }

    /// 签发路径必须包含协议要求的查询参数。
    #[test]
    fn chat_endpoint_has_query() {
        assert!(QoderRegion::Global.chat_endpoint().contains("Encode=1"));
        assert!(
            QoderRegion::Global
                .chat_endpoint()
                .contains("AgentId=agent_common")
        );
        assert!(
            QoderRegion::Cn
                .model_list_endpoint()
                .starts_with("https://gateway.qoder.com.cn")
        );
    }

    /// 双层信封要能解出内层文本。
    #[test]
    fn decodes_nested_envelope() {
        let inner = json!({"choices":[{"delta":{"content":"hi"},"finish_reason":null}]});
        let outer = json!({"statusCodeValue": 200, "body": inner.to_string()});
        let events = decode_envelope(&outer).expect("decode");
        assert!(matches!(&events[0], Event::Text { text } if text == "hi"));
    }

    /// 非 200 信封必须上抛错误。
    #[test]
    fn non_200_envelope_fails() {
        let outer = json!({"statusCodeValue": 500, "body": "boom"});
        let error = decode_envelope(&outer).expect_err("must fail");
        assert_eq!(error.code(), "qoder_envelope_error");
    }

    /// 内层业务错误码必须上抛。
    #[test]
    fn business_code_fails() {
        let inner = json!({"code": "115", "message": "quota"});
        assert!(decode_delta(&inner).is_err());
    }

    /// 排队错误应识别等待时间。
    #[test]
    fn detects_queue_error() {
        let message = r#"{"code":"403","message":"{\"code\":\"10605\",\"message\":\"{\\\"isQueued\\\":true,\\\"retryAfterSeconds\\\":30}\"}"}"#;
        let error = AppError::Upstream(UpstreamError {
            status: Some(403),
            code: "qoder_envelope_error".to_string(),
            message: message.to_string(),
            retryable: true,
            execution_unknown: false,
        });
        assert_eq!(queue_retry_after(&error), Some(30));
    }

    /// 非排队错误不得触发重试。
    #[test]
    fn ignores_non_queue_error() {
        let error = AppError::Upstream(UpstreamError {
            status: Some(401),
            code: "qoder_envelope_error".to_string(),
            message: "invalid signature".to_string(),
            retryable: false,
            execution_unknown: false,
        });
        assert_eq!(queue_retry_after(&error), None);
    }

    /// 等待时间可能被多层转义包裹。
    #[test]
    fn reads_numeric_field() {
        assert_eq!(
            numeric_field(r#"retryAfterSeconds\":30,"#, "retryAfterSeconds"),
            Some(30)
        );
        assert_eq!(numeric_field(r#"waitTime\":12"#, "waitTime"), Some(12));
        assert_eq!(numeric_field("no digits", "waitTime"), None);
    }

    /// 日期格式必须是 GMT HTTP 日期。
    #[test]
    fn formats_http_date() {
        // 2026-11-18T00:00:00Z
        let text = format_http_date(1_763_424_000);
        assert!(text.ends_with("GMT"), "got {text}");
        assert_eq!(text.len(), 29);
    }

    /// 凭证缺失必需字段要报错。
    #[test]
    fn validates_credential() {
        assert!(QoderCredential::default().validate().is_err());
        let credential = QoderCredential {
            uid: "u".into(),
            device_token: "dt-1".into(),
            ..QoderCredential::default()
        };
        assert!(credential.validate().is_ok());
    }

    /// 登录得到的凭证必须自带稳定设备标识。
    #[test]
    fn builds_credential_from_login() {
        let login = oauth::LoginResult {
            uid: "u1".into(),
            name: "n".into(),
            user_type: String::new(),
            organization_id: String::new(),
            organization_name: String::new(),
            email: String::new(),
            device_token: "dt-1".into(),
            refresh_token: "drt-1".into(),
        };
        let credential = QoderCredential::from_login(&login);
        assert_eq!(credential.aid, "u1");
        assert_eq!(credential.user_type, "personal_standard");
        assert_eq!(credential.machine_id.len(), 36);
        assert_eq!(credential.machine_token.len(), 43);
    }

    /// user 消息必须使用 contents 数组，其他角色使用 content 字符串。
    #[test]
    fn encodes_messages_for_qoder() {
        let user = Message::text(Role::User, "hi");
        let encoded = encode_message(&user).expect("user");
        assert_eq!(encoded["role"], "user");
        assert_eq!(encoded["content"], "");
        assert_eq!(encoded["contents"][0]["text"], "hi");

        let system = Message::text(Role::System, "rules");
        let encoded = encode_message(&system).expect("system");
        assert_eq!(encoded["content"], "rules");
        assert!(encoded.get("contents").is_none());
    }

    /// 模型目录解析：只保留启用模型，并读取上下文窗口。
    #[test]
    fn parses_model_catalog() {
        let root = json!({
            "assistant": [
                {"key": "auto", "display_name": "Auto", "enable": true,
                 "context_config": {"200K": {"token_count": 200000, "is_default": false},
                                    "400K": {"token_count": 400000, "is_default": true}}},
                {"key": "off", "display_name": "Off", "enable": false}
            ]
        });
        let models = parse_models(&root);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "auto");
        assert_eq!(models[0].capabilities.context_window, Some(400000));
    }

    /// 档位按 Token 数合并；冲突默认与非法大值不能产生任意默认或溢出窗口。
    #[test]
    fn context_tiers_merge_duplicates_and_reject_conflicting_defaults() {
        let limits = qoder_token_limits(&json!({
            "context_window":100000,
            "context_config":{
                "standard":{"token_count":200000,"is_default":true},
                "alias":{"token_count":"200000","is_default":false},
                "large":{"token_count":400000,"is_default":true},
                "invalid":{"token_count":4294967296_u64,"is_default":true},
                "zero":{"token_count":0}
            }
        }));
        assert_eq!(limits.context_window, Some(400000));
        assert_eq!(limits.context_tiers.len(), 2);
        assert!(limits.context_tiers.iter().all(|tier| !tier.is_default));
    }

    /// 续期响应字段可能嵌套在 data 中。
    #[test]
    fn picks_token_from_nested_data() {
        let value = json!({"data": {"deviceToken": "dt-2", "refreshToken": "drt-2"}});
        assert_eq!(
            pick_string(&value, &["deviceToken"]).as_deref(),
            Some("dt-2")
        );
        assert_eq!(
            pick_string(&value, &["refreshToken"]).as_deref(),
            Some("drt-2")
        );
    }
}
