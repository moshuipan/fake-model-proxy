//! 小浣熊官网登录、滚动续期、原生 OpenAI 工具流和积分余额。

mod catalog;
mod checkin;
pub mod client;
pub mod credential;

use crate::accounts::{
    Account, AccountPool, FailureKind,
    remote::{CredentialRefresher, QuotaConfidence, QuotaInfo, QuotaReader},
};
use crate::backend::{Backend, CostTier, openai_compat};
use crate::core::{AppError, Capabilities, ChatRequest, ModelInfo, stream::EventStream};
use crate::store::{BackendRecord, Store};
use async_trait::async_trait;
use client::{Client, decode, protocol_error, transport_error};
use credential::Credential;
use reqwest::Method;
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

/// 凭证每次从账号库读取，热重载不会复用旧令牌。
pub struct RaccoonBackend {
    record: BackendRecord,
    models: Vec<String>,
    capabilities: Capabilities,
    accounts: Arc<AccountPool>,
    store: crate::plugins::ScopedHostServices,
    client: Client,
}

impl RaccoonBackend {
    /// 构造账号、目录和额度能力。
    pub fn new(
        record: &BackendRecord,
        accounts: Vec<Account>,
        store: Store,
    ) -> Result<Self, AppError> {
        Self::new_with_timeout(record, accounts, store, Duration::from_secs(10))
    }

    /// 跟随网关连接时限，账号 API 保持单次二十秒上限。
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

    /// 从插件宿主构造共享的推理、额度与任务实例。
    pub fn new_with_host(
        record: &BackendRecord,
        host: crate::plugins::ScopedHostServices,
    ) -> Result<Self, AppError> {
        let accounts = host.accounts();
        let timeout = host.connect_timeout;
        let mut capabilities = Capabilities {
            tools: true,
            parallel_tools: true,
            images: false,
            reasoning: false,
            context_window: None,
        };
        if !record.capabilities.trim().is_empty() {
            let overrides: openai_compat::CapabilityOverride =
                serde_json::from_str(&record.capabilities)
                    .map_err(|_| AppError::bad_request("小浣熊能力覆盖格式不正确"))?;
            overrides.apply(&mut capabilities);
        }
        Ok(Self {
            record: record.clone(),
            models: record
                .models
                .split(',')
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(ToOwned::to_owned)
                .collect(),
            capabilities,
            accounts: Arc::new(AccountPool::new(accounts)),
            store: host,
            client: Client::new(record, timeout)?,
        })
    }

    /// 校验账号归属及当前服务配置，阻止删除、停用或改地址后发送旧凭证。
    async fn load_account(&self, id: &str) -> Result<Account, AppError> {
        let configured = self.store.get_backend(&self.record.id).await?;
        if configured.is_none_or(|backend| {
            !backend.enabled
                || backend.kind != self.record.kind
                || backend.base_url != self.record.base_url
                || crate::plugins::service::catalog_identity(&backend)
                    != crate::plugins::service::catalog_identity(&self.record)
        }) {
            return Err(AppError::bad_request("小浣熊服务配置已变更，请重新操作"));
        }
        self.store
            .get_account(id)
            .await?
            .filter(|account| account.enabled && account.backend_id == self.record.id)
            .ok_or_else(|| AppError::bad_request("小浣熊账号已移除、停用或不属于此服务"))
    }

    /// 串行续期和 CAS 保存；并发 401 复用已更新令牌而不再次刷新。
    async fn credential(
        &self,
        id: &str,
        force: bool,
        rejected: Option<&str>,
    ) -> Result<Credential, AppError> {
        let current = Credential::parse(&self.load_account(id).await?.credential)?;
        if !force
            && !current.needs_refresh()
            && rejected.is_none_or(|token| token != current.access_token)
        {
            return Ok(current);
        }
        let _guard = self.store.lock_credentials().await;
        let account = self.load_account(id).await?;
        let credential = Credential::parse(&account.credential)?;
        if !force
            && !credential.needs_refresh()
            && rejected.is_none_or(|token| token != credential.access_token)
        {
            return Ok(credential);
        }
        let updated = self.client.refresh(&credential).await?;
        self.load_account(id).await?;
        self.store
            .replace_backend_credential(&account, &self.record, &updated.serialize()?)
            .await?;
        Credential::parse(&self.load_account(id).await?.credential)
    }

    /// 账号选择遵循账号池和路由锁定，不回退到其他账号的余额。
    fn select_account(&self) -> Result<Account, AppError> {
        self.accounts
            .select()
            .ok_or_else(|| AppError::bad_request("小浣熊尚无可用账号，请在控制台完成官方授权"))
    }

    /// 读取明确业务数据，只在 HTTP 401 后刷新并重试一次。
    async fn get_json(&self, id: &str, path: &str) -> Result<Value, AppError> {
        let mut credential = self.credential(id, false, None).await?;
        for attempt in 0..2 {
            let response = self
                .client
                .request(Method::GET, path, &credential)
                .timeout(Duration::from_secs(20))
                .send()
                .await
                .map_err(|_| transport_error())?;
            match decode(response).await {
                Err(error) if error.status().as_u16() == 401 && attempt == 0 => {
                    credential = self
                        .credential(id, false, Some(&credential.access_token))
                        .await?;
                }
                result => return result,
            }
        }
        Err(protocol_error("小浣熊鉴权重试失败"))
    }

