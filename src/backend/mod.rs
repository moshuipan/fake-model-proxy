//! 后端抽象与注册表。
//!
//! 后端只负责两件事：把归一请求变成上游 wire 格式，把上游响应解码成事件流。
//! 它不感知入站协议，也不做跨后端路由。

pub mod autoglm;
pub(crate) mod catalog;
pub mod minimax_code;
pub mod modelscope;
pub mod monkeycode;
pub mod openai_compat;
pub mod qoder;
pub mod raccoon;
pub mod trae_personal;
pub mod workbuddy;

use crate::accounts::Account;
use crate::core::error::AppError;
use crate::core::stream::EventStream;
use crate::core::{Capabilities, ChatRequest, ModelInfo};
use crate::store::BackendRecord;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio_util::sync::CancellationToken;

/// 费用层级。排序与「只用免费」筛选都依赖它。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum CostTier {
    /// 已确认的免费额度。
    Free,
    /// 套餐内额度，超出可能计费。
    Subscription,
    /// 明确按量计费。
    Paid,
    /// 未知。默认不进入「只用免费」候选。
    Unknown,
}

impl CostTier {
    /// 从配置字符串解析；未知值一律视为 Unknown。
    pub fn parse(value: &str) -> CostTier {
        match value.trim().to_lowercase().as_str() {
            "free" => CostTier::Free,
            "subscription" => CostTier::Subscription,
            "paid" => CostTier::Paid,
            _ => CostTier::Unknown,
        }
    }

    /// 返回配置字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            CostTier::Free => "free",
            CostTier::Subscription => "subscription",
            CostTier::Paid => "paid",
            CostTier::Unknown => "unknown",
        }
    }
}

/// 后端契约。
#[async_trait]
pub trait Backend: Send + Sync {
    /// 稳定标识，用于模型寻址与日志。
    fn id(&self) -> &str;

    /// 人类可读名称。
    fn name(&self) -> &str;

    /// 后端类型，例如 `openai_compat`。
    fn kind(&self) -> &str;

    /// 静态能力声明。未知能力一律为 false。
    fn capabilities(&self) -> Capabilities;

    /// 费用层级。
    fn cost_tier(&self) -> CostTier;

    /// auto 排序优先级，数值越小越优先。
    fn priority(&self) -> u8;

    /// 每日 token 预算，0 表示不限制。
    fn daily_token_budget(&self) -> u64 {
        0
    }

    /// 所有模型和账号共享的生成 QPS，0 表示不限制。
    fn qps(&self) -> u32 {
        0
    }

    /// 配置里声明的模型。动态目录后端可返回空。
    fn static_models(&self) -> Vec<String>;

    /// 列出该后端提供的模型。
    async fn list_models(&self) -> Result<Vec<ModelInfo>, AppError>;

    /// 检查适配器能否无损表达请求；只返回本地原因，不含正文、凭证或远端错误。
    fn validate_request(&self, _request: &ChatRequest) -> Result<(), String> {
        Ok(())
    }

    /// 上游是否透传并执行请求中的输出 token 上限。
    fn output_enforced(&self) -> bool {
        true
    }

    /// 将旧目录字段归一为统一约束；厂商限制由适配器覆盖，核心不判断名称。
    fn model_limits(&self, model: &ModelInfo) -> crate::core::TokenLimits {
        let mut limits = model.token_limits.clone();
        if limits == crate::core::TokenLimits::default() {
            limits.context_window = model.capabilities.context_window;
        }
        limits
    }

    /// 账号后端返回可用性快照；网关不需要账号池，返回 None。
    fn routing_accounts(&self) -> Option<Vec<crate::accounts::AccountState>> {
        None
    }

    /// 读取选定账号额度；缺少已确认的计费规则时保持未知。
    async fn routing_quota(
        &self,
        account_id: &str,
    ) -> Result<crate::accounts::remote::QuotaInfo, AppError> {
        Ok(crate::accounts::remote::QuotaInfo::unknown(
            self.id(),
            account_id,
            "quota unavailable",
        ))
    }

