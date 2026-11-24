//! MiniMax Code 原生后端：独立 OAuth、Messages 网关、动态目录与账号任务。

mod checkin;
pub mod oauth;
pub mod wire;

use crate::accounts::remote::{CredentialRefresher, QuotaConfidence, QuotaInfo, QuotaReader};
use crate::accounts::{Account, AccountPool, FailureKind};
use crate::backend::{Backend, CostTier};
use crate::core::stream::EventStream;
use crate::core::{
    AppError, Capabilities, ChatRequest, ContextTier, ModelInfo, TokenLimits, UpstreamError,
};
use crate::store::{BackendRecord, Store};
use crate::tasks::{TaskKind, TaskOutcome, TaskProvider, TaskStatus};
use async_trait::async_trait;
use oauth::{Credential, OAuth};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// 服务区域独立于 API key 或客户端安装。
#[derive(Default, Deserialize, schemars::JsonSchema)]
pub(crate) struct Options {
    #[serde(default)]
    region: String,
}

/// 仅允许官方区域地址或本机测试端点，防止凭证发往第三方网关。
pub fn profile(record: &BackendRecord) -> Result<(String, String), AppError> {
    let options: Options = serde_json::from_str(if record.options.trim().is_empty() {
        "{}"
    } else {
        &record.options
    })
    .map_err(|_| AppError::bad_request("MiniMax 后端选项不是有效 JSON"))?;
    let region = match options.region.as_str() {
        "" | "cn" => "cn",
        "global" | "en" | "intl" => "global",
        _ => return Err(AppError::bad_request("MiniMax 区域须为 cn 或 global")),
    };
    let base = if record.base_url.trim().is_empty() {
        if region == "global" {
            "https://agent.minimax.io"
        } else {
            "https://agent.minimax.cn"
        }
    } else {
        record.base_url.trim().trim_end_matches('/')
    };
    let url =
        reqwest::Url::parse(base).map_err(|_| AppError::bad_request("MiniMax 网关地址无效"))?;
    let official = url.scheme() == "https"
        && url.port().is_none()
        && url.host_str().is_some_and(|host| {
            if region == "global" {
                host == "agent.minimax.io"
            } else {
                matches!(host, "agent.minimax.cn" | "agent.minimaxi.com")
            }
        });
    let loopback = url.scheme() == "http"
        && url
            .host_str()
            .is_some_and(|host| matches!(host, "127.0.0.1" | "[::1]" | "localhost"));
    if !(official || loopback)
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.path(), "" | "/" | "/mavis/api/v1/llm/v1")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(AppError::bad_request(
            "MiniMax 网关须属于所选区域的官方域名",
        ));
    }
    Ok((region.into(), url.origin().ascii_serialization()))
}

/// 一份服务对应账号池；所有实例通过 Store 协调刷新。
pub struct MiniMaxBackend {
    record: BackendRecord,
    accounts: Arc<AccountPool>,
    store: crate::plugins::ScopedHostServices,
    client: reqwest::Client,
    oauth: OAuth,
    base: String,
    capabilities: Capabilities,
    cache: Mutex<Option<(tokio::time::Instant, Vec<ModelInfo>)>>,
}

impl MiniMaxBackend {
    /// 创建后端，不读取客户端登录态或执行网络请求。
    pub fn new(
        record: &BackendRecord,
        accounts: Vec<Account>,
        store: Store,
    ) -> Result<Self, AppError> {
        Self::new_with_timeout(record, accounts, store, std::time::Duration::from_secs(10))
    }

    /// 独立连接时限不限制生成流总长，总时限由网关负责。
    pub fn new_with_timeout(
        record: &BackendRecord,
        accounts: Vec<Account>,
        store: Store,
        timeout: std::time::Duration,
    ) -> Result<Self, AppError> {
        Self::new_with_host(
            record,
            crate::plugins::ScopedHostServices::legacy(record, &accounts, store, timeout),
        )
    }

