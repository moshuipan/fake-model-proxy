//! Anthropic Messages 入站协议。
//!
//! 请求归一成 [`ChatRequest`]，响应与流式事件按 Messages 规范重新编码。
//! 思考内容只透传，不伪造 signature。

use crate::core::error::AppError;
use crate::core::stream::{ToolCallAccumulator, resolve_finish_reason};
use crate::core::{
    ChatRequest, ContentPart, Event, FinishReason, Message, Protocol, Role, ToolCall, ToolChoice,
    ToolDef, Usage,
};
use crate::ingress::chat::authorize;
use crate::state::AppState;
use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::extract::rejection::BytesRejection;
use axum::http::HeaderMap;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 流式帧通道容量。
const STREAM_CHANNEL_CAPACITY: usize = 64;

/// 处理 `POST /v1/messages`。
pub async fn messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, MessagesError> {
    authorize(&state, &headers).map_err(MessagesError)?;
    let body = crate::ingress::buffered_body(&state, body).map_err(MessagesError)?;
    let payload: Value = serde_json::from_slice(&body)
        .map_err(|error| MessagesError(AppError::bad_request(format!("invalid JSON: {error}"))))?;
    crate::audit::payload(&payload);
    let session = crate::ingress::session_id(&headers);
    let request = parse_messages_request(&payload, session).map_err(MessagesError)?;
    let cancel = CancellationToken::new();
    let upstream_request = request.clone();

    if request.stream {
        let (events, address) =
            crate::ingress::start_generation(&state, upstream_request, cancel.clone())
                .await
                .map_err(MessagesError)?;
        let mut request = request;
        request.model = address;
        Ok(messages_stream(request, events, cancel))
    } else {
        let (events, address) =
            crate::ingress::start_generation(&state, upstream_request, cancel.clone())
                .await
                .map_err(MessagesError)?;
        let completed = collect_message(address, events)
            .await
            .map_err(MessagesError)?;
        Ok(Json(completed).into_response())
    }
}

/// Messages 错误包装，输出 Anthropic 错误形状。
pub struct MessagesError(pub AppError);

impl IntoResponse for MessagesError {
    /// 转成 `{"type":"error","error":{...}}`。
    fn into_response(self) -> Response {
        let status = self.0.status();
        let kind = match &self.0 {
            AppError::BadRequest { .. } => "invalid_request_error",
            AppError::PayloadTooLarge { .. } => "request_too_large",
            AppError::NotFound { .. } => "not_found_error",
            AppError::Upstream(error) if error.status == Some(429) => "rate_limit_error",
            AppError::Upstream(_) => "api_error",
            AppError::Internal { .. } => "api_error",
        };
        let mut response = (
            status,
            Json(json!({
                "type": "error",
                "error": {"type": kind, "message": self.0.message()},
            })),
        )
            .into_response();
        response.extensions_mut().insert(self.0);
        response
    }
}

/// 解析 Messages 请求。
pub fn parse_messages_request(
    body: &Value,
    session: Option<String>,
) -> Result<ChatRequest, AppError> {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let max_tokens = body
        .get("max_tokens")
        .and_then(Value::as_u64)
        .map(|value| value as u32);
    if max_tokens.is_none() {
        return Err(AppError::bad_request("max_tokens is required"));
    }
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let mut messages = Vec::new();
    if let Some(system) = body.get("system") {
        let text = extract_system_text(system)?;
        if !text.is_empty() {
            messages.push(Message::text(Role::System, text));
        }
    }
    let raw_messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| AppError::bad_request("messages must be an array"))?;
    for raw in raw_messages {
        messages.extend(parse_message(raw)?);
    }
    if messages.is_empty() {
        return Err(AppError::bad_request("messages must not be empty"));
    }
    let tools = body
        .get("tools")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(parse_tool).collect::<Vec<_>>())
        .unwrap_or_default();
    let tool_choice = body.get("tool_choice").and_then(parse_tool_choice);
    let stop = body
        .get("stop_sequences")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let reasoning_effort = body.get("thinking").and_then(parse_thinking);
    Ok(ChatRequest {
        protocol: Protocol::Messages,
        model,
        messages,
        tools,
        tool_choice,
        max_tokens,
        temperature: body.get("temperature").and_then(Value::as_f64),
        top_p: body.get("top_p").and_then(Value::as_f64),
        stop,
        reasoning_effort,
        stream,
        session,
    })
}