    /// 固定路由选择的账号，账号池不得在调用阶段再次轮询。
    async fn chat_on_account(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
        account_id: Option<&str>,
    ) -> Result<EventStream, AppError> {
        match account_id {
            Some(id) => {
                crate::accounts::ROUTED_ACCOUNT
                    .scope(id.to_string(), self.chat(request, cancel))
                    .await
            }
            None => self.chat(request, cancel).await,
        }
    }

    /// 发起一次生成，返回归一事件流。取消令牌触发时必须尽快停止上游读取。
    async fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, AppError>;
}

/// 后端注册表。配置变更后整体替换，读路径持读锁。
#[derive(Default)]
pub struct Registry {
    snapshot: RwLock<RegistrySnapshot>,
}

/// 后端与调用名称作为同一快照替换，避免热加载期间交叉路由。
#[derive(Default, Clone)]
pub struct RegistrySnapshot {
    backends: Vec<Arc<dyn Backend>>,
    aliases: HashMap<String, crate::model_names::ModelAlias>,
    runtimes: HashMap<String, crate::plugins::ProviderRuntime>,
    connections: HashMap<String, (serde_json::Value, crate::plugins::ProviderRuntime)>,
}

impl Registry {
    /// 捕获请求使用的完整注册表，后续配置发布不改变这个 generation。
    pub fn fork(&self) -> Self {
        Self {
            snapshot: RwLock::new(
                self.snapshot
                    .read()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone(),
            ),
        }
    }

    /// 创建空注册表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 用配置记录重建全部后端。禁用记录不会被装入。
    pub fn reload(
        &self,
        records: &[BackendRecord],
        accounts: &[Account],
        store: &crate::store::Store,
    ) -> Result<(), AppError> {
        self.reload_with_aliases(records, accounts, store, &[])
    }

    /// 一次性装配后端与持久名称映射。
    pub fn reload_with_aliases(
        &self,
        records: &[BackendRecord],
        accounts: &[Account],
        store: &crate::store::Store,
        aliases: &[crate::model_names::ModelAlias],
    ) -> Result<(), AppError> {
        self.reload_with_timeout(
            records,
            accounts,
            store,
            aliases,
            std::time::Duration::from_secs(10),
        )
    }

    /// 原子替换配置与客户端实例，长期网关健康和租约由 Router 持有。
    pub fn reload_with_timeout(
        &self,
        records: &[BackendRecord],
        accounts: &[Account],
        store: &crate::store::Store,
        aliases: &[crate::model_names::ModelAlias],
        timeout: std::time::Duration,
    ) -> Result<(), AppError> {
        let prepared = Self::prepare(
            records,
            accounts,
            store,
            aliases,
            timeout,
            Default::default(),
        )?;
        self.publish(prepared);
        Ok(())
    }

    /// 在数据库提交前准备完整能力快照；不进行网络动作或改变当前 generation。
    pub fn prepare(
        records: &[BackendRecord],
        accounts: &[Account],
        store: &crate::store::Store,
        aliases: &[crate::model_names::ModelAlias],
        timeout: std::time::Duration,
        quota_refresh: Arc<crate::accounts::quota_refresh::Control>,
    ) -> Result<RegistrySnapshot, AppError> {
        Self::prepare_snapshot(
            records,
            accounts,
            store,
            aliases,
            timeout,
            quota_refresh,
            None,
        )
    }

    /// 连接身份不变时复用底层实例和账号池，仅替换宿主策略包装。
    pub fn prepare_reusing(
        &self,
        records: &[BackendRecord],
        accounts: &[Account],
        store: &crate::store::Store,
        aliases: &[crate::model_names::ModelAlias],
        timeout: std::time::Duration,
        quota_refresh: Arc<crate::accounts::quota_refresh::Control>,
    ) -> Result<RegistrySnapshot, AppError> {
        let previous = self
            .snapshot
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        Self::prepare_snapshot(
            records,
            accounts,
            store,
            aliases,
            timeout,
            quota_refresh,
            Some(&previous),
        )
    }

