//! 模型列表入站协议。

use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use serde_json::{Value, json};

/// 处理 `GET /v1/models`，对外仅展示持久调用名称。
pub async fn list_models(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, crate::core::AppError> {
    // 模型列表同样要求鉴权，避免把后端目录暴露给任意本机进程。
    authorize(&state, &headers)?;
    let models = state.public_models().await?;
    let mut data = models.iter().map(|model| json!({
        "id": model.id, "object": "model", "created": 0, "owned_by": "fakemodelproxy",
        "display_name": model.display_name, "price_factor": model.price_factor,
        "original_price_factor": model.original_price_factor, "price_valid_until": model.price_valid_until,
        "capabilities": model.capabilities, "token_limits": model.token_limits,
    })).collect::<Vec<_>>();
    data.push(json!({
        "id": "auto", "object": "model", "created": 0, "owned_by": "fakemodelproxy",
        "display_name": "Auto · 自动额度池", "routing_strategy": "auto", "virtual": true,
        "token_limits": {},
        "routing_note": "能力与健康准入 → 费用及额度分层 → 配置优先级 → 同域倍率 → 同级负载分散；同会话保持绑定",
    }));
    Ok(Json(json!({
        "object": "list",
        "data": data,
    })))
}

/// 复用 chat 入口的鉴权规则。
fn authorize(state: &AppState, headers: &HeaderMap) -> Result<(), crate::core::AppError> {
    if !state.config.requires_api_key() {
        return Ok(());
    }
    let provided = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .or_else(|| {
            headers
                .get("x-api-key")
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
        });
    match provided {
        Some(value) if value == state.config.api_key => Ok(()),
        _ => Err(crate::core::AppError::BadRequest {
            code: "invalid_api_key".to_string(),
            message: "invalid api key".to_string(),
        }),
    }
}
