//! 魔搭官方推理传输、魔粒余额与每日登录奖励；访问令牌只存于服务配置。

use super::{Backend, CostTier, openai_compat::OpenAiCompatBackend};
use crate::accounts::{
    Account, AccountState,
    quota::*,
    quota_refresh,
    remote::{QuotaConfidence, QuotaInfo, QuotaReader},
};
use crate::core::{
    AppError, Capabilities, ChatRequest, ModelInfo, UpstreamError, stream::EventStream,
};
use crate::store::{BackendRecord, Store};
use crate::tasks::{TaskKind, TaskOutcome, TaskProvider, TaskStatus, scheduler};
use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

pub(crate) const SOURCE: &str = "modelscope_backend_api_key";

/// 精确识别官方推理地址，旧兼容配置可在重载时迁移。
pub(crate) fn official_url(base: &str) -> bool {
    reqwest::Url::parse(base.trim()).is_ok_and(|url| {
        url.origin().ascii_serialization() == "https://api-inference.modelscope.cn"
            && url.username().is_empty()
            && url.password().is_none()
            && url.path().trim_end_matches('/') == "/v1"
            && url.query().is_none()
            && url.fragment().is_none()
    })
}

/// 生产端点固定为官网；回环端点供集成测试使用，不允许任意网站接收访问令牌。
pub(crate) fn portal(record: &BackendRecord) -> Result<String, AppError> {
    if official_url(&record.base_url) {
        return Ok("https://modelscope.cn".into());
    }
    let url = reqwest::Url::parse(record.base_url.trim())
        .map_err(|_| AppError::bad_request("ModelScope 推理地址无效"))?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if loopback
        && matches!(url.scheme(), "http" | "https")
        && url.path().trim_end_matches('/') == "/v1"
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
    {
        return Ok(url.origin().ascii_serialization());
    }
    Err(AppError::bad_request(
        "ModelScope 需要官方推理地址 https://api-inference.modelscope.cn/v1",
    ))
}

/// 引用账号以服务及令牌指纹区分身份，替换令牌后不沿用旧余额和领取记录。
pub(crate) fn reference(record: &BackendRecord) -> Account {
    let fingerprint = format!("{:x}", Sha256::digest(record.api_key.as_bytes()));
    Account {
        id: format!("modelscope:{}:{}", record.id, &fingerprint[..24]),
        backend_id: record.id.clone(),
        name: "魔搭 Access Token".into(),
        credential: json!({"source":SOURCE,"fingerprint":fingerprint}).to_string(),
        enabled: record.enabled,
    }
}

/// 三项能力共享同一令牌来源，推理继续使用现有 OpenAI 协议转换。
pub struct ModelScopeBackend {
    record: BackendRecord,
    store: crate::plugins::ScopedHostServices,
    portal: String,
    client: reqwest::Client,
    transport: OpenAiCompatBackend,
    details: Mutex<HashMap<String, (Instant, Value)>>,
}

impl ModelScopeBackend {
    /// 创建携带令牌的专用客户端，余额和登录请求不跟随重定向。
    pub fn new(record: &BackendRecord, store: Store, timeout: Duration) -> Result<Self, AppError> {
        Self::new_with_host(
            record,
            crate::plugins::ScopedHostServices::legacy(record, &[], store, timeout),
        )
    }

    /// 专属余额和兼容推理共用单个服务宿主。
    pub fn new_with_host(
        record: &BackendRecord,
        host: crate::plugins::ScopedHostServices,
    ) -> Result<Self, AppError> {
        let timeout = host.connect_timeout;
        Ok(Self {
            portal: portal(record)?,
            record: record.clone(),
            store: host,
            client: reqwest::Client::builder()
                .connect_timeout(timeout)
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            transport: OpenAiCompatBackend::new_with_timeout(record, "modelscope", timeout)?,
            details: Mutex::new(HashMap::new()),
        })
    }