    /// 基于旧快照预备本地能力，过程中不改变当前 generation。
    fn prepare_snapshot(
        records: &[BackendRecord],
        accounts: &[Account],
        store: &crate::store::Store,
        aliases: &[crate::model_names::ModelAlias],
        timeout: std::time::Duration,
        quota_refresh: Arc<crate::accounts::quota_refresh::Control>,
        previous: Option<&RegistrySnapshot>,
    ) -> Result<RegistrySnapshot, AppError> {
        let mut backends: Vec<Arc<dyn Backend>> = Vec::new();
        let mut runtimes = HashMap::new();
        let mut connections = HashMap::new();
        for record in records.iter().filter(|record| record.enabled) {
            let mut scoped_accounts = accounts.iter().filter(|a| a.backend_id == record.id)
                .map(|a| serde_json::json!({"id":a.id,"credential":a.credential,"enabled":a.enabled})).collect::<Vec<_>>();
            scoped_accounts.sort_by_key(|a| a["id"].as_str().unwrap_or_default().to_string());
            let identity = serde_json::json!({
                "connection": crate::plugins::service::connection_identity(record),
                "accounts": scoped_accounts, "timeout_ms":timeout.as_millis(),
            });
            let mut runtime = if let Some((_, runtime)) = previous
                .and_then(|p| p.connections.get(&record.id))
                .filter(|(old, _)| *old == identity)
            {
                runtime.clone()
            } else {
                let host = crate::plugins::ScopedHostServices::new(
                    record,
                    accounts,
                    store.clone(),
                    timeout,
                    quota_refresh.clone(),
                );
                crate::plugins::builtin::registry().prepare(record, host)?
            };
            connections.insert(record.id.clone(), (identity, runtime.clone()));
            runtime.inference = Arc::new(crate::plugins::service::ConfiguredBackend::new(
                record,
                runtime.inference,
            ));
            backends.push(runtime.inference.clone());
            runtimes.insert(record.id.clone(), runtime);
        }
        Ok(RegistrySnapshot {
            backends,
            runtimes,
            connections,
            aliases: aliases
                .iter()
                .map(|alias| (alias.alias.clone(), alias.clone()))
                .collect(),
        })
    }

    /// 无 I/O 发布已经准备好的 generation，旧 Arc 保持在途流完整。
    pub fn publish(&self, snapshot: RegistrySnapshot) {
        *self
            .snapshot
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = snapshot;
    }

    /// 读取同一 generation 的推理与可选管理能力。
    pub fn runtime(&self, id: &str) -> Option<crate::plugins::ProviderRuntime> {
        self.snapshot.read().ok()?.runtimes.get(id).cloned()
    }

    /// 获取全部已启用实例的能力快照。
    pub fn runtimes(&self) -> Vec<crate::plugins::ProviderRuntime> {
        self.snapshot
            .read()
            .map(|guard| guard.runtimes.values().cloned().collect())
            .unwrap_or_default()
    }

    /// 列出全部已启用后端。
    pub fn list(&self) -> Vec<Arc<dyn Backend>> {
        self.snapshot
            .read()
            .map(|guard| guard.backends.clone())
            .unwrap_or_default()
    }

    /// 按 id 查找后端。
    pub fn get(&self, id: &str) -> Option<Arc<dyn Backend>> {
        self.snapshot
            .read()
            .ok()?
            .backends
            .iter()
            .find(|backend| backend.id() == id)
            .cloned()
    }

