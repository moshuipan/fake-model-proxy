//! 唯一内置注册清单；新增插件不修改公共调度、额度或任务服务。

use super::{PluginRegistry, ProviderPlugin};
use std::sync::{Arc, OnceLock};

/// 返回进程内固定的能力目录；实例的启停由配置控制。
pub fn registry() -> &'static PluginRegistry {
    static REGISTRY: OnceLock<PluginRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let plugins: Vec<Arc<dyn ProviderPlugin>> = vec![
            Arc::new(super::openai_compat::PLUGIN),
            Arc::new(super::modelscope::PLUGIN),
            Arc::new(super::qoder::PLUGIN),
            Arc::new(super::workbuddy::PLUGIN),
            Arc::new(super::autoglm::PLUGIN),
            Arc::new(super::minimax_code::PLUGIN),
            Arc::new(super::monkeycode::PLUGIN),
            Arc::new(super::raccoon::PLUGIN),
            Arc::new(super::trae_personal::PLUGIN),
        ];
        PluginRegistry::new(plugins)
            .expect("built-in plugins must have unique compatible identities")
    })
}