/// system 字段可以是字符串或文本块数组。
fn extract_system_text(value: &Value) -> Result<String, AppError> {
    match value {
        Value::String(text) => Ok(text.clone()),
        Value::Array(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("text")
                    && let Some(text) = block.get("text").and_then(Value::as_str)
                {
                    parts.push(text.to_string());
                }
            }
            Ok(parts.join("\n"))
        }
        _ => Err(AppError::bad_request(
            "system must be a string or an array of text blocks",
        )),
    }
}

/// 工具结果连续排列，正文保留相对次序并置于结果后，每个结果保留自己的调用 ID。
fn parse_message(raw: &Value) -> Result<Vec<Message>, AppError> {
    let role = match raw.get("role").and_then(Value::as_str) {
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        Some(other) => {
            return Err(AppError::bad_request(format!(
                "unsupported message role `{other}`"
            )));
        }
        None => return Err(AppError::bad_request("message.role is required")),
    };
    let mut content = Vec::new();
    let mut tool_calls = Vec::new();
    let mut messages = Vec::new();
    match raw.get("content") {
        Some(Value::String(text)) => content.push(ContentPart::text(text.clone())),
        Some(Value::Array(blocks)) => {
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let text = block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        content.push(ContentPart::text(text));
                    }
                    Some("image") => {
                        let url = parse_image_source(block.get("source"))?;
                        content.push(ContentPart::ImageUrl { url, detail: None });
                    }
                    Some("tool_use") => {
                        let id = block
                            .get("id")
                            .and_then(Value::as_str)
                            .ok_or_else(|| AppError::bad_request("tool_use requires id"))?;
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .ok_or_else(|| AppError::bad_request("tool_use requires name"))?;
                        let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                        tool_calls.push(crate::core::ToolCall {
                            id: id.to_string(),
                            name: name.to_string(),
                            arguments: input.to_string(),
                        });
                    }
                    Some("tool_result") => {
                        if role != Role::User {
                            return Err(AppError::bad_request(
                                "tool_result requires a user message",
                            ));
                        }
                        let id = block
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                AppError::bad_request("tool_result requires tool_use_id")
                            })?;
                        messages.push(Message {
                            role: Role::Tool,
                            content: parse_tool_result_content(block.get("content"))?,
                            tool_calls: Vec::new(),
                            tool_call_id: Some(id.to_string()),
                            name: None,
                        });
                    }
                    // thinking / redacted_thinking 不回传给上游，避免伪造签名。
                    Some("thinking") | Some("redacted_thinking") => {}
                    Some(other) => {
                        return Err(AppError::bad_request(format!(
                            "unsupported content block `{other}`"
                        )));
                    }
                    None => return Err(AppError::bad_request("content block requires type")),
                }
            }
        }
        Some(_) => {
            return Err(AppError::bad_request(
                "message.content must be a string or an array",
            ));
        }
        None => return Err(AppError::bad_request("message.content is required")),
    }
    if !content.is_empty() || !tool_calls.is_empty() || messages.is_empty() {
        messages.push(Message {
            role,
            content,
            tool_calls,
            tool_call_id: None,
            name: None,
        });
    }
    Ok(messages)
}

/// 解析图片来源，非法来源明确拒收，避免静默丢失截图。
fn parse_image_source(source: Option<&Value>) -> Result<String, AppError> {
    let source = source.ok_or_else(|| AppError::bad_request("image requires source"))?;
    match source.get("type").and_then(Value::as_str) {
        Some("base64") => {
            let media_type = source
                .get("media_type")
                .and_then(Value::as_str)
                .unwrap_or("image/png");
            let data = source
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| AppError::bad_request("base64 image requires data"))?;
            Ok(format!("data:{media_type};base64,{data}"))
        }
        Some("url") => source
            .get("url")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| AppError::bad_request("url image requires url")),
        _ => Err(AppError::bad_request("unsupported image source type")),
    }
}