    /// 推理、续期、余额与任务使用同一宿主和内部缓存。
    pub fn new_with_host(
        record: &BackendRecord,
        host: crate::plugins::ScopedHostServices,
    ) -> Result<Self, AppError> {
        let accounts = host.accounts();
        let timeout = host.connect_timeout;
        let (region, base) = profile(record)?;
        let mut oauth = OAuth::new(&region)?;
        if base.starts_with("http://") {
            oauth.origin = base.clone();
        }
        let mut capabilities = Capabilities {
            tools: true,
            parallel_tools: true,
            images: true,
            reasoning: true,
            context_window: None,
        };
        if !record.capabilities.trim().is_empty() {
            let overrides: crate::backend::openai_compat::CapabilityOverride =
                serde_json::from_str(&record.capabilities)
                    .map_err(|_| AppError::bad_request("MiniMax 能力覆盖格式不正确"))?;
            overrides.apply(&mut capabilities);
        }
        Ok(Self {
            record: record.clone(),
            accounts: Arc::new(AccountPool::new(accounts)),
            store: host,
            client: reqwest::Client::builder()
                .connect_timeout(timeout)
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            oauth,
            base,
            capabilities,
            cache: Mutex::new(None),
        })
    }

    /// 每次从数据库读取启用账号并确认区域，避免账号编辑后用旧快照。
    async fn account(&self, id: &str) -> Result<Account, AppError> {
        self.store
            .get_account(id)
            .await?
            .filter(|a| a.enabled && a.backend_id == self.record.id)
            .ok_or_else(|| AppError::bad_request("MiniMax 账号已删除或停用"))
    }

    /// 串行恢复刷新提交并仅轮换当前凭证，网络结果未知时保留围栏。
    async fn credential(
        &self,
        id: &str,
        force: bool,
        rejected: Option<&str>,
    ) -> Result<Credential, AppError> {
        let _guard = self.store.lock_credentials().await;
        let account = self.account(id).await?;
        self.store
            .recover_credential_refresh(id, &account.credential)
            .await?;
        let account = self.account(id).await?;
        let current = Credential::parse(&account.credential)?;
        if current.region != self.oauth.region {
            return Err(AppError::bad_request(
                "MiniMax 账号与服务区域不同，请重新授权",
            ));
        }
        if !force
            && !current.expires_soon()
            && rejected.is_none_or(|token| token != current.access_token)
        {
            return Ok(current);
        }
        self.store
            .begin_credential_refresh(id, &account.credential)
            .await?;
        let next = match self.oauth.refresh(&current).await {
            Ok(next) => next,
            Err(error) => {
                // 收到明确的服务端拒绝说明没有成功换票；网络或坏响应不证明这一点。
                if error.code() == "minimax_oauth_error" {
                    self.store.abort_credential_refresh(id).await?;
                }
                return Err(error);
            }
        };
        self.store
            .finish_credential_refresh(id, &account.credential, &next.serialize()?)
            .await?;
        let latest = Credential::parse(&self.account(id).await?.credential)?;
        if latest.region != self.oauth.region {
            return Err(AppError::bad_request("账号在续期期间已变更，请重试"));
        }
        Ok(latest)
    }

