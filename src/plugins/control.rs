//! 授权控制适配器：复用受控页面与会话实现，装配信息由各插件注册。
//!
//! 该层属于可信 HTTP/应用装配边界，推理工厂只接收 ScopedHostServices。

use crate::{core::AppError, state::AppState};
use futures_util::future::BoxFuture;
use serde_json::Value;

/// 注册的授权动作直接调用现有会话用例，不通过内部 HTTP 请求重入服务器。
pub type AuthAction = fn(AppState, Value) -> BoxFuture<'static, Result<Value, AppError>>;

/// 只有已接入实际执行器的授权操作才能被管理目录发现。
#[derive(Clone, Copy)]
pub struct AuthControl {
    pub start: AuthAction,
    pub poll: AuthAction,
    pub cancel: AuthAction,
}

/// 生成薄适配器；厂商会话仍检查服务归属、截止时间和凭证写回围栏。
macro_rules! auth_action {
    ($name:ident, $handler:path) => {
        /// 把类型化授权 handler 适配为操作服务的 JSON 调用。
        fn $name(
            state: crate::state::AppState,
            input: serde_json::Value,
        ) -> futures_util::future::BoxFuture<
            'static,
            Result<serde_json::Value, crate::core::AppError>,
        > {
            Box::pin(async move {
                let input = serde_json::from_value(input).map_err(|_| {
                    crate::application::config_types::error(
                        "schema_invalid",
                        "授权输入不符合插件契约",
                    )
                })?;
                $handler(state, input).await.map(|value| value.0)
            })
        }
    };
}
pub(crate) use auth_action;

/// 统一注册三种基本授权动作与兼容路由，复杂受控页面由插件追加。
macro_rules! auth_control {
    ($id:literal, $module:ident) => {
        super::control::auth_action!(auth_start, $module::start_authorized);
        super::control::auth_action!(auth_poll, $module::poll_authorized);
        super::control::auth_action!(auth_cancel, $module::cancel_authorized);
        const AUTH: super::control::AuthControl = super::control::AuthControl {
            start: auth_start,
            poll: auth_poll,
            cancel: auth_cancel,
        };
        /// 挂载插件授权的兼容 HTTP 入口。
        fn auth_routes() -> axum::Router<crate::state::AppState> {
            axum::Router::new()
                .route(
                    concat!("/api/oauth/", $id, "/start"),
                    axum::routing::post($module::start),
                )
                .route(
                    concat!("/api/oauth/", $id, "/poll"),
                    axum::routing::post($module::poll),
                )
                .route(
                    concat!("/api/oauth/", $id, "/cancel"),
                    axum::routing::post($module::cancel),
                )
        }
    };
}
pub(crate) use auth_control;

/// 无授权能力的纯推理插件不挂载任何额外端点。
pub fn no_routes() -> axum::Router<AppState> {
    axum::Router::new()
}
