//! 配置应用协调器：校验、计划、预备、事务提交、发布，所有写入口共用。

use super::config_types::*;
use crate::{
    accounts::Account, core::AppError, model_names::ModelAlias, state::AppState,
    store::BackendRecord,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::Ordering;

pub use crate::plugins::service::connection_identity;

pub type PendingSecrets = BTreeMap<String, String>;

/// 对当前 AppState 的短生命周期用例视图，无第二份运行状态。
pub struct ConfigService<'a> {
    pub(super) state: &'a AppState,
}

/// 写入令牌固定锁顺序，授权会话可在等待期间继续响应取消。
pub struct ConfigWriteGuard<'a> {
    state: &'a AppState,
    _configuration: tokio::sync::MutexGuard<'a, ()>,
    _credentials: tokio::sync::MutexGuard<'a, ()>,
}

/// 可审阅的计划没有秘密值；标明涉及实例和连接重建。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigPlan {
    pub id: String,
    pub expected_revision: u64,
    pub digest: String,
    pub contract_digest: String,
    pub expires_at: u64,
    pub changes: Vec<ConfigDiff>,
    pub requires_restart: bool,
}

/// 字段路径而非敏感旧值/新值描述变更。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigDiff {
    pub path: String,
    pub action: String,
    pub rebuild_connection: bool,
}

/// active_revision 由无 I/O 发布完成后返回；持久记录支持幂等重试。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyResult {
    pub desired_revision: u64,
    pub active_revision: u64,
    pub changes: Vec<ConfigDiff>,
}

/// 配置读取同时暴露期望和实际生效版本。
#[derive(Debug, Clone, Serialize)]
pub struct ConfigView {
    pub desired_revision: u64,
    pub active_revision: u64,
    pub document: ConfigDocument,
}

/// 提交前完整构造；发布步骤不做数据库、网络或可失败配置校验。
struct Prepared {
    registry: crate::backend::RegistrySnapshot,
    records: Vec<BackendRecord>,
    accounts: Vec<Account>,
    catalogs: std::collections::HashMap<String, Vec<crate::core::ModelInfo>>,
}

impl<'a> ConfigService<'a> {
    /// 复用应用的串行协调器和运行注册表。
    pub fn new(state: &'a AppState) -> Self {
        Self { state }
    }