/// 工具结果保留有序文本与图片，明确拒收无法表达的内容块。
fn parse_tool_result_content(value: Option<&Value>) -> Result<Vec<ContentPart>, AppError> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(vec![ContentPart::text(text.clone())]),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|block| match block.get("type").and_then(Value::as_str) {
                Some("text") => Ok(ContentPart::text(
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                )),
                Some("image") => Ok(ContentPart::ImageUrl {
                    url: parse_image_source(block.get("source"))?,
                    detail: None,
                }),
                Some(other) => Err(AppError::bad_request(format!(
                    "unsupported tool_result content block `{other}`"
                ))),
                None => Err(AppError::bad_request(
                    "tool_result content block requires type",
                )),
            })
            .collect(),
        Some(_) => Err(AppError::bad_request(
            "tool_result.content must be a string or an array",
        )),
    }
}

/// 解析工具定义。Messages 使用 input_schema。
fn parse_tool(raw: &Value) -> Option<ToolDef> {
    Some(ToolDef {
        name: raw.get("name").and_then(Value::as_str)?.to_string(),
        description: raw
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        parameters: raw
            .get("input_schema")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
    })
}

/// 解析 tool_choice：auto / any / tool。
fn parse_tool_choice(raw: &Value) -> Option<ToolChoice> {
    match raw.get("type").and_then(Value::as_str) {
        Some("auto") => Some(ToolChoice::Auto),
        Some("any") => Some(ToolChoice::Required),
        Some("none") => Some(ToolChoice::None),
        Some("tool") => raw
            .get("name")
            .and_then(Value::as_str)
            .map(|name| ToolChoice::Named(name.to_string())),
        _ => None,
    }
}

/// thinking.budget_tokens 映射为推理强度。
fn parse_thinking(value: &Value) -> Option<String> {
    match value.get("type").and_then(Value::as_str) {
        Some("enabled") => {
            let budget = value
                .get("budget_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            Some(
                match budget {
                    0..=4096 => "low",
                    4097..=16384 => "medium",
                    16385..=32768 => "high",
                    _ => "xhigh",
                }
                .to_string(),
            )
        }
        Some("adaptive") => Some("medium".to_string()),
        _ => None,
    }
}

/// 客户端断开时取消上游。
struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    /// 流被丢弃时触发取消。
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Anthropic 流式编码状态。
struct MessagesEncoder {
    message_id: String,
    model: String,
    /// 当前打开的 content block 索引。
    current_index: Option<u32>,
    current_kind: Option<BlockKind>,
    /// 下一个可用的 block 索引。
    next_index: u32,
    /// 工具之后的内容等调用完成再输出，保持顺序且不提前关闭交错参数。
    buffered: Vec<BufferedBlock>,
    tool_indices: BTreeSet<u32>,
}

/// 缓冲块按首次出现排序；同一工具的后续分片不创建新块。
enum BufferedBlock {
    Content { kind: BlockKind, text: String },
    Tool(u32),
}

/// 当前块类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Thinking,
    Tool,
}

impl MessagesEncoder {
    /// 新建编码器。
    fn new(model: String) -> Self {
        MessagesEncoder {
            message_id: format!("msg_{}", uuid::Uuid::new_v4().simple()),
            model,
            current_index: None,
            current_kind: None,
            next_index: 0,
            buffered: Vec::new(),
            tool_indices: BTreeSet::new(),
        }
    }

    /// 关闭当前块；没有打开的块时返回 None。
    fn close_block(&mut self) -> Option<SseEvent> {
        let index = self.current_index.take()?;
        self.current_kind = None;
        Some(sse(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": index}),
        ))
    }

