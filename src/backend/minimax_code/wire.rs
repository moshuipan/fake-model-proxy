//! Anthropic Messages 出站编码与严格 SSE 解码。

use crate::core::{
    AppError, ChatRequest, ContentPart, Event, FinishReason, Role, ToolCallDelta, ToolChoice,
    UpstreamError, Usage,
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 构造完整 Anthropic 消息，工具参数不合法时明确拒绝，不替换为 {}。
pub fn body(request: &ChatRequest) -> Result<Value, AppError> {
    let mut system = Vec::new();
    let mut messages: Vec<Value> = Vec::new();
    for message in &request.messages {
        if matches!(message.role.normalized(), Role::System) {
            if message.has_image() {
                return Err(AppError::bad_request("MiniMax system 消息不支持图片"));
            }
            system.push(message.text_content());
            continue;
        }
        let role = if message.role == Role::Assistant {
            "assistant"
        } else {
            "user"
        };
        let mut blocks = Vec::new();
        for part in &message.content {
            blocks.push(match part {
                ContentPart::Text { text } => json!({"type":"text","text":text}),
                ContentPart::ImageUrl { url, .. } => {
                    let source = if let Some(data) = url.strip_prefix("data:") {
                        let (media, data) = data
                            .split_once(";base64,")
                            .filter(|(media, data)| media.starts_with("image/") && !data.is_empty())
                            .ok_or_else(|| AppError::bad_request("MiniMax 图片 data URL 无效"))?;
                        json!({"type":"base64","media_type":media,"data":data})
                    } else {
                        json!({"type":"url","url":url})
                    };
                    json!({"type":"image","source":source})
                }
            });
        }
        if message.role == Role::Tool {
            let id = message
                .tool_call_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| AppError::bad_request("工具结果缺少 tool_call_id"))?;
            blocks = vec![json!({"type":"tool_result","tool_use_id":id,"content":blocks})];
        } else {
            for call in &message.tool_calls {
                let input: Value = serde_json::from_str(&call.arguments)
                    .map_err(|_| AppError::bad_request("工具调用参数不是有效 JSON"))?;
                if !input.is_object() || call.id.is_empty() || call.name.is_empty() {
                    return Err(AppError::bad_request(
                        "MiniMax 工具调用需要 id、name 和 JSON 对象参数",
                    ));
                }
                blocks.push(json!({"type":"tool_use","id":call.id,"name":call.name,"input":input}));
            }
        }
        if blocks.is_empty() {
            blocks.push(json!({"type":"text","text":""}));
        }
        if let Some(previous) = messages.last_mut()
            && previous["role"] == role
        {
            previous["content"]
                .as_array_mut()
                .expect("blocks")
                .extend(blocks);
        } else {
            messages.push(json!({"role":role,"content":blocks}));
        }
    }
    if messages.is_empty() || messages[0]["role"] != "user" {
        return Err(AppError::bad_request("MiniMax 对话需要以用户消息开始"));
    }
    let mut out = json!({
        "model":request.model,"messages":messages,"stream":true,
        "max_tokens":request.max_tokens.unwrap_or(8192)
    });
    if !system.is_empty() {
        out["system"] = json!(system.join("\n"));
    }
    if let Some(v) = request.temperature {
        out["temperature"] = json!(v);
    }
    if let Some(v) = request.top_p {
        out["top_p"] = json!(v);
    }
    if !request.stop.is_empty() {
        out["stop_sequences"] = json!(request.stop);
    }
    if !request.tools.is_empty() {
        out["tools"] =
            json!(request.tools.iter().map(|tool| json!({
            "name":tool.name,"description":tool.description,"input_schema":tool.parameters
        })).collect::<Vec<_>>());
    }
    if let Some(choice) = &request.tool_choice {
        out["tool_choice"] = match choice {
            ToolChoice::Auto => json!({"type":"auto"}),
            ToolChoice::None => json!({"type":"none"}),
            ToolChoice::Required => json!({"type":"any"}),
            ToolChoice::Named(name) => json!({"type":"tool","name":name}),
        };
    }
    if let Some(effort) = &request.reasoning_effort {
        if matches!(effort.as_str(), "none" | "minimal") {
            if request.model.contains("Flash") {
                return Err(AppError::bad_request(
                    "MiniMax Flash 模型不支持关闭 thinking",
                ));
            }
            out["thinking"] = json!({"type":"disabled"});
        } else if matches!(effort.as_str(), "low" | "medium" | "high" | "xhigh" | "max") {
            out["thinking"] = json!({"type":"adaptive"});
            out["output_config"] = json!({"effort":effort});
        } else {
            return Err(AppError::bad_request("MiniMax 推理强度无效"));
        }
    }
    Ok(out)
}