    /// 所有入站经原生 OpenAI Chat SSE，保留工具、系统提示和输出上限。
    async fn chat_inner(
        &self,
        mut request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, AppError> {
        let account = self.select_account()?;
        let model = request
            .model
            .strip_prefix(&format!("{}/", self.record.id))
            .unwrap_or(&request.model)
            .to_string();
        request.stream = true;
        let body = openai_compat::OpenAiCompatBackend::build_body(&request, &model);
        let mut credential = self.credential(&account.id, false, None).await?;
        let send = |credential: &Credential| {
            self.client
                .request(Method::POST, client::CHAT_PATH, credential)
                .timeout(Duration::from_secs(300))
                .json(&body)
                .send()
        };
        let mut response = send(&credential).await.map_err(|_| transport_error())?;
        if response.status().as_u16() == 401 {
            crate::gateway::retry::additional_submission().await?;
            credential = self
                .credential(&account.id, false, Some(&credential.access_token))
                .await?;
            response = send(&credential).await.map_err(|_| transport_error())?;
        }
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
            return Err(client::http_error(status));
        }
        if !response
            .headers()
            .get("content-type")
            .and_then(|header| header.to_str().ok())
            .is_some_and(|value| value.contains("text/event-stream"))
        {
            decode(response).await?;
            return Err(protocol_error("小浣熊未返回预期的 SSE 流"));
        }
        self.accounts.report_success(&account.id);
        let (sender, receiver) = mpsc::channel(32);
        tokio::spawn(async move {
            openai_compat::pump_sse(response, sender, cancel).await;
        });
        Ok(Box::pin(ReceiverStream::new(receiver)))
    }
}

#[async_trait]
impl Backend for RaccoonBackend {
    /// 返回服务 ID。
    fn id(&self) -> &str {
        &self.record.id
    }
    /// 返回服务名称。
    fn name(&self) -> &str {
        &self.record.name
    }
    /// 返回接入类型。
    fn kind(&self) -> &str {
        "raccoon"
    }
    /// 返回配置及官方目录支持的基础能力。
    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }
    /// 混合每日积分与购买积分时费用层级由用户明确配置。
    fn cost_tier(&self) -> CostTier {
        CostTier::parse(&self.record.cost_tier)
    }
    /// 使用服务路由优先级。
    fn priority(&self) -> u8 {
        self.record.priority
    }
    /// 使用服务每日预算。
    fn daily_token_budget(&self) -> u64 {
        self.record.daily_token_budget
    }
    /// 使用已同步的模型白名单。
    fn static_models(&self) -> Vec<String> {
        self.models.clone()
    }
    /// 路由可锁定一个具体账号。
    fn routing_accounts(&self) -> Option<Vec<crate::accounts::AccountState>> {
        Some(self.accounts.snapshot())
    }
    /// 读取选中账号的实际积分余额。
    async fn routing_quota(&self, account_id: &str) -> Result<QuotaInfo, AppError> {
        QuotaReader::read(self, &self.load_account(account_id).await?).await
    }
    /// 查询真实目录，未知结构不使用硬编码旧模型填补。
    async fn list_models(&self) -> Result<Vec<ModelInfo>, AppError> {
        let account = self.select_account()?;
        let data = self.get_json(&account.id, client::MODELS_PATH).await?;
        let models = catalog::parse(&data, &self.capabilities);
        if models.is_empty() {
            return Err(protocol_error("小浣熊模型目录为空或格式已变化"));
        }
        Ok(models)
    }
    /// 客户端取消覆盖令牌获取及建流。
    async fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, AppError> {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(AppError::BadRequest {
                code: "request_cancelled".into(), message: "请求已取消".into(),
            }),
            result = self.chat_inner(request, cancel.clone()) => result,
        }
    }
}

#[async_trait]
impl CredentialRefresher for RaccoonBackend {
    /// 返回所属服务。
    fn backend_id(&self) -> &str {
        &self.record.id
    }
    /// 手动续期沿用相同串行锁和条件保存。
    async fn refresh(&self, account: &Account) -> Result<Account, AppError> {
        self.credential(&account.id, true, None).await?;
        self.load_account(&account.id).await
    }
}

#[async_trait]
impl QuotaReader for RaccoonBackend {
    /// 返回积分账户所属服务。
    fn backend_id(&self) -> &str {
        &self.record.id
    }
    /// 可用积分已包含每日积分；不重复相加，也不将 daily_points 当成午夜重置桶。
    async fn read(&self, account: &Account) -> Result<QuotaInfo, AppError> {
        let data = self.get_json(&account.id, client::BALANCE_PATH).await?;
        let balance = data
            .get("available_points")
            .and_then(crate::accounts::quota::value_units)
            .filter(|balance| *balance >= 0)
            .ok_or_else(|| protocol_error("小浣熊未返回有效可用积分"))?;
        let remaining = crate::accounts::quota::decimal_string(balance);
        let daily = data
            .get("daily_points")
            .and_then(crate::accounts::quota::value_units)
            .filter(|points| *points >= 0 && *points <= balance)
            .map(crate::accounts::quota::decimal_string);
        Ok(QuotaInfo {
            routing: Some(crate::accounts::quota::QuotaSnapshot {
                remaining: Some(remaining.clone()),
                unit: "积分".into(),
                quota_scope_id: format!("{}/{}", self.record.id, account.id),
                valid_until: None,
                unlimited: false,
                complete: true,
            }),
            backend_id: self.record.id.clone(),
            account_id: account.id.clone(),
            used: None,
            total: None,
            remaining: remaining.parse().ok(),
            unit: Some("积分".into()),
            reset_at: None,
            confidence: QuotaConfidence::Verified,
            note: Some(match daily {
                Some(daily) => {
                    format!("其中未过期每日积分 {daily}；当天登录可保留当天积分至含发放日的第七天")
                }
                None => "每日积分自动发放；当天登录可保留当天积分至含发放日的第七天".into(),
            }),
            checked_at: crate::records::now_seconds(),
        })
    }
}
