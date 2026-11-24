//! MiniMax Code 在 BYOK/custom-provider 请求中已有原生会话头。

use super::{ClientKind, ClientPlugin, session_header, user_agent_is};
use axum::http::HeaderMap;

pub(super) struct MiniMaxCode;

impl ClientPlugin for MiniMaxCode {
    /// 对齐客户端命名空间，和上游服务类型保持独立。
    fn kind(&self) -> ClientKind {
        ClientKind::MinimaxCode
    }

    /// 原生头足以启用插件，无需客户端增加 FMP 专用头。
    fn recognizes(&self, headers: &HeaderMap) -> bool {
        session_header(headers, "x-mavis-session-id").is_some()
            || user_agent_is(headers, &["MiniMaxCode", "MiniMax-Code", "MiniMaxAgent"])
    }

    /// agent-id 是代理身份，不能替代 session-id。
    fn native_session(&self, headers: &HeaderMap) -> Option<String> {
        session_header(headers, "x-mavis-session-id")
    }
}
