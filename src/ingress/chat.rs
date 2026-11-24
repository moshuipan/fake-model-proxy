//! Chat Completions 入站协议。

use crate::core::error::AppError;
use crate::core::stream::{ToolCallAccumulator, resolve_finish_reason};
use crate::core::{
    ChatRequest, ContentPart, Event, FinishReason, Message, Protocol, Role, ToolCall, ToolChoice,
    ToolDef, Usage,
};
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
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 流式帧通道容量。
const STREAM_CHANNEL_CAPACITY: usize = 64;

/// 处理 `POST /v1/chat/completions`。
pub async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, AppError> {
    let body = crate::ingress::buffered_body(&state, body)?;
    chat_completions_inner(&state, headers, body).await
}

/// 解析请求并按流式选项编码响应。
async fn chat_completions_inner(
    state: &AppState,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    authorize(state, &headers)?;
    let payload: Value = serde_json::from_slice(&body).map_err(|error| {
        AppError::bad_request(format!("request body is not valid JSON: {error}"))
    })?;
    crate::audit::payload(&payload);
    let session = crate::ingress::session_id(&headers);
    let request = parse_chat_request(&payload, session)?;
    if request.model.is_empty() {
        return Err(AppError::bad_request("model is required"));
    }
    tracing::debug!(
        protocol = request.protocol.as_str(),
        model = %request.model,
        tools = request.has_tools(),
        images = request.has_image(),
        session = request.session.as_deref().unwrap_or(""),
        "chat request accepted"
    );
    let cancel = CancellationToken::new();
    let upstream_request = request.clone();
    let stream = request.stream;

    if stream {
        // 流式：先确保能拿到上游事件，再建立 SSE。
        let (events, address) =
            crate::ingress::start_generation(state, upstream_request, cancel.clone()).await?;
        let mut request = request;
        request.model = address;
        Ok(stream_response(request, events, cancel))
    } else {
        let (events, address) =
            crate::ingress::start_generation(state, upstream_request, cancel.clone()).await?;
        let completed = collect_completion(address.clone(), events).await?;
        Ok(Json(completed).into_response())
    }
}

/// 校验调用凭证。未配置 key 时只允许本地回环使用。
pub(crate) fn authorize(state: &AppState, headers: &HeaderMap) -> Result<(), AppError> {
    if !state.config.requires_api_key() {
        return Ok(());
    }
    let provided = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .or_else(|| {
            headers
                .get("x-api-key")
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
        });
    match provided {
        Some(value) if constant_time_eq(value, &state.config.api_key) => Ok(()),
        Some(_) => Err(AppError::BadRequest {
            code: "invalid_api_key".to_string(),
            message: "invalid api key".to_string(),
        }),
        None => Err(AppError::BadRequest {
            code: "missing_api_key".to_string(),
            message: "missing api key".to_string(),
        }),
    }
}

/// 定长比较，避免直接使用 `==` 造成时序差异。
fn constant_time_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// 解析 Chat Completions 请求体。
pub fn parse_chat_request(body: &Value, session: Option<String>) -> Result<ChatRequest, AppError> {
    validate_supported_constraints(body)?;
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let stream = match body.get("stream") {
        Some(Value::Bool(value)) => *value,
        None => false,
        Some(_) => return Err(AppError::bad_request("stream must be a boolean")),
    };
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| AppError::bad_request("messages must be an array"))?
        .iter()
        .map(parse_message)
        .collect::<Result<Vec<_>, _>>()?;
    if messages.is_empty() {
        return Err(AppError::bad_request("messages must not be empty"));
    }
    let tools = body
        .get("tools")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(parse_tool).collect::<Vec<_>>())
        .unwrap_or_default();
    let tool_choice = body.get("tool_choice").and_then(parse_tool_choice);
    let max_tokens = body
        .get("max_completion_tokens")
        .or_else(|| body.get("max_tokens"))
        .and_then(Value::as_u64)
        .map(|value| value as u32);
    let temperature = body.get("temperature").and_then(Value::as_f64);
    let top_p = body.get("top_p").and_then(Value::as_f64);
    let stop = parse_stop(body.get("stop"));
    let reasoning_effort = body
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    Ok(ChatRequest {
        protocol: Protocol::Chat,
        model,
        messages,
        tools,
        tool_choice,
        max_tokens,
        temperature,
        top_p,
        stop,
        reasoning_effort,
        stream,
        session,
    })
}

