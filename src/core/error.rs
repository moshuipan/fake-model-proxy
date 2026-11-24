//! 统一错误类型与 HTTP 映射。
//!
//! 入站编码器需要稳定的错误形状；这里把内部错误收敛成有限的几类，
//! 避免每个 handler 自己拼装状态码。

use crate::core::contract::UpstreamError;
use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// 网关内部错误。
#[derive(Debug, Clone, PartialEq)]
pub enum AppError {
    /// 请求不合法，可直接告知客户端修正。
    BadRequest { code: String, message: String },
    /// 对话请求超过本地缓冲上限，不应重试相同请求或归因于上游。
    PayloadTooLarge { limit_bytes: usize },
    /// 资源不存在。
    NotFound { message: String },
    /// 上游失败。携带是否可重试与执行结果是否未知。
    Upstream(UpstreamError),
    /// 本地内部错误，通常不暴露细节。
    Internal { message: String },
}

impl AppError {
    /// 构造参数错误。
    pub fn bad_request(message: impl Into<String>) -> Self {
        AppError::BadRequest {
            code: "invalid_request_error".to_string(),
            message: message.into(),
        }
    }

    /// 返回对应的 HTTP 状态码。
    pub fn status(&self) -> StatusCode {
        match self {
            AppError::BadRequest { code, .. } => match code.as_str() {
                "invalid_admin_key" => StatusCode::UNAUTHORIZED,
                "forbidden" => StatusCode::FORBIDDEN,
                "revision_conflict" | "idempotency_conflict" | "operation_busy" => {
                    StatusCode::CONFLICT
                }
                "plan_expired" => StatusCode::GONE,
                _ => StatusCode::BAD_REQUEST,
            },
            AppError::PayloadTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            AppError::NotFound { .. } => StatusCode::NOT_FOUND,
            AppError::Upstream(error) => error
                .status
                .and_then(|status| StatusCode::from_u16(status).ok())
                .filter(|status| status.is_client_error() || status.is_server_error())
                .unwrap_or(StatusCode::BAD_GATEWAY),
            AppError::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// 返回错误码字符串。
    pub fn code(&self) -> String {
        match self {
            AppError::BadRequest { code, .. } => code.clone(),
            AppError::PayloadTooLarge { .. } => "request_body_too_large".into(),
            AppError::NotFound { .. } => "not_found".to_string(),
            AppError::Upstream(error) => error.code.clone(),
            AppError::Internal { .. } => "internal_error".to_string(),
        }
    }

    /// 返回可安全展示给客户端的错误消息。
    pub fn message(&self) -> String {
        match self {
            AppError::BadRequest { message, .. } => message.clone(),
            AppError::PayloadTooLarge { limit_bytes } => format!(
                "request body exceeds the configured limit of {limit_bytes} bytes; shorten the conversation or increase FMP_MAX_REQUEST_BODY_MB and restart"
            ),
            AppError::NotFound { message } => message.clone(),
            AppError::Upstream(error) => error.message.clone(),
            AppError::Internal { message } => message.clone(),
        }
    }
}

impl std::fmt::Display for AppError {
    /// 便于日志与 anyhow 转换的单行展示。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code(), self.message())
    }
}

impl std::error::Error for AppError {}

impl From<sqlx::Error> for AppError {
    /// 数据库错误统一转为内部错误，避免把 SQL 细节回显给客户端。
    fn from(error: sqlx::Error) -> Self {
        AppError::Internal {
            message: format!("database error: {error}"),
        }
    }
}

impl From<reqwest::Error> for AppError {
    /// 仅连接建立前失败可证明未执行；发送/读取超时保守标为未知。
    fn from(error: reqwest::Error) -> Self {
        let mut upstream = UpstreamError::transport(error.to_string());
        if error.is_connect() {
            upstream.retryable = true;
            upstream.execution_unknown = false;
        }
        AppError::Upstream(upstream)
    }
}

/// OpenAI 风格错误响应体。
pub fn error_body(error: &AppError) -> serde_json::Value {
    json!({
        "error": {
            "message": error.message(),
            "type": match error {
                AppError::BadRequest { .. } | AppError::PayloadTooLarge { .. } => "invalid_request_error",
                AppError::NotFound { .. } => "not_found_error",
                AppError::Upstream(_) => "upstream_error",
                AppError::Internal { .. } => "internal_error",
            },
            "code": error.code(),
            "retryable": match error {
                AppError::Upstream(upstream) => upstream.retryable && !upstream.execution_unknown,
                AppError::BadRequest { code, .. } => code == "revision_conflict",
                _ => false,
            },
            "hint": match error.code().as_str() {
                "revision_conflict" => "读取当前 revision 后重新生成计划",
                "secret_missing" => "先写入秘密或设置服务器环境变量，再引用返回的 secret_ref",
                "forbidden" => "使用具备所需 scope 的管理身份",
                "invalid_admin_key" => "提供有效的 X-Admin-Key 或 Bearer 密钥",
                "plan_expired" => "重新生成计划",
                "schema_invalid" => "读取 /api/v2/schema 和插件 config_schema，修正字段后重试",
                "execution_unknown" => "先核对上游结果，不自动重放",
                _ => "",
            },
        }
    })
}

impl IntoResponse for AppError {
    /// 统一转成 JSON 错误响应。
    fn into_response(self) -> Response {
        let status = self.status();
        let mut response = (status, Json(error_body(&self))).into_response();
        response.extensions_mut().insert(self);
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 上游 429 必须映射为 429，便于客户端理解限流。
    #[test]
    fn upstream_status_is_preserved() {
        let error = AppError::Upstream(UpstreamError::status(429, "slow down"));
        assert_eq!(error.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(error.code(), "upstream_error");
    }

    /// 缺失状态码的上游错误落到 502，而不是 500。
    #[test]
    fn transport_error_maps_to_bad_gateway() {
        let error = AppError::Upstream(UpstreamError::transport("connection reset"));
        assert_eq!(error.status(), StatusCode::BAD_GATEWAY);
    }
}