    /// 构造已授权的区域请求，禁止重定向和压缩造成的 SSE 聚合。
    fn request(
        &self,
        method: Method,
        path: &str,
        credential: &Credential,
    ) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&credential.access_token)
            .header("User-Agent", "MiniMaxAgent")
            .header("x-api-key", "sk-xxx")
            .header("anthropic-version", "2023-06-01")
            .header("Accept-Encoding", "identity")
    }

    /// 账号接口在明确 401 后刷新并重试一次，其余错误不重新花令牌。
    async fn account_json(&self, id: &str, method: Method, path: &str) -> Result<Value, AppError> {
        let mut credential = self.credential(id, false, None).await?;
        for attempt in 0..2 {
            let mut request = self
                .request(method.clone(), path, &credential)
                .header("Accept", "application/json")
                .header("platform", "desktop")
                .header("X-Mavis-Agent-Id", "main")
                .timeout(Duration::from_secs(20));
            if !credential.subject.is_empty() {
                request = request.header("user-id", &credential.subject);
            }
            if method == Method::POST {
                request = request.json(&json!({}));
            }
            let response = request.send().await.map_err(|_| {
                AppError::Upstream(UpstreamError {
                    status: None,
                    code: "minimax_transport_error".into(),
                    message: "MiniMax 账号请求网络失败".into(),
                    retryable: method == Method::GET,
                    execution_unknown: method == Method::POST,
                })
            })?;
            if response.status().as_u16() == 401 && attempt == 0 {
                credential = self
                    .credential(id, false, Some(&credential.access_token))
                    .await?;
                continue;
            }
            return decode(response).await.map_err(|error| match error {
                AppError::Upstream(mut upstream)
                    if method == Method::POST && upstream.status.is_some_and(|s| s >= 500) =>
                {
                    upstream.retryable = false;
                    upstream.execution_unknown = true;
                    AppError::Upstream(upstream)
                }
                other => other,
            });
        }
        Err(AppError::bad_request("MiniMax 账号鉴权失败"))
    }

    /// 读取实时目录，并按上游 TTL 缓存；失败时保留已有目录但不虚构模型。
    async fn models(&self) -> Result<Vec<ModelInfo>, AppError> {
        let mut cache = self.cache.lock().await;
        if let Some((expires, models)) = cache.as_ref()
            && *expires > tokio::time::Instant::now()
        {
            return Ok(models.clone());
        }
        let result = async {
            let response = self
                .client
                .get(format!("{}/mavis/api/v1/models", self.base))
                .query(&[
                    (
                        "region",
                        if self.oauth.region == "global" {
                            "en"
                        } else {
                            "cn"
                        },
                    ),
                    ("buildEnv", "prod"),
                ])
                .timeout(Duration::from_secs(15))
                .send()
                .await?;
            let value = decode(response).await?;
            let list = catalog(&value)?;
            let ttl = value["ttlSeconds"].as_u64().unwrap_or(300).clamp(1, 3600);
            Ok::<_, AppError>((tokio::time::Instant::now() + Duration::from_secs(ttl), list))
        }
        .await;
        match result {
            Ok(snapshot) => {
                let models = snapshot.1.clone();
                *cache = Some(snapshot);
                Ok(models)
            }
            Err(error) => {
                if let Some((_, models)) = cache.as_ref() {
                    tracing::warn!(backend=%self.record.id, "MiniMax 目录读取失败，暂用最后成功快照");
                    Ok(models.clone())
                } else {
                    Err(error)
                }
            }
        }
    }

    /// 发送一次模型请求；只有尚未接受生成的 401 可以进行一次鉴权恢复。
    async fn chat_inner(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, AppError> {
        let account = self.accounts.select().ok_or_else(|| {
            AppError::bad_request("MiniMax 尚无可用账号，请在控制台完成独立 OAuth 授权")
        })?;
        let body = wire::body(&request)?;
        let session = request
            .session
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let mut credential = self.credential(&account.id, false, None).await?;
        let mut response = None;
        for attempt in 0..2 {
            if attempt > 0 {
                crate::gateway::retry::additional_submission().await?;
            }
            let send = self
                .request(Method::POST, "/mavis/api/v1/llm/v1/messages", &credential)
                .header("Accept", "text/event-stream")
                .header("X-Mavis-Session-Id", &session)
                .header("X-Mavis-Agent-Id", "main")
                .header("X-Mavis-Timezone-Offset", "28800")
                .timeout(Duration::from_secs(600))
                .json(&body)
                .send();
            let received = tokio::select! {
                _ = cancel.cancelled() => return Err(AppError::bad_request("请求已取消")),
                result = send => result.map_err(|_| AppError::Upstream(UpstreamError {
                    status:None, code:"minimax_transport_error".into(), message:"MiniMax 生成请求网络失败，执行结果未知".into(),
                    retryable:false, execution_unknown:true,
                }))?,
            };
            if received.status().as_u16() == 401 && attempt == 0 {
                credential = self
                    .credential(&account.id, false, Some(&credential.access_token))
                    .await?;
                continue;
            }
            response = Some(received);
            break;
        }
        let response = response.ok_or_else(|| AppError::bad_request("MiniMax 鉴权重试失败"))?;
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
            return Err(http_error(status));
        }
        if !response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("text/event-stream"))
        {
            return Err(wire::protocol_error("MiniMax 未返回预期的 SSE 流"));
        }
        self.accounts.report_success(&account.id);
        let (sender, receiver) = tokio::sync::mpsc::channel(32);
        tokio::spawn(wire::pump(response, sender, cancel));
        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(
            receiver,
        )))
    }

    /// 先查当天状态再领取；领取网络失败返回未知，不自动重复提交。
    async fn checkin(&self, id: &str) -> Result<TaskOutcome, AppError> {
        let status = self
            .account_json(
                id,
                Method::GET,
                "/minimax-cloud/api/v1/signin/status?timezone_id=Asia%2FShanghai",
            )
            .await?;
        let days = status["data"]["days"]
            .as_array()
            .ok_or_else(|| wire::protocol_error("MiniMax 签到面板格式已变化"))?;
        let Some(today) = days.iter().find(|v| v["is_today"] == true) else {
            return Ok(TaskOutcome::skipped("MiniMax 今日签到活动不可用"));
        };
        match today["status"].as_i64() {
            Some(2) => {}
            Some(3) => return Ok(checkin::already_done()),
            _ => return Ok(TaskOutcome::skipped("MiniMax 今日不可领取")),
        }
        let claim = match self
            .account_json(
                id,
                Method::POST,
                "/minimax-cloud/api/v1/signin/claim?timezone_id=Asia%2FShanghai",
            )
            .await
        {
            Ok(value) => value,
            Err(AppError::Upstream(error)) if error.execution_unknown => {
                return Ok(TaskOutcome {
                    status: TaskStatus::Unknown,
                    message: "MiniMax 签到结果未知，请核对后再操作".into(),
                    amount: None,
                });
            }
            Err(error) => return Err(error),
        };
        Ok(checkin::claim_outcome(&claim["data"]))
    }
}