    /// 公开模型详情不携带令牌；编码路径片段并在目录刷新预算内完成。
    async fn model_detail(&self, id: &str, deadline: Instant) -> Option<Value> {
        let parts = id.split('/').collect::<Vec<_>>();
        if parts.len() != 2
            || parts
                .iter()
                .any(|part| part.is_empty() || *part == "." || *part == "..")
        {
            return None;
        }
        let remaining = deadline.checked_duration_since(Instant::now())?;
        let mut url = reqwest::Url::parse(&format!("{}/api/v1/models", self.portal)).ok()?;
        {
            let mut path = url.path_segments_mut().ok()?;
            for part in parts {
                path.push(part);
            }
        }
        let response = self
            .client
            .get(url)
            .timeout(remaining.min(Duration::from_secs(4)))
            .send()
            .await
            .ok()?;
        Self::decode(response, true).await.ok()
    }

    /// 外部请求前核对当前服务、令牌和引用账号，旧执行器不能使用已替换身份。
    async fn account(&self, id: &str) -> Result<Account, AppError> {
        let backend = self.store.get_backend(&self.record.id).await?;
        if backend.is_none_or(|current| {
            !current.enabled
                || current.kind != "modelscope"
                || current.api_key != self.record.api_key
                || current.base_url != self.record.base_url
                || current.options != self.record.options
        }) || self.record.api_key.trim().is_empty()
        {
            return Err(AppError::bad_request(
                "ModelScope 服务或访问令牌已变更，请刷新配置",
            ));
        }
        let expected = reference(&self.record);
        self.store
            .get_account(id)
            .await?
            .filter(|account| {
                account.enabled
                    && account.id == expected.id
                    && account.backend_id == expected.backend_id
                    && account.credential == expected.credential
            })
            .ok_or_else(|| AppError::bad_request("ModelScope 引用账号已变更或停用"))
    }

    /// 仅接受官方成功信封；错误消息不回显令牌、Cookie 或响应正文。
    async fn decode(response: reqwest::Response, sdk: bool) -> Result<Value, AppError> {
        if !response.status().is_success() {
            return Err(AppError::Upstream(UpstreamError::status(
                response.status().as_u16(),
                "ModelScope 接口拒绝请求，请核对访问令牌",
            )));
        }
        let value: Value = response.json().await.map_err(|_| protocol_error())?;
        let success = value
            .get(if sdk { "Success" } else { "success" })
            .and_then(Value::as_bool);
        if success != Some(true)
            || sdk
                && value
                    .get("Code")
                    .is_none_or(|code| code != 200 && code != "200")
        {
            return Err(AppError::bad_request(
                "ModelScope 接口未通过业务校验，请核对访问令牌",
            ));
        }
        value
            .get(if sdk { "Data" } else { "data" })
            .cloned()
            .ok_or_else(protocol_error)
    }

    /// 魔粒接口使用 Bearer Access Token，GET 不触发领取动作。
    async fn magicubes(&self, path: &str) -> Result<Value, AppError> {
        let response = self
            .client
            .get(format!("{}/openapi/v1/magicubes/{path}", self.portal))
            .bearer_auth(&self.record.api_key)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|_| protocol_error())?;
        Self::decode(response, false).await
    }

    /// 只用 available_balance 表示可用额度；总余额、冻结金额单独解释。
    fn balance(&self, account: &Account, data: Value) -> Result<QuotaInfo, AppError> {
        let available = data
            .get("available_balance")
            .and_then(value_units)
            .filter(|value| *value >= 0)
            .ok_or_else(protocol_error)?;
        let total = data
            .get("total_balance")
            .and_then(value_units)
            .filter(|value| *value >= 0)
            .ok_or_else(protocol_error)?;
        let frozen = data
            .get("frozen_amount")
            .and_then(value_units)
            .filter(|value| *value >= 0)
            .ok_or_else(protocol_error)?;
        let disabled = data
            .get("is_disabled")
            .and_then(Value::as_bool)
            .ok_or_else(protocol_error)?;
        let checked_at = crate::records::now_seconds();
        Ok(QuotaInfo {
            backend_id: self.record.id.clone(),
            account_id: account.id.clone(),
            used: None,
            total: None,
            remaining: None,
            unit: Some("魔粒".into()),
            reset_at: None,
            confidence: QuotaConfidence::Verified,
            note: Some(format!(
                "总余额 {} 魔粒，冻结 {} 魔粒{}；每日奖励按发放时间起 24 小时有效",
                decimal_string(total),
                decimal_string(frozen),
                if disabled {
                    "；钱包已停用，可用额度按 0 处理"
                } else {
                    ""
                }
            )),
            checked_at,
            routing: Some(QuotaSnapshot {
                remaining: Some(decimal_string(if disabled { 0 } else { available })),
                unit: "魔粒".into(),
                quota_scope_id: format!("{}/{}", self.record.id, account.id),
                valid_until: Some(checked_at.saturating_add(60)),
                unlimited: false,
                complete: true,
            }),
        })
    }
}

