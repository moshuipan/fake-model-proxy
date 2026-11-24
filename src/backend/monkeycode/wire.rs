//! MonkeyCode 模型协议选择、系统提示签名与 Responses 流转换。

use crate::backend::{minimax_code, openai_compat};
use crate::core::{
    AppError, ChatRequest, Event, FinishReason, Role, ToolCallDelta, ToolChoice, Usage,
};
use futures_util::StreamExt;
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 官方模型目录中的三个接口类型，不按模型名称猜测协议。
#[derive(Clone, Copy)]
pub(super) enum Protocol {
    Chat,
    Messages,
    Responses,
}

impl Protocol {
    /// 只接受官方明确的接口类型。
    pub(super) fn parse(value: &str) -> Result<Self, AppError> {
        match value {
            "openai_chat" => Ok(Self::Chat),
            "anthropic" => Ok(Self::Messages),
            "openai_responses" => Ok(Self::Responses),
            _ => Err(AppError::bad_request(
                "MonkeyCode 需要官方目录或显式 interface_type：openai_chat / anthropic / openai_responses",
            )),
        }
    }
}

/// 编码后读取实际系统提示再签名；缺少系统消息时使用透明的通用提示。
pub(super) fn encode(
    request: &ChatRequest,
    protocol: Protocol,
    secret: &str,
) -> Result<(&'static str, Value, String), AppError> {
    let mut request = request.clone();
    request.stream = true;
    if !request.messages.iter().any(|message| {
        message.role.normalized() == Role::System && !message.text_content().is_empty()
    }) {
        request.messages.insert(
            0,
            crate::core::Message::text(Role::System, "You are a helpful assistant."),
        );
    }
    let (path, body, prompt) = match protocol {
        Protocol::Chat => {
            let body = openai_compat::OpenAiCompatBackend::build_body(&request, &request.model);
            let prompt = body["messages"]
                .as_array()
                .and_then(|messages| messages.iter().find(|message| message["role"] == "system"))
                .and_then(|message| message["content"].as_str())
                .filter(|prompt| !prompt.is_empty())
                .ok_or_else(|| AppError::bad_request("MonkeyCode 系统消息必须是非空文本"))?
                .to_string();
            ("chat/completions", body, prompt)
        }
        Protocol::Messages => {
            let body = minimax_code::wire::body(&request)?;
            let prompt = body["system"].as_str().unwrap_or_default().to_string();
            ("messages", body, prompt)
        }
        Protocol::Responses => {
            let body = responses_body(&request)?;
            let prompt = body["instructions"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            ("responses", body, prompt)
        }
    };
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|_| AppError::bad_request("MonkeyCode 签名密钥无效"))?;
    mac.update(prompt.as_bytes());
    let signature = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok((path, body, format!("v1={signature}")))
}

/// Responses 保留对话、工具调用和工具结果；系统消息集中到 instructions。
fn responses_body(request: &ChatRequest) -> Result<Value, AppError> {
    let encoded = openai_compat::OpenAiCompatBackend::build_body(request, &request.model);
    let mut input = Vec::new();
    let mut system = Vec::new();
    for message in encoded["messages"].as_array().into_iter().flatten() {
        if message["role"] == "system" {
            system.push(
                message["content"]
                    .as_str()
                    .ok_or_else(|| AppError::bad_request("MonkeyCode 系统消息不支持图片"))?
                    .to_string(),
            );
            continue;
        }
        if message["role"] == "tool" {
            input.push(json!({"type":"function_call_output","call_id":message["tool_call_id"],"output":message["content"]}));
            continue;
        }
        let content = match message["content"].as_array() {
            Some(parts) => parts.iter().map(|part| {
                if part["type"] == "image_url" {
                    json!({"type":"input_image","image_url":part.pointer("/image_url/url"),"detail":part.pointer("/image_url/detail").and_then(Value::as_str).unwrap_or("auto")})
                } else {
                    json!({"type":if message["role"] == "assistant" { "output_text" } else { "input_text" },"text":part["text"]})
                }
            }).collect::<Vec<_>>(),
            None => vec![json!({"type":if message["role"] == "assistant" { "output_text" } else { "input_text" },"text":message["content"]})],
        };
        if message["content"]
            .as_str()
            .is_none_or(|text| !text.is_empty())
        {
            input.push(json!({"role":message["role"],"content":content}));
        }
        for call in message["tool_calls"].as_array().into_iter().flatten() {
            input.push(json!({"type":"function_call","call_id":call["id"],"name":call.pointer("/function/name"),"arguments":call.pointer("/function/arguments")}));
        }
    }
    let mut body = json!({"model":request.model,"instructions":system.join("\n"),"input":input,"stream":true,"store":false});
    for (key, value) in [
        ("max_output_tokens", request.max_tokens.map(|v| json!(v))),
        ("temperature", request.temperature.map(|v| json!(v))),
        ("top_p", request.top_p.map(|v| json!(v))),
    ] {
        if let Some(value) = value {
            body[key] = value;
        }
    }
    if !request.stop.is_empty() {
        return Err(AppError::bad_request(
            "MonkeyCode Responses 模型不支持 stop 参数",
        ));
    }
    if !request.tools.is_empty() {
        body["tools"] = json!(request.tools.iter().map(|tool| json!({"type":"function","name":tool.name,"description":tool.description,"parameters":tool.parameters})).collect::<Vec<_>>());
    }
    if let Some(choice) = &request.tool_choice {
        body["tool_choice"] = match choice {
            ToolChoice::Auto => json!("auto"),
            ToolChoice::None => json!("none"),
            ToolChoice::Required => json!("required"),
            ToolChoice::Named(name) => json!({"type":"function","name":name}),
        };
    }
    if let Some(effort) = &request.reasoning_effort {
        body["reasoning"] = json!({"effort":effort});
    }
    Ok(body)
}

