//! 管理 API 与控制台静态页。
//!
//! 首版只提供后端配置的增删改查与连通测试，不做多用户与权限体系。

// Rust 路径兼容旧集成测试；实现和路由归各插件目录所有。
pub use crate::plugins::{
    autoglm::auth as autoglm_oauth, minimax_code::auth as minimax_oauth,
    monkeycode::login as monkeycode, raccoon::auth as raccoon_oauth,
    trae_personal::auth as trae_personal,
};
pub mod v2;

use crate::core::AppError;
use crate::state::AppState;
use crate::store::BackendRecord;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::http::header;
use axum::response::{Html, IntoResponse};
use serde::Deserialize;
use serde_json::{Value, json};

/// 管理页 HTML，构建时内嵌。
const CONSOLE_HTML: &str = include_str!("../../web/index.html");

/// 返回控制台页面。
pub async fn console() -> Html<&'static str> {
    Html(CONSOLE_HTML)
}

/// 返回内嵌样式；随二进制发布，不依赖外部 CDN。
pub async fn console_styles() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        include_str!("../../web/console.css"),
    )
}

/// 返回内嵌交互脚本，重启升级后由浏览器重新校验。
pub async fn console_script() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        include_str!("../../web/console.js"),
    )
}

/// 每次 AutoGLM 授权使用独立 SDK 页面，避免旧验证回调混入新会话。
pub async fn autoglm_captcha_page() -> impl IntoResponse {
    (
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        Html(include_str!("../../web/autoglm-captcha.html")),
    )
}

/// 返回统一下拉的渐进增强脚本，保持原生表单值和校验。
pub async fn select_script() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        include_str!("../../web/select.js"),
    )
}

/// 返回下拉浮层样式，与控制台同源发布。
pub async fn select_styles() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        include_str!("../../web/select.css"),
    )
}

/// 提供控制台所需的运行状态；不返回任何凭证。
pub async fn console_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    Ok(Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "api_key_required": state.config.requires_api_key(),
        "admin_key_required": !state.config.admin_key.is_empty(),
        "checkin_scheduler_enabled": state.store.checkin_settings().await?.enabled,
        "checkin_backends": state.task_providers().iter()
            .filter(|provider| provider.kind() == crate::tasks::TaskKind::Checkin)
            .map(|provider| provider.backend_id().to_string()).collect::<Vec<_>>(),
        "record_capacity": state.records.persistence().capacity,
        "max_request_body_bytes": state.config.max_request_body_bytes,
        "record_persistence": state.records.persistence(),
    })))
}

/// 读取模型目录并原子保存白名单与显示名，仅更新目录相关字段。
pub async fn sync_models(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    let handle = state
        .registry
        .get(&id)
        .ok_or_else(|| AppError::bad_request("请先启用该服务"))?;
    let expected = state
        .store
        .get_backend(&id)
        .await?
        .ok_or_else(|| AppError::bad_request("服务已删除"))?;
    let models = handle.list_models().await?;
    if models.is_empty() {
        return Err(AppError::bad_request(
            "模型目录为空，请先完成登录或检查配置",
        ));
    }
    state
        .config_service()
        .sync_models(&expected, &models)
        .await?;
    Ok(Json(json!({"models": models, "backend": id})))
}

/// 后端配置请求体。`id` 为空时新增。
#[derive(Debug, Deserialize)]
pub struct BackendInput {
    #[serde(default)]
    pub id: String,
    pub name: String,
    #[serde(default = "default_kind")]
    pub kind: String,
    pub base_url: String,
    /// 为空时保留原值，便于前端不回显密钥。
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub models: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// auto 排序优先级，数值越小越优先。
    #[serde(default = "default_priority")]
    pub priority: u8,
    /// 费用层级：free / subscription / paid / unknown。
    #[serde(default = "default_cost_tier")]
    pub cost_tier: String,
    /// 后端专属选项 JSON（自定义头、端点覆盖等）。
    #[serde(default)]
    pub options: String,
    /// 能力覆盖 JSON。
    #[serde(default)]
    pub capabilities: String,
    /// 每日 token 预算，0 表示不限制。
    #[serde(default)]
    pub daily_token_budget: u64,
}

