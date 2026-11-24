//! MiniMax Code 能力装配。

use super::{ScopedHostServices, contract::*};
pub mod auth;
super::control::auth_control!("minimax_code", auth);
use crate::{backend::minimax_code::MiniMaxBackend, core::AppError, store::BackendRecord};
use std::sync::Arc;

pub const PLUGIN: PluginDefinition = PluginDefinition {
    describe,
    validate_record: validate,
    build,
    routes: auth_routes,
    auth: Some(AUTH),
    accounts: AccountControl::MANUAL,
};

/// 设备码流程不要求 AI 打开个人客户端。
fn describe() -> PluginDescriptor {
    PluginDescriptor::new(
        "minimax_code",
        "MiniMax Code",
        "reverse",
        config_schema::<crate::backend::minimax_code::Options>(),
        Features {
            inference: true,
            catalog: true,
            accounts: true,
            quota: true,
            credential_refresh: true,
            checkin: true,
        },
    )
    .with_auth("device_code", true)
}

/// 区域与官方域校验由协议实现提供。
fn validate(record: &BackendRecord) -> Result<(), AppError> {
    crate::backend::minimax_code::profile(record).map(|_| ())
}

/// 凭证围栏和目录缓存在全部能力间共享。
fn build(record: &BackendRecord, host: ScopedHostServices) -> Result<ProviderRuntime, AppError> {
    let backend = Arc::new(MiniMaxBackend::new_with_host(record, host)?);
    Ok(ProviderRuntime {
        inference: backend.clone(),
        quota: Some(backend.clone()),
        credential_refresh: Some(backend.clone()),
        tasks: vec![backend],
    })
}
