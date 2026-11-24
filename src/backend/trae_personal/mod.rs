//! TRAE CN 个人版后端：官方个人授权、动态目录与原生 raw-chat。

pub mod catalog;
pub mod checkin;
pub mod credential;
pub mod device;
mod quota;
pub mod wire;

use crate::accounts::remote::{CredentialRefresher, QuotaInfo, QuotaReader};
use crate::accounts::{Account, AccountPool, FailureKind};
use crate::backend::{Backend, CostTier};
use crate::core::stream::EventStream;
use crate::core::{AppError, Capabilities, ChatRequest, ModelInfo, UpstreamError};
use crate::store::{BackendRecord, Store};
use async_trait::async_trait;
use credential::Credential;
use device::ClientDevice;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

pub const GATEWAY: &str = "https://trae-api-cn.mchost.guru";
pub const WEBSITE: &str = "https://www.trae.cn";
/// 官方 CN AI 扩展区域配置的 versionCode；与 GUI、内核和构建版本无关。
const NATIVE_API_VERSION_CODE: &str = "20260212";

/// 运行时选项和插件 schema 共用类型，保留旧 CN 区域配置。
#[derive(Default, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct Options {
    #[serde(default)]
    #[schemars(regex(pattern = "^(|cn)$"))]
    region: String,
    /// 官方设备信息显式进入服务配置；后端不读取客户端文件或自动注册新设备。
    #[serde(default)]
    pub(crate) client_device: Option<ClientDevice>,
}

impl Options {
    /// 运行时与配置服务共享设备校验，routing 仍由宿主处理。
    pub(crate) fn parse(record: &BackendRecord) -> Result<Self, AppError> {
        let options: Value = serde_json::from_str(if record.options.trim().is_empty() {
            "{}"
        } else {
            &record.options
        })
        .map_err(|_| AppError::bad_request("TRAE 个人版选项须为 JSON 对象"))?;
        if !options.is_object()
            || options.as_object().is_some_and(|options| {
                options
                    .keys()
                    .any(|key| !matches!(key.as_str(), "region" | "routing" | "client_device"))
            })
        {
            return Err(AppError::bad_request(
                "TRAE 个人版选项仅支持 region、routing 和 client_device",
            ));
        }
        let options: Self = serde_json::from_value(options)
            .map_err(|_| AppError::bad_request("TRAE 个人版 region 或客户端设备配置无效"))?;
        if !matches!(options.region.as_str(), "" | "cn") {
            return Err(AppError::bad_request(
                "TRAE 个人版 region 必须为空字符串或 cn",
            ));
        }
        if let Some(device) = &options.client_device {
            device.validate()?;
        }
        Ok(options)
    }
}

/// 固定个人版中国区；只允许官方模型域名和本机协议测试端点。
pub fn profile(record: &BackendRecord) -> Result<String, AppError> {
    Options::parse(record)?;
    let base = if record.base_url.is_empty() {
        GATEWAY
    } else {
        record.base_url.trim_end_matches('/')
    };
    let url =
        reqwest::Url::parse(base).map_err(|_| AppError::bad_request("TRAE 个人网关地址无效"))?;
    let official = url.scheme() == "https"
        && url.host_str() == Some("trae-api-cn.mchost.guru")
        && url.port().is_none();
    let local = url.scheme() == "http"
        && url
            .host_str()
            .is_some_and(|host| matches!(host, "127.0.0.1" | "localhost" | "[::1]"));
    if !(official || local)
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(AppError::bad_request("TRAE 个人网关须为官方 CN 域名"));
    }
    Ok(url.origin().ascii_serialization())
}

type CatalogCache = HashMap<String, (tokio::time::Instant, Vec<catalog::Model>)>;

/// 模型快照按账号缓存；每次请求从 Store 读取当前凭证。
pub struct TraePersonalBackend {
    record: BackendRecord,
    accounts: Arc<AccountPool>,
    store: crate::plugins::ScopedHostServices,
    client: reqwest::Client,
    base: String,
    device: Option<ClientDevice>,
    cache: Mutex<CatalogCache>,
}

impl TraePersonalBackend {
    /// 构造后端不读取个人客户端文件，不发起网络请求。
    pub fn new(
        record: &BackendRecord,
        accounts: Vec<Account>,
        store: Store,
    ) -> Result<Self, AppError> {
        Self::new_with_timeout(record, accounts, store, Duration::from_secs(10))
    }