    /// 所有配置写入先锁配置再锁凭证；不在等待锁时占用授权终态。
    pub async fn lock_write(&self) -> ConfigWriteGuard<'a> {
        ConfigWriteGuard {
            state: self.state,
            _configuration: self.state.reload_lock.lock().await,
            _credentials: self.state.store.lock_credentials().await,
        }
    }

    /// 从旧库建立首次基线，此后版本文档为唯一配置事实源。
    async fn baseline(&self) -> Result<(u64, ConfigDocument), AppError> {
        let (revision, document) = self.state.store.config_current().await?;
        if let Some(document) = document {
            return Ok((revision, document));
        }
        let records = self.state.store.list_backends().await?;
        for record in &records {
            let control = crate::plugins::builtin::registry()
                .get(&record.kind)?
                .account_control();
            if control.managed {
                let derived = (control.derive)(record);
                for old in self.state.store.list_accounts_for(&record.id).await? {
                    if !derived.iter().any(|a| a.id == old.id) {
                        self.state.store.delete_account(&old.id).await?;
                    }
                }
                for account in derived {
                    self.state.store.upsert_account(&account).await?;
                }
            }
        }
        let catalogs = self.state.store.list_model_catalogs().await?;
        self.state
            .store
            .ensure_model_aliases(&records, &catalogs)
            .await?;
        let mut secrets = PendingSecrets::new();
        let services = records
            .iter()
            .map(|r| from_record(r, &mut secrets))
            .collect::<Result<_, _>>()?;
        let mut accounts = Vec::new();
        for account in self.state.store.list_accounts().await? {
            if !records
                .iter()
                .find(|r| r.id == account.backend_id)
                .is_some_and(|r| {
                    crate::plugins::builtin::registry()
                        .get(&r.kind)
                        .is_ok_and(|p| p.account_control().managed)
                })
            {
                accounts.push(from_account(&account, &mut secrets));
            }
        }
        let checkin = self.state.store.checkin_settings().await?;
        let mut document = ConfigDocument {
            api_version: "fmp/v2".into(),
            kind: "Configuration".into(),
            services,
            accounts,
            model_aliases: self.state.store.list_model_aliases().await?,
            gateway: self.state.store.gateway_settings().await?,
            checkin: crate::tasks::scheduler::ScheduleInput {
                enabled: checkin.enabled,
                time: checkin.time,
                random_minutes: checkin.random_minutes,
            },
        };
        document.normalize();
        let (records, _) = self.records(&document, &secrets).await?;
        self.state
            .store
            .bootstrap_configuration(&document, &secrets, &records)
            .await?;
        Ok((0, document))
    }

    /// 读取完整脱敏声明；引用名称可见，秘密值永不返回。
    pub async fn current(&self) -> Result<ConfigView, AppError> {
        let _guard = self.state.reload_lock.lock().await;
        let (revision, document) = self.baseline().await?;
        Ok(ConfigView {
            desired_revision: revision,
            active_revision: self.state.active_revision.load(Ordering::Acquire),
            document,
        })
    }

    /// 启动或动态凭证刷新后重新装配已提交配置，失败不会发布半个快照。
    pub async fn activate(&self) -> Result<(), AppError> {
        let _guard = self.lock_write().await;
        let (revision, document) = self.baseline().await?;
        let prepared = self.prepare(&document, &PendingSecrets::new()).await?;
        self.publish(prepared, &document, revision);
        Ok(())
    }

    /// 只做结构、插件版本、引用与语义检查，不产生远程请求或配置 revision。
    pub async fn validate(&self, change: ConfigChange) -> Result<ValidationReport, AppError> {
        let _guard = self.state.reload_lock.lock().await;
        let (_, mut document) = self.baseline().await?;
        document.merge(change)?;
        Ok(self
            .validate_document(&document, &PendingSecrets::new())
            .await)
    }

    /// 在指定版本创建可持久审阅的计划，过期后必须重新规划。
    pub async fn plan(&self, expected: u64, change: ConfigChange) -> Result<ConfigPlan, AppError> {
        let _guard = self.state.reload_lock.lock().await;
        let (revision, before) = self.baseline().await?;
        require_revision(revision, expected)?;
        let mut document = before.clone();
        document.merge(change)?;
        self.complete_aliases(&mut document).await?;
        require_valid(
            self.validate_document(&document, &PendingSecrets::new())
                .await,
        )?;
        let plan = make_plan(expected, &before, &document);
        self.state.store.save_config_plan(&plan, &document).await?;
        Ok(plan)
    }

    /// 绑定计划摘要的幂等 apply；提交前重新校验所有引用及完整运行实例。
    pub async fn apply(&self, id: &str, key: &str, actor: &str) -> Result<ApplyResult, AppError> {
        valid_key(key)?;
        let _guard = self.lock_write().await;
        let (plan, document) = self.state.store.config_plan(id).await?;
        let request_digest = digest(&json!({"plan_id":id,"digest":plan.digest}));
        if let Some(mut result) = self
            .state
            .store
            .config_apply_result(key, &request_digest)
            .await?
        {
            result.active_revision = self.state.active_revision.load(Ordering::Acquire);
            return Ok(result);
        }
        if plan.expires_at <= now() {
            return Err(error("plan_expired", "计划已过期，请重新生成"));
        }
        if plan.contract_digest != contract_digest() {
            return Err(error(
                "plugin_incompatible",
                "插件契约已变化，请重新生成计划",
            ));
        }
        if plan.digest != digest(&document) {
            return Err(error("plan_invalid", "计划摘要不匹配"));
        }
        let (revision, _) = self.baseline().await?;
        require_revision(revision, plan.expected_revision)?;
        self.commit(
            &document,
            &PendingSecrets::new(),
            &plan,
            key,
            &request_digest,
            actor,
        )
        .await
    }

    /// 旧 API 和授权完成也走相同事务；闭包仅编辑声明和待写入秘密。
    pub async fn mutate<F>(&self, actor: &str, edit: F) -> Result<ApplyResult, AppError>
    where
        F: FnOnce(&mut ConfigDocument, &mut PendingSecrets) -> Result<(), AppError>,
    {
        let guard = self.lock_write().await;
        self.mutate_locked(&guard, actor, edit).await
    }

    /// 授权终态持有写入令牌后复用同一事务，不递归获取互斥锁。
    async fn mutate_locked<F>(
        &self,
        guard: &ConfigWriteGuard<'_>,
        actor: &str,
        edit: F,
    ) -> Result<ApplyResult, AppError>
    where
        F: FnOnce(&mut ConfigDocument, &mut PendingSecrets) -> Result<(), AppError>,
    {
        if !std::ptr::eq(guard.state, self.state) {
            return Err(error("scope_invalid", "配置写入令牌不属于当前应用"));
        }
        let (revision, before) = self.baseline().await?;
        let mut document = before.clone();
        let mut secrets = PendingSecrets::new();
        edit(&mut document, &mut secrets)?;
        document.normalize();
        self.complete_aliases(&mut document).await?;
        let plan = make_plan(revision, &before, &document);
        if plan.changes.is_empty() {
            if !self.state.configuration_initialized.load(Ordering::Acquire) {
                let prepared = self.prepare(&document, &secrets).await?;
                self.publish(prepared, &document, revision);
            }
            return Ok(ApplyResult {
                desired_revision: revision,
                active_revision: self.state.active_revision.load(Ordering::Acquire),
                changes: Vec::new(),
            });
        }
        let key = uuid::Uuid::new_v4().to_string();
        self.commit(&document, &secrets, &plan, &key, &plan.digest, actor)
            .await
    }

    /// 回滚形成普通计划；继续使用当前凭证引用，绝不回退已轮换 token。
    pub async fn rollback_plan(&self, expected: u64, target: u64) -> Result<ConfigPlan, AppError> {
        let _guard = self.state.reload_lock.lock().await;
        let (revision, before) = self.baseline().await?;
        require_revision(revision, expected)?;
        let mut document = self.state.store.config_revision(target).await?;
        for account in &mut document.accounts {
            if let Some(current) = before.accounts.iter().find(|a| a.id == account.id) {
                account.credential = current.credential.clone();
            }
        }
        for service in &mut document.services {
            if let Some(current) = before.services.iter().find(|s| s.id == service.id) {
                service.credentials = current.credentials.clone();
            }
        }
        // 已公开的名称必须一直保留，包括目标版本之后创建的名称。
        for alias in &before.model_aliases {
            if !document
                .model_aliases
                .iter()
                .any(|a| a.alias == alias.alias)
            {
                let mut alias = alias.clone();
                alias.is_primary = !document
                    .model_aliases
                    .iter()
                    .any(|a| a.is_primary && a.address() == alias.address());
                document.model_aliases.push(alias);
            }
        }
        document.normalize();
        require_valid(
            self.validate_document(&document, &PendingSecrets::new())
                .await,
        )?;
        let plan = make_plan(revision, &before, &document);
        self.state.store.save_config_plan(&plan, &document).await?;
        Ok(plan)
    }

    /// 保存登录结果时再次比较服务连接身份，拒绝跨区域或撤销后的迟到授权。
    pub async fn save_login(
        &self,
        account: &Account,
        expected: &BackendRecord,
    ) -> Result<bool, AppError> {
        let guard = self.lock_write().await;
        self.save_login_locked(&guard, account, expected).await
    }

    /// 调用者已获得写锁并复查授权有效期，将身份提交为配置 revision。
    pub async fn save_login_locked(
        &self,
        guard: &ConfigWriteGuard<'_>,
        account: &Account,
        expected: &BackendRecord,
    ) -> Result<bool, AppError> {
        // 兼容投影也是旧适配器的条件保存围栏，不能接受绕过服务发生的外部变更。
        if self
            .state
            .store
            .get_backend(&expected.id)
            .await?
            .is_none_or(|current| {
                !current.enabled
                    || crate::plugins::service::catalog_identity(&current)
                        != crate::plugins::service::catalog_identity(expected)
            })
        {
            return Ok(false);
        }
        let result = self
            .mutate_locked(guard, "auth.complete", |document, secrets| {
                let current = document
                    .services
                    .iter()
                    .find(|s| s.id == expected.id)
                    .ok_or_else(|| error("auth_service_changed", "服务已删除"))?;
                let expected_config = from_record(expected, &mut PendingSecrets::new())?;
                if !current.enabled
                    || current.plugin_id != expected.kind
                    || current.config.base_url != expected.base_url
                    || current.config.options != expected_config.config.options
                    || account.backend_id != expected.id
                {
                    return Err(error("auth_service_changed", "服务连接配置已变化"));
                }
                document.accounts.retain(|a| a.id != account.id);
                document.accounts.push(from_account(account, secrets));
                Ok(())
            })
            .await;
        match result {
            Err(e) if e.code() == "auth_service_changed" => Ok(false),
            result => result.map(|_| true),
        }
    }

    /// 引用只在服务端解析；错误不包含引用内容对应的值。
    async fn secret(
        &self,
        reference: &SecretRef,
        pending: &PendingSecrets,
    ) -> Result<String, AppError> {
        let name = &reference.secret_ref;
        if let Some(value) = pending.get(name) {
            return Ok(value.clone());
        }
        let value = if let Some(name) = name.strip_prefix("env:") {
            if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                return Err(error("secret_ref_invalid", "环境变量引用名称不合法"));
            }
            std::env::var(name).ok()
        } else if name.starts_with("store:") {
            self.state.store.config_secret(name).await?
        } else {
            return Err(error(
                "secret_ref_invalid",
                "仅支持 env: 和 store: 凭证引用",
            ));
        };
        value.ok_or_else(|| error("secret_missing", "服务器无法解析凭证引用"))
    }

    /// 投影为旧适配器需要的记录；敏感字段只存在进程内部与凭证存储中。
    async fn records(
        &self,
        document: &ConfigDocument,
        pending: &PendingSecrets,
    ) -> Result<(Vec<BackendRecord>, Vec<Account>), AppError> {
        let mut records = Vec::new();
        for service in &document.services {
            let mut options = service.config.options.clone();
            let object = options
                .as_object_mut()
                .ok_or_else(|| error("schema_invalid", "options 必须为对象"))?;
            let routing = serde_json::to_value(&service.policy.routing).expect("routing JSON");
            if routing
                != serde_json::to_value(crate::routing::planner::RouteSettings::default())
                    .expect("default routing JSON")
            {
                object.insert("routing".into(), routing);
            }
            let mut headers = serde_json::Map::new();
            for (name, reference) in &service.credentials.headers {
                headers.insert(
                    name.clone(),
                    Value::String(self.secret(reference, pending).await?),
                );
            }
            if !headers.is_empty() {
                object.insert("headers".into(), Value::Object(headers));
            }
            records.push(BackendRecord {
                id: service.id.clone(),
                name: service.name.clone(),
                kind: service.plugin_id.clone(),
                base_url: service.config.base_url.clone(),
                api_key: match &service.credentials.api_key {
                    Some(reference) => self.secret(reference, pending).await?,
                    None => String::new(),
                },
                models: service.config.models.join(","),
                enabled: service.enabled,
                priority: service.policy.priority,
                cost_tier: service.policy.cost_tier.clone(),
                options: options.to_string(),
                capabilities: service.config.capabilities.to_string(),
                daily_token_budget: service.policy.daily_token_budget,
            });
        }
        let mut accounts = Vec::new();
        for spec in &document.accounts {
            let record = records
                .iter()
                .find(|r| r.id == spec.service_id)
                .ok_or_else(|| error("account_invalid", "账号所属服务不存在"))?;
            let control = crate::plugins::builtin::registry()
                .get(&record.kind)?
                .account_control();
            if control.managed {
                return Err(error(
                    "account_managed",
                    "此插件账号从服务凭证派生，不能重复声明",
                ));
            }
            accounts.push(Account {
                id: spec.id.clone(),
                backend_id: spec.service_id.clone(),
                name: spec.name.clone(),
                enabled: spec.enabled,
                credential: (control.normalize)(
                    record,
                    &self.secret(&spec.credential, pending).await?,
                )?,
            });
        }
        // 单令牌虚拟账号是插件配置的派生视图，不作为用户账号重复声明。
        for record in &records {
            accounts.extend((crate::plugins::builtin::registry()
                .get(&record.kind)?
                .account_control()
                .derive)(record));
        }
        Ok((records, accounts))
    }

    /// 汇总字段诊断；schema 报错只暴露路径和规则，避免泄漏非法秘密值。
    async fn validate_document(
        &self,
        document: &ConfigDocument,
        pending: &PendingSecrets,
    ) -> ValidationReport {
        let mut diagnostics = Vec::new();
        check_schema(
            &schema::<ConfigDocument>(),
            &serde_json::to_value(document).expect("config JSON"),
            "",
            &mut diagnostics,
        );
        if document.api_version != "fmp/v2" || document.kind != "Configuration" {
            diagnostic(
                &mut diagnostics,
                "version_unsupported",
                "/api_version",
                "需要 fmp/v2 Configuration",
            );
        }
        let mut ids = HashSet::new();
        for (index, service) in document.services.iter().enumerate() {
            let path = format!("/services/{index}");
            if !valid_id(&service.id) || !ids.insert(service.id.clone()) {
                diagnostic(
                    &mut diagnostics,
                    "service_id_invalid",
                    &format!("{path}/id"),
                    "服务 ID 必须唯一且仅包含字母、数字、点、短横线或下划线",
                );
            }
            let Ok(plugin) = crate::plugins::builtin::registry().get(&service.plugin_id) else {
                diagnostic(
                    &mut diagnostics,
                    "plugin_unknown",
                    &format!("{path}/plugin_id"),
                    "插件未注册",
                );
                continue;
            };
            let descriptor = plugin.descriptor();
            if service.api_version != "fmp/v2"
                || service.kind != "Service"
                || service.plugin_config_version != descriptor.config_schema_version
            {
                diagnostic(
                    &mut diagnostics,
                    "plugin_incompatible",
                    &path,
                    "服务或插件配置版本不兼容",
                );
            }
            check_schema(
                &descriptor.config_schema,
                &serde_json::to_value(&service.config).expect("service JSON"),
                &format!("{path}/config"),
                &mut diagnostics,
            );
            if service.config.options.get("headers").is_some()
                || service.config.options.get("routing").is_some()
            {
                diagnostic(
                    &mut diagnostics,
                    "field_moved",
                    &format!("{path}/config/options"),
                    "请求头须使用 credentials.headers 引用；路由策略放在 policy",
                );
            }
            if service
                .config
                .models
                .iter()
                .any(|id| id.trim().is_empty() || id.contains(','))
            {
                diagnostic(
                    &mut diagnostics,
                    "model_id_invalid",
                    &format!("{path}/config/models"),
                    "模型 ID 不能为空或包含逗号",
                );
            }
            if service.policy.daily_token_budget > i64::MAX as u64 {
                diagnostic(
                    &mut diagnostics,
                    "budget_invalid",
                    &format!("{path}/policy/daily_token_budget"),
                    "预算超出存储范围",
                );
            }
            for (field, reference) in service
                .credentials
                .api_key
                .iter()
                .map(|r| ("api_key".to_string(), r))
                .chain(
                    service
                        .credentials
                        .headers
                        .iter()
                        .map(|(k, v)| (format!("headers/{k}"), v)),
                )
            {
                if let Err(e) = self.secret(reference, pending).await {
                    diagnostic(
                        &mut diagnostics,
                        &e.code(),
                        &format!("{path}/credentials/{field}"),
                        &e.message(),
                    );
                }
            }
        }
        let mut account_ids = HashSet::new();
        for (index, account) in document.accounts.iter().enumerate() {
            let path = format!("/accounts/{index}");
            if !account.credential.secret_ref.starts_with("store:") {
                diagnostic(
                    &mut diagnostics,
                    "credential_not_rotatable",
                    &format!("{path}/credential"),
                    "账号凭证必须使用 store: 引用，以便原子轮换；env: 仅用于服务密钥和请求头",
                );
            }
            if !valid_id(&account.id)
                || !account_ids.insert(&account.id)
                || !ids.contains(&account.service_id)
            {
                diagnostic(
                    &mut diagnostics,
                    "account_invalid",
                    &path,
                    "账号 ID 重复、不合法或所属服务不存在",
                );
            }
            if let Err(e) = self.secret(&account.credential, pending).await {
                diagnostic(
                    &mut diagnostics,
                    &e.code(),
                    &format!("{path}/credential"),
                    &e.message(),
                );
            }
        }
        let mut names = HashSet::new();
        let mut primary = HashSet::new();
        for (index, alias) in document.model_aliases.iter().enumerate() {
            if crate::model_names::validate_alias(&alias.alias).is_err()
                || !names.insert(&alias.alias)
                || (alias.is_primary && !primary.insert(alias.address()))
            {
                diagnostic(
                    &mut diagnostics,
                    "alias_invalid",
                    &format!("/model_aliases/{index}"),
                    "调用名称不合法或主名称重复",
                );
            }
        }
        if document.gateway.validate().is_err() {
            diagnostic(
                &mut diagnostics,
                "gateway_invalid",
                "/gateway",
                "网关容量或时限超出允许范围",
            );
        }
        if document.checkin.validate().is_err() {
            diagnostic(
                &mut diagnostics,
                "schedule_invalid",
                "/checkin",
                "签到时间或随机窗口不合法",
            );
        }
        if diagnostics.is_empty() {
            match self.records(document, pending).await {
                Ok((records, _)) => {
                    for (index, record) in records.iter().enumerate() {
                        if crate::plugins::builtin::registry()
                            .validate(record)
                            .is_err()
                        {
                            diagnostic(
                                &mut diagnostics,
                                "plugin_config_invalid",
                                &format!("/services/{index}/config"),
                                "插件语义校验失败，请检查地址与专属配置",
                            );
                        }
                        if document.model_aliases.iter().any(|alias| {
                            crate::model_names::check_native_collision(alias, &records).is_err()
                        }) {
                            diagnostic(
                                &mut diagnostics,
                                "alias_conflict",
                                "/model_aliases",
                                "调用名称遮蔽了其他模型原始名称",
                            );
                        }
                    }
                }
                Err(e) => diagnostic(&mut diagnostics, &e.code(), "", &e.message()),
            }
        }
        for diagnostic in &mut diagnostics {
            if let Some(index) = diagnostic
                .path
                .strip_prefix("/services/")
                .and_then(|path| path.split('/').next())
                .and_then(|index| index.parse::<usize>().ok())
                && let Some(service) = document.services.get(index)
            {
                let id = if valid_id(&service.id) {
                    &service.id
                } else {
                    "<invalid>"
                };
                let plugin = crate::plugins::builtin::registry()
                    .get(&service.plugin_id)
                    .map(|plugin| plugin.descriptor().id)
                    .unwrap_or_else(|_| "<unknown>".into());
                diagnostic.message = format!("服务 {id}（插件 {plugin}）：{}", diagnostic.message);
            }
        }
        ValidationReport {
            valid: diagnostics.is_empty(),
            diagnostics,
        }
    }

    /// 在内存中分配缺失名称；提交前没有配置写入。
    async fn complete_aliases(&self, document: &mut ConfigDocument) -> Result<(), AppError> {
        let catalogs = self.state.store.list_model_catalogs().await?;
        let records = document
            .services
            .iter()
            .map(|s| BackendRecord {
                id: s.id.clone(),
                name: s.name.clone(),
                kind: s.plugin_id.clone(),
                models: s.config.models.join(","),
                ..Default::default()
            })
            .collect::<Vec<_>>();
        for record in &records {
            let mut models = record
                .models
                .split(',')
                .filter(|v| !v.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>();
            if let Some(catalog) = catalogs.get(&record.id) {
                models.extend(catalog.iter().map(|m| m.id.clone()));
            }
            models.sort();
            models.dedup();
            for model_id in models {
                if document
                    .model_aliases
                    .iter()
                    .any(|a| a.backend_id == record.id && a.model_id == model_id && a.is_primary)
                {
                    continue;
                }
                let display = catalogs
                    .get(&record.id)
                    .and_then(|c| c.iter().find(|m| m.id == model_id))
                    .and_then(|m| m.display_name.as_deref());
                let base = crate::model_names::default_alias(record, &model_id, display);
                let mut suffix = 1;
                loop {
                    let alias = ModelAlias {
                        alias: if suffix == 1 {
                            base.clone()
                        } else {
                            format!("{base}-{suffix}")
                        },
                        backend_id: record.id.clone(),
                        model_id: model_id.clone(),
                        is_primary: true,
                    };
                    if !document
                        .model_aliases
                        .iter()
                        .any(|a| a.alias == alias.alias)
                        && crate::model_names::check_native_collision(&alias, &records).is_ok()
                    {
                        document.model_aliases.push(alias);
                        break;
                    }
                    suffix += 1;
                }
            }
        }
        document.normalize();
        Ok(())
    }

    /// 先预备完整实例；不能在数据库已经提交后才发现配置无法装配。
    async fn prepare(
        &self,
        document: &ConfigDocument,
        secrets: &PendingSecrets,
    ) -> Result<Prepared, AppError> {
        document.gateway.validate()?;
        let (records, accounts) = self.records(document, secrets).await?;
        let mut catalogs = self.state.store.list_model_catalogs().await?;
        let old = self.state.store.list_backends().await?;
        catalogs.retain(|id, _| {
            records.iter().any(|new| {
                new.id == *id
                    && old.iter().any(|old| {
                        old.id == *id
                            && crate::plugins::service::catalog_identity(old)
                                == crate::plugins::service::catalog_identity(new)
                    })
            })
        });
        let registry = self.state.registry.prepare_reusing(
            &records,
            &accounts,
            &self.state.store,
            &document.model_aliases,
            std::time::Duration::from_millis(document.gateway.connect_timeout_ms),
            self.state.quota_refresh.clone(),
        )?;
        Ok(Prepared {
            registry,
            records,
            accounts,
            catalogs,
        })
    }

    /// 单次事务后无失败点发布；凭证锁阻止预备和提交之间的自动续期覆盖。
    async fn commit(
        &self,
        document: &ConfigDocument,
        secrets: &PendingSecrets,
        plan: &ConfigPlan,
        key: &str,
        request_digest: &str,
        actor: &str,
    ) -> Result<ApplyResult, AppError> {
        require_valid(self.validate_document(document, secrets).await)?;
        let previous = self.state.store.list_backends().await?;
        let prepared = self.prepare(document, secrets).await?;
        let revision = plan.expected_revision + 1;
        let result = ApplyResult {
            desired_revision: revision,
            active_revision: revision,
            changes: plan.changes.clone(),
        };
        self.state
            .store
            .commit_configuration(
                plan.expected_revision,
                document,
                &prepared.records,
                &prepared.accounts,
                secrets,
                key,
                request_digest,
                actor,
                &result,
            )
            .await?;
        // 成功提交后撤销旧连接会话；仅调整显示名或优先级不撤销登录。
        for old in previous {
            if prepared
                .records
                .iter()
                .find(|r| r.id == old.id)
                .is_none_or(|new| {
                    !new.enabled
                        || crate::plugins::service::catalog_identity(new)
                            != crate::plugins::service::catalog_identity(&old)
                })
            {
                self.state.auth.cancel_service(&old.id).await;
            }
        }
        self.publish(prepared, document, revision);
        Ok(result)
    }

    /// 无网络、无数据库、无可失败校验的快照发布。
    fn publish(&self, prepared: Prepared, document: &ConfigDocument, revision: u64) {
        let _guard = self
            .state
            .generation_lock
            .write()
            .unwrap_or_else(|p| p.into_inner());
        self.state.registry.publish(prepared.registry);
        self.state.router.metadata.configure_registry(
            &prepared.records,
            &prepared.catalogs,
            &self.state.registry,
        );
        self.state
            .router
            .gateway
            .publish_settings(document.gateway.clone());
        let mut tasks = crate::tasks::TaskRegistry::new();
        for runtime in self.state.registry.runtimes() {
            for task in runtime.tasks {
                tasks.register(task);
            }
        }
        *self.state.tasks.write().unwrap_or_else(|p| p.into_inner()) = tasks;
        self.state
            .active_revision
            .store(revision, Ordering::Release);
        self.state
            .configuration_initialized
            .store(true, Ordering::Release);
        self.state.checkin_scheduler.changed.notify_one();
        self.state.quota_refresh.changed.notify_one();
    }
}

