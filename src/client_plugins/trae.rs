//! TRAE 插件：尚无经过核实的第三方会话头，因此仅允许历史推断。

use super::{ClientKind, ClientPlugin, user_agent_is};
use axum::http::HeaderMap;

pub(super) struct Trae;

impl ClientPlugin for Trae {
    /// 标记 TRAE 客户端。
    fn kind(&self) -> ClientKind {
        ClientKind::Trae
    }

    /// 明确自报品牌的 UA 可选解析器；未携带品牌时使用专用 Base URL。
    fn recognizes(&self, headers: &HeaderMap) -> bool {
        user_agent_is(headers, &["Trae", "Trae-CN"])
            || (user_agent_is(headers, &["hertz"])
                && headers
                    .get("x-model-provider")
                    .is_some_and(|value| value == "custom_responses_compatible")
                && headers
                    .get("x-bridge-transport")
                    .is_some_and(|value| value == "aha"))
    }

    /// 内部 chat_session_id 尚未证明进入第三方请求，不猜测头名。
    fn native_session(&self, _headers: &HeaderMap) -> Option<String> {
        None
    }

    /// 历史连续性同时用于记录与账号调度，不透传厂商会话。
    fn infer_history(&self) -> bool {
        true
    }
}
