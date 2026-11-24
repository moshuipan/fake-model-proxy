//! 核心契约：请求/事件归一、错误映射与工具参数归并。

pub mod contract;
pub mod error;
pub mod history;
pub mod stream;

pub use contract::{
    Capabilities, ChatRequest, ContentPart, ContextTier, Event, FinishReason, Message, ModelInfo,
    Protocol, Role, TokenLimits, ToolCall, ToolCallDelta, ToolChoice, ToolDef, UpstreamError,
    Usage,
};
pub use error::AppError;
