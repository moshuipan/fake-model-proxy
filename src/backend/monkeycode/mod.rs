//! MonkeyCode 原生模型网关：账号密钥、请求签名与按模型协议编码。

pub mod checkin;
pub mod credential;
mod pow;
mod wire;

use crate::accounts::remote::{QuotaConfidence, QuotaInfo, QuotaReader};
use crate::accounts::{Account, AccountPool, FailureKind};
use crate::backend::{Backend, CostTier};
use crate::core::stream::EventStream;
use crate::core::{AppError, Capabilities, ChatRequest, ModelInfo, TokenLimits, UpstreamError};
use crate::store::BackendRecord;
use async_trait::async_trait;
use credential::Credential;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// 未提供网页登录态时可显式配置单一上游协议。
#[derive(Default, Deserialize, schemars::JsonSchema)]
pub(crate) struct Options {
    #[serde(default)]
    region: String,
    #[serde(default)]
    interface_type: String,
}

/// 官方区域地址或本机测试网关；账号 API 与推理 API 使用不同主机。
pub fn profile(record: &BackendRecord) -> Result<(String, String), AppError> {
    let options: Options = serde_json::from_str(if record.options.trim().is_empty() {
        "{}"
    } else {
        &record.options
    })
    .map_err(|_| AppError::bad_request("MonkeyCode 后端选项不是有效 JSON"))?;
    let domain = match options.region.as_str() {
        "" | "cn" => "monkeycode-ai.com",
        "global" | "intl" => "monkeycode-ai.net",
        _ => return Err(AppError::bad_request("MonkeyCode 区域须为 cn 或 global")),
    };
    if !options.interface_type.is_empty() {
        wire::Protocol::parse(&options.interface_type)?;
    }
    let expected = format!("https://proxy.{domain}/v1");
    let base = if record.base_url.trim().is_empty() {
        expected.clone()
    } else {
        record.base_url.trim().trim_end_matches('/').into()
    };
    if base == expected {
        return Ok((format!("https://{domain}"), base));
    }
    let url = reqwest::Url::parse(&base)
        .map_err(|_| AppError::bad_request("MonkeyCode 模型网关地址无效"))?;
    if matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"))
        && matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && url.path() == "/v1"
    {
        return Ok((base.trim_end_matches("/v1").into(), base));
    }
    Err(AppError::bad_request(
        "MonkeyCode 仅支持对应区域的官方模型网关或本机测试地址",
    ))
}

/// 目录中可调用的模型及其明确声明的出站协议。
#[derive(Clone)]
struct RemoteModel {
    info: ModelInfo,
    protocol: wire::Protocol,
}

/// 每个账号的模型列表独立缓存，避免团队模型与会员权限串用。
type CatalogCache = HashMap<String, (tokio::time::Instant, Vec<RemoteModel>)>;

/// MonkeyCode 使用账号池，复用现有入站协议和 Auto 路由。
pub struct MonkeyCodeBackend {
    record: BackendRecord,
    server: String,
    gateway: String,
    accounts: AccountPool,
    client: reqwest::Client,
    cache: Mutex<CatalogCache>,
}

impl MonkeyCodeBackend {
    /// 从服务配置构造；未登录时仍可保存服务并显示待登录状态。
    pub fn new(record: &BackendRecord, accounts: Vec<Account>) -> Result<Self, AppError> {
        Self::new_with_timeout(record, accounts, Duration::from_secs(10))
    }

