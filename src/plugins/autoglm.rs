//! AutoGLM 能力装配。

use super::{ScopedHostServices, contract::*};
pub mod auth;
super::control::auth_control!("autoglm", auth);
use crate::{backend::autoglm::AutoGlmBackend, core::AppError, store::BackendRecord};
use std::sync::Arc;

pub const PLUGIN: PluginDefinition = PluginDefinition {
    describe,
    validate_record: validate,
    build,
    routes,
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
    crate::backend::autoglm::credential::Credential::parse(value)?.serialize()
}

/// 验证码视图只在本插件命名空间挂载。
fn routes() -> axum::Router<crate::state::AppState> {
    use axum::routing::{get, post};
    auth_routes()
        .route(
            "/assets/autoglm-captcha.html",
            get(crate::admin::autoglm_captcha_page),
        )
        .route("/auth/autoglm/{id}", get(auth::browser_page))
        .route(
            "/auth/autoglm/{id}/captcha-config",
            post(auth::browser_captcha_config),
        )
        .route(
            "/auth/autoglm/{id}/send-code",
            post(auth::browser_send_code),
        )
        .route("/auth/autoglm/{id}/login", post(auth::browser_login))
        .route("/auth/autoglm/{id}/status", post(auth::browser_status))
        .route("/auth/autoglm/{id}/cancel", post(auth::browser_cancel))
        .route(
            "/api/oauth/autoglm/captcha-config",
            post(auth::captcha_config),
        )
}

/// 复杂验证码保留受控授权视图，其他界面使用统一能力描述。
fn describe() -> PluginDescriptor {
    let mut descriptor = PluginDescriptor::new(
        "autoglm",
        "AutoGLM",
        "reverse",
        config_schema::<crate::backend::autoglm::client::Options>(),
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
    if let Some(auth) = descriptor
        .operations
        .iter_mut()
        .find(|op| op.id == "auth.start")
    {
        auth.input_schema = serde_json::json!({
            "type":"object","additionalProperties":false,
            "properties":{
                "provider":{"type":"string","enum":["zai","google"]},
                "device_id":{"type":"string"},
                "ali_captcha_verify_param":{"type":"string"}
            }
        });
    }
    descriptor
}

/// 解析区域和端点，构造时不发送请求。
fn validate(record: &BackendRecord) -> Result<(), AppError> {
    crate::backend::autoglm::client::Client::new(record).map(|_| ())
}

/// 推理与管理动作复用一个服务实例。
fn build(record: &BackendRecord, host: ScopedHostServices) -> Result<ProviderRuntime, AppError> {
    let backend = Arc::new(AutoGlmBackend::new_with_host(record, host)?);
    Ok(ProviderRuntime {
        inference: backend.clone(),
        quota: Some(backend.clone()),
        credential_refresh: Some(backend.clone()),
        tasks: vec![backend],
    })
}