/// 默认优先级。
fn default_priority() -> u8 {
    100
}

/// 默认费用层级。
fn default_cost_tier() -> String {
    "unknown".to_string()
}

/// 默认后端类型。
fn default_kind() -> String {
    "openai_compat".to_string()
}

/// 默认启用。
fn default_enabled() -> bool {
    true
}

/// 列出后端配置，密钥脱敏。
pub async fn list_backends(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    let records = state.store.list_backends().await?;
    let catalogs = state.store.list_model_catalogs().await?;
    Ok(Json(json!({
        "backends": records.iter().map(|record| json!({
            "id": record.id,
            "name": record.name,
            "kind": record.kind,
            "base_url": record.base_url,
            "api_key_masked": mask_key(&record.api_key),
            "has_api_key": !record.api_key.is_empty(),
            "models": record.models,
            "model_catalog": catalogs.get(&record.id).cloned().unwrap_or_default(),
            "enabled": record.enabled,
            "priority": record.priority,
            "cost_tier": record.cost_tier,
            "options": crate::config_bundle::redact_options(&record.options),
            "capabilities": record.capabilities,
            "daily_token_budget": record.daily_token_budget,
        })).collect::<Vec<_>>(),
    })))
}

/// 新增或更新后端配置。
pub async fn upsert_backend(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<BackendInput>,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    if input.name.trim().is_empty() {
        return Err(AppError::bad_request("name is required"));
    }
    crate::plugins::builtin::registry().get(&input.kind)?;
    if input.base_url.trim().is_empty() {
        return Err(AppError::bad_request("base_url is required"));
    }
    let id = if input.id.trim().is_empty() {
        uuid::Uuid::new_v4().simple().to_string()
    } else {
        input.id.trim().to_string()
    };
    // 未提供新密钥时沿用已有值，避免编辑其他字段时清空凭证。
    let existing = state.store.get_backend(&id).await?;
    let api_key = input.api_key.trim().to_string();
    let record = BackendRecord {
        id: id.clone(),
        name: input.name.trim().to_string(),
        kind: input.kind,
        base_url: input.base_url.trim().to_string(),
        api_key,
        models: input.models.trim().to_string(),
        enabled: input.enabled,
        priority: input.priority,
        cost_tier: input.cost_tier,
        options: input.options,
        capabilities: input.capabilities,
        daily_token_budget: input.daily_token_budget,
    };
    crate::plugins::contract::validate_record(&record)?;
    state.config_service().save_backend(&record).await?;
    if existing.as_ref().is_some_and(|previous| {
        !record.enabled
            || previous.kind != record.kind
            || previous.base_url != record.base_url
            || previous.options != record.options
    }) {
        state.auth.cancel_service(&id).await;
    }
    Ok(Json(json!({"id": id})))
}

/// 删除后端配置。
pub async fn delete_backend(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    state.config_service().delete_backend(&id).await?;
    state.auth.cancel_service(&id).await;
    Ok(Json(json!({"deleted": id})))
}

/// 测试连通性：用配置里的密钥拉一次模型目录。
pub async fn test_backend(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    let backend = state.registry.get(&id).ok_or_else(|| AppError::NotFound {
        message: format!("backend `{id}` is not loaded; save it first"),
    })?;
    match backend.list_models().await {
        Ok(models) => Ok(Json(json!({
            "ok": true,
            "backend": id,
            "models": models.iter().map(|model| model.id.clone()).collect::<Vec<_>>(),
        }))),
        Err(error) => Ok(Json(json!({
            "ok": false,
            "backend": id,
            "code": error.code(),
            "upstream_status": match &error {
                AppError::Upstream(upstream) => upstream.status,
                _ => None,
            },
            "message": error.message(),
        }))),
    }
}

/// 健康检查。
pub async fn health() -> impl IntoResponse {
    Json(json!({"ok": true}))
}

/// 读取持久策略与当前负载，不暴露凭证或历史正文。
pub async fn gateway_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    Ok(Json(json!({
        "settings": state.router.gateway.settings(),
        "runtime": state.router.gateway.snapshot(),
    })))
}