    /// 使用网关配置的连接时限构建账号池传输客户端。
    pub fn new_with_timeout(
        record: &BackendRecord,
        accounts: Vec<Account>,
        timeout: Duration,
    ) -> Result<Self, AppError> {
        let (server, gateway) = profile(record)?;
        let client = reqwest::Client::builder()
            .connect_timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| AppError::Internal {
                message: "MonkeyCode HTTP 客户端构造失败".into(),
            })?;
        Ok(Self {
            record: record.clone(),
            server,
            gateway,
            accounts: AccountPool::new(accounts),
            client,
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// 解析并校验账号凭证，禁止跨区域使用模型密钥。
    fn credential(&self, account: &Account) -> Result<Credential, AppError> {
        let credential = Credential::parse(&account.credential)?;
        credential.validate_origin(&self.server, &self.gateway)?;
        Ok(credential)
    }

    /// 用网页登录态访问账号接口；模型密钥不能代替 Cookie。
    async fn account_json(&self, credential: &Credential, path: &str) -> Result<Value, AppError> {
        if credential.cookie.is_empty() {
            return Err(AppError::bad_request(
                "MonkeyCode 目录和额度查询需要有效登录态，请在控制台重新登录",
            ));
        }
        let response = self
            .client
            .get(format!("{}{}", self.server, path))
            .header("Cookie", &credential.cookie)
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|_| error(None, "MonkeyCode 账号接口连接失败", false))?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(error(
                Some(status),
                if status == 401 || (300..400).contains(&status) {
                    "MonkeyCode 登录态已失效，请在控制台重新登录"
                } else {
                    "MonkeyCode 账号接口请求失败"
                },
                false,
            ));
        }
        let value: Value = response
            .json()
            .await
            .map_err(|_| error(Some(status), "MonkeyCode 账号响应不是 JSON", false))?;
        if !matches!(value.get("code"), Some(Value::Number(code)) if code.as_i64() == Some(0))
            && value.get("code").and_then(Value::as_str) != Some("0")
        {
            return Err(error(
                Some(status),
                "MonkeyCode 账号接口返回业务错误",
                false,
            ));
        }
        value
            .get("data")
            .filter(|value| !value.is_null())
            .cloned()
            .ok_or_else(|| error(Some(status), "MonkeyCode 账号响应缺少 data", false))
    }

    /// 读取全部可用模型；超出会员权限的档位、隐藏项与占位项不进入路由。
    async fn models(&self, account: &Account) -> Result<Vec<RemoteModel>, AppError> {
        let credential = self.credential(account)?;
        let mut cache = self.cache.lock().await;
        if let Some((expires, models)) = cache.get(&account.id)
            && *expires > tokio::time::Instant::now()
        {
            return Ok(models.clone());
        }
        if credential.cookie.is_empty() {
            let options: Options = serde_json::from_str(if self.record.options.is_empty() {
                "{}"
            } else {
                &self.record.options
            })
            .map_err(|_| AppError::bad_request("MonkeyCode 选项无效"))?;
            let protocol = wire::Protocol::parse(&options.interface_type)?;
            return Ok(self
                .static_models()
                .into_iter()
                .map(|id| RemoteModel {
                    info: ModelInfo {
                        display_name: Some(id.clone()),
                        id,
                        price_factor: None,
                        original_price_factor: None,
                        price_valid_until: None,
                        capabilities: Capabilities {
                            tools: true,
                            parallel_tools: true,
                            ..Default::default()
                        },
                        token_limits: TokenLimits::default(),
                    },
                    protocol,
                })
                .collect());
        }
        let plan = self
            .account_json(&credential, "/api/v1/users/subscription")
            .await
            .ok()
            .and_then(|value| value["plan"].as_str().map(String::from))
            .unwrap_or_default();
        let mut result = Vec::new();
        let mut cursor = String::new();
        let mut visited = HashSet::new();
        loop {
            let mut path = "/api/v1/users/models?limit=200".to_string();
            if !cursor.is_empty() {
                let encoded: String =
                    reqwest::Url::parse_with_params("http://localhost/", &[("cursor", &cursor)])
                        .map_err(|_| AppError::bad_request("MonkeyCode 分页游标无效"))?
                        .query()
                        .unwrap_or_default()
                        .into();
                path.push('&');
                path.push_str(&encoded);
            }
            let value = self.account_json(&credential, &path).await?;
            let items = value["models"]
                .as_array()
                .ok_or_else(|| error(None, "MonkeyCode 模型目录格式已变化", false))?;
            for item in items {
                let Some(model) = catalog_entry(item, &plan)? else {
                    continue;
                };
                if result
                    .iter()
                    .any(|old: &RemoteModel| old.info.id == model.info.id)
                {
                    continue;
                }
                result.push(model);
            }
            if value
                .pointer("/page/has_next_page")
                .and_then(Value::as_bool)
                != Some(true)
            {
                break;
            }
            cursor = value
                .pointer("/page/cursor")
                .and_then(Value::as_str)
                .filter(|cursor| !cursor.is_empty())
                .ok_or_else(|| error(None, "MonkeyCode 模型分页缺少游标", false))?
                .into();
            if !visited.insert(cursor.clone()) || visited.len() > 50 {
                return Err(error(None, "MonkeyCode 模型分页未推进", false));
            }
        }
        cache.insert(
            account.id.clone(),
            (
                tokio::time::Instant::now() + Duration::from_secs(300),
                result.clone(),
            ),
        );
        Ok(result)
    }
}