/// HTTP 错误不泄漏令牌或上游响应正文，跨域跳转保留登录态。
fn http_error(status: u16) -> AppError {
    AppError::Upstream(UpstreamError {
        status: Some(if (300..400).contains(&status) {
            502
        } else {
            status
        }),
        code: "minimax_upstream_error".into(),
        message: if (300..400).contains(&status) {
            "MiniMax 返回跨域跳转，请检查区域和网络；授权未被清除".into()
        } else {
            format!("MiniMax 上游 HTTP {status}")
        },
        retryable: status == 429,
        execution_unknown: status >= 500 || status == 408,
    })
}

/// HTTP 与业务信封均须成功，HTTP 200 不代表账号接口成功。
async fn decode(response: reqwest::Response) -> Result<Value, AppError> {
    if !response.status().is_success() {
        return Err(http_error(response.status().as_u16()));
    }
    let value: Value = response
        .json()
        .await
        .map_err(|_| wire::protocol_error("MiniMax 返回非 JSON 响应"))?;
    if value["base_resp"]["status_code"]
        .as_i64()
        .is_some_and(|v| v != 0)
    {
        return Err(wire::protocol_error("MiniMax 账号接口返回业务错误"));
    }
    Ok(value)
}

/// 仅使用官方配置声明的模型、能力和上下文默认值。
pub fn catalog(value: &Value) -> Result<Vec<ModelInfo>, AppError> {
    let models = value["providers"]
        .as_array()
        .and_then(|providers| providers.iter().find(|p| p["providerId"] == "minimax"))
        .and_then(|p| p["config"]["models"].as_object())
        .ok_or_else(|| wire::protocol_error("MiniMax 模型目录格式已变化"))?;
    let mut out = Vec::new();
    for (id, raw) in models {
        let context = crate::backend::catalog::token_count(&raw["limit"]["context"]);
        let capabilities = crate::backend::catalog::capabilities(
            raw,
            &Capabilities {
                tools: raw["tool_call"].as_bool().unwrap_or(false),
                parallel_tools: raw["tool_call"].as_bool().unwrap_or(false),
                images: false,
                reasoning: false,
                context_window: context,
            },
        );
        let context_tiers = raw["context_window_options"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(crate::backend::catalog::token_count)
            .map(|tokens| ContextTier {
                tokens,
                is_default: Some(tokens) == context,
            })
            .collect();
        out.push(ModelInfo {
            id: id.clone(),
            display_name: Some(format!("MiniMax {}", raw["name"].as_str().unwrap_or(id))),
            price_factor: None,
            original_price_factor: None,
            price_valid_until: None,
            capabilities,
            token_limits: TokenLimits {
                context_window: context,
                max_input_tokens: None,
                max_output_tokens: crate::backend::catalog::token_count(&raw["limit"]["output"]),
                context_tiers,
            },
        });
    }
    if out.is_empty() {
        return Err(wire::protocol_error("MiniMax 模型目录为空"));
    }
    Ok(out)
}

#[async_trait]
impl Backend for MiniMaxBackend {
    /// MiniMax 动态目录仍需要一个已启用账号才能进入 Auto。
    fn routing_accounts(&self) -> Option<Vec<crate::accounts::AccountState>> {
        Some(self.accounts.snapshot())
    }

    /// 复用官方余额账本及凭证刷新围栏。
    async fn routing_quota(&self, id: &str) -> Result<QuotaInfo, AppError> {
        let account = self.account(id).await?;
        QuotaReader::read(self, &account).await
    }

    /// 服务稳定标识。
    fn id(&self) -> &str {
        &self.record.id
    }
    /// 服务显示名称。
    fn name(&self) -> &str {
        &self.record.name
    }
    /// 原生适配器类型。
    fn kind(&self) -> &str {
        "minimax_code"
    }
    /// 当前通用契约支持文本、图片、工具和思考。
    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }
    /// 费用采用配置，不把公开目录视为免费额度。
    fn cost_tier(&self) -> CostTier {
        CostTier::parse(&self.record.cost_tier)
    }
    /// 自动路由优先级。
    fn priority(&self) -> u8 {
        self.record.priority
    }
    /// 每日预算复用现有用量账本。
    fn daily_token_budget(&self) -> u64 {
        self.record.daily_token_budget
    }
    /// 已同步白名单；尚未同步时由动态目录发现。
    fn static_models(&self) -> Vec<String> {
        self.record
            .models
            .split(',')
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(String::from)
            .collect()
    }
    /// 动态目录，不预置过时的模型。
    async fn list_models(&self) -> Result<Vec<ModelInfo>, AppError> {
        self.models().await
    }
    /// 所有入站协议共用一个原生事件流。
    async fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, AppError> {
        self.chat_inner(request, cancel).await
    }
}

