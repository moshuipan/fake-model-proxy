//! MonkeyCode 推理、余额和签到能力装配。

use super::{ScopedHostServices, contract::*};
pub mod login;
use login::flow as auth;
super::control::auth_control!("monkeycode", auth);
use crate::{
    backend::monkeycode::{MonkeyCodeBackend, checkin::MonkeyCodeCheckin},
    core::AppError,
    store::BackendRecord,
};
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

/// 人工凭证必须绑定已保存的服务域，禁止跨服务发送 Cookie。
fn normalize_credential(record: &BackendRecord, value: &str) -> Result<String, AppError> {
    if value.is_empty() {
        return Ok(String::new());
    }
    let parsed = crate::backend::monkeycode::credential::Credential::parse(value)?;
    let (server, gateway) = crate::backend::monkeycode::profile(record)?;
    parsed.validate_origin(&server, &gateway)?;
    parsed.serialize()
}

/// 未实现的令牌续期不出现在能力或操作菜单中。
fn describe() -> PluginDescriptor {
    PluginDescriptor::new(
        "monkeycode",
        "MonkeyCode",
        "reverse",
        config_schema::<crate::backend::monkeycode::Options>(),
        Features {
            inference: true,
            catalog: true,
            accounts: true,
            quota: true,
            checkin: true,
            ..Default::default()
        },
    )
    .with_auth("local_browser", true)
}

/// 区域和协议类型由插件自身校验。
fn validate(record: &BackendRecord) -> Result<(), AppError> {
    crate::backend::monkeycode::profile(record).map(|_| ())
}

/// 签到后的余额读取复用推理实例。
fn build(record: &BackendRecord, host: ScopedHostServices) -> Result<ProviderRuntime, AppError> {
    let backend = Arc::new(MonkeyCodeBackend::new_with_timeout(
        record,
        host.accounts(),
        host.connect_timeout,
    )?);
    let task = Arc::new(MonkeyCodeCheckin::new_with_host(
        record,
        host,
        backend.clone(),
    )?);
    Ok(ProviderRuntime {
        inference: backend.clone(),
        quota: Some(backend),
        credential_refresh: None,
        tasks: vec![task],
    })
}
