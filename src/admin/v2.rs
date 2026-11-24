//! 版本化管理 HTTP 传输；业务权限、事务和操作状态都由应用服务执行。

use crate::{
    application::{
        access::{Principal, Scope},
        api_schema::{self, ApplyInput, PresetInput, RollbackInput, SecretInput},
        config_types::{self, ConfigChange},
        management::{ExplainInput, Management, PlanInput},
        operation_service::{OperationRequest, OperationService},
    },
    core::AppError,
    state::AppState,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::HeaderMap,
    routing::{get, post},
};
use serde_json::{Value, json};

/// 目录路径保持稳定，插件新增无需修改公共端点。
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v2/identity", get(identity))
        .route("/api/v2/plugins", get(plugins))
        .route("/api/v2/plugins/{id}", get(plugin))
        .route("/api/v2/presets", get(presets).post(import_presets))
        .route("/api/v2/schema", get(schema))
        .route("/api/v2/openapi.json", get(openapi))
        .route("/api/v2/config", get(config))
        .route("/api/v2/config/validate", post(validate))
        .route("/api/v2/config/plans", post(plan))
        .route("/api/v2/config/plans/{id}", get(get_plan))
        .route("/api/v2/config/plans/{id}/apply", post(apply))
        .route("/api/v2/config/rollback", post(rollback))
        .route("/api/v2/secrets", post(secret))
        .route("/api/v2/operations", get(operations).post(start_operation))
        .route("/api/v2/operations/{id}", get(operation))
        .route("/api/v2/operations/{id}/cancel", post(cancel_operation))
        .route("/api/v2/routes/explain", post(explain))
        .layer(axum::middleware::from_fn(json_rejection))
}

/// JSON 解析拒绝也使用稳定错误码，避免回显输入中的秘密或输出纯文本错误。
async fn json_rejection(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let response = next.run(request).await;
    if matches!(
        response.status(),
        axum::http::StatusCode::BAD_REQUEST
            | axum::http::StatusCode::UNPROCESSABLE_ENTITY
            | axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE
    ) && response.extensions().get::<AppError>().is_none()
    {
        return config_types::error("schema_invalid", "请求必须是符合接口 schema 的 JSON 对象")
            .into_response();
    }
    response
}
/// 只在传输边界读取密钥；后续用例携带不可由 JSON 输入伪造的身份。
fn principal(state: &AppState, headers: &HeaderMap) -> Result<Principal, AppError> {
    Principal::authenticate(
        &state.config,
        headers
            .get("x-admin-key")
            .and_then(|v| v.to_str().ok())
            .or_else(|| {
                headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "))
            }),
    )
}

/// 返回当前权限，帮助客户端选择可执行动作。
async fn identity(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Principal>, AppError> {
    Ok(Json(principal(&state, &headers)?))
}

/// 返回已安装插件，不包含账号就绪度推断。
async fn plugins(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let actor = principal(&state, &headers)?;
    Ok(Json(
        json!({"plugins":Management::new(&state, &actor).plugins()?}),
    ))
}

/// 读取单个插件完整 schema、授权流程和动作目录。
async fn plugin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let actor = principal(&state, &headers)?;
    let plugins = Management::new(&state, &actor).plugins()?;
    Ok(Json(json!(
        plugins
            .into_iter()
            .find(|p| p.id == id)
            .ok_or_else(|| AppError::NotFound {
                message: "插件未安装".into()
            })?
    )))
}

/// schema 直接从 Rust 类型生成，CLI 和 UI 无独立字段事实源。
async fn schema(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    principal(&state, &headers)?.require(Scope::Read)?;
    Ok(Json(api_schema::bundle()))
}

/// OpenAPI 与 schema 使用执行入口共享的 Rust 输入类型。
async fn openapi(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    principal(&state, &headers)?.require(Scope::Read)?;
    Ok(Json(api_schema::openapi()))
}

/// 公开预设目录与插件能力目录分开，不据此推断账号费用。
async fn presets(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let actor = principal(&state, &headers)?;
    Ok(Json(Management::new(&state, &actor).presets().await?))
}

/// 外部数据先做全量契约验证再事务保存。
async fn import_presets(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<PresetInput>,
) -> Result<Json<Value>, AppError> {
    let actor = principal(&state, &headers)?;
    Ok(Json(
        Management::new(&state, &actor)
            .import_presets(input.presets)
            .await?,
    ))
}

/// 返回脱敏配置和生效版本。
async fn config(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let actor = principal(&state, &headers)?;
    Ok(Json(json!(Management::new(&state, &actor).config().await?)))
}

/// 校验不产生远程副作用。
async fn validate(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(change): Json<ConfigChange>,
) -> Result<Json<Value>, AppError> {
    let actor = principal(&state, &headers)?;
    Ok(Json(json!(
        Management::new(&state, &actor).validate(change).await?
    )))
}

/// 持久保存可审阅计划。
async fn plan(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<PlanInput>,
) -> Result<Json<Value>, AppError> {
    let actor = principal(&state, &headers)?;
    Ok(Json(json!(
        Management::new(&state, &actor).plan(input).await?
    )))
}

/// 计划查询适用于跨进程 CLI 和浏览器。
async fn get_plan(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let actor = principal(&state, &headers)?;
    Ok(Json(json!(
        Management::new(&state, &actor).get_plan(&id).await?
    )))
}

/// 应用仅绑定已审阅的计划 ID，不接收额外隐式变更。
async fn apply(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<ApplyInput>,
) -> Result<Json<Value>, AppError> {
    let actor = principal(&state, &headers)?;
    Ok(Json(json!(
        Management::new(&state, &actor)
            .apply(&id, &input.idempotency_key)
            .await?
    )))
}

/// 回滚接口返回新计划，绝不回退动态 token 或用量。
async fn rollback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<RollbackInput>,
) -> Result<Json<Value>, AppError> {
    let actor = principal(&state, &headers)?;
    Ok(Json(json!(
        Management::new(&state, &actor)
            .rollback(input.expected_revision, input.target_revision)
            .await?
    )))
}

/// 写入不回显秘密；新引用须通过配置计划绑定。
async fn secret(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<SecretInput>,
) -> Result<Json<Value>, AppError> {
    let actor = principal(&state, &headers)?;
    Ok(Json(
        Management::new(&state, &actor)
            .write_secret(&input.value)
            .await?,
    ))
}

/// 操作历史不触发上游查询。
async fn operations(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let actor = principal(&state, &headers)?;
    Ok(Json(
        json!({"operations":OperationService::new(&state, &actor).list().await?}),
    ))
}

/// 接收动作后返回可轮询 ID，不等待用户操作。
async fn start_operation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<OperationRequest>,
) -> Result<Json<Value>, AppError> {
    let actor = principal(&state, &headers)?;
    Ok(Json(json!(
        OperationService::new(&state, &actor).start(input).await?
    )))
}

/// 读取操作状态；响应不包含内部 session ID。
async fn operation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let actor = principal(&state, &headers)?;
    Ok(Json(json!(
        OperationService::new(&state, &actor).get(&id).await?
    )))
}

/// 显式取消保留上游已经完成的授权结果。
async fn cancel_operation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let actor = principal(&state, &headers)?;
    Ok(Json(json!(
        OperationService::new(&state, &actor).cancel(&id).await?
    )))
}

/// 解释只读内存快照，额度或目录刷新必须单独发操作。
async fn explain(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<ExplainInput>,
) -> Result<Json<Value>, AppError> {
    let actor = principal(&state, &headers)?;
    Ok(Json(Management::new(&state, &actor).explain(input)?))
}
