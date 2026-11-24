//! ModelScope 组合兼容推理与专属控制能力。

use super::{ScopedHostServices, contract::*};
use crate::{
    backend::modelscope::{ModelScopeBackend, ModelScopeCheckin},
    core::AppError,
    store::BackendRecord,
};
use std::sync::Arc;

pub const PLUGIN: PluginDefinition = PluginDefinition {
    describe,
    validate_record: validate,
    build,
    routes: super::control::no_routes,
    auth: None,
    accounts: AccountControl {
        managed: true,
        derive: derived_accounts,
        normalize: manual_credential,
    },
};

/// 官方单令牌账号是服务声明的派生视图。
fn derived_accounts(record: &BackendRecord) -> Vec<crate::accounts::Account> {
    if record.api_key.trim().is_empty() {
        Vec::new()
    } else {
        vec![crate::backend::modelscope::reference(record)]
    }
}

/// 人工账号不能覆盖官方单令牌引用。
fn manual_credential(_: &BackendRecord, _: &str) -> Result<String, AppError> {
    Err(AppError::bad_request(
        "账号由服务 Access Token 管理，请编辑服务凭证",
    ))
}

/// 网关分类不限制其专属额度和每日任务能力。
fn describe() -> PluginDescriptor {
    PluginDescriptor::new(
        "modelscope",
        "ModelScope 魔搭",
        "gateway",
        config_schema::<crate::backend::openai_compat::BackendOptions>(),
        Features {
            inference: true,
            catalog: true,
            accounts: true,
            quota: true,
            checkin: true,
            ..Default::default()
        },
    )
}

/// 官方域与启用令牌条件在统一配置写入口校验。
fn validate(record: &BackendRecord) -> Result<(), AppError> {
    crate::backend::modelscope::portal(record)?;
    if record.enabled && record.api_key.trim().is_empty() {
        return Err(AppError::bad_request(
            "启用 ModelScope 需要官方 Access Token",
        ));
    }
    Ok(())
}

/// 签到持有同一后端的 Arc，不再重新创建客户端。
fn build(record: &BackendRecord, host: ScopedHostServices) -> Result<ProviderRuntime, AppError> {
    let backend = Arc::new(ModelScopeBackend::new_with_host(record, host)?);
    Ok(ProviderRuntime {
        inference: backend.clone(),
        quota: Some(backend.clone()),
        credential_refresh: None,
        tasks: vec![Arc::new(ModelScopeCheckin::from_backend(backend))],
    })
}
