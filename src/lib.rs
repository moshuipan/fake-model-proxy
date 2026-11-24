//! fakemodelproxy 库入口。
//!
//! 二进制与集成测试共用同一套模块装配。

pub mod accounts;
pub mod admin;
pub mod application;
pub mod audit;
pub mod backend;
pub mod client_plugins;
pub mod config;
pub mod config_bundle;
pub mod core;
pub mod gateway;
pub mod ingress;
pub mod model_names;
pub mod plugins;
pub mod presets;
pub mod records;
pub mod routing;
pub mod state;
pub mod store;
pub mod tasks;
pub mod tooling;
pub mod usage;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{delete, get, post};
use state::AppState;

/// 构造完整路由。管理页、协议入口与健康检查共用同一状态。
pub fn build_router(state: AppState) -> Router {
    let inference = Router::new()
        .route(
            "/v1/chat/completions",
            post(ingress::chat::chat_completions),
        )
        .route("/v1/messages", post(ingress::messages::messages))
        .route("/v1/responses", post(ingress::responses::responses))
        .layer(DefaultBodyLimit::max(state.config.max_request_body_bytes))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            audit::middleware,
        ));
    Router::new()
        .nest(
            "/clients/trae",
            inference
                .clone()
                .route("/v1/models", get(ingress::models::list_models))
                .layer(axum::Extension(client_plugins::ClientKind::Trae)),
        )
        .nest(
            "/clients/minimax-code",
            inference
                .clone()
                .route("/v1/models", get(ingress::models::list_models))
                .layer(axum::Extension(client_plugins::ClientKind::MinimaxCode)),
        )
        .merge(inference)
        .route("/", get(admin::console))
        .route("/assets/console.css", get(admin::console_styles))
        .route("/assets/console.js", get(admin::console_script))
        .merge(plugins::builtin::registry().control_routes())
        .merge(admin::v2::routes())
        .route("/assets/select.js", get(admin::select_script))
        .route("/assets/select.css", get(admin::select_styles))
        .route("/api/console/status", get(admin::console_status))
        .route(
            "/api/gateway",
            get(admin::gateway_settings).put(admin::save_gateway_settings),
        )
        .route("/health", get(admin::health))
        .route("/v1/models", get(ingress::models::list_models))
        .route(
            "/api/model-aliases",
            get(admin::list_model_aliases).put(admin::rename_model_alias),
        )
        .route(
            "/api/backends",
            get(admin::list_backends).post(admin::upsert_backend),
        )
        .route("/api/backends/{id}", delete(admin::delete_backend))
        .route("/api/backends/{id}/test", post(admin::test_backend))
        .route("/api/backends/{id}/models/sync", post(admin::sync_models))
        .route(
            "/api/accounts",
            get(admin::list_accounts).post(admin::upsert_account),
        )
        .route("/api/accounts/{id}", delete(admin::delete_account))
        .route("/api/accounts/{id}/refresh", post(admin::refresh_account))
        .route("/api/quota", get(admin::read_quota))
        .route("/api/quota/cached", get(admin::cached_quota))
        .route("/api/usage", get(admin::usage_report))
        .route("/api/records", get(admin::list_records))
        .route("/api/tasks", get(admin::list_tasks))
        .route(
            "/api/tasks/checkin-schedule",
            get(admin::checkin_schedule).put(admin::save_checkin_schedule),
        )
        .route(
            "/api/tasks/checkin-all",
            get(admin::checkin_all_status).post(admin::start_checkin_all),
        )
        .route("/api/tasks/{backend_id}/checkin", post(admin::run_checkin))
        .route("/api/route/plan", get(admin::plan_route))
        .route("/api/health/backends", get(admin::backend_health))
        .route("/api/presets", get(admin::list_presets))
        .route("/api/config/export", get(admin::export_config))
        .route("/api/config/import", post(admin::import_config))
        .with_state(state)
}