#[async_trait]
impl QuotaReader for ModelScopeBackend {
    /// 返回额度归属。
    fn backend_id(&self) -> &str {
        &self.record.id
    }
    /// 查询后再次校验身份，凭证变更的迟到响应不能发布。
    async fn read(&self, account: &Account) -> Result<QuotaInfo, AppError> {
        let current = self.account(&account.id).await?;
        if current.credential != account.credential {
            return Err(protocol_error());
        }
        let data = self.magicubes("balance").await?;
        self.account(&account.id).await?;
        self.balance(account, data)
    }
}

#[async_trait]
impl Backend for ModelScopeBackend {
    /// 返回服务标识。
    fn id(&self) -> &str {
        self.transport.id()
    }
    /// 返回服务名称。
    fn name(&self) -> &str {
        self.transport.name()
    }
    /// 声明魔搭类型。
    fn kind(&self) -> &str {
        "modelscope"
    }
    /// 沿用显式能力配置。
    fn capabilities(&self) -> Capabilities {
        self.transport.capabilities()
    }
    /// 沿用费用配置。
    fn cost_tier(&self) -> CostTier {
        self.transport.cost_tier()
    }
    /// 沿用排序配置。
    fn priority(&self) -> u8 {
        self.transport.priority()
    }
    /// 沿用代理预算。
    fn daily_token_budget(&self) -> u64 {
        self.transport.daily_token_budget()
    }
    /// 保留带组织前缀的模型白名单。
    fn static_models(&self) -> Vec<String> {
        self.transport.static_models()
    }
    /// 推理目录确定可调用 ID，官方任务详情补全模型能力；刷新失败保留已知详情。
    async fn list_models(&self) -> Result<Vec<ModelInfo>, AppError> {
        let models = self.transport.list_models().await?;
        let cached = self
            .details
            .lock()
            .ok()
            .map(|mut cache| {
                cache.retain(|id, _| models.iter().any(|model| &model.id == id));
                cache.clone()
            })
            .unwrap_or_default();
        // 推理列表最多 15 秒，详情共用 9 秒预算，不突破路由后台的 25 秒超时。
        let deadline = Instant::now() + Duration::from_secs(9);
        let mut enriched = stream::iter(models.into_iter().enumerate())
            .map(|(index, mut model)| {
                let cached = cached.get(&model.id).cloned();
                async move {
                    let fresh = if cached
                        .as_ref()
                        .is_some_and(|(at, _)| at.elapsed() < Duration::from_secs(300))
                    {
                        None
                    } else {
                        self.model_detail(&model.id, deadline).await
                    };
                    let detail = fresh
                        .as_ref()
                        .or_else(|| cached.as_ref().map(|(_, value)| value));
                    let chat = detail.is_none_or(super::catalog::chat_model);
                    if let Some(detail) = detail {
                        model.capabilities =
                            super::catalog::capabilities(detail, &model.capabilities);
                        let limits = super::catalog::token_limits(detail);
                        model.token_limits.context_window =
                            limits.context_window.or(model.token_limits.context_window);
                        model.token_limits.max_input_tokens = limits
                            .max_input_tokens
                            .or(model.token_limits.max_input_tokens);
                        model.token_limits.max_output_tokens = limits
                            .max_output_tokens
                            .or(model.token_limits.max_output_tokens);
                    }
                    (index, model, chat, fresh)
                }
            })
            .buffer_unordered(8)
            .collect::<Vec<_>>()
            .await;
        enriched.sort_by_key(|(index, _, _, _)| *index);
        if let Ok(mut cache) = self.details.lock() {
            for (_, model, _, fresh) in &mut enriched {
                if let Some(detail) = fresh.take() {
                    cache.insert(model.id.clone(), (Instant::now(), detail));
                }
            }
        }
        Ok(enriched
            .into_iter()
            .filter_map(|(_, model, chat, _)| chat.then_some(model))
            .collect())
    }
    /// 单令牌引用账号参与路由额度选择。
    fn routing_accounts(&self) -> Option<Vec<AccountState>> {
        let account = reference(&self.record);
        Some(if self.record.api_key.trim().is_empty() {
            vec![]
        } else {
            vec![AccountState {
                id: account.id,
                name: account.name,
                enabled: account.enabled,
                cooling: false,
                cooldown_remaining_secs: 0,
                consecutive_failures: 0,
            }]
        })
    }
    /// 路由读取与控制台使用同一魔粒快照。
    async fn routing_quota(&self, id: &str) -> Result<QuotaInfo, AppError> {
        self.read(&self.account(id).await?).await
    }
    /// 发送前校验令牌仍属于此实例。
    async fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, AppError> {
        self.account(&reference(&self.record).id).await?;
        self.transport.chat(request, cancel).await
    }
}