    /// 连接时限由网关配置；生成总时限与取消由网关管理。
    pub fn new_with_timeout(
        record: &BackendRecord,
        accounts: Vec<Account>,
        store: Store,
        timeout: Duration,
    ) -> Result<Self, AppError> {
        Self::new_with_host(
            record,
            crate::plugins::ScopedHostServices::legacy(record, &accounts, store, timeout),
        )
    }

    /// 插件所有能力共享账号宿主、HTTP 客户端与目录缓存。
    pub fn new_with_host(
        record: &BackendRecord,
        host: crate::plugins::ScopedHostServices,
    ) -> Result<Self, AppError> {
        let accounts = host.accounts();
        let timeout = host.connect_timeout;
        Ok(Self {
            record: record.clone(),
            accounts: Arc::new(AccountPool::new(accounts)),
            store: host,
            client: reqwest::Client::builder()
                .connect_timeout(timeout)
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            base: profile(record)?,
            device: Options::parse(record)?.client_device,
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// 官方换票域与模型域分别确定；本机测试共用本机服务器。
    fn auth_origin(&self) -> &str {
        if self.base.starts_with("http://") {
            &self.base
        } else {
            credential::AUTH_ORIGIN
        }
    }

    /// 个人余额和签到共用显式设备；缺省使用官方未注册回退值 0，不伪造硬件或版本。
    fn personal_api_request(&self, path: &str, credential: &Credential) -> reqwest::RequestBuilder {
        let request = self
            .client
            .post(format!("{}{path}", self.auth_origin()))
            .header(
                "Authorization",
                format!("Cloud-IDE-JWT {}", credential.access_token),
            );
        match &self.device {
            Some(device) => device.headers(request),
            None => request.header("x-device-id", "0"),
        }
    }

    /// 服务归属、启用状态和配置在每次用票前验证。
    async fn account(&self, id: &str) -> Result<Account, AppError> {
        let backend = self
            .store
            .get_backend(&self.record.id)
            .await?
            .filter(|backend| {
                backend.enabled
                    && backend.kind == "trae_personal"
                    && backend.base_url == self.record.base_url
                    && crate::plugins::service::catalog_identity(backend)
                        == crate::plugins::service::catalog_identity(&self.record)
            })
            .ok_or_else(|| AppError::bad_request("TRAE 个人服务已停用或配置已变更"))?;
        self.store
            .get_account(id)
            .await?
            .filter(|account| account.enabled && account.backend_id == backend.id)
            .ok_or_else(|| AppError::bad_request("TRAE 个人账号已删除或停用"))
    }

    /// Store 锁和持久围栏阻止多个实例重复使用同一刷新票据。
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
        if !force
            && !current.expires_soon()
            && rejected.is_none_or(|token| token != current.access_token)
        {
            return Ok(current);
        }
        if current.refresh_token.is_empty() {
            return Err(AppError::bad_request("TRAE 个人授权需要重新网页登录"));
        }
        self.store
            .begin_credential_refresh(id, &account.credential)
            .await?;
        let next = match Credential::exchange(
            &self.client,
            self.auth_origin(),
            &current.refresh_token,
            &current.subject,
        )
        .await
        {
            Ok(next) => next,
            Err(error) => {
                if error.code() == "trae_personal_oauth_rejected" {
                    self.store.abort_credential_refresh(id).await?;
                }
                return Err(error);
            }
        };
        self.store
            .finish_credential_refresh(id, &account.credential, &next.serialize()?)
            .await?;
        let latest = Credential::parse(&self.account(id).await?.credential)?;
        if latest.subject != current.subject {
            return Err(AppError::bad_request("TRAE 个人账号在续期期间已变更"));
        }
        Ok(latest)
    }

    /// 个人 raw-chat 使用 Cloud-IDE-JWT 和个人用户 ID，禁止重定向。
    fn request(&self, path: &str, credential: &Credential) -> reqwest::RequestBuilder {
        let request = self
            .client
            .post(format!("{}{path}", self.base))
            .header(
                "Authorization",
                format!("Cloud-IDE-JWT {}", credential.access_token),
            )
            .header("X-Cloudide-Token", &credential.access_token)
            .header("X-Ide-Token", &credential.access_token)
            .header("X-Uid", &credential.subject)
            .header("X-App-Id", "6eefa01c-1036-4c7e-9ca5-d891f63bfcd8")
            .header("X-Ide-Version-Code", NATIVE_API_VERSION_CODE)
            .header("X-App-Version", "default")
            .header("Accept-Encoding", "identity");
        match &self.device {
            Some(device) => device.native_headers(request),
            None => request.header("X-Ide-Version-Type", "stable"),
        }
    }

    /// 验证官方网页授权得到的个人凭证并读取目录。
    pub async fn verify(&self, credential: &Credential) -> Result<Vec<ModelInfo>, AppError> {
        Ok(self
            .fetch_catalog(credential)
            .await?
            .into_iter()
            .map(|model| model.info)
            .collect())
    }

    /// 动态目录是读取操作，不接受非 JSON 或跨域响应。
    async fn fetch_catalog(
        &self,
        credential: &Credential,
    ) -> Result<Vec<catalog::Model>, AppError> {
        let response = self
            .request("/api/ide/v1/get_detail_param", credential)
            .header("Accept", "application/json")
            .json(&json!({"function":"chat_v3","need_prompt":false,"poly_prompt":false}))
            .timeout(Duration::from_secs(25))
            .send()
            .await
            .map_err(|_| error(None, "TRAE 个人模型目录网络失败", false))?;
        if !response.status().is_success() {
            return Err(error(
                Some(response.status().as_u16()),
                "TRAE 个人目录鉴权或读取失败",
                false,
            ));
        }
        let value: Value = response
            .json()
            .await
            .map_err(|_| error(None, "TRAE 个人模型目录不是 JSON", false))?;
        catalog::parse(&value)
    }

    /// 同一账号缓存五分钟，失败不返回其他账号的权限快照。
    async fn models(&self, id: &str) -> Result<Vec<catalog::Model>, AppError> {
        let mut credential = self.credential(id, false, None).await?;
        let mut cache = self.cache.lock().await;
        if let Some((expiry, models)) = cache.get(id)
            && *expiry > tokio::time::Instant::now()
        {
            return Ok(models.clone());
        }
        let result = self.fetch_catalog(&credential).await;
        let models = match result {
            Err(AppError::Upstream(upstream)) if upstream.status == Some(401) => {
                credential = self
                    .credential(id, false, Some(&credential.access_token))
                    .await?;
                self.fetch_catalog(&credential).await?
            }
            other => other?,
        };
        cache.insert(
            id.into(),
            (
                tokio::time::Instant::now() + Duration::from_secs(300),
                models.clone(),
            ),
        );
        Ok(models)
    }
}

#[async_trait]
impl Backend for TraePersonalBackend {
    /// 服务稳定标识。
    fn id(&self) -> &str {
        &self.record.id
    }
    /// 服务显示名称。
    fn name(&self) -> &str {
        &self.record.name
    }
    /// 与退役内部类型分离。
    fn kind(&self) -> &str {
        "trae_personal"
    }
    /// 模型级能力由动态目录提供。
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tools: true,
            parallel_tools: false,
            images: false,
            reasoning: true,
            context_window: None,
        }
    }
    /// 个人权限不代表免费额度。
    fn cost_tier(&self) -> CostTier {
        CostTier::parse(&self.record.cost_tier)
    }
    /// 自动路由顺序。
    fn priority(&self) -> u8 {
        self.record.priority
    }
    /// 复用本地用量预算。
    fn daily_token_budget(&self) -> u64 {
        self.record.daily_token_budget
    }
    /// 返回已同步的模型白名单。
    fn static_models(&self) -> Vec<String> {
        self.record
            .models
            .split(',')
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(String::from)
            .collect()
    }
    /// 账号池遵守网关已经选定的账号。
    fn routing_accounts(&self) -> Option<Vec<crate::accounts::AccountState>> {
        Some(self.accounts.snapshot())
    }
    /// 直接读取个人版官方积分余额，模型扣费规则独立处理。
    async fn routing_quota(&self, id: &str) -> Result<QuotaInfo, AppError> {
        self.read_quota(id).await
    }
    /// 目录采用当前可用个人账号，不预置模型名。
    async fn list_models(&self) -> Result<Vec<ModelInfo>, AppError> {
        let account = self
            .accounts
            .select()
            .ok_or_else(|| AppError::bad_request("请先登录 TRAE 个人账号"))?;
        Ok(self
            .models(&account.id)
            .await?
            .into_iter()
            .map(|model| model.info)
            .collect())
    }
    /// 三种入站协议共用原生流；仅明确 401 可恢复一次授权。
    async fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, AppError> {
        let account = self
            .accounts
            .select()
            .ok_or_else(|| AppError::bad_request("TRAE 个人版尚无可用账号"))?;
        let model = tokio::select! {
            _ = cancel.cancelled() => return Err(AppError::bad_request("请求已取消")),
            models = self.models(&account.id) => models?,
        }
        .into_iter()
        .find(|model| model.info.id == request.model)
        .ok_or_else(|| AppError::bad_request("该 TRAE 个人账号没有此模型权限，请同步目录"))?;
        let body = wire::body(&request, &model)?;
        let mut credential = self.credential(&account.id, false, None).await?;
        for attempt in 0..2 {
            if attempt > 0 {
                crate::gateway::retry::additional_submission().await?;
            }
            let send = self
                .request("/api/ide/v2/llm_raw_chat", &credential)
                .header("Accept", "text/event-stream")
                .header("X-Ide-Function", "chat_v3")
                .header("X-App-Function", "chat_v3")
                .json(&body)
                .send();
            let response = tokio::select! {
                _ = cancel.cancelled() => return Err(AppError::bad_request("请求已取消")),
                result = send => result.map_err(|_| error(None, "TRAE 个人生成网络失败，执行结果未知", true))?,
            };
            let status = response.status().as_u16();
            if status == 401 && attempt == 0 {
                credential = self
                    .credential(&account.id, false, Some(&credential.access_token))
                    .await?;
                continue;
            }
            if !response.status().is_success() {
                crate::gateway::retry::capture_retry_after(&response);
                self.accounts
                    .report_failure(&account.id, FailureKind::classify(Some(status), ""));
                return Err(error(
                    Some(status),
                    "TRAE 个人模型请求失败",
                    status >= 500 || status == 408,
                ));
            }
            if !response
                .headers()
                .get("content-type")
                .and_then(|header| header.to_str().ok())
                .is_some_and(|header| header.contains("text/event-stream"))
            {
                return Err(error(None, "TRAE 个人网关未返回 SSE", true));
            }
            self.accounts.report_success(&account.id);
            let (sender, receiver) = tokio::sync::mpsc::channel(32);
            tokio::spawn(wire::pump(response, sender, cancel.clone()));
            return Ok(Box::pin(crate::core::stream::cancel_on_drop(
                tokio_stream::wrappers::ReceiverStream::new(receiver),
                cancel,
            )));
        }
        Err(AppError::bad_request("TRAE 个人授权恢复失败"))
    }
}

