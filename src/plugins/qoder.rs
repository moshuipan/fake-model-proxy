//! Qoder 的私有协议能力组合。

use super::{ScopedHostServices, contract::*};
pub mod auth;
super::control::auth_control!("qoder", auth);
use crate::{backend::qoder::*, core::AppError, store::BackendRecord};
use std::sync::Arc;

pub const PLUGIN: PluginDefinition = PluginDefinition {
    describe,
    validate_record: validate,
    build,
    routes: auth_routes,
    auth: Some(AUTH),
    accounts: AccountControl::MANUAL,
};

/// Qoder 专属选项只公开服务区域，账号私有端点留在凭证域。
#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Options {
    #[serde(default)]
    #[allow(dead_code)]
    region: Option<QoderRegion>,
}

/// 声明当前设备登录流程与已实现的任务。
fn describe() -> PluginDescriptor {
    PluginDescriptor::new(
        "qoder",
        "Qoder",
        "reverse",
        config_schema::<Options>(),
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

/// 不接受未定义区域，防止拼写错误静默选择全球区。
fn validate(record: &BackendRecord) -> Result<(), AppError> {
    let raw: serde_json::Value = serde_json::from_str(if record.options.is_empty() {
        "{}"
    } else {
        &record.options
    })
    .map_err(|_| AppError::bad_request("invalid Qoder options"))?;
    if raw
        .get("region")
        .and_then(|v| v.as_str())
        .is_some_and(|r| !matches!(r, "cn" | "global"))
    {
        return Err(AppError::bad_request("Qoder region must be cn or global"));
    }
    Ok(())
}

/// 协议各视图在一个服务运行实例中注册，续期提交由宿主执行 CAS。
fn build(record: &BackendRecord, host: ScopedHostServices) -> Result<ProviderRuntime, AppError> {
    let region = QoderRegion::from_options(&record.options)?;
    let backend = Arc::new(QoderBackend::new_with_timeout(
        record.id.clone(),
        record.name.clone(),
        region,
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
        Arc::new(QoderRefresher::new(record.id.clone(), region)?),
        host.clone(),
    );
    Ok(ProviderRuntime {
        inference: backend,
        quota: Some(Arc::new(QoderQuota::new(record.id.clone(), region)?)),
        credential_refresh: Some(Arc::new(refresh)),
        tasks: vec![Arc::new(crate::tasks::checkin::QoderCheckin::new(
            record.id.clone(),
            region,
            host.accounts(),
        )?)],
    })
}
