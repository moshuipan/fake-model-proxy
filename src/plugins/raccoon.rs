//! 小浣熊能力装配。

use super::{ScopedHostServices, contract::*};
pub mod auth;
super::control::auth_control!("raccoon", auth);
use crate::{backend::raccoon::RaccoonBackend, core::AppError, store::BackendRecord};
use std::sync::Arc;

pub const PLUGIN: PluginDefinition = PluginDefinition {
    describe,
    validate_record: validate,
    build,
    routes: auth_routes,
    auth: Some(AUTH),
    accounts: AccountControl {
        normalize: normalize_credential,
        ..AccountControl::MANUAL
    },
};

/// 人工导入和 v2 引用使用同一凭证规范化规则。
fn normalize_credential(_: &BackendRecord, value: &str) -> Result<String, AppError> {
    if value.is_empty() {
        return Ok(String::new());
    }
    crate::backend::raccoon::credential::Credential::parse(value)?.serialize()
}

/// 发布实际实现的能力和网页登录交互。
fn describe() -> PluginDescriptor {
    PluginDescriptor::new(
        "raccoon",
        "小浣熊",
        "reverse",
        config_schema::<EmptyOptions>(),
        Features {
            inference: true,
            catalog: true,
            accounts: true,
            quota: true,
            credential_refresh: true,
            checkin: true,
        },
    )
    .with_auth("open_url", true)
}

/// 官方域约束属于插件。
fn validate(record: &BackendRecord) -> Result<(), AppError> {
    crate::backend::raccoon::client::profile(record).map(|_| ())
}

/// 所有能力共享一个客户端、账号池和凭证协调器。
fn build(record: &BackendRecord, host: ScopedHostServices) -> Result<ProviderRuntime, AppError> {
    let backend = Arc::new(RaccoonBackend::new_with_host(record, host)?);
    Ok(ProviderRuntime {
        inference: backend.clone(),
        quota: Some(backend.clone()),
        credential_refresh: Some(backend.clone()),
        tasks: vec![backend],
    })
}