/// 一次响应的块索引与累计 usage，不把未知值补成零。
#[derive(Default)]
pub struct Decoder {
    tools: HashMap<u32, u32>,
    next_tool: u32,
    input: Option<u32>,
    read: Option<u32>,
    write: Option<u32>,
    output: Option<u32>,
    finish: Option<FinishReason>,
    pub stopped: bool,
}

impl Decoder {
    /// 更新服务端累计用量字段，缓存输入与普通输入保持可区分的计算口径。
    fn usage(&mut self, value: &Value) {
        for (target, key) in [
            (&mut self.input, "input_tokens"),
            (&mut self.read, "cache_read_input_tokens"),
            (&mut self.write, "cache_creation_input_tokens"),
            (&mut self.output, "output_tokens"),
        ] {
            if let Some(v) = value[key].as_u64().and_then(|v| u32::try_from(v).ok()) {
                *target = Some(v);
            }
        }
    }

    /// 单个 SSE JSON 事件转换为归一事件；坏帧、未知工具索引和重复终态不容忍。
    pub fn event(&mut self, value: &Value) -> Result<Vec<Event>, AppError> {
        if self.stopped {
            return Err(protocol_error("MiniMax 在结束后继续返回数据"));
        }
        let mut out = Vec::new();
        match value["type"].as_str() {
            Some("message_start") => self.usage(&value["message"]["usage"]),
            Some("content_block_start") if value["content_block"]["type"] == "tool_use" => {
                let index = index(value)?;
                let block = &value["content_block"];
                let id = block["id"].as_str().filter(|v| !v.is_empty());
                let name = block["name"].as_str().filter(|v| !v.is_empty());
                if self.tools.contains_key(&index) || id.is_none() || name.is_none() {
                    return Err(protocol_error("MiniMax 工具块格式无效"));
                }
                let tool_index = self.next_tool;
                self.next_tool += 1;
                self.tools.insert(index, tool_index);
                let input = block["input"]
                    .as_object()
                    .filter(|v| !v.is_empty())
                    .map(|_| block["input"].to_string());
                out.push(Event::ToolCall(ToolCallDelta {
                    index: tool_index,
                    id: id.map(String::from),
                    name: name.map(String::from),
                    arguments: input,
                }));
            }
            Some("content_block_start") => {
                let block = &value["content_block"];
                if let Some(text) = block["text"].as_str().filter(|v| !v.is_empty()) {
                    out.push(Event::Text { text: text.into() });
                }
            }
            Some("content_block_delta") => {
                let delta = &value["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => out.push(Event::Text {
                        text: required(delta, "text")?.into(),
                    }),
                    Some("thinking_delta") => out.push(Event::Reasoning {
                        text: required(delta, "thinking")?.into(),
                    }),
                    Some("input_json_delta") => {
                        let tool_index = self
                            .tools
                            .get(&index(value)?)
                            .copied()
                            .ok_or_else(|| protocol_error("MiniMax 返回未知工具参数块"))?;
                        out.push(Event::ToolCall(ToolCallDelta {
                            index: tool_index,
                            id: None,
                            name: None,
                            arguments: Some(required(delta, "partial_json")?.into()),
                        }));
                    }
                    Some("signature_delta") => {}
                    _ => return Err(protocol_error("MiniMax 返回未支持的内容增量")),
                }
            }
            Some("message_delta") => {
                self.usage(&value["usage"]);
                self.finish = match value["delta"]["stop_reason"].as_str() {
                    Some("end_turn" | "stop_sequence") => Some(FinishReason::Stop),
                    Some("tool_use") => Some(FinishReason::ToolCalls),
                    Some("max_tokens") => Some(FinishReason::Length),
                    Some("refusal") => Some(FinishReason::ContentFilter),
                    None => self.finish,
                    _ => return Err(protocol_error("MiniMax 返回未知终止原因")),
                };
            }
            Some("message_stop") => {
                let input = self
                    .input
                    .and_then(|n| n.checked_add(self.read.unwrap_or(0)))
                    .and_then(|n| n.checked_add(self.write.unwrap_or(0)));
                out.push(Event::Usage(Usage {
                    input_tokens: input,
                    output_tokens: self.output,
                    total_tokens: input.zip(self.output).and_then(|(a, b)| a.checked_add(b)),
                }));
                let finish = self
                    .finish
                    .ok_or_else(|| protocol_error("MiniMax 流缺少终止原因"))?;
                out.push(Event::Finish(finish));
                self.stopped = true;
            }
            Some("error") => return Err(protocol_error("MiniMax 上游返回流内错误")),
            Some("ping" | "content_block_stop") => {}
            _ => return Err(protocol_error("MiniMax SSE 事件格式无效")),
        }
        Ok(out)
    }
}