/// 各协议共用归一事件契约，非流式入站由现有收集器汇总。
pub(super) fn spawn_pump(
    protocol: Protocol,
    response: reqwest::Response,
    sender: mpsc::Sender<Result<Event, AppError>>,
    cancel: CancellationToken,
) {
    match protocol {
        Protocol::Chat => {
            tokio::spawn(openai_compat::pump_sse(response, sender, cancel));
        }
        Protocol::Messages => {
            tokio::spawn(minimax_code::wire::pump(response, sender, cancel));
        }
        Protocol::Responses => {
            tokio::spawn(pump_responses(response, sender, cancel));
        }
    }
}

/// 记录已输出的工具参数，完成事件补齐未流出的尾部并检查一致性。
#[derive(Default)]
struct ToolState {
    index: u32,
    arguments: String,
}

/// Responses 事件按 item_id / output_index 区分工具，不混合并行参数。
#[derive(Default)]
struct ResponsesDecoder {
    tools: HashMap<u64, ToolState>,
    text_seen: bool,
    reasoning_seen: bool,
    finished: bool,
}

impl ResponsesDecoder {
    /// 将原生 Responses 事件转换为文本、思考、工具、用量和终止原因。
    fn event(&mut self, value: &Value) -> Result<Vec<Event>, AppError> {
        let mut out = Vec::new();
        let kind = value["type"].as_str().unwrap_or_default();
        match kind {
            "response.output_text.delta" => {
                self.text_seen = true;
                out.push(Event::Text {
                    text: value["delta"].as_str().unwrap_or_default().into(),
                });
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                self.reasoning_seen = true;
                out.push(Event::Reasoning {
                    text: value["delta"].as_str().unwrap_or_default().into(),
                });
            }
            "response.output_item.added" | "response.output_item.done"
                if value["item"]["type"] == "function_call" =>
            {
                let index = value["output_index"]
                    .as_u64()
                    .ok_or_else(|| super::error(None, "MonkeyCode Responses 工具缺少索引", true))?;
                self.tool(index, &value["item"], &mut out)?;
            }
            "response.function_call_arguments.delta" => {
                let index = value["output_index"]
                    .as_u64()
                    .ok_or_else(|| super::error(None, "MonkeyCode Responses 工具缺少索引", true))?;
                let tool = self
                    .tools
                    .get_mut(&index)
                    .ok_or_else(|| super::error(None, "MonkeyCode Responses 工具未开始", true))?;
                let delta = value["delta"]
                    .as_str()
                    .ok_or_else(|| super::error(None, "MonkeyCode Responses 参数分片无效", true))?;
                tool.arguments.push_str(delta);
                out.push(Event::ToolCall(ToolCallDelta {
                    index: tool.index,
                    id: None,
                    name: None,
                    arguments: Some(delta.into()),
                }));
            }
            "response.completed" | "response.incomplete" => {
                let response = &value["response"];
                for (index, item) in response["output"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    match item["type"].as_str() {
                        Some("function_call") => self.tool(index as u64, item, &mut out)?,
                        Some("message") if !self.text_seen => {
                            for part in item["content"].as_array().into_iter().flatten() {
                                if part["type"] == "output_text"
                                    && let Some(text) = part["text"].as_str()
                                {
                                    out.push(Event::Text { text: text.into() });
                                }
                            }
                        }
                        Some("reasoning") if !self.reasoning_seen => {
                            for part in item["summary"].as_array().into_iter().flatten() {
                                if let Some(text) = part["text"].as_str() {
                                    out.push(Event::Reasoning { text: text.into() });
                                }
                            }
                        }
                        _ => {}
                    }
                }
                let count = |key| {
                    response["usage"][key]
                        .as_u64()
                        .and_then(|v| u32::try_from(v).ok())
                };
                out.push(Event::Usage(Usage {
                    input_tokens: count("input_tokens"),
                    output_tokens: count("output_tokens"),
                    total_tokens: count("total_tokens"),
                }));
                let reason = if kind == "response.incomplete" {
                    match response["incomplete_details"]["reason"].as_str() {
                        Some("max_output_tokens") => FinishReason::Length,
                        Some("content_filter") => FinishReason::ContentFilter,
                        _ => {
                            return Err(super::error(
                                None,
                                "MonkeyCode Responses 未完整结束",
                                true,
                            ));
                        }
                    }
                } else if self.tools.is_empty() {
                    FinishReason::Stop
                } else {
                    FinishReason::ToolCalls
                };
                out.push(Event::Finish(reason));
                self.finished = true;
            }
            "response.failed" | "error" => {
                return Err(super::error(None, "MonkeyCode Responses 生成失败", true));
            }
            _ => {}
        }
        Ok(out)
    }