/// 当前统一协议没有这些约束的保真表示；显式拒绝，不能接收后静默删除。
pub(crate) fn validate_supported_constraints(body: &Value) -> Result<(), AppError> {
    let unsupported = ["parallel_tool_calls", "response_format"]
        .into_iter()
        .find(|key| body.get(*key).is_some_and(|value| !value.is_null()))
        .or_else(|| {
            body.get("n")
                .filter(|value| !value.is_null() && value.as_u64() != Some(1))
                .map(|_| "n")
        })
        .or_else(|| {
            body.get("tools")
                .and_then(Value::as_array)
                .filter(|tools| tools.iter().any(tool_requests_strict))
                .map(|_| "tools.function.strict")
        });
    if let Some(field) = unsupported {
        return Err(AppError::BadRequest {
            code: "unsupported_request_constraint".into(),
            message: format!("FMP 当前不能保留请求约束 {field}，请移除该字段或使用原生 API"),
        });
    }
    if let (Some(old), Some(new)) = (body.get("max_tokens"), body.get("max_completion_tokens"))
        && !old.is_null()
        && !new.is_null()
        && old != new
    {
        return Err(AppError::bad_request(
            "max_tokens 与 max_completion_tokens 不能提供不同值",
        ));
    }
    Ok(())
}

/// 判断工具定义是否要求严格的参数模式；`false` 与默认行为一致，可无损忽略。
fn tool_requests_strict(tool: &Value) -> bool {
    match tool
        .get("strict")
        .or_else(|| tool.pointer("/function/strict"))
    {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(_) => true,
    }
}

/// 解析 stop 字段：字符串或字符串数组。
fn parse_stop(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(text)) => vec![text.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// 解析单条消息。
fn parse_message(raw: &Value) -> Result<Message, AppError> {
    let role = match raw.get("role").and_then(Value::as_str) {
        Some("system") => Role::System,
        Some("developer") => Role::Developer,
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        Some("tool") => Role::Tool,
        Some(other) => {
            return Err(AppError::bad_request(format!(
                "unsupported message role `{other}`"
            )));
        }
        None => return Err(AppError::bad_request("message.role is required")),
    };
    let content = parse_content(raw.get("content"))?;
    let tool_calls = raw
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(parse_tool_call).collect::<Vec<_>>())
        .unwrap_or_default();
    let tool_call_id = raw
        .get("tool_call_id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let name = raw
        .get("name")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    Ok(Message {
        role,
        content,
        tool_calls,
        tool_call_id,
        name,
    })
}

/// 解析消息内容：字符串、片段数组或 null。
fn parse_content(value: Option<&Value>) -> Result<Vec<ContentPart>, AppError> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(vec![ContentPart::text(text.clone())]),
        Some(Value::Array(parts)) => {
            let mut result = Vec::with_capacity(parts.len());
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
                        result.push(ContentPart::text(text));
                    }
                    Some("image_url") => {
                        let url = part
                            .get("image_url")
                            .and_then(|value| {
                                value
                                    .get("url")
                                    .and_then(Value::as_str)
                                    .or_else(|| value.as_str())
                            })
                            .ok_or_else(|| {
                                AppError::bad_request("image_url part requires a url")
                            })?;
                        let detail = part
                            .get("image_url")
                            .and_then(|value| value.get("detail"))
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned);
                        result.push(ContentPart::ImageUrl {
                            url: url.to_string(),
                            detail,
                        });
                    }
                    // 未知片段类型直接拒绝，避免静默丢内容。
                    Some(other) => {
                        return Err(AppError::bad_request(format!(
                            "unsupported content part type `{other}`"
                        )));
                    }
                    None => return Err(AppError::bad_request("content part requires type")),
                }
            }
            Ok(result)
        }
        Some(_) => Err(AppError::bad_request(
            "message.content must be a string, an array or null",
        )),
    }
}

/// 解析工具调用（历史消息中使用）。
fn parse_tool_call(raw: &Value) -> Option<ToolCall> {
    let function = raw.get("function")?;
    Some(ToolCall {
        id: raw.get("id").and_then(Value::as_str)?.to_string(),
        name: function.get("name").and_then(Value::as_str)?.to_string(),
        arguments: function
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or("{}")
            .to_string(),
    })
}

