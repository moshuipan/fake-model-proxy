//! 兼容 API 只需要预设数据，不需要新的协议插件。

use super::{ScopedHostServices, contract::*};
use crate::{backend::openai_compat::OpenAiCompatBackend, core::AppError, store::BackendRecord};
use std::sync::Arc;

pub const PLUGIN: PluginDefinition = PluginDefinition {
    describe,
    validate_record: validate,
    build,
    routes: super::control::no_routes,
    auth: None,
    accounts: AccountControl::MANUAL,
};

/// 导出与传输实现相同的 options 类型。
fn describe() -> PluginDescriptor {
    PluginDescriptor::new(
        "openai_compat",
        "OpenAI 兼容",
        "gateway",
        config_schema::<crate::backend::openai_compat::BackendOptions>(),
        Features {
            inference: true,
            catalog: true,
            ..Default::default()
        },
    )
}

/// 只解析配置并构造本地客户端，不探测上游。
fn validate(record: &BackendRecord) -> Result<(), AppError> {
    OpenAiCompatBackend::new(record).map(|_| ())
}

/// 纯兼容连接不虚构额度和续期能力。
fn build(record: &BackendRecord, host: ScopedHostServices) -> Result<ProviderRuntime, AppError> {
    Ok(ProviderRuntime::inference(Arc::new(
        OpenAiCompatBackend::new_with_timeout(record, &record.kind, host.connect_timeout)?,
    )))
}