/// 读取必需字符串字段，不让损坏帧静默丢失内容。
fn required<'a>(value: &'a Value, key: &str) -> Result<&'a str, AppError> {
    value[key]
        .as_str()
        .ok_or_else(|| protocol_error("MiniMax SSE 字段无效"))
}

/// 读取块索引，拒绝截断或缺失的索引。
fn index(value: &Value) -> Result<u32, AppError> {
    value["index"]
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| protocol_error("MiniMax SSE 块索引无效"))
}

/// 协议错误不回显上游原文。
pub fn protocol_error(message: &str) -> AppError {
    AppError::Upstream(UpstreamError {
        status: Some(502),
        code: "minimax_protocol_error".into(),
        message: message.into(),
        retryable: false,
        execution_unknown: true,
    })
}

/// 支持 LF / CRLF、多行 data、分片 UTF-8，并仅在真实终态后完成。
pub async fn pump(
    response: reqwest::Response,
    sender: mpsc::Sender<Result<Event, AppError>>,
    cancel: CancellationToken,
) {
    let result = pump_inner(response, &sender, &cancel).await;
    if let Err(error) = result {
        let _ = sender.send(Err(error)).await;
    }
}

/// 上游及下游均受取消和关闭约束，帧缓冲不超过两 MiB。
async fn pump_inner(
    response: reqwest::Response,
    sender: &mpsc::Sender<Result<Event, AppError>>,
    cancel: &CancellationToken,
) -> Result<(), AppError> {
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();
    let mut decoder = Decoder::default();
    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            _ = sender.closed() => return Ok(()),
            item = tokio::time::timeout(std::time::Duration::from_secs(120), stream.next()) =>
                item.map_err(|_| protocol_error("MiniMax 流空闲超时"))?,
        };
        match chunk {
            Some(Ok(chunk)) => buffer.extend_from_slice(&chunk),
            Some(Err(_)) => return Err(protocol_error("MiniMax 响应流中断")),
            None => {
                if !decoder.stopped {
                    return Err(protocol_error("MiniMax 响应在终态前中断"));
                }
                return Ok(());
            }
        }
        loop {
            let lf = buffer.windows(2).position(|v| v == b"\n\n").map(|i| (i, 2));
            let crlf = buffer
                .windows(4)
                .position(|v| v == b"\r\n\r\n")
                .map(|i| (i, 4));
            let Some((end, sep)) = [lf, crlf].into_iter().flatten().min_by_key(|v| v.0) else {
                break;
            };
            let frame = String::from_utf8(buffer.drain(..end + sep).collect())
                .map_err(|_| protocol_error("MiniMax SSE 不是 UTF-8"))?;
            let data = frame
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(|v| v.strip_prefix(' ').unwrap_or(v))
                .collect::<Vec<_>>()
                .join("\n");
            if data.is_empty() {
                continue;
            }
            let value: Value =
                serde_json::from_str(&data).map_err(|_| protocol_error("MiniMax SSE JSON 损坏"))?;
            for event in decoder.event(&value)? {
                tokio::select! {
                    _ = cancel.cancelled() => return Ok(()),
                    result = sender.send(Ok(event)) => if result.is_err() { return Ok(()) },
                }
            }
            if decoder.stopped {
                return Ok(());
            }
        }
        if buffer.len() > 2 * 1024 * 1024 {
            return Err(protocol_error("MiniMax SSE 帧超过缓冲上限"));
        }
    }
}
