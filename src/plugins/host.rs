//! 插件只能访问本服务的凭证与命名空间状态，不能获取整个 Store/AppState。

use crate::accounts::{
    Account, quota_refresh,
    remote::{QuotaInfo, QuotaReader},
};
use crate::core::AppError;
use crate::store::{BackendRecord, Store};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// 可信进程内插件的依赖边界，不是不可信代码沙箱。
#[derive(Clone)]
pub struct ScopedHostServices {
    store: Store,
    record: Arc<BackendRecord>,
    accounts: Arc<Vec<Account>>,
    pub connect_timeout: Duration,
    pub quota_refresh: Arc<quota_refresh::Control>,
    pub shutdown: CancellationToken,
}

/// 为只返回新凭证的旧适配器补齐宿主串行化和版本比较。
pub struct PersistedRefresher {
    inner: Arc<dyn crate::accounts::remote::CredentialRefresher>,
    host: ScopedHostServices,
}

impl PersistedRefresher {
    /// 将私有协议刷新器包装为可安全共享的管理能力。
    pub fn new(
        inner: Arc<dyn crate::accounts::remote::CredentialRefresher>,
        host: ScopedHostServices,
    ) -> Self {
        Self { inner, host }
    }
}

#[async_trait::async_trait]
impl crate::accounts::remote::CredentialRefresher for PersistedRefresher {
    /// 返回被授权的服务作用域。
    fn backend_id(&self) -> &str {
        &self.host.record.id
    }

    /// 同一账号并发调用复用新值；迟到结果通过凭证比较拒绝覆盖。
    async fn refresh(&self, account: &Account) -> Result<Account, AppError> {
        let _guard = self.host.lock_credentials().await;
        let current = self.host.require_account(&account.id).await?;
        if current.credential != account.credential {
            return Ok(current);
        }
        let updated = self.inner.refresh(&current).await?;
        if !self
            .host
            .replace_credential(&current.id, &current.credential, &updated.credential)
            .await?
        {
            return Err(AppError::bad_request(
                "account or service changed during credential refresh",
            ));
        }
        Ok(updated)
    }
}

impl ScopedHostServices {
    /// 宿主为单个服务建立作用域，过滤其他服务的账号。
    pub fn new(
        record: &BackendRecord,
        accounts: &[Account],
        store: Store,
        timeout: Duration,
        quota_refresh: Arc<quota_refresh::Control>,
    ) -> Self {
        Self {
            store,
            record: Arc::new(record.clone()),
            accounts: Arc::new(
                accounts
                    .iter()
                    .filter(|a| a.backend_id == record.id)
                    .cloned()
                    .collect(),
            ),
            connect_timeout: timeout,
            quota_refresh,
            shutdown: CancellationToken::new(),
        }
    }

    /// 兼容直接构造后端的测试和外部调用。
    pub fn legacy(
        record: &BackendRecord,
        accounts: &[Account],
        store: Store,
        timeout: Duration,
    ) -> Self {
        Self::new(record, accounts, store, timeout, Default::default())
    }

    /// 获取本 generation 的账号选择快照；使用凭证前仍从宿主读取当前值。
    pub fn accounts(&self) -> Vec<Account> {
        self.accounts.as_ref().clone()
    }

    /// 获取宿主时钟，便于插件记录有效期。
    pub fn now_seconds(&self) -> u64 {
        crate::records::now_seconds()
    }