/// 将旧对象中的秘密移入引用，供基线迁移及 v1 兼容保存复用。
pub fn from_record(
    record: &BackendRecord,
    secrets: &mut PendingSecrets,
) -> Result<ServiceSpec, AppError> {
    let mut options: Value = if record.options.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&record.options)
            .map_err(|_| error("schema_invalid", "options JSON 无法解析"))?
    };
    let object = options
        .as_object_mut()
        .ok_or_else(|| error("schema_invalid", "options 必须为对象"))?;
    let routing = object
        .remove("routing")
        .map(serde_json::from_value)
        .transpose()
        .map_err(|_| error("schema_invalid", "routing 无法解析"))?
        .unwrap_or_default();
    let mut credentials = ServiceCredentials::default();
    if !record.api_key.is_empty() {
        credentials.api_key = Some(stage_secret(&record.api_key, secrets));
    }
    if let Some(headers) = object.remove("headers") {
        let headers: BTreeMap<String, String> = serde_json::from_value(headers)
            .map_err(|_| error("schema_invalid", "headers 必须为字符串映射"))?;
        for (name, value) in headers {
            credentials
                .headers
                .insert(name, stage_secret(&value, secrets));
        }
    }
    Ok(ServiceSpec {
        api_version: "fmp/v2".into(),
        kind: "Service".into(),
        id: record.id.clone(),
        name: record.name.clone(),
        plugin_id: record.kind.clone(),
        plugin_config_version: 1,
        enabled: record.enabled,
        config: ServiceConfig {
            base_url: record.base_url.clone(),
            models: record
                .models
                .split(',')
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .map(str::to_string)
                .collect(),
            options,
            capabilities: if record.capabilities.is_empty() {
                json!({})
            } else {
                serde_json::from_str(&record.capabilities)
                    .map_err(|_| error("schema_invalid", "capabilities 无法解析"))?
            },
        },
        credentials,
        policy: ServicePolicy {
            priority: record.priority,
            cost_tier: record.cost_tier.clone(),
            daily_token_budget: record.daily_token_budget,
            routing,
        },
    })
}