/// 官方目录字段保持各自语义；模型 ID 使用 model，不能使用配置 UUID。
fn catalog_entry(item: &Value, plan: &str) -> Result<Option<RemoteModel>, AppError> {
    let Some(id) = item["model"].as_str().filter(|id| !id.trim().is_empty()) else {
        return Ok(None);
    };
    let permission_id = id.to_ascii_lowercase();
    if item["is_hidden"] == true
        || matches!(
            permission_id.as_str(),
            "monkeycode-basic" | "monkeycode-pro" | "monkeycode-ultra"
        )
        || (permission_id.starts_with("monkeycode-ultra") && !matches!(plan, "flagship" | "ultra"))
        || (permission_id.starts_with("monkeycode-pro")
            && !matches!(plan, "pro" | "flagship" | "ultra"))
    {
        return Ok(None);
    }
    let protocol = match wire::Protocol::parse(item["interface_type"].as_str().unwrap_or("")) {
        Ok(protocol) => protocol,
        Err(_) => return Ok(None),
    };
    let context = crate::backend::catalog::token_count(&item["context_limit"]);
    Ok(Some(RemoteModel {
        info: ModelInfo {
            id: id.into(),
            display_name: Some(
                item["remark"]
                    .as_str()
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or(id)
                    .into(),
            ),
            price_factor: None,
            original_price_factor: None,
            price_valid_until: None,
            capabilities: crate::backend::catalog::capabilities(
                item,
                &Capabilities {
                    tools: true,
                    parallel_tools: true,
                    images: false,
                    reasoning: false,
                    context_window: context,
                },
            ),
            token_limits: TokenLimits {
                context_window: context,
                max_output_tokens: crate::backend::catalog::token_count(&item["output_limit"]),
                ..Default::default()
            },
        },
        protocol,
    }))
}

/// 上游错误不携带响应正文或密钥；生成提交后的传输失败不得自动重放。
fn error(status: Option<u16>, message: &str, execution_unknown: bool) -> AppError {
    AppError::Upstream(UpstreamError {
        status,
        code: "monkeycode_upstream_error".into(),
        message: message.into(),
        retryable: !execution_unknown && status.is_some_and(|status| status == 429),
        execution_unknown,
    })
}

