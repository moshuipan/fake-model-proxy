//! 归一历史的工具关联约束：调用批次之后必须连续提供完整且唯一的结果。

use super::{AppError, Message, Role};
use std::collections::HashSet;

/// 校验工具调用/结果的关联，不改写历史、补造结果或保存工具内容。
pub fn validate_tool_history(messages: &[Message]) -> Result<(), AppError> {
    let mut pending = HashSet::new();
    for (index, message) in messages.iter().enumerate() {
        if message.role == Role::Tool {
            if !message.tool_calls.is_empty() {
                return Err(invalid(index, "a tool result must not contain tool calls"));
            }
            let id = message
                .tool_call_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| invalid(index, "a tool result requires tool_call_id"))?;
            if !pending.remove(id) {
                return Err(invalid(
                    index,
                    "tool result is orphaned, duplicated, or does not match a pending call",
                ));
            }
            continue;
        }
        if !pending.is_empty() {
            return Err(invalid(
                index,
                "tool results must immediately follow their assistant call batch",
            ));
        }
        if message.tool_call_id.is_some() {
            return Err(invalid(
                index,
                "tool_call_id requires a tool result message",
            ));
        }
        if !message.tool_calls.is_empty() && message.role != Role::Assistant {
            return Err(invalid(index, "tool calls require an assistant message"));
        }
        for call in &message.tool_calls {
            if call.id.is_empty() {
                return Err(invalid(index, "a tool call requires a nonempty id"));
            }
            if !pending.insert(call.id.as_str()) {
                return Err(invalid(
                    index,
                    "tool call ids must be unique within a batch",
                ));
            }
        }
    }
    if !pending.is_empty() {
        return Err(invalid(messages.len(), "tool calls are missing results"));
    }
    Ok(())
}

/// 有界诊断只包含位置和结构原因，不回显调用 ID、参数或结果正文。
fn invalid(index: usize, reason: &str) -> AppError {
    AppError::BadRequest {
        code: "invalid_tool_history".into(),
        message: format!("invalid tool history at messages[{index}]: {reason}"),
    }
}