#[async_trait]
impl CredentialRefresher for MiniMaxBackend {
    /// 账号所属服务。
    fn backend_id(&self) -> &str {
        &self.record.id
    }
    /// 管理页面强制续期，使用与聊天相同的锁和持久围栏。
    async fn refresh(&self, account: &Account) -> Result<Account, AppError> {
        self.credential(&account.id, true, None).await?;
        self.account(&account.id).await
    }
}

#[async_trait]
impl QuotaReader for MiniMaxBackend {
    /// 余额所属服务。
    fn backend_id(&self) -> &str {
        &self.record.id
    }
    /// 保留积分小数，按百分之一积分整数存储，不截断余额。
    async fn read(&self, account: &Account) -> Result<QuotaInfo, AppError> {
        let value = self
            .account_json(
                &account.id,
                Method::GET,
                "/minimax-cloud/api/v1/credit/details",
            )
            .await?;
        let details = value["details"]
            .as_array()
            .or(value["data"]["details"].as_array())
            .ok_or_else(|| wire::protocol_error("MiniMax 积分账本格式已变化"))?;
        let mut total = 0i64;
        let mut used = 0i64;
        let mut remaining = 0i64;
        let now = crate::records::now_seconds() * 1000;
        let mut expire = None;
        for item in details {
            let expiry = item["expire_at_ms"]
                .as_u64()
                .ok_or_else(|| wire::protocol_error("MiniMax 积分批次缺少到期时间"))?;
            if expiry <= now {
                continue;
            }
            total = total
                .checked_add(credit_units(&item["granted_amount"])?)
                .ok_or_else(|| wire::protocol_error("MiniMax 积分余额溢出"))?;
            used = used
                .checked_add(credit_units(&item["consumed_amount"])?)
                .ok_or_else(|| wire::protocol_error("MiniMax 积分余额溢出"))?;
            remaining = remaining
                .checked_add(credit_units(&item["remaining_amount"])?)
                .ok_or_else(|| wire::protocol_error("MiniMax 积分余额溢出"))?;
            expire = Some(expire.map_or(expiry, |v: u64| v.min(expiry)));
        }
        Ok(QuotaInfo {
            routing: Some(crate::accounts::quota::QuotaSnapshot {
                remaining: Some(remaining.to_string()),
                unit: "0.01 积分".into(),
                quota_scope_id: format!(
                    "minimax:{}:{}",
                    self.oauth.region,
                    Credential::parse(&account.credential)?.subject
                ),
                valid_until: expire.map(|value| value / 1000),
                unlimited: false,
                complete: true,
            }),
            backend_id: self.record.id.clone(),
            account_id: account.id.clone(),
            total: Some(total),
            used: Some(used),
            remaining: Some(remaining),
            unit: Some("0.01 积分".into()),
            reset_at: expire.and_then(|v| {
                time::OffsetDateTime::from_unix_timestamp((v / 1000) as i64)
                    .ok()
                    .and_then(|date| {
                        date.format(&time::format_description::well_known::Rfc3339)
                            .ok()
                    })
            }),
            confidence: QuotaConfidence::Verified,
            note: Some("仅统计未过期积分；数值单位为百分之一积分，不代表订阅套餐窗口".into()),
            checked_at: crate::records::now_seconds(),
        })
    }
}

/// 将账本的小数金额换成百分之一积分，拒绝未知、负数和非有限值。
fn credit_units(value: &Value) -> Result<i64, AppError> {
    let number = value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse::<f64>().ok()))
        .filter(|v| v.is_finite() && *v >= 0.0 && *v < (i64::MAX / 100) as f64)
        .ok_or_else(|| wire::protocol_error("MiniMax 账本金额无效"))?;
    Ok((number * 100.0).round() as i64)
}

#[async_trait]
impl TaskProvider for MiniMaxBackend {
    /// 每日账号签到。
    fn kind(&self) -> TaskKind {
        TaskKind::Checkin
    }
    /// 签到所属服务。
    fn backend_id(&self) -> &str {
        &self.record.id
    }
    /// 无账号参数不领取。
    async fn run(&self, _period: &str) -> Result<TaskOutcome, AppError> {
        Err(AppError::bad_request("MiniMax 签到需要指定账号"))
    }
    /// 复用批量、随机间隔和定时调度。
    async fn run_for_account(&self, id: &str, _period: &str) -> Result<TaskOutcome, AppError> {
        self.checkin(id).await
    }
}