    /// 打开一个新块并返回起始事件。
    fn open_block(&mut self, kind: BlockKind, payload: Value) -> SseEvent {
        let index = self.next_index;
        self.next_index += 1;
        self.current_index = Some(index);
        self.current_kind = Some(kind);
        sse(
            "content_block_start",
            json!({"type":"content_block_start","index":index,"content_block": payload}),
        )
    }

    /// 首个工具前实时发送文本，之后合并同类片段并保持内容块的首次顺序。
    async fn content(
        &mut self,
        sender: &mpsc::Sender<Result<crate::audit::SseFrame, std::convert::Infallible>>,
        kind: BlockKind,
        text: &str,
    ) -> Result<(), ()> {
        if !self.buffered.is_empty() {
            if let Some(BufferedBlock::Content {
                kind: previous,
                text: buffered,
            }) = self.buffered.last_mut()
                && *previous == kind
            {
                buffered.push_str(text);
            } else {
                self.buffered.push(BufferedBlock::Content {
                    kind,
                    text: text.into(),
                });
            }
            return Ok(());
        }
        if self.current_kind != Some(kind) {
            if let Some(close) = self.close_block() {
                sender.send(Ok(close.into())).await.map_err(|_| ())?;
            }
            let payload = if kind == BlockKind::Thinking {
                json!({"type":"thinking","thinking":""})
            } else {
                json!({"type":"text","text":""})
            };
            let open = self.open_block(kind, payload);
            sender.send(Ok(open.into())).await.map_err(|_| ())?;
        }
        let delta = if kind == BlockKind::Thinking {
            json!({"type":"thinking_delta","thinking":text})
        } else {
            json!({"type":"text_delta","text":text})
        };
        let frame = sse(
            "content_block_delta",
            json!({
                "type":"content_block_delta","index":self.current_index,"delta":delta
            }),
        );
        sender.send(Ok(frame.into())).await.map_err(|_| ())
    }

    /// 完整工具仅输出一次；工具之间的文本和思考保留首次出现的位置。
    async fn flush_buffered(
        &mut self,
        sender: &mpsc::Sender<Result<crate::audit::SseFrame, std::convert::Infallible>>,
        calls: &BTreeMap<u32, ToolCall>,
    ) -> Result<(), ()> {
        for block in std::mem::take(&mut self.buffered) {
            match block {
                BufferedBlock::Content { kind, text } => {
                    self.content(sender, kind, &text).await?;
                }
                BufferedBlock::Tool(index) => {
                    if let Some(close) = self.close_block() {
                        sender.send(Ok(close.into())).await.map_err(|_| ())?;
                    }
                    let call = &calls[&index];
                    let open = self.open_block(
                        BlockKind::Tool,
                        json!({
                            "type":"tool_use","id":call.id,"name":call.name,"input":{}
                        }),
                    );
                    sender.send(Ok(open.into())).await.map_err(|_| ())?;
                    let frame = sse(
                        "content_block_delta",
                        json!({
                            "type":"content_block_delta","index":self.current_index,
                            "delta":{"type":"input_json_delta","partial_json":call.arguments}
                        }),
                    );
                    sender.send(Ok(frame.into())).await.map_err(|_| ())?;
                }
            }
        }
        Ok(())
    }
}

/// 构造一个 SSE 事件。
fn sse(event: &str, payload: Value) -> SseEvent {
    SseEvent::default().event(event).data(payload.to_string())
}