#[async_trait]
impl CredentialRefresher for TraePersonalBackend {
    /// 刷新归属服务。
    fn backend_id(&self) -> &str {
        &self.record.id
    }
    /// 管理操作与生成共享刷新围栏。
    async fn refresh(&self, account: &Account) -> Result<Account, AppError> {
        self.credential(&account.id, true, None).await?;
        self.account(&account.id).await
    }
}

#[async_trait]
impl QuotaReader for TraePersonalBackend {
    /// 额度归属服务。
    fn backend_id(&self) -> &str {
        &self.record.id
    }
    /// 续期可以轮换票据，已验证的个人 JWT 仍须绑定同一个账号。
    fn same_credential_identity(&self, before: &str, after: &str) -> bool {
        match (Credential::parse(before), Credential::parse(after)) {
            (Ok(before), Ok(after)) => before.subject == after.subject,
            _ => false,
        }
    }
    /// 个人版模型权限与计费额度分别处理。
    async fn read(&self, account: &Account) -> Result<QuotaInfo, AppError> {
        self.read_quota(&account.id).await
    }
}

/// 不回显上游正文、JWT、刷新票据或回调参数。
pub fn error(status: Option<u16>, message: &str, unknown: bool) -> AppError {
    AppError::Upstream(UpstreamError {
        status: status.map(|status| {
            if (300..400).contains(&status) {
                502
            } else {
                status
            }
        }),
        code: "trae_personal_upstream_error".into(),
        message: match status {
            Some(status) => format!("{message}（HTTP {status}）"),
            None => message.into(),
        },
        retryable: status == Some(429) && !unknown,
        execution_unknown: unknown,
    })
}