    /// 解析模型地址。
    ///
    /// 优先解析持久调用名称，兼容内部地址及全局唯一的原始模型名。
    /// 歧义与不存在都返回 400，不做猜测。
    pub fn resolve(&self, model: &str) -> Result<(Arc<dyn Backend>, String), AppError> {
        let guard = self.snapshot.read().map_err(|_| AppError::Internal {
            message: "backend registry lock poisoned".into(),
        })?;
        let backends = &guard.backends;
        let address = guard.aliases.get(model).map(|alias| alias.address());
        let model = address.as_deref().unwrap_or(model);
        if let Some((backend_id, model_id)) = model.split_once('/') {
            let backend = backends
                .iter()
                .find(|backend| backend.id() == backend_id)
                .cloned()
                .ok_or_else(|| {
                    AppError::bad_request(format!(
                        "unknown backend `{backend_id}` in model address"
                    ))
                })?;
            // 声明了模型列表即视为白名单；未声明时交给上游判断。
            let declared = backend.static_models();
            if !declared.is_empty() && !declared.iter().any(|candidate| candidate == model_id) {
                return Err(AppError::bad_request(format!(
                    "unknown model `{model_id}` on backend `{backend_id}`"
                )));
            }
            return Ok((backend, model_id.to_string()));
        }
        let mut matches: Vec<(Arc<dyn Backend>, String)> = Vec::new();
        for backend in backends {
            if backend
                .static_models()
                .iter()
                .any(|candidate| candidate == model)
            {
                matches.push((backend.clone(), model.to_string()));
            }
        }
        match matches.len() {
            1 => Ok(matches.remove(0)),
            0 => Err(AppError::bad_request(format!(
                "unknown model `{model}`; choose a calling name from /v1/models"
            ))),
            _ => Err(AppError::bad_request(format!(
                "model `{model}` exists on multiple backends; choose a calling name from /v1/models"
            ))),
        }
    }

    /// 动态发现名称后只替换映射，调用者持有应用重载锁。
    pub fn update_aliases(
        &self,
        aliases: &[crate::model_names::ModelAlias],
    ) -> Result<(), AppError> {
        let mut guard = self.snapshot.write().map_err(|_| AppError::Internal {
            message: "backend registry lock poisoned".into(),
        })?;
        guard.aliases = aliases
            .iter()
            .map(|alias| (alias.alias.clone(), alias.clone()))
            .collect();
        Ok(())
    }

    /// 把实际内部地址转换为主调用名称；旧构造入口仍允许回退到原地址。
    pub fn public_model_id(&self, address: &str) -> String {
        self.snapshot
            .read()
            .ok()
            .and_then(|guard| {
                guard
                    .aliases
                    .values()
                    .find(|alias| alias.is_primary && alias.address() == address)
                    .map(|alias| alias.alias.clone())
            })
            .unwrap_or_else(|| address.to_string())
    }

    /// 汇总所有后端的模型，id 以 `backend_id/model` 暴露。
    pub async fn list_models(&self) -> Vec<ModelInfo> {
        let backends = self.list();
        let mut result = Vec::new();
        for backend in backends {
            match backend.list_models().await {
                Ok(models) => {
                    for model in models {
                        result.push(ModelInfo {
                            id: format!("{}/{}", backend.id(), model.id),
                            display_name: model.display_name.or_else(|| Some(model.id.clone())),
                            price_factor: model.price_factor,
                            original_price_factor: model.original_price_factor,
                            price_valid_until: model.price_valid_until,
                            capabilities: model.capabilities,
                            token_limits: model.token_limits,
                        });
                    }
                }
                Err(error) => {
                    tracing::warn!(backend = backend.id(), %error, "list models failed");
                }
            }
        }
        result
    }
}

/// 便于测试的注册表构造：直接注入后端。
impl Registry {
    /// 用现成后端构造注册表。
    pub fn from_backends(backends: Vec<Arc<dyn Backend>>) -> Self {
        let registry = Registry::new();
        if let Ok(mut guard) = registry.snapshot.write() {
            guard.backends = backends;
        }
        registry
    }
}

/// 后端类型到展示名。
pub fn kind_label(kind: &str) -> String {
    crate::plugins::builtin::registry()
        .get(kind)
        .map(|plugin| plugin.descriptor().display_name)
        .unwrap_or_else(|_| "未知类型".into())
}

/// 按 kind 统计，供管理页展示。
pub fn kind_counts(records: &[BackendRecord]) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for record in records {
        *counts.entry(record.kind.clone()).or_insert(0) += 1;
    }
    counts
}