/// 人工登录或修改凭证分配新引用；自动续期沿用当前引用。
pub fn from_account(account: &Account, secrets: &mut PendingSecrets) -> AccountSpec {
    AccountSpec {
        id: account.id.clone(),
        service_id: account.backend_id.clone(),
        name: account.name.clone(),
        enabled: account.enabled,
        credential: stage_secret(&account.credential, secrets),
    }
}

/// 待写入秘密只在最终配置事务落库，失败的 validate 不写入秘密。
pub fn stage_secret(value: &str, secrets: &mut PendingSecrets) -> SecretRef {
    let reference = format!("store:{}", uuid::Uuid::new_v4());
    secrets.insert(reference.clone(), value.into());
    SecretRef {
        secret_ref: reference,
    }
}

/// 收集规则和逐字段路径；每次最多 64 条，不格式化包含原始值的错误。
fn check_schema(schema: &Value, value: &Value, prefix: &str, diagnostics: &mut Vec<Diagnostic>) {
    match jsonschema::validator_for(schema) {
        Ok(validator) => {
            let limit = diagnostics.len() + 64;
            for e in validator.iter_errors(value).take(64) {
                let path = format!("{prefix}{}", e.instance_path());
                let message = format!("字段不满足 schema 规则 {}", e.schema_path());
                if let jsonschema::error::ValidationErrorKind::AdditionalProperties { unexpected } =
                    e.kind()
                {
                    for key in unexpected.iter().take(limit - diagnostics.len()) {
                        let key = key.replace('~', "~0").replace('/', "~1");
                        diagnostic(
                            diagnostics,
                            "schema_invalid",
                            &format!("{path}/{key}"),
                            &format!("未知字段；{message}"),
                        );
                    }
                } else {
                    diagnostic(diagnostics, "schema_invalid", &path, &message);
                }
                if diagnostics.len() == limit {
                    break;
                }
            }
        }
        Err(_) => diagnostic(
            diagnostics,
            "plugin_schema_invalid",
            prefix,
            "插件 schema 无法编译",
        ),
    }
}