#[async_trait]
impl Backend for MonkeyCodeBackend {
    /// 返回稳定服务标识。
    fn id(&self) -> &str {
        &self.record.id
    }
    /// 返回显示名称。
    fn name(&self) -> &str {
        &self.record.name
    }
    /// 返回独立后端类型。
    fn kind(&self) -> &str {
        "monkeycode"
    }
    /// 目录逐模型细化能力；后端支持三种原生协议。
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tools: true,
            parallel_tools: true,
            ..Default::default()
        }
    }
    /// 按配置声明费用，不把会员模型视为无限免费。
    fn cost_tier(&self) -> CostTier {
        CostTier::parse(&self.record.cost_tier)
    }
    /// 使用现有优先级排序。
    fn priority(&self) -> u8 {
        self.record.priority
    }
    /// 使用现有每日预算。
    fn daily_token_budget(&self) -> u64 {
        self.record.daily_token_budget
    }
    /// 返回手工配置或已同步的模型白名单。
    fn static_models(&self) -> Vec<String> {
        self.record
            .models
            .split(',')
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(String::from)
            .collect()
    }
    /// 为 Auto 暴露账号可用性。
    fn routing_accounts(&self) -> Option<Vec<crate::accounts::AccountState>> {
        Some(self.accounts.snapshot())
    }
    /// 使用选定账号查询余额。
    async fn routing_quota(&self, id: &str) -> Result<QuotaInfo, AppError> {
        let account = self
            .accounts
            .get_available(id)
            .ok_or_else(|| AppError::bad_request("MonkeyCode 账号不可用"))?;
        self.read(&account).await
    }
    /// 合并启用账号的可调用目录，单个账号失效不遮蔽其他账号。
    async fn list_models(&self) -> Result<Vec<ModelInfo>, AppError> {
        let mut result: Vec<ModelInfo> = Vec::new();
        let mut last_error = None;
        for state in self
            .accounts
            .snapshot()
            .into_iter()
            .filter(|state| state.enabled)
        {
            let Some(account) = self.accounts.get_available(&state.id) else {
                continue;
            };
            match self.models(&account).await {
                Ok(models) => {
                    for model in models {
                        if !result.iter().any(|old| old.id == model.info.id) {
                            result.push(model.info);
                        }
                    }
                }
                Err(error) => last_error = Some(error),
            }
        }
        if result.is_empty()
            && let Some(error) = last_error
        {
            return Err(error);
        }
        Ok(result)
    }
    /// 按账号权限选择模型协议，签名与实际发出的系统提示完全一致。
    async fn chat(
        &self,
        mut request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, AppError> {
        let account = self
            .accounts
            .select()
            .ok_or_else(|| AppError::bad_request("MonkeyCode 尚无可用账号，请在控制台登录"))?;
        request.model = request
            .model
            .strip_prefix(&format!("{}/", self.record.id))
            .unwrap_or(&request.model)
            .into();
        let models = tokio::select! {
            _ = cancel.cancelled() => return Err(AppError::bad_request("请求已取消")),
            result = self.models(&account) => result?,
        };
        let model = models
            .iter()
            .find(|model| model.info.id == request.model)
            .ok_or_else(|| {
                AppError::Upstream(UpstreamError {
                    status: Some(403),
                    code: "monkeycode_account_model_unavailable".into(),
                    message: "该 MonkeyCode 账号未提供此模型，或会员等级未覆盖".into(),
                    retryable: true,
                    execution_unknown: false,
                })
            })?;
        let credential = self.credential(&account)?;
        let (path, body, signature) =
            wire::encode(&request, model.protocol, &credential.signing_secret)?;
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(AppError::bad_request("请求已取消")),
            result = self.client.post(format!("{}/{}", self.gateway, path))
                .bearer_auth(&credential.api_key)
                .header("x-api-key", &credential.api_key)
                .header("anthropic-version", "2023-06-01")
                .header("X-OhMyAgent-Signature", signature)
                .header("Accept", "text/event-stream")
                .timeout(Duration::from_secs(600))
                .json(&body).send() => result.map_err(|_| error(None, "MonkeyCode 生成连接失败，执行结果未知", true))?,
        };
        if !response.status().is_success() {
            crate::gateway::retry::capture_retry_after(&response);
            let status = response.status().as_u16();
            self.accounts.report_failure(
                &account.id,
                if status == 402 {
                    FailureKind::QuotaExhausted
                } else {
                    FailureKind::classify(Some(status), "")
                },
            );
            return Err(error(
                Some(status),
                "MonkeyCode 模型请求失败，请检查凭证、会员权限和额度",
                status >= 500,
            ));
        }
        if !response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("text/event-stream"))
        {
            return Err(error(None, "MonkeyCode 未返回预期的 SSE 流", true));
        }
        self.accounts.report_success(&account.id);
        let (sender, receiver) = tokio::sync::mpsc::channel(32);
        wire::spawn_pump(model.protocol, response, sender, cancel);
        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(
            receiver,
        )))
    }
}

#[async_trait]
impl QuotaReader for MonkeyCodeBackend {
    /// 返回余额所属服务。
    fn backend_id(&self) -> &str {
        &self.record.id
    }
    /// 积分和每日 Token 不相加；积分使用十进制定点保持精度。
    async fn read(&self, account: &Account) -> Result<QuotaInfo, AppError> {
        let credential = self.credential(account)?;
        if credential.cookie.is_empty() {
            return Ok(QuotaInfo::unknown(
                &self.record.id,
                &account.id,
                "缺少有效登录态，额度未知，请在控制台重新登录",
            ));
        }
        let wallet = self
            .account_json(&credential, "/api/v1/users/wallet")
            .await?;
        let mut info = QuotaInfo::unknown(&self.record.id, &account.id, "钱包未提供积分余额");
        if let Some(balance) =
            crate::accounts::quota::value_units(&wallet["balance"]).filter(|value| *value >= 0)
        {
            let remaining = crate::accounts::quota::decimal_string(balance);
            info.remaining = remaining.parse().ok();
            info.unit = Some("积分".into());
            info.confidence = QuotaConfidence::Verified;
            info.routing = Some(crate::accounts::quota::QuotaSnapshot {
                remaining: Some(remaining),
                unit: "积分".into(),
                quota_scope_id: format!("monkeycode:{}:{}", self.server, account.id),
                complete: true,
                ..Default::default()
            });
            info.note = Some("积分余额；每日模型 Token 是独立额度，不能由积分推算".into());
        }
        if let Some(remaining) = wallet["daily_token_balance"].as_u64() {
            info.note = Some(format!(
                "{}；每日 Token 剩余 {}，总量 {}",
                info.note.as_deref().unwrap_or("积分未知"),
                remaining,
                wallet["daily_token_limit"]
                    .as_u64()
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "未知".into())
            ));
        }
        Ok(info)
    }
}