/// 保存网关策略并重建传输客户端，在途请求保留原设置和租约。
pub async fn save_gateway_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(settings): Json<crate::gateway::GatewaySettings>,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    state
        .config_service()
        .mutate("legacy.gateway.save", |document, _| {
            document.gateway = settings;
            Ok(())
        })
        .await?;
    gateway_settings(State(state), headers).await
}

/// 账号输入。
#[derive(Debug, Deserialize)]
pub struct AccountInput {
    #[serde(default)]
    pub id: String,
    pub backend_id: String,
    pub name: String,
    /// 凭证 JSON，由后端解释结构。
    #[serde(default)]
    pub credential: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

/// 列出账号。凭证只返回是否已配置，不回显内容。
pub async fn list_accounts(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    let accounts = state.store.list_accounts().await?;
    Ok(Json(json!({
        "accounts": accounts.iter().map(|account| json!({
            "id": account.id,
            "backend_id": account.backend_id,
            "name": account.name,
            "enabled": account.enabled,
            "has_credential": !account.credential.is_empty(),
        })).collect::<Vec<_>>(),
    })))
}

/// 新增或更新账号。
pub async fn upsert_account(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<AccountInput>,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    if input.name.trim().is_empty() {
        return Err(AppError::bad_request("name is required"));
    }
    let backend = state
        .store
        .get_backend(&input.backend_id)
        .await?
        .ok_or_else(|| AppError::NotFound {
            message: format!("backend `{}` not found", input.backend_id),
        })?;
    let id = if input.id.trim().is_empty() {
        uuid::Uuid::new_v4().simple().to_string()
    } else {
        input.id.trim().to_string()
    };
    let control = crate::plugins::builtin::registry()
        .get(&backend.kind)?
        .account_control();
    let credential = (control.normalize)(&backend, input.credential.trim())?;
    state
        .config_service()
        .save_account(&crate::accounts::Account {
            id: id.clone(),
            backend_id: input.backend_id,
            name: input.name.trim().to_string(),
            credential,
            enabled: input.enabled,
        })
        .await?;
    Ok(Json(json!({"id": id})))
}

/// 删除账号。
pub async fn delete_account(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    state.config_service().delete_account(&id).await?;
    Ok(Json(json!({"deleted": id})))
}

/// 最近请求记录。
pub async fn list_records(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    let limit = query
        .get("limit")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(state.records.persistence().capacity);
    let records = state.records.list(state.records.persistence().capacity);
    let matched: Vec<_> = records
        .into_iter()
        .filter(|record| {
            query
                .get("trace_id")
                .is_none_or(|value| value == &record.trace_id)
                && query
                    .get("session_id")
                    .is_none_or(|value| record.session_id.as_ref() == Some(value))
                && query.get("client").is_none_or(|value| {
                    record.client.is_some_and(|client| client.as_str() == value)
                })
                && query.get("session_confidence").is_none_or(|value| {
                    record
                        .session
                        .as_ref()
                        .is_some_and(|session| match session.confidence {
                            crate::client_plugins::SessionConfidence::Explicit => {
                                value == "explicit"
                            }
                            crate::client_plugins::SessionConfidence::Inferred => {
                                value == "inferred"
                            }
                        })
                })
        })
        .collect();
    let matched_count = matched.len();
    Ok(Json(json!({
        "records": matched.into_iter().take(limit.max(1)).collect::<Vec<_>>(),
        "total": state.records.len(),
        "matched": matched_count,
        "persistence": state.records.persistence(),
    })))
}

/// 最近任务记录。
pub async fn list_tasks(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    let limit = query
        .get("limit")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(50);
    Ok(Json(json!({"tasks": state.store.list_tasks(limit).await?})))
}

/// 读取持久配置及各账号的下一次执行时间。
pub async fn checkin_schedule(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<crate::tasks::scheduler::ScheduleView>, AppError> {
    authorize(&state, &headers)?;
    Ok(Json(crate::tasks::scheduler::view(&state).await?))
}

/// 页面保存签到窗口并立即唤醒调度器。
pub async fn save_checkin_schedule(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<crate::tasks::scheduler::ScheduleInput>,
) -> Result<Json<crate::tasks::scheduler::ScheduleView>, AppError> {
    authorize(&state, &headers)?;
    Ok(Json(crate::tasks::scheduler::save(&state, &input).await?))
}

/// 启动所有启用账号的后台签到批次，重复点击复用运行中的任务。
pub async fn start_checkin_all(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<crate::tasks::batch::BatchView>, AppError> {
    authorize(&state, &headers)?;
    Ok(Json(crate::tasks::batch::start(&state).await?))
}

/// 读取最近批次的逐账号状态，刷新页面后仍能跟踪后台执行。
pub async fn checkin_all_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<crate::tasks::batch::BatchView>, AppError> {
    authorize(&state, &headers)?;
    Ok(Json(state.checkin_batch.view()))
}

/// 立即执行某后端的签到。
pub async fn run_checkin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(backend_id): Path<String>,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    let provider = state
        .task_providers()
        .into_iter()
        .find(|provider| provider.backend_id() == backend_id)
        .ok_or_else(|| AppError::NotFound {
            message: format!("backend `{backend_id}` has no check-in capability"),
        })?;
    let accounts = state.store.list_accounts_for(&backend_id).await?;
    let period = today();
    let mut results = Vec::new();
    for account in accounts.iter().filter(|account| account.enabled) {
        let outcome =
            crate::tasks::run_and_record(&state.store, provider.as_ref(), &account.id, &period)
                .await?;
        results.push(json!({
            "account": account.id,
            "status": outcome.status.as_str(),
            "message": outcome.message,
            "amount": outcome.amount,
        }));
    }
    Ok(Json(json!({"period": period, "results": results})))
}

/// auto 路由预演，返回候选与排除原因。
pub async fn plan_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    let model = query
        .get("model")
        .cloned()
        .unwrap_or_else(|| "auto".to_string());
    let tools = query
        .get("tools")
        .map(|value| value == "true")
        .unwrap_or(false);
    let images = query
        .get("images")
        .map(|value| value == "true")
        .unwrap_or(false);
    let Some((policy, free_only)) = crate::routing::RoutePolicy::parse(&model) else {
        return Err(AppError::bad_request(
            "model must be auto (legacy auto aliases remain accepted)",
        ));
    };
    let mut request = crate::core::ChatRequest {
        protocol: crate::core::Protocol::Chat,
        model,
        messages: vec![crate::core::Message::text(
            crate::core::Role::User,
            "route preview",
        )],
        tools: if tools {
            vec![crate::core::ToolDef {
                name: "probe".to_string(),
                description: String::new(),
                parameters: json!({"type": "object"}),
            }]
        } else {
            Vec::new()
        },
        tool_choice: None,
        max_tokens: query
            .get("output_tokens")
            .map(|value| value.parse::<u32>())
            .transpose()
            .map_err(|_| AppError::bad_request("output_tokens must be an unsigned integer"))?,
        temperature: None,
        top_p: None,
        stop: Vec::new(),
        reasoning_effort: None,
        stream: false,
        session: None,
    };
    let input_tokens = query
        .get("input_tokens")
        .map(|value| value.parse::<u32>())
        .transpose()
        .map_err(|_| AppError::bad_request("input_tokens must be an unsigned integer"))?
        .unwrap_or(0);
    if input_tokens > 2_000_000 {
        return Err(AppError::bad_request(
            "preview input_tokens must be <= 2000000",
        ));
    }
    if input_tokens > 0 {
        request.messages = vec![crate::core::Message::text(
            crate::core::Role::User,
            "x".repeat(input_tokens as usize),
        )];
    }
    if images {
        request.messages[0]
            .content
            .push(crate::core::ContentPart::ImageUrl {
                url: String::new(),
                detail: None,
            });
    }
    state.router.metadata.refresh(&state.registry).await;
    let decision = state
        .router
        .plan(&state.registry, &request, policy, free_only);
    Ok(Json(json!({
        "strategy": policy.as_str(),
        "comparison_scope": decision.comparison_scope,
        "affinity_hit": decision.affinity_hit,
        "comparison_note": "按费用及额度可信度分层，层内按优先级；同计费范围优先低倍率，同级分散服务与账号。候选顺序为静态分层，实际分配还取决于容量与会话绑定。",
        "auto_allow_paid_fallback": state.router.gateway.settings().auto_allow_paid_fallback && !free_only,
        "reservation": "reservation_unavailable: upstream request pricing is not declared",
        "candidates": decision.candidates.iter().map(|candidate| json!({
            "backend": candidate.backend.id(),
            "backend_name": candidate.backend.name(),
            "model": candidate.model,
            "cost_tier": candidate.cost_tier.as_str(),
            "pool": candidate.pool(),
            "quota_known": candidate.quota_known(),
            "routing_rank": candidate.auto_rank(),
            "priority": candidate.priority,
            "latency_ms": candidate.latency_ms,
            "account_id": candidate.account_id,
            "price_factor": candidate.price_factor,
            "comparison_scope": candidate.comparison_scope,
            "quota": candidate.quota,
            "limits": candidate.limits,
        })).collect::<Vec<_>>(),
        "rejected": decision.rejected.iter().map(|item| json!({
            "model": item.model,
            "reason": item.reason,
        })).collect::<Vec<_>>(),
    })))
}

/// 刷新账号凭证。
pub async fn refresh_account(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    let account = state
        .store
        .get_account(&id)
        .await?
        .ok_or_else(|| AppError::NotFound {
            message: format!("account `{id}` not found"),
        })?;
    let refresher = state
        .registry
        .runtime(&account.backend_id)
        .and_then(|runtime| runtime.credential_refresh)
        .ok_or_else(|| {
            AppError::bad_request("enabled service does not support credential refresh")
        })?;
    match refresher.refresh(&account).await {
        Ok(updated) => {
            state.reload_backends().await?;
            Ok(Json(json!({"account": updated.id, "refreshed": true})))
        }
        Err(error) => Ok(Json(json!({
            "account": id,
            "refreshed": false,
            "code": error.code(),
            "message": error.message(),
        }))),
    }
}

/// 手动更新额度，复用服务端批处理并保存下一次十二小时更新时间。
pub async fn read_quota(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<crate::accounts::quota_refresh::View>, AppError> {
    authorize(&state, &headers)?;
    Ok(Json(
        crate::accounts::quota_refresh::refresh(&state, true, crate::records::now_seconds())
            .await?,
    ))
}

/// 控制台只读本地额度缓存，不因页面刷新重复访问厂商。
pub async fn cached_quota(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<crate::accounts::quota_refresh::View>, AppError> {
    authorize(&state, &headers)?;
    Ok(Json(crate::accounts::quota_refresh::view(&state).await?))
}

/// 导出配置。默认脱敏。
pub async fn export_config(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    let include_secrets = query
        .get("include_secrets")
        .map(|value| value == "true")
        .unwrap_or(false);
    let backends = state.store.list_backends().await?;
    let accounts = state.store.list_accounts().await?;
    let mut bundle = crate::config_bundle::ConfigBundle::build(backends, accounts, include_secrets);
    bundle.model_aliases = state.store.list_model_aliases().await?;
    bundle.gateway = Some(state.store.gateway_settings().await?);
    Ok(Json(serde_json::to_value(bundle).map_err(|error| {
        AppError::Internal {
            message: format!("serialize config failed: {error}"),
        }
    })?))
}

/// 导入配置。按 id 合并；空密钥保留原值。
pub async fn import_config(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(bundle): Json<crate::config_bundle::ConfigBundle>,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    bundle.validate()?;
    let imported_backends = bundle.backends.len();
    let imported_accounts = bundle.accounts.len();
    state.config_service().import_bundle(&bundle).await?;
    Ok(Json(json!({
        "backends": imported_backends,
        "accounts": imported_accounts,
    })))
}

/// 返回模型主调用名称与兼容的历史名称。
pub async fn list_model_aliases(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    Ok(Json(
        json!({"aliases": state.store.list_model_aliases().await?}),
    ))
}

/// 主名称编辑必须携带旧值，防止覆盖另一个页面的修改。
#[derive(Deserialize)]
pub struct ModelAliasInput {
    pub backend_id: String,
    pub model_id: String,
    pub alias: String,
    pub previous_alias: String,
}

/// 编辑调用名称并热加载映射；允许给停用服务编辑名称。
pub async fn rename_model_alias(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<ModelAliasInput>,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    let record = state
        .store
        .get_backend(&input.backend_id)
        .await?
        .ok_or_else(|| AppError::bad_request("模型所属服务已被删除"))?;
    let catalogs = state.store.list_model_catalogs().await?;
    let configured = !record.models.trim().is_empty();
    if configured
        && !record
            .models
            .split(',')
            .map(str::trim)
            .any(|id| id == input.model_id)
        || !configured
            && !catalogs
                .get(&record.id)
                .is_some_and(|models| models.iter().any(|model| model.id == input.model_id))
            && !state.store.list_model_aliases().await?.iter().any(|alias| {
                alias.backend_id == input.backend_id && alias.model_id == input.model_id
            })
    {
        return Err(AppError::bad_request("模型已从当前目录移除"));
    }
    state
        .config_service()
        .rename_alias(
            &input.backend_id,
            &input.model_id,
            &input.alias,
            &input.previous_alias,
        )
        .await?;
    Ok(Json(
        json!({"aliases": state.store.list_model_aliases().await?}),
    ))
}

/// 列出官方网关预设。
pub async fn list_presets(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    Ok(Json(json!({
        "presets": crate::presets::catalog(&state.store).await?.iter().map(|p| p.view()).collect::<Vec<_>>(),
    })))
}

/// 当日用量与预算。
pub async fn usage_report(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    let backends = state.store.list_backends().await?;
    let rows = state
        .usage
        .snapshot()
        .into_iter()
        .map(|(backend_id, used)| {
            let budget = backends
                .iter()
                .find(|record| record.id == backend_id)
                .map(|record| record.daily_token_budget)
                .unwrap_or(0);
            json!({
                "backend_id": backend_id,
                "day": crate::usage::today(),
                "used_tokens": used,
                "budget_tokens": budget,
                "over_budget": budget > 0 && used >= budget,
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({"usage": rows})))
}

/// 后端健康与延迟快照。
pub async fn backend_health(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    Ok(Json(json!({
        "backends": state.router.health.snapshot(),
        "sessions": state.router.affinity.len(),
    })))
}

/// 返回北京时间日期 YYYY-MM-DD，与自动签到周期保持一致。
fn today() -> String {
    crate::tasks::scheduler::day_at(crate::tasks::scheduler::now())
}

/// 管理鉴权。未配置 admin key 时视为本地单用户模式。
pub(crate) fn authorize(state: &AppState, headers: &HeaderMap) -> Result<(), AppError> {
    let provided = headers
        .get("x-admin-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .or_else(|| {
            headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "))
                .map(str::trim)
        });
    let principal = crate::application::access::Principal::authenticate(&state.config, provided)?;
    // v1 混合读写与秘密导出，限制为全权限，防止低权限密钥绕过 v2。
    for scope in [
        crate::application::access::Scope::Read,
        crate::application::access::Scope::ConfigWrite,
        crate::application::access::Scope::Operation,
        crate::application::access::Scope::SecretWrite,
    ] {
        principal.require(scope)?;
    }
    Ok(())
}

/// 密钥脱敏，只保留末四位。
fn mask_key(key: &str) -> String {
    if key.is_empty() {
        return String::new();
    }
    let length = key.chars().count();
    if length <= 4 {
        return "*".repeat(length);
    }
    let suffix = key.chars().skip(length - 4).collect::<String>();
    format!("****{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 短密钥必须完全遮蔽，不能泄露长度以外的信息。
    #[test]
    fn masks_short_key_fully() {
        assert_eq!(mask_key("abc"), "***");
        assert_eq!(mask_key(""), "");
    }

    /// 长密钥只保留末四位。
    #[test]
    fn masks_long_key_suffix() {
        assert_eq!(mask_key("sk-1234567890"), "****7890");
    }
}
