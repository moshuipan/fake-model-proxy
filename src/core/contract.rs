//! 统一请求/事件契约。
//!
//! 所有入站协议先归一成 [`ChatRequest`]，所有后端都把上游流解码成
//! [`Event`] 序列，再由入站协议编码回客户端。协议差异只允许出现在
//! ingress（解析/编码）与 backend（构造/解码）两侧。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 入站协议类型，仅用于错误格式与个别字段语义差异。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Chat,
    Messages,
    Responses,
}

impl Protocol {
    /// 返回协议名，用于日志与错误体。
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::Chat => "chat",
            Protocol::Messages => "messages",
            Protocol::Responses => "responses",
        }
    }
}

/// 消息角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

impl Role {
    /// 把 developer 归一为 system，其余保持原样。
    pub fn normalized(self) -> Role {
        match self {
            Role::Developer => Role::System,
            other => other,
        }
    }
}

/// 消息内容片段。文本与图片是首版支持的最小集合。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    ImageUrl {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

impl ContentPart {
    /// 构造文本片段。
    pub fn text(value: impl Into<String>) -> Self {
        ContentPart::Text { text: value.into() }
    }
}

/// 一次工具调用。arguments 保持上游原始 JSON 字符串，避免提前解析失败。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// 统一消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default)]
    pub content: Vec<ContentPart>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Message {
    /// 构造纯文本消息。供测试与后续协议入口复用。
    #[allow(dead_code)]
    pub fn text(role: Role, value: impl Into<String>) -> Self {
        Message {
            role,
            content: vec![ContentPart::text(value)],
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        }
    }

    /// 拼接该消息的全部文本片段，用于日志与本地估算。
    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(text.as_str()),
                ContentPart::ImageUrl { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// 是否包含图片片段。
    pub fn has_image(&self) -> bool {
        self.content
            .iter()
            .any(|part| matches!(part, ContentPart::ImageUrl { .. }))
    }
}

/// 客户端声明的工具定义。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// JSON Schema。上游可能拒绝未知关键字，由后端按需裁剪。
    #[serde(default)]
    pub parameters: Value,
}

/// 工具选择策略。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    /// 指定函数名。
    Named(String),
}

/// 归一后的生成请求。
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub protocol: Protocol,
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub tool_choice: Option<ToolChoice>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub stop: Vec<String>,
    /// 推理强度：minimal/low/medium/high/xhigh/max，由各协议映射。
    pub reasoning_effort: Option<String>,
    pub stream: bool,
    /// 明确厂商会话标识；推断 routing_group 保存在内部审计上下文。
    pub session: Option<String>,
}

impl ChatRequest {
    /// 是否声明了工具。
    pub fn has_tools(&self) -> bool {
        !self.tools.is_empty()
    }

    /// 是否包含图片输入。
    pub fn has_image(&self) -> bool {
        self.messages.iter().any(Message::has_image)
    }

    /// 返回最后一条用户消息的文本，供自动路由的轻量特征使用。
    #[allow(dead_code)]
    pub fn last_user_text(&self) -> Option<String> {
        self.messages
            .iter()
            .rev()
            .find(|message| matches!(message.role.normalized(), Role::User))
            .map(Message::text_content)
    }
}

/// 用量。未知字段保持 None，不用 0 冒充真实值。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u32>,
}

/// 终止原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
}

/// 工具参数分片。index 标识同一次响应内的第几个工具调用。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallDelta {
    pub index: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

/// 归一事件流。网关只消费这些事件，不感知上游 wire 格式。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    /// 正文增量。
    Text { text: String },
    /// 思考增量，与正文分离。
    Reasoning { text: String },
    /// 工具调用增量。
    ToolCall(ToolCallDelta),
    /// 用量信息，可能在流末尾出现。
    Usage(Usage),
    /// 终止原因，一次响应最多一个。
    Finish(FinishReason),
}

/// 后端标识的静态能力。未知能力一律视为不支持，避免误路由。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    pub tools: bool,
    pub parallel_tools: bool,
    pub images: bool,
    pub reasoning: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
}