/// 解析工具定义。非 function 类型忽略。
fn parse_tool(raw: &Value) -> Option<ToolDef> {
    let kind = raw
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("function");
    if kind != "function" {
        tracing::debug!(kind, "ignoring unsupported tool type");
        return None;
    }
    let function = raw.get("function")?;
    Some(ToolDef {
        name: function.get("name").and_then(Value::as_str)?.to_string(),
        description: function
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        parameters: function
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
            .get("function")
            .and_then(|function| function.get("name"))
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

/// 构造 SSE 响应。
fn stream_response(
    request: ChatRequest,
    mut events: crate::core::stream::EventStream,
    cancel: CancellationToken,
) -> Response {
    let (sender, receiver) = mpsc::channel(STREAM_CHANNEL_CAPACITY);
    let downstream_cancel = cancel.clone();
    let model = request.model.clone();
    tokio::spawn(async move {
        let _guard = CancelOnDrop(cancel);
        let completion_id = new_completion_id();
        let created = now_seconds();
        let mut accumulator = ToolCallAccumulator::new();
        let mut finish = None;
        let mut usage = None;

        if send_frame(&sender, &role_frame(&completion_id, created, &model))
            .await
            .is_err()
        {
            return;
        }
        while let Some(item) = events.next().await {
            match item {
                Ok(Event::Text { text }) => {
                    if send_frame(&sender, &text_frame(&completion_id, created, &model, &text))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(Event::Reasoning { text }) => {
                    if send_frame(
                        &sender,
                        &reasoning_frame(&completion_id, created, &model, &text),
                    )
                    .await
                    .is_err()
                    {
                        return;
                    }
                }
                Ok(Event::ToolCall(delta)) => {
                    accumulator.push(delta.clone());
                    if send_frame(
                        &sender,
                        &tool_frame(&completion_id, created, &model, &delta),
                    )
                    .await
                    .is_err()
                    {
                        return;
                    }
                }
                Ok(Event::Usage(value)) => usage = Some(value),
                Ok(Event::Finish(reason)) => finish = Some(reason),
                Err(error) => {
                    let _ = send_frame(&sender, &error_frame(&error)).await;
                    return;
                }
            }
        }
        let reason = resolve_finish_reason(finish, !accumulator.is_empty());
        if send_frame(
            &sender,
            &finish_frame(&completion_id, created, &model, reason),
        )
        .await
        .is_err()
        {
            return;
        }
        if let Some(usage) = usage
            && send_frame(
                &sender,
                &usage_frame(&completion_id, created, &model, usage),
            )
            .await
            .is_err()
        {
            return;
        }
        let _ = sender
            .send(Ok(crate::audit::SseFrame::terminal(
                SseEvent::default().data("[DONE]"),
            )))
            .await;
    });

    Sse::new(crate::core::stream::cancel_on_drop(
        crate::audit::sse_stream(receiver),
        downstream_cancel,
    ))
    .keep_alive(KeepAlive::default())
    .into_response()
}

/// 发送一个 SSE 帧。
async fn send_frame(
    sender: &mpsc::Sender<Result<crate::audit::SseFrame, std::convert::Infallible>>,
    payload: &Value,
) -> Result<(), ()> {
    sender
        .send(Ok(SseEvent::default().data(payload.to_string()).into()))
        .await
        .map_err(|_| ())
}

/// 流式首帧：声明 role。
fn role_frame(id: &str, created: u64, model: &str) -> Value {
    chunk(
        id,
        created,
        model,
        json!({"role": "assistant", "content": ""}),
        Value::Null,
    )
}

/// 流式正文帧。
fn text_frame(id: &str, created: u64, model: &str, text: &str) -> Value {
    chunk(id, created, model, json!({"content": text}), Value::Null)
}

/// 流式思考帧。
fn reasoning_frame(id: &str, created: u64, model: &str, text: &str) -> Value {
    chunk(
        id,
        created,
        model,
        json!({"reasoning_content": text}),
        Value::Null,
    )
}

/// 流式工具帧。
fn tool_frame(id: &str, created: u64, model: &str, delta: &crate::core::ToolCallDelta) -> Value {
    let mut call = json!({"index": delta.index});
    if let Some(value) = &delta.id {
        call["id"] = json!(value);
        call["type"] = json!("function");
    }
    let mut function = json!({});
    if let Some(name) = &delta.name {
        function["name"] = json!(name);
    }
    if let Some(arguments) = &delta.arguments {
        function["arguments"] = json!(arguments);
    }
    call["function"] = function;
    chunk(
        id,
        created,
        model,
        json!({"tool_calls": [call]}),
        Value::Null,
    )
}

/// 流式终止帧。
fn finish_frame(id: &str, created: u64, model: &str, reason: FinishReason) -> Value {
    chunk(
        id,
        created,
        model,
        json!({}),
        json!(finish_reason_str(reason)),
    )
}

/// 流式用量帧。
fn usage_frame(id: &str, created: u64, model: &str, usage: Usage) -> Value {
    let mut frame = chunk(id, created, model, json!({}), Value::Null);
    frame["usage"] = json!({
        "prompt_tokens": usage.input_tokens.unwrap_or(0),
        "completion_tokens": usage.output_tokens.unwrap_or(0),
        "total_tokens": usage.total_tokens.unwrap_or(0),
    });
    frame
}

/// 流内错误帧。不再发送 `[DONE]`，让客户端明确失败。
fn error_frame(error: &AppError) -> Value {
    crate::core::error::error_body(error)
}

/// 组装 chunk 外层结构。
fn chunk(id: &str, created: u64, model: &str, delta: Value, finish_reason: Value) -> Value {
    json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish_reason,
        }],
    })
}