/// 构造固定结构诊断。
fn diagnostic(out: &mut Vec<Diagnostic>, code: &str, path: &str, message: &str) {
    out.push(Diagnostic {
        code: code.into(),
        path: path.into(),
        message: message.into(),
        hint: "根据插件目录和字段 schema 修正后重新 validate".into(),
    });
}

/// apply 使用稳定错误码，详细诊断可通过 validate 查询。
fn require_valid(report: ValidationReport) -> Result<(), AppError> {
    if let Some(diagnostic) = report.diagnostics.first() {
        Err(error(
            &diagnostic.code,
            format!("{}: {}", diagnostic.path, diagnostic.message),
        ))
    } else {
        Ok(())
    }
}

/// 同一版本计划不能覆盖之后的人工或 AI 修改。
fn require_revision(actual: u64, expected: u64) -> Result<(), AppError> {
    if actual == expected {
        Ok(())
    } else {
        Err(error(
            "revision_conflict",
            "配置版本已变化，请重新读取并规划",
        ))
    }
}

/// 限制持久幂等键，防止无界输入和日志控制字符。
fn valid_key(key: &str) -> Result<(), AppError> {
    if key.is_empty() || key.len() > 128 || key.chars().any(char::is_control) {
        Err(error("idempotency_key_invalid", "需要 1–128 字符幂等键"))
    } else {
        Ok(())
    }
}

