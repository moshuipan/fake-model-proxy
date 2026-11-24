//! 应用共享状态。

use crate::backend::Registry;
use crate::config::Config;
use crate::records::RecordLog;
use crate::routing::Router;
use crate::store::Store;
use crate::tasks::TaskRegistry;
use crate::usage::UsageLedger;
use std::sync::{Arc, RwLock};

/// 全局状态，按 Axum `State` 注入。
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub store: Store,
    pub registry: Arc<Registry>,
    /// 最近请求记录。
    pub records: Arc<RecordLog>,
    /// 客户端历史关联只保存指纹，容量跟随请求记录，热加载不清空。
    pub client_sessions: Arc<crate::client_plugins::HistoryIndex>,
    /// 可插拔后台任务。配置变更后整体重建。
    pub tasks: Arc<RwLock<TaskRegistry>>,
    /// auto 路由上下文（健康与亲和跨请求保留）。
    pub router: Arc<Router>,
    /// 当日用量账本。
    pub usage: Arc<UsageLedger>,
    /// 插件专有授权会话及统一撤销生命周期。
    pub auth: Arc<crate::plugins::auth::AuthRegistry>,
    pub operations: Arc<crate::application::operation_service::OperationControl>,
    /// 自动签到的唤醒、计划锁和运行状态。
    pub checkin_scheduler: Arc<crate::tasks::scheduler::SchedulerControl>,
    /// 一键签到的最近批次与运行进度。
    pub checkin_batch: Arc<crate::tasks::batch::BatchControl>,
    /// 服务端额度更新器，调度与手动请求共享一把锁。
    pub quota_refresh: Arc<crate::accounts::quota_refresh::Control>,
    /// 名称分配和后端快照替换共享锁，不覆盖生成流。
    pub(crate) reload_lock: Arc<tokio::sync::Mutex<()>>,
    /// 仅在完整运行快照发布之后递增。
    pub(crate) active_revision: Arc<std::sync::atomic::AtomicU64>,
    pub(crate) configuration_initialized: Arc<std::sync::atomic::AtomicBool>,
    /// 只覆盖快照交换和同步捕获，不覆盖数据库、网络或生成流。
    pub(crate) generation_lock: Arc<RwLock<()>>,
}

impl AppState {
    /// 管理入口共用配置协调器，不直接写兼容投影表。
    pub fn config_service(&self) -> crate::application::config_service::ConfigService<'_> {
        crate::application::config_service::ConfigService::new(self)
    }

    /// 构造状态。
    pub fn new(
        config: Arc<Config>,
        store: Store,
        registry: Arc<Registry>,
        records: Arc<RecordLog>,
        tasks: Arc<RwLock<TaskRegistry>>,
        router: Arc<Router>,
        usage: Arc<UsageLedger>,
    ) -> Self {
        let client_sessions = Arc::new(crate::client_plugins::HistoryIndex::new(
            records.persistence().capacity,
        ));
        router.metadata.attach_store(store.clone());
        AppState {
            config,
            store,
            registry,
            records,
            client_sessions,
            tasks,
            router,
            usage,
            auth: Default::default(),
            operations: Default::default(),
            checkin_scheduler: Arc::new(crate::tasks::scheduler::SchedulerControl::default()),
            checkin_batch: Arc::new(crate::tasks::batch::BatchControl::default()),
            quota_refresh: Default::default(),
            reload_lock: Arc::new(tokio::sync::Mutex::new(())),
            active_revision: Default::default(),
            configuration_initialized: Default::default(),
            generation_lock: Default::default(),
        }
    }

    /// 从数据库重新加载后端与任务执行器，保证读写一致。
    pub async fn reload_backends(&self) -> Result<(), crate::core::AppError> {
        self.config_service().activate().await
    }

    /// 动态目录先补齐持久名称，再返回客户端可调用的目录。
    pub async fn public_models(
        &self,
    ) -> Result<Vec<crate::core::ModelInfo>, crate::core::AppError> {
        let mut models = self.registry.list_models().await;
        models.retain(|model| self.registry.resolve(&model.id).is_ok());
        let records = self.store.list_backends().await?;
        let mut catalogs = self.store.list_model_catalogs().await?;
        for model in &models {
            if let Some((backend, native)) = model.id.split_once('/') {
                let mut entry = model.clone();
                entry.id = native.into();
                catalogs.entry(backend.into()).or_default().push(entry);
            }
        }
        for record in records {
            if let Some(catalog) = catalogs.get(&record.id) {
                self.store.cache_routing_models(&record, catalog).await?;
            }
        }
        self.config_service()
            .mutate("catalog.aliases", |_, _| Ok(()))
            .await?;
        for model in &mut models {
            model.id = self.registry.public_model_id(&model.id);
        }
        Ok(models)
    }

    /// 取任务执行器快照。
    pub fn task_providers(&self) -> Vec<Arc<dyn crate::tasks::TaskProvider>> {
        self.tasks
            .read()
            .map(|guard| guard.providers())
            .unwrap_or_default()
    }
}
