//! OpenAI Responses 入站协议。
//!
//! 首版只支持完整历史 input 的无状态请求；`previous_response_id`、
//! `conversation`、`background` 明确拒绝，不静默降级。

use crate::core::error::AppError;
use crate::core::stream::ToolCallAccumulator;
use crate::core::{
    ChatRequest, ContentPart, Event, FinishReason, Message, Protocol, Role, ToolChoice, ToolDef,
    Usage,
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
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 流式帧通道容量。
const STREAM_CHANNEL_CAPACITY: usize = 64;

/// 处理 `POST /v1/responses`。
pub async fn responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, AppError> {
    authorize(&state, &headers)?;
    let body = crate::ingress::buffered_body(&state, body)?;
    let payload: Value = serde_json::from_slice(&body)
        .map_err(|error| AppError::bad_request(format!("invalid JSON: {error}")))?;
    crate::audit::payload(&payload);
    reject_unsupported(&payload)?;
    let session = crate::ingress::session_id(&headers);
    let request = parse_responses_request(&payload, session)?;
    let cancel = CancellationToken::new();
    let upstream_request = request.clone();

    if request.stream {
        let (events, address) =
            crate::ingress::start_generation(&state, upstream_request, cancel.clone()).await?;
        let mut request = request;
        request.model = address;
        Ok(responses_stream(request, events, cancel))
    } else {
        let (events, address) =
            crate::ingress::start_generation(&state, upstream_request, cancel.clone()).await?;
        let mut request = request;
        request.model = address;
        let completed = collect_response(request, events).await?;
        Ok(Json(completed).into_response())
    }
}

/// 明确拒绝首版不支持的状态化能力。
fn reject_unsupported(body: &Value) -> Result<(), AppError> {
    if body.get("previous_response_id").is_some() {
        return Err(AppError::bad_request(
            "previous_response_id is not supported; send the full input history",
        ));
    }
    if body.get("conversation").is_some() {
        return Err(AppError::bad_request(
            "conversation is not supported; this API is stateless",
        ));
    }
    if body.get("background").and_then(Value::as_bool) == Some(true) {
        return Err(AppError::bad_request(
            "background responses are unsupported",
        ));
    }
    Ok(())
}

/// 解析 Responses 请求。
pub fn parse_responses_request(
    body: &Value,
    session: Option<String>,
) -> Result<ChatRequest, AppError> {
    crate::ingress::chat::validate_supported_constraints(body)?;
    if body
        .pointer("/text/format/type")
        .and_then(Value::as_str)
        .is_some_and(|format| format != "text")
    {
        return Err(AppError::BadRequest {
            code: "unsupported_request_constraint".into(),
            message: "FMP 当前不能保留 text.format 结构化输出约束，请使用原生 API".into(),
        });
    }
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let mut messages = Vec::new();
    if let Some(instructions) = body.get("instructions").and_then(Value::as_str)
        && !instructions.trim().is_empty()
    {
        messages.push(Message::text(Role::System, instructions));
    }
    match body.get("input") {
        Some(Value::String(text)) => messages.push(Message::text(Role::User, text.clone())),
        Some(Value::Array(items)) => {
            for item in items {
                parse_input_item(item, &mut messages)?;
            }
        }
        Some(_) => {
            return Err(AppError::bad_request(
                "input must be a string or an array of items",
            ));
        }
        None => return Err(AppError::bad_request("input is required")),
    }
    if messages.is_empty() {
        return Err(AppError::bad_request("input must not be empty"));
    }
    let tools = body
        .get("tools")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(parse_tool).collect::<Vec<_>>())
        .unwrap_or_default();
    let tool_choice = body.get("tool_choice").and_then(parse_tool_choice);
    let reasoning_effort = body
        .get("reasoning")
        .and_then(|value| value.get("effort"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    Ok(ChatRequest {
        protocol: Protocol::Responses,
        model,
        messages,
        tools,
        tool_choice,
        max_tokens: body
            .get("max_output_tokens")
            .and_then(Value::as_u64)
            .map(|value| value as u32),
        temperature: body.get("temperature").and_then(Value::as_f64),
        top_p: body.get("top_p").and_then(Value::as_f64),
        stop: Vec::new(),
        reasoning_effort,
        stream,
        session,
    })
}

/// 解析单个 input item。
fn parse_input_item(item: &Value, messages: &mut Vec<Message>) -> Result<(), AppError> {
    match item.get("type").and_then(Value::as_str) {
        Some("message") | None => {
            let role = match item.get("role").and_then(Value::as_str).unwrap_or("user") {
                "system" => Role::System,
                "developer" => Role::Developer,
                "assistant" => Role::Assistant,
                "user" => Role::User,
                other => {
                    return Err(AppError::bad_request(format!(
                        "unsupported input role `{other}`"
                    )));
                }
            };
            push_input_message(
                messages,
                Message {
                    role,
                    content: parse_input_content(item.get("content"))?,
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                    name: None,
                },
            );
        }
        Some("function_call") => {
            let call_id = item
                .get("call_id")
                .and_then(Value::as_str)
                .ok_or_else(|| AppError::bad_request("function_call requires call_id"))?;
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| AppError::bad_request("function_call requires name"))?;
            let arguments = item
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            push_input_message(
                messages,
                Message {
                    role: Role::Assistant,
                    content: Vec::new(),
                    tool_calls: vec![crate::core::ToolCall {
                        id: call_id.to_string(),
                        name: name.to_string(),
                        arguments: arguments.to_string(),
                    }],
                    tool_call_id: None,
                    name: None,
                },
            );
        }
        Some("function_call_output") => {
            let call_id = item
                .get("call_id")
                .and_then(Value::as_str)
                .ok_or_else(|| AppError::bad_request("function_call_output requires call_id"))?;
            messages.push(Message {
                role: Role::Tool,
                content: parse_function_output(item.get("output"))?,
                tool_calls: Vec::new(),
                tool_call_id: Some(call_id.to_string()),
                name: None,
            });
        }
        // reasoning item 不回传，避免伪造加密内容。
        Some("reasoning") => {}
        Some(other) => {
            return Err(AppError::bad_request(format!(
                "unsupported input item type `{other}`"
            )));
        }
    }
    Ok(())
}

/// 同一 Responses assistant 回合由多个 flat item 组成，合并正文和并行调用。
fn push_input_message(messages: &mut Vec<Message>, message: Message) {
    if message.role == Role::Assistant
        && let Some(previous) = messages.last_mut()
        && previous.role == Role::Assistant
    {
        previous.content.extend(message.content);
        previous.tool_calls.extend(message.tool_calls);
    } else {
        messages.push(message);
    }
}

/// 工具图文数组使用内容解析；普通 JSON 输出保持原有文本语义。
fn parse_function_output(value: Option<&Value>) -> Result<Vec<ContentPart>, AppError> {
    if let Some(Value::Array(parts)) = value
        && parts.iter().any(|part| {
            part.get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| {
                    kind.starts_with("input_") || matches!(kind, "text" | "output_text")
                })
        })
    {
        return parse_input_content(value);
    }
    let text = match value {
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    Ok(vec![ContentPart::text(text)])
}

/// 解析 input content：字符串或片段数组。
fn parse_input_content(value: Option<&Value>) -> Result<Vec<ContentPart>, AppError> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(vec![ContentPart::text(text.clone())]),
        Some(Value::Array(parts)) => {
            let mut result = Vec::new();
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("input_text") | Some("output_text") | Some("text") => {
                        result.push(ContentPart::text(
                            part.get("text").and_then(Value::as_str).unwrap_or_default(),
                        ));
                    }
                    Some("input_image") => {
                        let url =
                            part.get("image_url")
                                .and_then(Value::as_str)
                                .ok_or_else(|| {
                                    AppError::bad_request("input_image requires image_url")
                                })?;
                        result.push(ContentPart::ImageUrl {
                            url: url.to_string(),
                            detail: part
                                .get("detail")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned),
                        });
                    }
                    Some(other) => {
                        return Err(AppError::bad_request(format!(
                            "unsupported content part `{other}`"
                        )));
                    }
                    None => return Err(AppError::bad_request("content part requires type")),
                }
            }
            Ok(result)
        }
        Some(_) => Err(AppError::bad_request(
            "content must be a string or an array",
        )),
    }
}