/// 今日每日登录奖励的官方状态。
struct Rule {
    used: u64,
    earned: i64,
}
/// 两个独立奖励规则，绑定奖励不能从每日奖励推算。
struct Rules {
    daily: Rule,
    aliyun: Rule,
}
impl Rules {
    /// 缺失或无效状态不能被当成未领取。
    fn parse(data: &Value) -> Result<Self, AppError> {
        let rows = data.as_array().ok_or_else(protocol_error)?;
        let rule = |key: &str| -> Result<Rule, AppError> {
            let row = rows
                .iter()
                .find(|row| row["rule_key"] == key)
                .ok_or_else(protocol_error)?;
            Ok(Rule {
                used: row
                    .get("today_used")
                    .and_then(Value::as_u64)
                    .ok_or_else(protocol_error)?,
                earned: row
                    .get("today_earned_amount")
                    .and_then(Value::as_i64)
                    .filter(|amount| *amount >= 0)
                    .ok_or_else(protocol_error)?,
            })
        };
        Ok(Self {
            daily: rule("daily_active")?,
            aliyun: rule("aliyun_bindlogin")?,
        })
    }
    /// 只汇总官方今日实际发放数量。
    fn earned(&self) -> Option<i64> {
        self.daily.earned.checked_add(self.aliyun.earned)
    }
    /// 绑定状态未知时必须确认两项都完成。
    fn done(&self, bound: Option<bool>) -> bool {
        self.daily.used > 0 && (self.aliyun.used > 0 || bound == Some(false))
    }
}

/// 签到任务通过 SDK 官方登录触发奖励，Cookie 仅在本次任务的内存中保存。
pub struct ModelScopeCheckin {
    backend: Arc<ModelScopeBackend>,
}
impl ModelScopeCheckin {
    /// 复用服务凭证和现有额度更新锁。
    pub fn new(
        record: &BackendRecord,
        store: Store,
        refresh: Arc<quota_refresh::Control>,
    ) -> Result<Self, AppError> {
        let host = crate::plugins::ScopedHostServices::new(
            record,
            &[],
            store,
            Duration::from_secs(10),
            refresh,
        );
        Ok(Self::from_backend(Arc::new(
            ModelScopeBackend::new_with_host(record, host)?,
        )))
    }