    /// 创建不跟随重定向的 HTTP 客户端，避免凭证随上游重定向传播。
    pub fn http_client(&self) -> Result<reqwest::Client, AppError> {
        Ok(reqwest::Client::builder()
            .connect_timeout(self.connect_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()?)
    }

    /// 仅返回当前服务的配置。
    pub async fn get_backend(&self, id: &str) -> Result<Option<BackendRecord>, AppError> {
        if id != self.record.id {
            return Err(AppError::bad_request("host scope mismatch"));
        }
        self.store.get_backend(id).await
    }

    /// 仅返回当前服务所属账号；跨服务 ID 不暴露凭证。
    pub async fn get_account(&self, id: &str) -> Result<Option<Account>, AppError> {
        Ok(self
            .store
            .get_account(id)
            .await?
            .filter(|a| a.backend_id == self.record.id))
    }

    /// 列出本服务账号，禁止枚举其他插件状态。
    pub async fn list_accounts_for(&self, service_id: &str) -> Result<Vec<Account>, AppError> {
        if service_id != self.record.id {
            return Err(AppError::bad_request("host scope mismatch"));
        }
        self.store.list_accounts_for(service_id).await
    }

    /// 沿用跨重载续期锁；凭证写入另有版本比较。
    pub async fn lock_credentials(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.store.lock_credentials().await
    }

    /// 校验当前账号仍属于该服务。
    async fn require_account(&self, id: &str) -> Result<Account, AppError> {
        self.get_account(id)
            .await?
            .ok_or_else(|| AppError::bad_request("account outside host scope or deleted"))
    }

    /// 带凭证版本与服务配置比较地提交新值。
    pub async fn replace_credential(
        &self,
        id: &str,
        expected: &str,
        next: &str,
    ) -> Result<bool, AppError> {
        let mut account = self.require_account(id).await?;
        account.credential = expected.into();
        self.store
            .replace_backend_credential(&account, &self.record, next)
            .await
    }

    /// 为现有续期实现保留带配置比较的宿主入口。
    pub async fn replace_backend_credential(
        &self,
        account: &Account,
        record: &BackendRecord,
        next: &str,
    ) -> Result<bool, AppError> {
        if record.id != self.record.id || account.backend_id != self.record.id {
            return Err(AppError::bad_request("host scope mismatch"));
        }
        self.store
            .replace_backend_credential(account, &self.record, next)
            .await
    }

    /// 恢复该账号已经完成的换票提交，不重新请求上游。
    pub async fn recover_credential_refresh(
        &self,
        id: &str,
        current: &str,
    ) -> Result<(), AppError> {
        self.require_account(id).await?;
        self.store.recover_credential_refresh(id, current).await
    }

    /// 在可能轮换令牌的动作前写入持久围栏。
    pub async fn begin_credential_refresh(&self, id: &str, expected: &str) -> Result<(), AppError> {
        self.require_account(id).await?;
        self.store.begin_credential_refresh(id, expected).await
    }

    /// 仅在上游明确拒绝换票后释放围栏。
    pub async fn abort_credential_refresh(&self, id: &str) -> Result<(), AppError> {
        self.require_account(id).await?;
        self.store.abort_credential_refresh(id).await
    }

    /// 凭证轮换是运行状态，不递增配置 revision。
    pub async fn finish_credential_refresh(
        &self,
        id: &str,
        expected: &str,
        next: &str,
    ) -> Result<(), AppError> {
        self.require_account(id).await?;
        self.store
            .finish_credential_refresh(id, expected, next)
            .await
    }

    /// 签到后通过共享额度协调器刷新本服务账号。
    pub async fn refresh_quota(
        &self,
        account: &Account,
        reader: &dyn QuotaReader,
    ) -> Result<QuotaInfo, AppError> {
        if account.backend_id != self.record.id || reader.backend_id() != self.record.id {
            return Err(AppError::bad_request("host scope mismatch"));
        }
        quota_refresh::refresh_account_with_reader(
            &self.store,
            &self.quota_refresh,
            &self.record,
            account,
            reader,
        )
        .await
    }

    /// 读取当前插件与服务专属的小状态，版本不匹配由插件迁移。
    pub async fn read_state(
        &self,
        key: &str,
    ) -> Result<Option<(u32, serde_json::Value)>, AppError> {
        self.store
            .plugin_state(&self.record.kind, &self.record.id, key)
            .await
    }

    /// 更新插件小状态；账号、额度、任务仍使用公共类型化存储。
    pub async fn write_state(
        &self,
        key: &str,
        version: u32,
        value: &serde_json::Value,
    ) -> Result<(), AppError> {
        self.store
            .save_plugin_state(&self.record.kind, &self.record.id, key, version, value)
            .await
    }
}