/// Responses 的工具定义是扁平结构。
fn parse_tool(raw: &Value) -> Option<ToolDef> {
    let kind = raw
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("function");
    if kind != "function" {
        tracing::debug!(kind, "ignoring unsupported responses tool type");
        return None;
    }
    Some(ToolDef {
        name: raw.get("name").and_then(Value::as_str)?.to_string(),
        description: raw
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        parameters: raw
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
    })
}

/// 解析工具选择策略。
fn parse_tool_choice(raw: &Value) -> Option<ToolChoice> {
    match raw {
        Value::String(value) => match value.as_str() {
            "auto" => Some(ToolChoice::Auto),
            "none" => Some(ToolChoice::None),
            "required" => Some(ToolChoice::Required),
            _ => None,
        },
        Value::Object(object) => object
            .get("name")
            .and_then(Value::as_str)
            .map(|name| ToolChoice::Named(name.to_string())),
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

/// 输出快照及其发送进度；工具参数由累积器持有，终态时回填。
struct ResponseOutput {
    item: Value,
    upstream_index: Option<u32>,
    sent_bytes: usize,
    added: bool,
    done: bool,
}

/// 流式编码状态。上游工具索引与 Responses 输出索引分别保存。
struct ResponsesEncoder {
    response: Value,
    sequence_number: u64,
    output: Vec<ResponseOutput>,
    next_added: usize,
    message_item: Option<usize>,
    tool_indices: BTreeMap<u32, usize>,
    accumulator: ToolCallAccumulator,
}

impl ResponsesEncoder {
    /// 新建编码器。
    fn new(request: &ChatRequest) -> Self {
        ResponsesEncoder {
            response: response_base(request),
            sequence_number: 0,
            output: Vec::new(),
            next_added: 0,
            message_item: None,
            tool_indices: BTreeMap::new(),
            accumulator: ToolCallAccumulator::new(),
        }
    }

    /// 所有事件共享单调序号；客户端断开时立即停止发送。
    async fn emit(
        &mut self,
        sender: &mpsc::Sender<Result<crate::audit::SseFrame, std::convert::Infallible>>,
        event: &str,
        mut payload: Value,
    ) -> Result<(), ()> {
        payload["type"] = json!(event);
        payload["sequence_number"] = json!(self.sequence_number);
        self.sequence_number += 1;
        let frame = SseEvent::default().event(event).data(payload.to_string());
        let frame = if matches!(event, "response.completed" | "response.incomplete") {
            crate::audit::SseFrame::terminal(frame)
        } else {
            frame.into()
        };
        sender.send(Ok(frame)).await.map_err(|_| ())
    }

    /// 累积正文；工具之后再出现正文时创建新的 message。
    fn push_text(&mut self, text: &str) {
        let index = *self.message_item.get_or_insert_with(|| {
            let index = self.output.len();
            self.output.push(ResponseOutput {
                item: json!({
                    "type":"message","id":format!("msg_{}", uuid::Uuid::new_v4().simple()),
                    "role":"assistant","status":"in_progress",
                    "content":[{"type":"output_text","text":"","annotations":[],"logprobs":[]}]
                }),
                upstream_index: None,
                sent_bytes: 0,
                added: false,
                done: false,
            });
            index
        });
        if let Value::String(value) = &mut self.output[index].item["content"][0]["text"] {
            value.push_str(text);
        }
    }

    /// 关闭正文快照，发送由 flush 负责，确保先发 added 和全部 delta。
    fn close_message(&mut self, status: &str) {
        if let Some(index) = self.message_item.take() {
            self.output[index].item["status"] = json!(status);
        }
    }

    /// 每个上游工具索引只分配一次输出位置，不因交错分片创建新 item。
    fn push_tool(&mut self, delta: crate::core::ToolCallDelta) {
        self.close_message("completed");
        self.tool_indices.entry(delta.index).or_insert_with(|| {
            let index = self.output.len();
            self.output.push(ResponseOutput {
                item: json!({
                    "type":"function_call","id":format!("fc_{}", uuid::Uuid::new_v4().simple()),
                    "call_id":"","name":"","arguments":"","status":"in_progress"
                }),
                upstream_index: Some(delta.index),
                sent_bytes: 0,
                added: false,
                done: false,
            });
            index
        });
        self.accumulator.push(delta);
    }

    /// 按输出顺序发送已具备身份的 item，再补发缓冲的增量和结束事件。
    async fn flush(
        &mut self,
        sender: &mpsc::Sender<Result<crate::audit::SseFrame, std::convert::Infallible>>,
    ) -> Result<(), ()> {
        while self.next_added < self.output.len() {
            let index = self.next_added;
            let mut added = self.output[index].item.clone();
            if let Some(upstream) = self.output[index].upstream_index {
                let Some((call_id, name, _)) = self.accumulator.parts(upstream) else {
                    // 元数据晚到时保留位置，后续 item 不抢占它的输出顺序。
                    break;
                };
                added["call_id"] = json!(call_id);
                added["name"] = json!(name);
                self.output[index].item["call_id"] = added["call_id"].clone();
                self.output[index].item["name"] = added["name"].clone();
                added["arguments"] = json!("");
            } else {
                added["content"] = json!([]);
            }
            added["status"] = json!("in_progress");
            self.emit(
                sender,
                "response.output_item.added",
                json!({"output_index":index,"item":added}),
            )
            .await?;
            if self.output[index].upstream_index.is_none() {
                self.emit(
                    sender,
                    "response.content_part.added",
                    json!({
                        "item_id":self.output[index].item["id"],
                        "output_index":index,"content_index":0,
                        "part":{"type":"output_text","text":"","annotations":[],"logprobs":[]}
                    }),
                )
                .await?;
            }
            self.output[index].added = true;
            self.next_added += 1;
        }

        for index in 0..self.next_added {
            if self.output[index].done {
                continue;
            }
            let output = &self.output[index];
            let is_tool = output.upstream_index.is_some();
            let closing = output.item["status"] != "in_progress";
            let text = if let Some(upstream) = output.upstream_index {
                if closing {
                    output.item["arguments"].as_str().unwrap_or_default()
                } else {
                    self.accumulator
                        .parts(upstream)
                        .map(|(_, _, arguments)| arguments)
                        .unwrap_or_default()
                }
            } else {
                output.item["content"][0]["text"]
                    .as_str()
                    .unwrap_or_default()
            };
            // 无参工具的空白参数在终态统一为 {}，不提前发送无法保留的空白。
            let send_delta =
                text.len() > output.sent_bytes && (!is_tool || !text.trim().is_empty());
            if send_delta {
                let mut payload = json!({
                    "item_id":output.item["id"],"output_index":index,
                    "delta":&text[output.sent_bytes..]
                });
                let length = text.len();
                let event = if is_tool {
                    "response.function_call_arguments.delta"
                } else {
                    payload["content_index"] = json!(0);
                    payload["logprobs"] = json!([]);
                    "response.output_text.delta"
                };
                self.emit(sender, event, payload).await?;
                self.output[index].sent_bytes = length;
            }
            if closing {
                let item = self.output[index].item.clone();
                if is_tool {
                    self.emit(
                        sender,
                        "response.function_call_arguments.done",
                        json!({
                            "item_id":item["id"],"output_index":index,
                            "name":item["name"],"arguments":item["arguments"]
                        }),
                    )
                    .await?;
                } else {
                    self.emit(
                        sender,
                        "response.output_text.done",
                        json!({
                            "item_id":item["id"],"output_index":index,"content_index":0,
                            "text":item["content"][0]["text"],"logprobs":[]
                        }),
                    )
                    .await?;
                    self.emit(
                        sender,
                        "response.content_part.done",
                        json!({
                            "item_id":item["id"],"output_index":index,"content_index":0,
                            "part":item["content"][0]
                        }),
                    )
                    .await?;
                }
                self.emit(
                    sender,
                    "response.output_item.done",
                    json!({"output_index":index,"item":item}),
                )
                .await?;
                self.output[index].done = true;
            }
        }
        Ok(())
    }

    /// 校验完整工具并回填终态；残缺调用必须失败，不能标记为可执行。
    fn finalize(&mut self, status: &str) -> Result<(), String> {
        let calls = self.accumulator.finish()?;
        for (index, call) in self.tool_indices.values().copied().zip(calls) {
            let output = &mut self.output[index];
            if output.added
                && (output.item["call_id"] != call.id || output.item["name"] != call.name)
            {
                return Err("tool call identity changed during generation".to_string());
            }
            output.item["call_id"] = json!(call.id);
            output.item["name"] = json!(call.name);
            output.item["arguments"] = json!(call.arguments);
            output.item["status"] = json!(status);
        }
        self.close_message(status);
        Ok(())
    }

    /// 复用创建时的响应身份和元数据，附上所有已向客户端声明的输出。
    fn snapshot(&self, status: &str, usage: &Usage) -> Value {
        let mut response = self.response.clone();
        response["status"] = json!(status);
        response["output"] = Value::Array(
            self.output
                .iter()
                .filter(|output| output.added)
                .map(|output| {
                    let mut item = output.item.clone();
                    if item["status"] == "in_progress"
                        && let Some(upstream) = output.upstream_index
                        && let Some((_, _, arguments)) = self.accumulator.parts(upstream)
                    {
                        item["arguments"] = json!(arguments);
                    }
                    item
                })
                .collect(),
        );
        response["usage"] = response_usage(usage);
        response
    }
}

/// 构造 Responses 流式响应。
fn responses_stream(
    request: ChatRequest,
    mut events: crate::core::stream::EventStream,
    cancel: CancellationToken,
) -> Response {
    let (sender, receiver) = mpsc::channel::<
        Result<crate::audit::SseFrame, std::convert::Infallible>,
    >(STREAM_CHANNEL_CAPACITY);
    let downstream_cancel = cancel.clone();
    tokio::spawn(async move {
        let _guard = CancelOnDrop(cancel);
        let mut encoder = ResponsesEncoder::new(&request);
        let mut finish = None;
        let mut usage = Usage::default();

        for event in ["response.created", "response.in_progress"] {
            if encoder
                .emit(&sender, event, json!({"response":encoder.response}))
                .await
                .is_err()
            {
                return;
            }
        }

        while let Some(item) = events.next().await {
            match item {
                Ok(Event::Text { text }) => encoder.push_text(&text),
                Ok(Event::Reasoning { .. }) => {
                    // 首版不构造 reasoning item，避免伪造加密摘要。
                }
                Ok(Event::ToolCall(delta)) => encoder.push_tool(delta),
                Ok(Event::Usage(value)) => usage = value,
                Ok(Event::Finish(reason)) => finish = Some(reason),
                Err(error) => {
                    let mut response = encoder.snapshot("failed", &usage);
                    response["error"] = json!({"code":error.code(),"message":error.message()});
                    let _ = encoder
                        .emit(&sender, "response.failed", json!({"response":response}))
                        .await;
                    return;
                }
            }
            if encoder.flush(&sender).await.is_err() {
                return;
            }
        }

        let (status, incomplete_details) = response_status(finish);
        if let Err(message) = encoder.finalize(status) {
            let mut response = encoder.snapshot("failed", &usage);
            response["error"] = json!({"code":"invalid_tool_call","message":message});
            let _ = encoder
                .emit(&sender, "response.failed", json!({"response":response}))
                .await;
            return;
        }
        if encoder.flush(&sender).await.is_err() {
            return;
        }
        let mut response = encoder.snapshot(status, &usage);
        response["incomplete_details"] = incomplete_details;
        let event = if status == "completed" {
            "response.completed"
        } else {
            "response.incomplete"
        };
        let _ = encoder
            .emit(&sender, event, json!({"response":response}))
            .await;
    });

    Sse::new(crate::core::stream::cancel_on_drop(
        crate::audit::sse_stream(receiver),
        downstream_cancel,
    ))
    .keep_alive(KeepAlive::default())
    .into_response()
}

/// 收集事件，构造非流式响应。
async fn collect_response(
    request: ChatRequest,
    mut events: crate::core::stream::EventStream,
) -> Result<Value, AppError> {
    let mut encoder = ResponsesEncoder::new(&request);
    let mut finish = None;
    let mut usage = Usage::default();
    while let Some(item) = events.next().await {
        match item? {
            Event::Text { text } => encoder.push_text(&text),
            Event::Reasoning { .. } => {}
            Event::ToolCall(delta) => encoder.push_tool(delta),
            Event::Usage(value) => usage = value,
            Event::Finish(reason) => finish = Some(reason),
        }
    }
    let (status, incomplete_details) = response_status(finish);
    encoder.finalize(status).map_err(|error| {
        AppError::Upstream(crate::core::UpstreamError {
            status: None,
            code: "invalid_tool_call".to_string(),
            message: error,
            retryable: false,
            execution_unknown: false,
        })
    })?;
    for output in &mut encoder.output {
        output.added = output
            .upstream_index
            .is_none_or(|index| encoder.accumulator.parts(index).is_some());
    }
    let mut response = encoder.snapshot(status, &usage);
    response["incomplete_details"] = incomplete_details;
    Ok(response)
}

/// 构造响应公共元数据；流中所有生命周期事件复用同一身份和创建时间。
fn response_base(request: &ChatRequest) -> Value {
    json!({
        "id": format!("resp_{}", uuid::Uuid::new_v4().simple()),
        "object":"response",
        "created_at": now_seconds(),
        "status":"in_progress",
        "model":request.model,
        "output":[],
        "error":null,
        "incomplete_details":null,
        "parallel_tool_calls":true,
        "tool_choice":match &request.tool_choice {
            Some(ToolChoice::None) => json!("none"),
            Some(ToolChoice::Required) => json!("required"),
            Some(ToolChoice::Named(name)) => json!({"type":"function","name":name}),
            _ => json!("auto"),
        },
        "tools":request.tools.iter().map(|tool| json!({
            "type":"function","name":tool.name,"description":tool.description,
            "parameters":tool.parameters
        })).collect::<Vec<_>>(),
        "max_output_tokens":request.max_tokens,
        "temperature":request.temperature,
        "top_p":request.top_p,
        "usage":null
    })
}

/// 截断与过滤是 incomplete 终态，即使已经产生工具调用也不能宣称完成。
fn response_status(finish: Option<FinishReason>) -> (&'static str, Value) {
    match finish {
        Some(FinishReason::Length) => ("incomplete", json!({"reason":"max_output_tokens"})),
        Some(FinishReason::ContentFilter) => ("incomplete", json!({"reason":"content_filter"})),
        _ => ("completed", Value::Null),
    }
}

/// 构造 Responses 的用量字段。
fn response_usage(usage: &Usage) -> Value {
    json!({
        "input_tokens":usage.input_tokens.unwrap_or(0),
        "output_tokens":usage.output_tokens.unwrap_or(0),
        "total_tokens":usage.total_tokens.unwrap_or(0)
    })
}

/// 当前 Unix 秒。
fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 字符串 input 应转成单条用户消息。
    #[test]
    fn parses_string_input() {
        let body = json!({"model":"m","input":"hello"});
        let request = parse_responses_request(&body, None).expect("parse");
        assert_eq!(request.messages.len(), 1);
        assert_eq!(request.messages[0].text_content(), "hello");
    }

    /// instructions 应前置为 system 消息。
    #[test]
    fn parses_instructions_as_system() {
        let body = json!({"model":"m","instructions":"be brief","input":"hi"});
        let request = parse_responses_request(&body, None).expect("parse");
        assert_eq!(request.messages[0].role, Role::System);
    }

    /// function_call_output 应转成 tool 消息并保留 call_id。
    #[test]
    fn parses_function_call_output() {
        let body = json!({
            "model":"m",
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"go"}]},
                {"type":"function_call","call_id":"call_1","name":"echo","arguments":"{}"},
                {"type":"function_call_output","call_id":"call_1","output":"ok"}
            ]
        });
        let request = parse_responses_request(&body, None).expect("parse");
        assert_eq!(request.messages.len(), 3);
        assert_eq!(request.messages[2].role, Role::Tool);
        assert_eq!(request.messages[2].tool_call_id.as_deref(), Some("call_1"));
    }

    /// 状态化参数必须显式拒绝。
    #[test]
    fn rejects_stateful_fields() {
        assert!(reject_unsupported(&json!({"previous_response_id":"r1"})).is_err());
        assert!(reject_unsupported(&json!({"conversation":"c1"})).is_err());
        assert!(reject_unsupported(&json!({"background":true})).is_err());
        assert!(reject_unsupported(&json!({"store":false})).is_ok());
    }

    /// reasoning.effort 应映射为推理强度。
    #[test]
    fn maps_reasoning_effort() {
        let body = json!({"model":"m","input":"hi","reasoning":{"effort":"high"}});
        let request = parse_responses_request(&body, None).expect("parse");
        assert_eq!(request.reasoning_effort.as_deref(), Some("high"));
    }
}