/// 收集事件，构造非流式响应。
async fn collect_completion(
    model: String,
    mut events: crate::core::stream::EventStream,
) -> Result<Value, AppError> {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut accumulator = ToolCallAccumulator::new();
    let mut finish = None;
    let mut usage = Usage::default();
    while let Some(item) = events.next().await {
        match item? {
            Event::Text { text: delta } => text.push_str(&delta),
            Event::Reasoning { text: delta } => reasoning.push_str(&delta),
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
    let mut message = json!({"role": "assistant", "content": text});
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = json!(
            tool_calls
                .iter()
                .map(|call| json!({
                    "id": call.id,
                    "type": "function",
                    "function": {"name": call.name, "arguments": call.arguments},
                }))
                .collect::<Vec<_>>()
        );
    }
    Ok(json!({
        "id": new_completion_id(),
        "object": "chat.completion",
        "created": now_seconds(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason_str(reason),
        }],
        "usage": {
            "prompt_tokens": usage.input_tokens.unwrap_or(0),
            "completion_tokens": usage.output_tokens.unwrap_or(0),
            "total_tokens": usage.total_tokens.unwrap_or(0),
        },
    }))
}

/// 终止原因字符串。
fn finish_reason_str(reason: FinishReason) -> &'static str {
    match reason {
        FinishReason::Stop => "stop",
        FinishReason::ToolCalls => "tool_calls",
        FinishReason::Length => "length",
        FinishReason::ContentFilter => "content_filter",
    }
}

/// 生成 completion id。
fn new_completion_id() -> String {
    format!("chatcmpl-{}", uuid::Uuid::new_v4().simple())
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

    /// 字符串 content 应转成单个文本片段。
    #[test]
    fn parses_string_content() {
        let body = json!({
            "model": "demo/model-a",
            "messages": [{"role": "user", "content": "hello"}]
        });
        let request = parse_chat_request(&body, None).expect("parse");
        assert_eq!(request.messages.len(), 1);
        assert_eq!(request.messages[0].text_content(), "hello");
        assert!(!request.stream);
    }

    /// 多模态片段数组要保留图片与文本顺序。
    #[test]
    fn parses_multimodal_content() {
        let body = json!({
            "model": "demo/model-a",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "describe"},
                    {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}}
                ]
            }]
        });
        let request = parse_chat_request(&body, None).expect("parse");
        assert!(request.has_image());
        assert_eq!(request.messages[0].content.len(), 2);
    }

    /// 未声明的片段类型必须报错，不能静默丢弃。
    #[test]
    fn rejects_unknown_content_part() {
        let body = json!({
            "model": "demo/model-a",
            "messages": [{"role": "user", "content": [{"type": "audio", "data": "x"}]}]
        });
        assert!(parse_chat_request(&body, None).is_err());
    }

    /// tool_choice 指定函数时要保留函数名。
    #[test]
    fn parses_named_tool_choice() {
        let body = json!({
            "model": "demo/model-a",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"type": "function", "function": {"name": "echo", "parameters": {}}}],
            "tool_choice": {"type": "function", "function": {"name": "echo"}}
        });
        let request = parse_chat_request(&body, None).expect("parse");
        assert_eq!(request.tools.len(), 1);
        assert_eq!(request.tool_choice, Some(ToolChoice::Named("echo".into())));
    }

    /// 工具声明 `strict: false` 与默认行为一致，不能因此拒绝请求。
    #[test]
    fn accepts_non_strict_tool_definition() {
        let body = json!({
            "model": "demo/model-a",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [
                {"type": "function", "function": {"name": "echo", "parameters": {}, "strict": false}},
                {"type": "function", "name": "flat", "parameters": {}, "strict": false}
            ]
        });
        let request = parse_chat_request(&body, None).expect("parse");
        assert_eq!(request.tools.len(), 1);
    }

    /// 工具声明 `strict: true` 无法保真，必须显式拒绝。
    #[test]
    fn rejects_strict_tool_definition() {
        let body = json!({
            "model": "demo/model-a",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"type": "function", "name": "flat", "parameters": {}, "strict": true}]
        });
        assert!(parse_chat_request(&body, None).is_err());
    }

    /// stream 必须是布尔值。
    #[test]
    fn rejects_non_boolean_stream() {
        let body = json!({
            "model": "demo/model-a",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": "yes"
        });
        assert!(parse_chat_request(&body, None).is_err());
    }

    /// 空消息数组应被拒绝。
    #[test]
    fn rejects_empty_messages() {
        let body = json!({"model": "demo/model-a", "messages": []});
        assert!(parse_chat_request(&body, None).is_err());
    }

    /// 定长比较应只接受完全相同的值。
    #[test]
    fn constant_time_compare() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
    }
}