impl Default for Capabilities {
    /// 保守默认：只保证纯文本。
    fn default() -> Self {
        Capabilities {
            tools: false,
            parallel_tools: false,
            images: false,
            reasoning: false,
            context_window: None,
        }
    }
}

/// 上游错误。`execution_unknown` 表示请求可能已在上游执行但结果不可知。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpstreamError {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub execution_unknown: bool,
}

impl UpstreamError {
    /// 一般传输失败不能证明生成未执行；默认禁止重放。
    pub fn transport(message: impl Into<String>) -> Self {
        UpstreamError {
            status: None,
            code: "upstream_transport_error".to_string(),
            message: message.into(),
            retryable: false,
            execution_unknown: true,
        }
    }

    /// 构造一个上游业务错误。
    pub fn status(status: u16, message: impl Into<String>) -> Self {
        UpstreamError {
            status: Some(status),
            code: "upstream_error".to_string(),
            message: message.into(),
            retryable: status == 429,
            execution_unknown: matches!(status, 408 | 500..=599),
        }
    }
}

/// 上游目录声明的上下文档位；tokens 保留原始数量，不把 K 当成 1024。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextTier {
    pub tokens: u32,
    #[serde(default)]
    pub is_default: bool,
}

/// 模型的 Token 限制；输入上限与总上下文窗口含义不同，缺失表示未知。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenLimits {
    /// 上游明确声明的总窗口，或上下文档位中的最大值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_tiers: Vec<ContextTier>,
}

/// 后端返回的模型描述。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// 上游声明的积分消耗倍率；缺失表示未知，不能视作免费。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_factor: Option<f64>,
    /// 上游提供的优惠前倍率，缺失时不推测原价。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_price_factor: Option<f64>,
    /// 当前报价有效至此 Unix 秒；活动到期后应重新同步。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_valid_until: Option<u64>,
    #[serde(default)]
    pub capabilities: Capabilities,
    /// 新字段兼容旧目录缓存，不根据模型名称推测限制。
    #[serde(default)]
    pub token_limits: TokenLimits,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 旧 JSON 目录没有限制字段时仍可加载，保留原窗口而不推测新增值。
    #[test]
    fn old_model_catalog_remains_compatible() {
        let model: ModelInfo = serde_json::from_value(serde_json::json!({
            "id":"old",
            "capabilities":{"tools":false,"parallel_tools":false,"images":false,
                "reasoning":false,"context_window":200000}
        }))
        .unwrap();
        assert_eq!(model.capabilities.context_window, Some(200000));
        assert_eq!(model.token_limits, TokenLimits::default());
    }

    /// developer 角色必须归一成 system，避免后端出现未知角色。
    #[test]
    fn developer_role_normalizes_to_system() {
        assert_eq!(Role::Developer.normalized(), Role::System);
        assert_eq!(Role::User.normalized(), Role::User);
    }

    /// 文本拼接只收集文本片段，忽略图片。
    #[test]
    fn text_content_skips_images() {
        let message = Message {
            role: Role::User,
            content: vec![
                ContentPart::text("hello"),
                ContentPart::ImageUrl {
                    url: "https://example.com/a.png".into(),
                    detail: None,
                },
                ContentPart::text(" world"),
            ],
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        };
        assert_eq!(message.text_content(), "hello world");
        assert!(message.has_image());
    }

    /// 未知用量保持 None，不能被写成 0。
    #[test]
    fn usage_keeps_unknown_as_none() {
        let usage = Usage::default();
        let encoded = serde_json::to_string(&usage).expect("serialize usage");
        assert_eq!(encoded, "{}");
    }

    /// 最后一条用户消息按逆序查找，支持工具结果之后继续提问。
    #[test]
    fn last_user_text_uses_latest_user_turn() {
        let request = ChatRequest {
            protocol: Protocol::Chat,
            model: "m".into(),
            messages: vec![
                Message::text(Role::User, "first"),
                Message::text(Role::Assistant, "reply"),
                Message::text(Role::User, "second"),
            ],
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop: Vec::new(),
            reasoning_effort: None,
            stream: false,
            session: None,
        };
        assert_eq!(request.last_user_text().as_deref(), Some("second"));
    }
}
