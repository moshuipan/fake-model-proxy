//! 事件流辅助：类型别名与工具调用归并。

use crate::core::contract::{Event, FinishReason, ToolCall, ToolCallDelta};
use futures_util::Stream;
use std::collections::BTreeMap;
use std::pin::Pin;

/// 后端返回的事件流。错误表示上游在流中断开，需要向客户端透出。
pub type EventStream =
    Pin<Box<dyn Stream<Item = Result<Event, crate::core::error::AppError>> + Send>>;

/// 响应接收端被丢弃时立即取消，不依赖写流任务的下一次 send。
pub struct CancelOnDropStream<S> {
    stream: S,
    cancel: tokio_util::sync::CancellationToken,
}

/// 将 HTTP 响应体的生命周期绑定到上游取消信号。
pub fn cancel_on_drop<S>(
    stream: S,
    cancel: tokio_util::sync::CancellationToken,
) -> CancelOnDropStream<S> {
    CancelOnDropStream { stream, cancel }
}

impl<S: Stream + Unpin> Stream for CancelOnDropStream<S> {
    type Item = S::Item;
    /// 不更改事件内容、顺序与背压。
    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        Pin::new(&mut self.stream).poll_next(cx)
    }
}

impl<S> Drop for CancelOnDropStream<S> {
    /// 客户端关闭连接会唤醒正在等待上游事件的任务。
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// 按 index 累积工具调用分片。
///
/// 上游可能把 id、name、arguments 分散在多个 delta 里，也可能重复发送 name。
/// 这里保持出现顺序（BTreeMap 按 index 排序），并对重复片段做幂等拼接。
#[derive(Debug, Default)]
pub struct ToolCallAccumulator {
    calls: BTreeMap<u32, PartialToolCall>,
}

#[derive(Debug, Default)]
struct PartialToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl ToolCallAccumulator {
    /// 创建空累积器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 吸收一个工具调用分片。
    pub fn push(&mut self, delta: ToolCallDelta) {
        let entry = self.calls.entry(delta.index).or_default();
        if let Some(id) = delta.id
            && !id.is_empty()
        {
            entry.id = Some(id);
        }
        if let Some(name) = delta.name
            && !name.is_empty()
            && entry.name.as_deref() != Some(name.as_str())
        {
            entry.name = Some(name);
        }
        if let Some(arguments) = delta.arguments {
            entry.arguments.push_str(&arguments);
        }
    }

    /// 是否已收到任何分片。
    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    /// 借用已具备身份的调用分片，供编码器增量发送，避免每片复制全部参数。
    pub fn parts(&self, index: u32) -> Option<(&str, &str, &str)> {
        let call = self.calls.get(&index)?;
        Some((
            call.id.as_deref()?,
            call.name.as_deref()?,
            call.arguments.as_str(),
        ))
    }

    /// 归并成完整工具调用。缺失 id 或 name 的调用直接丢弃并返回错误说明，
    /// 避免把半截工具交给客户端执行。
    pub fn finish(&self) -> Result<Vec<ToolCall>, String> {
        let mut result = Vec::with_capacity(self.calls.len());
        for (index, call) in &self.calls {
            let id = call
                .id
                .clone()
                .ok_or_else(|| format!("tool call {index} missing id"))?;
            let name = call
                .name
                .clone()
                .ok_or_else(|| format!("tool call {index} missing name"))?;
            let arguments = if call.arguments.trim().is_empty() {
                "{}".to_string()
            } else {
                // 参数必须是合法 JSON，否则客户端会拿到不可执行的调用。
                serde_json::from_str::<serde_json::Value>(&call.arguments)
                    .map_err(|error| format!("tool call {index} has invalid arguments: {error}"))?;
                call.arguments.clone()
            };
            result.push(ToolCall {
                id,
                name,
                arguments,
            });
        }
        Ok(result)
    }
}

/// 保留截断与过滤终态，仅在正常结束或未声明时推断工具调用。
pub fn resolve_finish_reason(declared: Option<FinishReason>, has_tool_calls: bool) -> FinishReason {
    match declared {
        Some(FinishReason::Length) => FinishReason::Length,
        Some(FinishReason::ContentFilter) => FinishReason::ContentFilter,
        Some(FinishReason::ToolCalls) => FinishReason::ToolCalls,
        _ if has_tool_calls => FinishReason::ToolCalls,
        Some(FinishReason::Stop) => FinishReason::Stop,
        None => FinishReason::Stop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 分片归并应保留 id、name，并按顺序拼接参数。
    #[test]
    fn accumulates_fragmented_tool_call() {
        let mut accumulator = ToolCallAccumulator::new();
        accumulator.push(ToolCallDelta {
            index: 0,
            id: Some("call_1".into()),
            name: Some("echo".into()),
            arguments: Some("{\"text\":".into()),
        });
        accumulator.push(ToolCallDelta {
            index: 0,
            id: None,
            name: None,
            arguments: Some("\"hi\"}".into()),
        });
        let calls = accumulator.finish().expect("valid call");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "echo");
        assert_eq!(calls[0].arguments, "{\"text\":\"hi\"}");
    }

    /// 参数不是合法 JSON 时必须报错，不能把半截调用交给客户端。
    #[test]
    fn rejects_invalid_arguments() {
        let mut accumulator = ToolCallAccumulator::new();
        accumulator.push(ToolCallDelta {
            index: 0,
            id: Some("call_1".into()),
            name: Some("echo".into()),
            arguments: Some("{\"text\":".into()),
        });
        assert!(accumulator.finish().is_err());
    }

    /// 空参数应归一成空对象，兼容无参工具。
    #[test]
    fn empty_arguments_become_empty_object() {
        let mut accumulator = ToolCallAccumulator::new();
        accumulator.push(ToolCallDelta {
            index: 0,
            id: Some("call_1".into()),
            name: Some("ping".into()),
            arguments: None,
        });
        let calls = accumulator.finish().expect("valid call");
        assert_eq!(calls[0].arguments, "{}");
    }

    /// 有工具调用时终止原因必须是 tool_calls。
    #[test]
    fn tool_calls_override_finish_reason() {
        assert_eq!(
            resolve_finish_reason(Some(FinishReason::Stop), true),
            FinishReason::ToolCalls
        );
        assert_eq!(resolve_finish_reason(None, true), FinishReason::ToolCalls);
        assert_eq!(
            resolve_finish_reason(Some(FinishReason::Length), false),
            FinishReason::Length
        );
        assert_eq!(resolve_finish_reason(None, false), FinishReason::Stop);
    }
}
