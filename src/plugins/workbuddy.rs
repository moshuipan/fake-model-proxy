//! WorkBuddy 的区域、客户端画像与能力装配。

use super::{ScopedHostServices, contract::*};
pub mod auth;
super::control::auth_control!("workbuddy", auth);
use crate::{backend::workbuddy::*, core::AppError, store::BackendRecord};
use std::sync::Arc;

pub const PLUGIN: PluginDefinition = PluginDefinition {
    describe,
    validate_record: validate,
    build,
    routes: auth_routes,
    auth: Some(AUTH),
    accounts: AccountControl::MANUAL,
};

/// 账号授权与服务配置表单从同一目录发现。
fn describe() -> PluginDescriptor {
    PluginDescriptor::new(
        "workbuddy",
        "WorkBuddy",
        "reverse",
        config_schema::<crate::backend::workbuddy::BackendOptions>(),
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

/// 服务画像解析不触发网络动作。
fn validate(record: &BackendRecord) -> Result<(), AppError> {
    profile_from_options(&record.options).map(|_| ())
}

/// 各能力共享固定画像和服务作用域，账号刷新不能覆盖新登录。
fn build(record: &BackendRecord, host: ScopedHostServices) -> Result<ProviderRuntime, AppError> {
    let profile = profile_from_options(&record.options)?;
    let backend = Arc::new(WorkBuddyBackend::new_with_timeout(
        record.id.clone(),
        record.name.clone(),
        profile.clone(),
        record
            .models
            .split(',')
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(String::from)
            .collect(),
        host.accounts(),
        host.connect_timeout,
    )?);
    let refresh = super::host::PersistedRefresher::new(
        Arc::new(WorkBuddyRefresher::new(record.id.clone(), profile.clone())?),
        host.clone(),
    );
    Ok(ProviderRuntime {
        inference: backend,
        quota: Some(Arc::new(WorkBuddyQuota::new(
            record.id.clone(),
            record.name.clone(),
            profile.clone(),
        )?)),
        credential_refresh: Some(Arc::new(refresh)),
        tasks: vec![Arc::new(WorkBuddyCheckin::new(
            record.id.clone(),
            profile,
            host.accounts(),
        )?)],
    })
}
