//! 个人版原生消息与 SSE；工具由调用者执行，不启动服务端代理循环。

use super::catalog::Model;
use crate::core::{
    AppError, ChatRequest, Event, FinishReason, Role, ToolCallDelta, ToolChoice, Usage,
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const LIMIT: usize = 8 * 1024 * 1024;

/// 保留结构化工具历史；schema 与 tool_choice 使用上游要求的字符串。
pub fn body(request: &ChatRequest, model: &Model) -> Result<Value, AppError> {
    if request.has_image() {
        return Err(AppError::bad_request("TRAE 个人 raw-chat 暂不支持图片输入"));
    }
    if request.reasoning_effort.is_some() {
        return Err(AppError::bad_request(
            "TRAE 个人版暂未验证 reasoning_effort 控制",
        ));
    }
    if request.has_tools() && !model.info.capabilities.tools {
        return Err(AppError::bad_request("该 TRAE 个人模型未声明原生工具支持"));
    }
    let max = request
        .max_tokens
        .or(model.info.token_limits.max_output_tokens)
        .unwrap_or(4096);
    if model
        .info
        .token_limits
        .max_output_tokens
        .is_some_and(|limit| max > limit)
    {
        return Err(AppError::bad_request("请求输出上限超过 TRAE 个人模型限制"));
    }
    let mut messages = Vec::new();
    for message in &request.messages {
        let mut item = json!({"role":message.role.normalized(),"content":message.content});
        if !message.tool_calls.is_empty() {
            item["tool_calls"] = json!(
                message
                    .tool_calls
                    .iter()
                    .map(|call| json!({
                        "id":call.id,"type":"function",
                        "function_call":{"name":call.name,"arguments":call.arguments}
                    }))
                    .collect::<Vec<_>>()
            );
        }
        if message.role == Role::Tool {
            item["tool_call_id"] = json!(
                message
                    .tool_call_id
                    .as_ref()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| AppError::bad_request("TRAE 工具结果缺少 tool_call_id"))?
            );
        }
        if let Some(name) = &message.name {
            item["name"] = json!(name);
        }
        messages.push(item);
    }
    let session = request
        .session
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let mut value = json!({
        "config_name":model.config,"model_name":model.info.id,"config_source":model.source,
        "raw_chat_function":"chat_v3","session_id":session,"conversation_id":session,
        "stream":true,"max_tokens":max,"messages":messages
    });
    for (key, parameter) in [
        ("temperature", request.temperature),
        ("top_p", request.top_p),
    ] {
        if let Some(parameter) = parameter {
            value[key] = json!(parameter);
        }
    }
    if !request.stop.is_empty() {
        value["stop"] = json!(request.stop);
    }
    if !request.tools.is_empty() {
        value["tools"] =
            json!(request.tools.iter().map(|tool| json!({
            "type":"function","function":{"name":tool.name,"description":tool.description,
            "parameters":tool.parameters.to_string()}
        })).collect::<Vec<_>>());
    }
    if let Some(choice) = &request.tool_choice {
        value["tool_choice"] = json!(match choice {
            ToolChoice::Auto => "auto",
            ToolChoice::None => "none",
            ToolChoice::Required => "required",
            ToolChoice::Named(name) => name.as_str(),
        });
    }
    Ok(value)
}