    /// 同一工具只输出一次名称和 ID，完整参数仅补充尚未发送的后缀。
    fn tool(&mut self, index: u64, item: &Value, out: &mut Vec<Event>) -> Result<(), AppError> {
        let next_index = u32::try_from(self.tools.len())
            .map_err(|_| super::error(None, "工具数量超出限制", true))?;
        let tool = self.tools.entry(index).or_insert_with(|| {
            out.push(Event::ToolCall(ToolCallDelta {
                index: next_index,
                id: item["call_id"].as_str().map(String::from),
                name: item["name"].as_str().map(String::from),
                arguments: None,
            }));
            ToolState {
                index: next_index,
                ..Default::default()
            }
        });
        if let Some(arguments) = item["arguments"].as_str().filter(|text| !text.is_empty()) {
            let tail = arguments.strip_prefix(&tool.arguments).ok_or_else(|| {
                super::error(None, "MonkeyCode Responses 工具参数与流分片不一致", true)
            })?;
            if !tail.is_empty() {
                out.push(Event::ToolCall(ToolCallDelta {
                    index: tool.index,
                    id: None,
                    name: None,
                    arguments: Some(tail.into()),
                }));
            }
            tool.arguments = arguments.into();
        }
        Ok(())
    }
}

/// 严格解析 SSE，缺少完成事件视为截断；客户端取消立即停止读取。
async fn pump_responses(
    response: reqwest::Response,
    sender: mpsc::Sender<Result<Event, AppError>>,
    cancel: CancellationToken,
) {
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();
    let mut data = Vec::<String>::new();
    let mut decoder = ResponsesDecoder::default();
    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => { let _ = sender.send(Err(AppError::bad_request("请求已取消"))).await; return; },
            chunk = stream.next() => chunk,
        };
        match chunk {
            Some(Ok(bytes)) => buffer.extend_from_slice(&bytes),
            Some(Err(_)) => {
                let _ = sender
                    .send(Err(super::error(
                        None,
                        "MonkeyCode Responses 流读取失败",
                        true,
                    )))
                    .await;
                return;
            }
            None => {
                if !decoder.finished {
                    let _ = sender
                        .send(Err(super::error(
                            None,
                            "MonkeyCode Responses 流被截断",
                            true,
                        )))
                        .await;
                }
                return;
            }
        }
        if buffer.len() > 8 * 1024 * 1024 {
            let _ = sender
                .send(Err(super::error(
                    None,
                    "MonkeyCode Responses 事件超过大小限制",
                    true,
                )))
                .await;
            return;
        }
        while let Some(end) = buffer.iter().position(|byte| *byte == b'\n') {
            let line = buffer.drain(..=end).collect::<Vec<_>>();
            let line = match std::str::from_utf8(&line) {
                Ok(line) => line.trim_end_matches(['\r', '\n']),
                Err(_) => {
                    let _ = sender
                        .send(Err(super::error(
                            None,
                            "MonkeyCode Responses SSE 编码无效",
                            true,
                        )))
                        .await;
                    return;
                }
            };
            if let Some(line) = line.strip_prefix("data:") {
                data.push(line.strip_prefix(' ').unwrap_or(line).into());
            } else if line.is_empty() && !data.is_empty() {
                let payload = data.join("\n");
                data.clear();
                if payload == "[DONE]" {
                    continue;
                }
                let result = serde_json::from_str::<Value>(&payload)
                    .map_err(|_| super::error(None, "MonkeyCode Responses 事件不是 JSON", true))
                    .and_then(|value| decoder.event(&value));
                match result {
                    Ok(events) => {
                        for event in events {
                            if tokio::select! { _ = cancel.cancelled() => true, result = sender.send(Ok(event)) => result.is_err() }
                            {
                                return;
                            }
                        }
                        if decoder.finished {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(Err(error)).await;
                        return;
                    }
                }
            }
        }
    }
}
