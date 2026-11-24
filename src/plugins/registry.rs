//! 编译期插件目录，不用按厂商名称分支装配能力。

use super::{PluginDescriptor, ProviderPlugin, ProviderRuntime, ScopedHostServices};
use crate::{core::AppError, store::BackendRecord};
use std::collections::BTreeMap;
use std::sync::Arc;

/// 插件身份唯一、版本固定的宿主目录。
pub struct PluginRegistry {
    plugins: BTreeMap<String, Arc<dyn ProviderPlugin>>,
}

impl PluginRegistry {
    /// 校验重复注册和契约版本，错误在启动时暴露。
    pub fn new(plugins: Vec<Arc<dyn ProviderPlugin>>) -> Result<Self, AppError> {
        let mut registry = Self {
            plugins: BTreeMap::new(),
        };
        for plugin in plugins {
            let descriptor = plugin.descriptor();
            if descriptor.contract_version != super::contract::CONTRACT_VERSION
                || registry
                    .plugins
                    .insert(descriptor.id.clone(), plugin)
                    .is_some()
            {
                return Err(AppError::bad_request(format!(
                    "duplicate or incompatible plugin `{}`",
                    descriptor.id
                )));
            }
        }
        Ok(registry)
    }

    /// 获取实际注册的工厂，未知插件不可降级为兼容协议。
    pub fn get(&self, id: &str) -> Result<Arc<dyn ProviderPlugin>, AppError> {
        self.plugins
            .get(id)
            .cloned()
            .ok_or_else(|| AppError::bad_request(format!("unknown plugin `{id}`")))
    }

    /// 为 UI 和管理接口生成能力目录。
    pub fn descriptors(&self) -> Vec<PluginDescriptor> {
        self.plugins
            .values()
            .map(|plugin| plugin.descriptor())
            .collect()
    }

    /// 公共路由只合并目录，不枚举厂商的路径。
    pub fn control_routes(&self) -> axum::Router<crate::state::AppState> {
        self.plugins
            .values()
            .fold(axum::Router::new(), |router, plugin| {
                router.merge(plugin.control_routes())
            })
    }

    /// 无网络校验所有已知服务，包括当前禁用的声明。
    pub fn validate(&self, record: &BackendRecord) -> Result<(), AppError> {
        super::contract::validate_record(record)?;
        self.get(&record.kind)?.validate(record)
    }

    /// 使用同一个工厂完成校验和全部能力装配。
    pub fn prepare(
        &self,
        record: &BackendRecord,
        host: ScopedHostServices,
    ) -> Result<ProviderRuntime, AppError> {
        let plugin = self.get(&record.kind)?;
        super::contract::validate_record(record)?;
        plugin.validate(record)?;
        plugin.prepare(record, host)
    }
}