/// 稳定实例 ID 不包含协议路径分隔符。
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// 摘要基于有序 JSON 对象，不能由调用方自报。
fn digest(value: &impl Serialize) -> String {
    let value = serde_json::to_value(value).expect("configuration JSON");
    format!("{:x}", Sha256::digest(value.to_string().as_bytes()))
}

/// 插件 schema/版本变化后旧计划不能继续 apply。
fn contract_digest() -> String {
    digest(&crate::plugins::builtin::registry().descriptors())
}

/// 生成 15 分钟计划；只含变更路径，没有秘密旧值/新值。
fn make_plan(revision: u64, before: &ConfigDocument, after: &ConfigDocument) -> ConfigPlan {
    let mut changes = Vec::new();
    for service in &after.services {
        let old = before.services.iter().find(|s| s.id == service.id);
        if old.is_none_or(|old| digest(old) != digest(service)) {
            changes.push(ConfigDiff {
                path: format!("/services/{}", service.id),
                action: if old.is_some() { "update" } else { "create" }.into(),
                rebuild_connection: old.is_none_or(|old| {
                    old.plugin_id != service.plugin_id
                        || digest(&old.config) != digest(&service.config)
                        || digest(&old.credentials) != digest(&service.credentials)
                }),
            });
        }
    }
    for service in &before.services {
        if !after.services.iter().any(|s| s.id == service.id) {
            changes.push(ConfigDiff {
                path: format!("/services/{}", service.id),
                action: "remove".into(),
                rebuild_connection: true,
            });
        }
    }
    for (path, changed) in [
        (
            "/accounts",
            digest(&before.accounts) != digest(&after.accounts),
        ),
        (
            "/model_aliases",
            digest(&before.model_aliases) != digest(&after.model_aliases),
        ),
        (
            "/gateway",
            digest(&before.gateway) != digest(&after.gateway),
        ),
        (
            "/checkin",
            digest(&before.checkin) != digest(&after.checkin),
        ),
    ] {
        if changed {
            changes.push(ConfigDiff {
                path: path.into(),
                action: "update".into(),
                rebuild_connection: path == "/accounts",
            });
        }
    }
    ConfigPlan {
        id: uuid::Uuid::new_v4().to_string(),
        expected_revision: revision,
        digest: digest(after),
        contract_digest: contract_digest(),
        expires_at: now() + 900,
        changes,
        requires_restart: false,
    }
}

/// Unix 秒用于持久计划有效期。
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