/// 名称和 ID 是快照；参数分片与完整参数通过前缀验证合并。
#[derive(Default)]
struct Tool {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

/// 同一次原生生成的事件状态。
#[derive(Default)]
pub(crate) struct Decoder {
    tools: BTreeMap<u32, Tool>,
    pub(crate) finished: bool,
}

impl Decoder {
    /// 未知辅助事件忽略，错误与终态必须明确透出。
    pub(crate) fn event(&mut self, kind: &str, data: &str) -> Result<Vec<Event>, AppError> {
        if self.finished
            || !matches!(
                kind,
                "output" | "done" | "token_usage" | "error" | "internal_error"
            )
        {
            return Ok(Vec::new());
        }
        if matches!(kind, "error" | "internal_error") {
            return Err(super::error(None, "TRAE 个人模型生成失败", true));
        }
        let value: Value = serde_json::from_str(data)
            .map_err(|_| super::error(None, "TRAE SSE 事件不是有效 JSON", true))?;
        let mut events = Vec::new();
        if matches!(kind, "output" | "done") {
            if kind == "output" {
                if let Some(text) = value["response"].as_str().filter(|text| !text.is_empty()) {
                    events.push(Event::Text { text: text.into() });
                }
                if let Some(text) = value["reasoning_content"]
                    .as_str()
                    .filter(|text| !text.is_empty())
                {
                    events.push(Event::Reasoning { text: text.into() });
                }
            }
            for call in value["tool_calls"].as_array().into_iter().flatten() {
                let index = call["index"]
                    .as_u64()
                    .and_then(|index| u32::try_from(index).ok())
                    .ok_or_else(|| super::error(None, "TRAE 工具事件缺少索引", true))?;
                if self.tools.len() >= 1024 && !self.tools.contains_key(&index) {
                    return Err(super::error(None, "TRAE 工具数量超出限制", true));
                }
                let tool = self.tools.entry(index).or_default();
                let function = &call["function_call"];
                let mut delta = ToolCallDelta {
                    index,
                    id: None,
                    name: None,
                    arguments: None,
                };
                for (value, saved, output) in [
                    (&call["id"], &mut tool.id, &mut delta.id),
                    (&function["name"], &mut tool.name, &mut delta.name),
                ] {
                    if let Some(value) = value.as_str().filter(|value| !value.is_empty()) {
                        if saved.as_ref().is_some_and(|saved| saved != value) {
                            return Err(super::error(None, "TRAE 工具身份在流中发生变化", true));
                        }
                        if saved.is_none() {
                            *saved = Some(value.into());
                            *output = Some(value.into());
                        }
                    }
                }
                let partial = function["partial_arguments"]
                    .as_str()
                    .filter(|text| !text.is_empty());
                let complete = function["arguments"]
                    .as_str()
                    .filter(|text| !text.is_empty());
                if let Some(partial) = partial {
                    tool.arguments.push_str(partial);
                    delta.arguments = Some(partial.into());
                }
                if let Some(complete) = complete {
                    let tail = complete.strip_prefix(&tool.arguments).ok_or_else(|| {
                        super::error(None, "TRAE 工具完整参数与流分片不一致", true)
                    })?;
                    if !tail.is_empty() {
                        delta.arguments.get_or_insert_default().push_str(tail);
                        tool.arguments = complete.into();
                    }
                }
                if tool.arguments.len() > LIMIT {
                    return Err(super::error(None, "TRAE 工具参数超出大小限制", true));
                }
                if delta.id.is_some() || delta.name.is_some() || delta.arguments.is_some() {
                    events.push(Event::ToolCall(delta));
                }
            }
        }
        if kind == "token_usage" {
            let count = |key| {
                value[key]
                    .as_u64()
                    .and_then(|value| u32::try_from(value).ok())
            };
            events.push(Event::Usage(Usage {
                input_tokens: count("prompt_tokens"),
                output_tokens: count("completion_tokens"),
                total_tokens: count("total_tokens"),
            }));
        }
        if kind == "done" {
            if self
                .tools
                .values()
                .any(|tool| tool.id.is_none() || tool.name.is_none())
            {
                return Err(super::error(None, "TRAE 工具事件未完整结束", true));
            }
            let reason = match value["finish_reason"].as_str() {
                Some("stop" | "end_turn") | None => {
                    if self.tools.is_empty() {
                        FinishReason::Stop
                    } else {
                        FinishReason::ToolCalls
                    }
                }
                Some("tool_calls" | "function_call") => FinishReason::ToolCalls,
                Some("length" | "max_tokens" | "model_context_window_exceeded") => {
                    FinishReason::Length
                }
                Some("content_filter") => FinishReason::ContentFilter,
                _ => return Err(super::error(None, "TRAE 个人生成未正常结束", true)),
            };
            events.push(Event::Finish(reason));
            self.finished = true;
        }
        Ok(events)
    }
}

/// 在完整 UTF-8 行边界处理 SSE；事件和等待发送均响应取消。
pub async fn pump(
    response: reqwest::Response,
    sender: mpsc::Sender<Result<Event, AppError>>,
    cancel: CancellationToken,
) {
    let result = pump_inner(response, &sender, &cancel).await;
    if let Err(error) = result {
        tokio::select! { _ = cancel.cancelled() => {}, _ = sender.send(Err(error)) => {} }
    }
}

/// 明确 done 才结束，断流不会伪装为正常完成。
async fn pump_inner(
    response: reqwest::Response,
    sender: &mpsc::Sender<Result<Event, AppError>>,
    cancel: &CancellationToken,
) -> Result<(), AppError> {
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();
    let mut kind = String::new();
    let mut data = String::new();
    let mut decoder = Decoder::default();
    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            _ = sender.closed() => return Ok(()),
            chunk = stream.next() => chunk,
        };
        match chunk {
            Some(Ok(bytes)) => buffer.extend_from_slice(&bytes),
            Some(Err(_)) => return Err(super::error(None, "TRAE 个人生成流读取失败", true)),
            None => {
                return Err(super::error(
                    None,
                    "TRAE 个人生成流缺少 done，结果被截断",
                    true,
                ));
            }
        }
        if buffer.len() + data.len() > LIMIT {
            return Err(super::error(None, "TRAE SSE 事件超过大小限制", true));
        }
        while let Some(end) = buffer.iter().position(|byte| *byte == b'\n') {
            let line = buffer.drain(..=end).collect::<Vec<_>>();
            let line = std::str::from_utf8(&line)
                .map_err(|_| super::error(None, "TRAE SSE UTF-8 编码无效", true))?
                .trim_end_matches(['\r', '\n']);
            if let Some(value) = line.strip_prefix("event:") {
                kind = value.strip_prefix(' ').unwrap_or(value).into();
            } else if let Some(value) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(value.strip_prefix(' ').unwrap_or(value));
            } else if line.is_empty() {
                for event in decoder.event(&kind, &data)? {
                    tokio::select! {
                        _ = cancel.cancelled() => return Ok(()),
                        result = sender.send(Ok(event)) => if result.is_err() { return Ok(()); },
                    }
                }
                kind.clear();
                data.clear();
                if decoder.finished {
                    return Ok(());
                }
            }
            if data.len() > LIMIT {
                return Err(super::error(None, "TRAE SSE 数据超过大小限制", true));
            }
        }
    }
}
