//! 客户端会话插件：读取原生标识，或推断用于调度的历史连续性。
//! 推断组不改变请求正文或厂商侧会话身份。

mod history;
mod minimax_code;
mod trae;

pub use history::{HistoryIndex, HistorySnapshot, ReplyEvidence, ReplyFingerprint};

use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};

/// FMP 支持识别的客户端；专用入口可明确选择插件。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    Generic,
    Trae,
    MinimaxCode,
}

impl ClientKind {
    /// 返回持久化名称与会话键命名空间。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::Trae => "trae",
            Self::MinimaxCode => "minimax_code",
        }
    }
}

/// 明确表示字段来源，不将历史推断冒充客户端会话 ID。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionSource {
    FmpHeader,
    MavisHeader,
    HistoryNew,
    HistoryContinuation,
    HistoryToolAnchor,
    HistoryAmbiguous,
}

/// 明确 ID 只是客户端声明，不承担认证功能。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionConfidence {
    Explicit,
    Inferred,
}

/// 可复核的匹配规模，不保存正文、工具参数或其摘要。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEvidence {
    pub logical_messages: usize,
    pub matched_messages: usize,
    pub matched_tool_calls: usize,
    pub candidate_groups: usize,
    pub normalized_tool_ids: usize,
}

/// 审计分组依据。id 与记录的 session_id 相同，供 API 直接使用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLink {
    pub id: String,
    pub client: Option<ClientKind>,
    pub source: SessionSource,
    pub confidence: SessionConfidence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<SessionEvidence>,
}

impl SessionLink {
    /// 只有明确头可以进入现有路由与上游会话机制。
    pub fn routing_session(&self) -> Option<String> {
        (self.confidence == SessionConfidence::Explicit).then(|| self.id.clone())
    }
}

/// 请求入站时的识别结果，只保存白名单字段，不保存整个 HeaderMap。
#[derive(Debug, Clone, Default)]
pub struct Identification {
    pub client: Option<ClientKind>,
    pub session: Option<SessionLink>,
    pub infer_history: bool,
}

/// 每个插件只声明已核实的会话字段与允许的关联方式。
trait ClientPlugin: Sync {
    /// 返回插件身份。
    fn kind(&self) -> ClientKind;
    /// 根据原生会话字段或客户端自报 User-Agent 选择插件。
    fn recognizes(&self, headers: &HeaderMap) -> bool;
    /// 提取已核实的客户端原生会话 ID。
    fn native_session(&self, headers: &HeaderMap) -> Option<String>;
    /// 是否允许用完整历史与已交付输出推断记录关联。
    fn infer_history(&self) -> bool {
        false
    }
}

static PLUGINS: [&dyn ClientPlugin; 2] = [&minimax_code::MiniMaxCode, &trae::Trae];

/// 校验唯一、非空、有界且没有控制字符的会话头；不截断标识。
pub(crate) fn session_header(headers: &HeaderMap, name: &str) -> Option<String> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?.trim();
    if values.next().is_some()
        || value.is_empty()
        || value.len() > 256
        || !value.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return None;
    }
    Some(value.to_owned())
}

/// User-Agent 仅选择解析器，不能证明会话或用户身份。
pub(crate) fn user_agent_is(headers: &HeaderMap, names: &[&str]) -> bool {
    headers
        .get("user-agent")
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() <= 512)
        .is_some_and(|value| {
            value
                .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-')
                .any(|token| names.iter().any(|name| token.eq_ignore_ascii_case(name)))
        })
}

/// 专用地址优先选择插件；保留合法 FMP 头的兼容行为。
pub fn identify(headers: &HeaderMap, hint: Option<ClientKind>) -> Identification {
    let plugin = hint
        .and_then(|kind| PLUGINS.iter().copied().find(|plugin| plugin.kind() == kind))
        .or_else(|| {
            PLUGINS
                .iter()
                .copied()
                .find(|plugin| plugin.recognizes(headers))
        });
    let client = Some(plugin.map_or(ClientKind::Generic, ClientPlugin::kind));
    let session = if let Some(id) = session_header(headers, "x-fmp-session") {
        Some(SessionLink {
            client_session_id: Some(id.clone()),
            id,
            client,
            source: SessionSource::FmpHeader,
            confidence: SessionConfidence::Explicit,
            previous_request_id: None,
            evidence: None,
        })
    } else {
        plugin.and_then(|plugin| {
            plugin.native_session(headers).map(|native| SessionLink {
                id: format!("{}:{native}", plugin.kind().as_str()),
                client,
                source: SessionSource::MavisHeader,
                confidence: SessionConfidence::Explicit,
                client_session_id: Some(native),
                previous_request_id: None,
                evidence: None,
            })
        })
    };
    Identification {
        client,
        infer_history: session.is_none() && plugin.is_none_or(ClientPlugin::infer_history),
        session,
    }
}