/// 构造 Messages 流式响应。
fn messages_stream(
    request: ChatRequest,
    mut events: crate::core::stream::EventStream,
    cancel: CancellationToken,
) -> Response {
    let (sender, receiver) = mpsc::channel::<
        Result<crate::audit::SseFrame, std::convert::Infallible>,
    >(STREAM_CHANNEL_CAPACITY);
    let downstream_cancel = cancel.clone();
    let model = request.model.clone();
    let input_tokens = estimate_tokens(&request);
    tokio::spawn(async move {
        let _guard = CancelOnDrop(cancel);
        let mut encoder = MessagesEncoder::new(model);
        let mut accumulator = ToolCallAccumulator::new();
        let mut finish = None;
        let mut usage = Usage::default();

        // message_start 先发，usage 用本地估算填充。
        let start = sse(
            "message_start",
            json!({
                "type":"message_start",
                "message":{
                    "id": encoder.message_id,
                    "type":"message",
                    "role":"assistant",
                    "model": encoder.model,
                    "content":[],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage":{"input_tokens": input_tokens, "output_tokens": 0}
                }
            }),
        );
        if sender.send(Ok(start.into())).await.is_err() {
            return;
        }

        while let Some(item) = events.next().await {
            match item {
                Ok(Event::Text { text }) => {
                    if encoder
                        .content(&sender, BlockKind::Text, &text)
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(Event::Reasoning { text }) => {
                    if encoder
                        .content(&sender, BlockKind::Thinking, &text)
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(Event::ToolCall(delta)) => {
                    if encoder.tool_indices.insert(delta.index) {
                        encoder.buffered.push(BufferedBlock::Tool(delta.index));
                    }
                    accumulator.push(delta);
                }
                Ok(Event::Usage(value)) => usage = value,
                Ok(Event::Finish(reason)) => finish = Some(reason),
                Err(error) => {
                    let frame = sse(
                        "error",
                        json!({
                            "type":"error",
                            "error":{"type":"api_error","message": error.message()}
                        }),
                    );
                    let _ = sender.send(Ok(frame.into())).await;
                    return;
                }
            }
        }
        let calls = match accumulator.finish() {
            Ok(calls) => encoder.tool_indices.iter().copied().zip(calls).collect(),
            Err(message) => {
                let frame = sse(
                    "error",
                    json!({
                        "type":"error","error":{"type":"api_error","message":message}
                    }),
                );
                let _ = sender.send(Ok(frame.into())).await;
                return;
            }
        };
        if encoder.flush_buffered(&sender, &calls).await.is_err() {
            return;
        }
        if let Some(close) = encoder.close_block()
            && sender.send(Ok(close.into())).await.is_err()
        {
            return;
        }
        let reason = resolve_finish_reason(finish, !accumulator.is_empty());
        let delta = sse(
            "message_delta",
            json!({
                "type":"message_delta",
                "delta":{"stop_reason": anthropic_stop_reason(reason), "stop_sequence": null},
                "usage":{"output_tokens": usage.output_tokens.unwrap_or(0)}
            }),
        );
        if sender.send(Ok(delta.into())).await.is_err() {
            return;
        }
        let _ = sender
            .send(Ok(crate::audit::SseFrame::terminal(sse(
                "message_stop",
                json!({"type":"message_stop"}),
            ))))
            .await;
    });

    Sse::new(crate::core::stream::cancel_on_drop(
        crate::audit::sse_stream(receiver),
        downstream_cancel,
    ))
    .keep_alive(KeepAlive::default())
    .into_response()
}

/// 收集事件，构造非流式 Messages 响应。
async fn collect_message(
    model: String,
    mut events: crate::core::stream::EventStream,
) -> Result<Value, AppError> {
    let mut text = String::new();
    let mut thinking = String::new();
    let mut accumulator = ToolCallAccumulator::new();
    let mut finish = None;
    let mut usage = Usage::default();
    while let Some(item) = events.next().await {
        match item? {
            Event::Text { text: delta } => text.push_str(&delta),
            Event::Reasoning { text: delta } => thinking.push_str(&delta),
            Event::ToolCall(delta) => accumulator.push(delta),
            Event::Usage(value) => usage = value,
            Event::Finish(reason) => finish = Some(reason),
        }
    }
    let tool_calls = accumulator.finish().map_err(|error| {
        AppError::Upstream(crate::core::UpstreamError {
            status: None,
            code: "invalid_tool_call".to_string(),
            message: error,
            retryable: false,
            execution_unknown: false,
        })
    })?;
    let reason = resolve_finish_reason(finish, !tool_calls.is_empty());
    let mut content = Vec::new();
    if !thinking.is_empty() {
        // 没有官方签名，只回传思考文本，不生成伪 signature。
        content.push(json!({"type":"thinking","thinking": thinking}));
    }
    if !text.is_empty() {
        content.push(json!({"type":"text","text": text}));
    }
    for call in &tool_calls {
        let input: Value = serde_json::from_str(&call.arguments).unwrap_or_else(|_| json!({}));
        content.push(json!({
            "type":"tool_use",
            "id": call.id,
            "name": call.name,
            "input": input,
        }));
    }
    Ok(json!({
        "id": format!("msg_{}", uuid::Uuid::new_v4().simple()),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": anthropic_stop_reason(reason),
        "stop_sequence": null,
        "usage": {
            "input_tokens": usage.input_tokens.unwrap_or(0),
            "output_tokens": usage.output_tokens.unwrap_or(0),
        },
    }))
}

/// 终止原因映射到 Anthropic 语义。
fn anthropic_stop_reason(reason: FinishReason) -> &'static str {
    match reason {
        FinishReason::Stop => "end_turn",
        FinishReason::ToolCalls => "tool_use",
        FinishReason::Length => "max_tokens",
        FinishReason::ContentFilter => "refusal",
    }
}

/// 粗略估算输入 token，仅用于 message_start 的占位值。
fn estimate_tokens(request: &ChatRequest) -> u32 {
    let characters: usize = request
        .messages
        .iter()
        .map(|message| message.text_content().chars().count())
        .sum();
    (characters / 4).max(1) as u32
}

/// 当前时间戳，保留给后续 usage 计算扩展。
#[allow(dead_code)]
fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// system 字符串应前置为 system 消息。
    #[test]
    fn parses_system_string() {
        let body = json!({
            "model": "m",
            "max_tokens": 16,
            "system": "be brief",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let request = parse_messages_request(&body, None).expect("parse");
        assert_eq!(request.messages[0].role, Role::System);
        assert_eq!(request.messages[0].text_content(), "be brief");
    }

    /// tool_result 必须转成 tool 角色并保留调用 id。
    #[test]
    fn parses_tool_result() {
        let body = json!({
            "model": "m",
            "max_tokens": 16,
            "messages": [{
                "role": "user",
                "content": [{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}]
            }]
        });
        let request = parse_messages_request(&body, None).expect("parse");
        assert_eq!(request.messages[0].role, Role::Tool);
        assert_eq!(request.messages[0].tool_call_id.as_deref(), Some("toolu_1"));
    }

    /// assistant 的 tool_use 块要转成统一工具调用。
    #[test]
    fn parses_tool_use_block() {
        let body = json!({
            "model": "m",
            "max_tokens": 16,
            "messages": [{
                "role": "assistant",
                "content": [{"type":"tool_use","id":"toolu_1","name":"echo","input":{"text":"hi"}}]
            }]
        });
        let request = parse_messages_request(&body, None).expect("parse");
        assert_eq!(request.messages[0].tool_calls.len(), 1);
        assert_eq!(request.messages[0].tool_calls[0].name, "echo");
    }

    /// max_tokens 缺失必须拒绝。
    #[test]
    fn requires_max_tokens() {
        let body = json!({"model":"m","messages":[{"role":"user","content":"hi"}]});
        assert!(parse_messages_request(&body, None).is_err());
    }

    /// thinking 预算要映射为推理强度。
    #[test]
    fn maps_thinking_budget() {
        let body = json!({
            "model": "m",
            "max_tokens": 16,
            "thinking": {"type":"enabled","budget_tokens": 20000},
            "messages": [{"role": "user", "content": "hi"}]
        });
        let request = parse_messages_request(&body, None).expect("parse");
        assert_eq!(request.reasoning_effort.as_deref(), Some("high"));
    }

    /// any 与 tool 的选择策略映射。
    #[test]
    fn maps_tool_choice() {
        assert_eq!(
            parse_tool_choice(&json!({"type":"any"})),
            Some(ToolChoice::Required)
        );
        assert_eq!(
            parse_tool_choice(&json!({"type":"tool","name":"echo"})),
            Some(ToolChoice::Named("echo".into()))
        );
    }
}
