//! TRAE CN 个人服务的能力装配。

use super::{ScopedHostServices, contract::*};
pub mod auth;
mod device;
super::control::auth_control!("trae_personal", auth);
use crate::{backend::trae_personal::TraePersonalBackend, core::AppError, store::BackendRecord};
use std::sync::Arc;

pub const PLUGIN: PluginDefinition = PluginDefinition {
    describe,
    validate_record: validate,
    build,
    routes: control_routes,
    auth: Some(AUTH),
    accounts: AccountControl {
        normalize: normalize_credential,
        ..AccountControl::MANUAL
    },
};

/// 个人账号凭证验证留在服务插件，不影响独立企业实例。
fn normalize_credential(_: &BackendRecord, value: &str) -> Result<String, AppError> {
    if value.is_empty() {
        return Ok(String::new());
    }
    crate::backend::trae_personal::credential::Credential::parse(value)?.serialize()
}

/// 发布官方网页授权和个人余额能力。
fn describe() -> PluginDescriptor {
    let mut descriptor = PluginDescriptor::new(
        "trae_personal",
        "TRAE 个人版",
        "reverse",
        config_schema::<crate::backend::trae_personal::Options>(),
        Features {
            inference: true,
            catalog: true,
            accounts: true,
            quota: true,
            credential_refresh: true,
            checkin: true,
        },
    )
    .with_auth("open_url", true);
    descriptor.docs = "docs/providers/trae.md".into();
    descriptor.ui["config_helpers"] = serde_json::json!([device::descriptor()]);
    descriptor
}

/// 官方授权和设备采集均由个人插件装配，公共控制台只消费能力描述。
fn control_routes() -> axum::Router<crate::state::AppState> {
    auth_routes().merge(device::routes())
}

/// 不允许把个人票据发送到未知网关。
fn validate(record: &BackendRecord) -> Result<(), AppError> {
    crate::backend::trae_personal::profile(record).map(|_| ())
}

/// 授权后的目录、推理、额度和任务共享一份运行状态。
fn build(record: &BackendRecord, host: ScopedHostServices) -> Result<ProviderRuntime, AppError> {
    let backend = Arc::new(TraePersonalBackend::new_with_host(record, host)?);
    Ok(ProviderRuntime {
        inference: backend.clone(),
        quota: Some(backend.clone()),
        credential_refresh: Some(backend.clone()),
        tasks: vec![backend],
    })
}