    /// 注册任务视图时复用推理与额度实例。
    pub fn from_backend(backend: Arc<ModelScopeBackend>) -> Self {
        Self { backend }
    }
    /// 使用官方 SDK 的 AccessToken 登录，禁止持久化登录响应或 Cookie。
    async fn login(&self) -> Result<String, AppError> {
        let response = self
            .backend
            .client
            .post(format!("{}/api/v1/login", self.backend.portal))
            .json(&json!({"AccessToken":self.backend.record.api_key}))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|_| protocol_error())?;
        let cookies = response
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|header| header.to_str().ok())
            .filter_map(|header| header.split(';').next())
            .filter(|pair| pair.contains('='))
            .collect::<Vec<_>>()
            .join("; ");
        ModelScopeBackend::decode(response, true).await?;
        if !cookies
            .split("; ")
            .any(|pair| pair.starts_with("m_session_id="))
        {
            return Err(protocol_error());
        }
        Ok(cookies)
    }
    /// 网站端的每日活跃和绑定查询使用本次登录态。
    async fn session_get(&self, cookie: &str, path: &str) -> Result<Value, AppError> {
        let response = self
            .backend
            .client
            .get(format!("{}{path}", self.backend.portal))
            .header(reqwest::header::COOKIE, cookie)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|_| protocol_error())?;
        ModelScopeBackend::decode(response, true).await
    }
    /// 查询今日状态，不提交不存在的领取端点。
    async fn rules(&self) -> Result<Rules, AppError> {
        Rules::parse(&self.backend.magicubes("earn/rules").await?)
    }
    /// 登录之后同步本账号余额；余额失败保留已确认的奖励结果。
    async fn with_quota(&self, account: &Account, mut outcome: TaskOutcome) -> TaskOutcome {
        if !matches!(self.backend.store.refresh_quota(account, self.backend.as_ref()).await,
            Ok(info) if info.confidence == QuotaConfidence::Verified)
        {
            outcome.message.push_str("；额度暂未更新，可手动更新额度");
        }
        outcome
    }
    /// 一次登录后核对实发奖励；响应丢失、跨日或身份变化保留未知，禁止重放。
    async fn checkin(&self, id: &str, period: &str) -> Result<TaskOutcome, AppError> {
        let account = self.backend.account(id).await?;
        if scheduler::day_at(scheduler::now()) != period {
            return Err(AppError::bad_request(
                "ModelScope 每日任务周期不是当前北京时间日期",
            ));
        }
        let before = self.rules().await?;
        if scheduler::day_at(scheduler::now()) != period {
            return Err(AppError::bad_request(
                "ModelScope 状态查询跨日，请按新日期执行",
            ));
        }
        if before.done(None) {
            return Ok(self
                .with_quota(
                    &account,
                    TaskOutcome {
                        status: TaskStatus::AlreadyDone,
                        message: format!(
                            "ModelScope 今日登录奖励已发放 {} 魔粒",
                            before.earned().ok_or_else(protocol_error)?
                        ),
                        amount: None,
                    },
                )
                .await);
        }
        self.backend.account(id).await?;
        let session = self.login().await;
        let bound = if let Ok(cookie) = session {
            let _ = self.session_get(&cookie, "/api/v1/users/login/info").await;
            self.session_get(&cookie, "/api/v1/users/binding/check")
                .await
                .ok()
                .and_then(|data| data.get("IsBindingYun").and_then(Value::as_bool))
        } else {
            None
        };
        let after = self.rules().await;
        let outcome = if scheduler::day_at(scheduler::now()) != period
            || self.backend.account(id).await.is_err()
        {
            TaskOutcome::unknown("ModelScope 登录期间日期或访问令牌发生变化，请在官网核对奖励")
        } else if let Ok(after) = after
            && after.done(bound)
            && let Some(amount) = after
                .earned()
                .zip(before.earned())
                .and_then(|(a, b)| a.checked_sub(b))
                .filter(|a| *a >= 0)
        {
            TaskOutcome::succeeded(
                format!(
                    "ModelScope 每日登录已确认，本次新增 {amount} 魔粒{}",
                    if bound == Some(false) {
                        "；未绑定阿里云，跳过绑定登录奖励"
                    } else {
                        ""
                    }
                ),
                Some(amount),
            )
        } else {
            TaskOutcome::unknown("ModelScope 登录已尝试，今日奖励尚无法确认，请在官网核对")
        };
        Ok(self.with_quota(&account, outcome).await)
    }
}

#[async_trait]
impl TaskProvider for ModelScopeCheckin {
    /// 接入手动、批量和每日调度。
    fn kind(&self) -> TaskKind {
        TaskKind::Checkin
    }
    /// 返回所属服务。
    fn backend_id(&self) -> &str {
        &self.backend.record.id
    }
    /// 单令牌服务使用自动管理的引用账号。
    async fn run(&self, period: &str) -> Result<TaskOutcome, AppError> {
        self.checkin(&reference(&self.backend.record).id, period)
            .await
    }
    /// 保持现有任务按账号及自然日的持久幂等。
    async fn run_for_account(&self, id: &str, period: &str) -> Result<TaskOutcome, AppError> {
        self.checkin(id, period).await
    }
}

/// 官方结构或响应无法确认时返回安全错误，不回显响应正文。
fn protocol_error() -> AppError {
    AppError::Upstream(UpstreamError::status(
        502,
        "ModelScope 响应无法确认，请稍后查询或在官网核对",
    ))
}
