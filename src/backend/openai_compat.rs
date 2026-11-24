//! OpenAI 兼容后端。
//!
//! 覆盖 OpenRouter、阿里百炼、火山方舟等提供 `/chat/completions` 的网关。
//! 该后端只做 wire 转换：统一请求 → OpenAI body，上游 SSE/JSON → 统一事件。

use crate::backend::Backend;
use crate::core::error::AppError;
use crate::core::stream::EventStream;
use crate::core::{
    Capabilities, ChatRequest, ContentPart, FinishReason, Message, ModelInfo, Role, ToolChoice,
    UpstreamError, Usage,
};
use crate::store::BackendRecord;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

/// 事件通道容量。满时阻塞读取，形成对上游的背压。
const EVENT_CHANNEL_CAPACITY: usize = 32;

/// 能力覆盖。只覆盖显式给出的字段。
#[derive(Debug, Clone, Default, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct CapabilityOverride {
    tools: Option<bool>,
    parallel_tools: Option<bool>,
    images: Option<bool>,
    reasoning: Option<bool>,
    context_window: Option<u32>,
}

impl CapabilityOverride {
    /// 把覆盖项应用到基础能力上。
    pub(crate) fn apply(&self, base: &mut Capabilities) {
        if let Some(value) = self.tools {
            base.tools = value;
        }
        if let Some(value) = self.parallel_tools {
            base.parallel_tools = value;
        }
        if let Some(value) = self.images {
            base.images = value;
        }
        if let Some(value) = self.reasoning {
            base.reasoning = value;
        }
        if let Some(value) = self.context_window {
            base.context_window = Some(value);
        }
    }
}

/// 后端选项。来自配置的 options JSON，缺省时使用默认值。
#[derive(Debug, Clone, Default, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct BackendOptions {
    /// 额外请求头。会覆盖同名默认头。
    #[serde(default)]
    headers: std::collections::HashMap<String, String>,
    /// 对话路径，默认 `chat/completions`。
    #[serde(default)]
    chat_path: Option<String>,
    /// 模型列表路径，默认 `models`。
    #[serde(default)]
    models_path: Option<String>,
    /// 原生 OpenAI 保留 developer 并使用 max_completion_tokens，兼容模式沿用旧字段。
    #[serde(default)]
    wire_profile: Option<WireProfile>,
}

/// 原生与兼容协议分别选择字段，不按模型名称猜测。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WireProfile {
    Compatible,
    Openai,
}

/// OpenAI 兼容后端。
pub struct OpenAiCompatBackend {
    id: String,
    name: String,
    kind: String,
    base_url: String,
    api_key: String,
    models: Vec<String>,
    capabilities: Capabilities,
    cost_tier: crate::backend::CostTier,
    priority: u8,
    daily_token_budget: u64,
    options: BackendOptions,
    client: reqwest::Client,
}

impl OpenAiCompatBackend {
    /// 从配置记录构造后端。base_url 末尾斜杠会被去掉。
    pub fn new(record: &BackendRecord) -> Result<Self, AppError> {
        Self::new_with_kind(record, &record.kind)
    }

    /// 以指定类型构造。`workbuddy` 会在 OpenAI 兼容基础上附加默认客户端头。
    pub fn new_with_kind(record: &BackendRecord, kind: &str) -> Result<Self, AppError> {
        Self::new_with_timeout(record, kind, std::time::Duration::from_secs(10))
    }

    /// 网关重载时固定本实例的连接时限，在途请求不受后续配置影响。
    pub fn new_with_timeout(
        record: &BackendRecord,
        kind: &str,
        timeout: std::time::Duration,
    ) -> Result<Self, AppError> {
        let base_url = record.base_url.trim().trim_end_matches('/').to_string();
        if base_url.is_empty() {
            return Err(AppError::bad_request("backend base_url must not be empty"));
        }
        if !(base_url.starts_with("https://") || base_url.starts_with("http://")) {
            return Err(AppError::bad_request(
                "backend base_url must start with http:// or https://",
            ));
        }
        let models = record
            .models
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        let mut options: BackendOptions = if record.options.trim().is_empty() {
            BackendOptions::default()
        } else {
            serde_json::from_str(&record.options).map_err(|error| {
                AppError::bad_request(format!("backend options is not valid JSON: {error}"))
            })?
        };
        // WorkBuddy 走同一兼容实现，默认头由配置覆盖或使用占位值。
        // 私有接口是否接受外部工具仍需真实账号验证。
        if kind == "workbuddy" && !options.headers.contains_key("user-agent") {
            options
                .headers
                .insert("user-agent".to_string(), "CodeBuddy/1.0".to_string());
        }
        let client = reqwest::Client::builder()
            .connect_timeout(timeout)
            .redirect(if kind == "modelscope" {
                reqwest::redirect::Policy::none()
            } else {
                reqwest::redirect::Policy::default()
            })
            .build()
            .map_err(|error| AppError::Internal {
                message: format!("build http client failed: {error}"),
            })?;
        // 协议能承载图片不代表每个模型都能识图，未知能力由目录或显式配置补全。
        let mut capabilities = Capabilities::default();
        if !record.capabilities.trim().is_empty() {
            let parsed: CapabilityOverride =
                serde_json::from_str(&record.capabilities).map_err(|error| {
                    AppError::bad_request(format!(
                        "backend capabilities is not valid JSON: {error}"
                    ))
                })?;
            parsed.apply(&mut capabilities);
        }
        Ok(OpenAiCompatBackend {
            id: record.id.clone(),
            name: record.name.clone(),
            kind: kind.to_string(),
            base_url,
            api_key: record.api_key.clone(),
            models,
            capabilities,
            cost_tier: crate::backend::CostTier::parse(&record.cost_tier),
            priority: record.priority,
            daily_token_budget: record.daily_token_budget,
            options,
            client,
        })
    }

    /// 拼接上游端点。base_url 已包含版本前缀（如 `/v1`）。
    fn endpoint(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, path.trim_start_matches('/'))
    }

    /// 对话端点路径。
    fn chat_path(&self) -> &str {
        self.options
            .chat_path
            .as_deref()
            .unwrap_or("chat/completions")
    }

    /// 模型列表端点路径。
    fn models_path(&self) -> &str {
        self.options.models_path.as_deref().unwrap_or("models")
    }

    /// 官方 API 域缺省采用原生协议；其他网关只能通过显式配置选择该画像。
    fn wire_profile(&self) -> WireProfile {
        self.options.wire_profile.unwrap_or_else(|| {
            if reqwest::Url::parse(&self.base_url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .as_deref()
                == Some("api.openai.com")
            {
                WireProfile::Openai
            } else {
                WireProfile::Compatible
            }
        })
    }

    /// 在提供商边界应用角色和输出上限规则；共享编码继续兼容其他插件。
    fn build_profile_body(
        request: &ChatRequest,
        upstream_model: &str,
        profile: WireProfile,
    ) -> Value {
        let mut body = Self::build_body(request, upstream_model);
        if profile == WireProfile::Openai {
            if let Some(limit) = body.as_object_mut().unwrap().remove("max_tokens") {
                body["max_completion_tokens"] = limit;
            }
            for (encoded, message) in body["messages"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .zip(&request.messages)
            {
                if matches!(message.role, Role::Developer) {
                    encoded["role"] = json!("developer");
                }
            }
        }
        body
    }

    /// 构造 OpenAI 请求体。
    pub(crate) fn build_body(request: &ChatRequest, upstream_model: &str) -> Value {
        let messages = request
            .messages
            .iter()
            .map(Self::encode_message)
            .collect::<Vec<_>>();
        let mut body = json!({
            "model": upstream_model,
            "messages": messages,
            "stream": request.stream,
        });
        if request.stream {
            body["stream_options"] = json!({ "include_usage": true });
        }
        if let Some(max_tokens) = request.max_tokens {
            body["max_tokens"] = json!(max_tokens);
        }
        if let Some(temperature) = request.temperature {
            body["temperature"] = json!(temperature);
        }
        if let Some(top_p) = request.top_p {
            body["top_p"] = json!(top_p);
        }
        if !request.tools.is_empty() {
            body["tools"] = json!(
                request
                    .tools
                    .iter()
                    .map(|tool| json!({
                        "type": "function",
                        "function": {
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.parameters,
                        }
                    }))
                    .collect::<Vec<_>>()
            );
        }
        if let Some(choice) = &request.tool_choice {
            body["tool_choice"] = match choice {
                ToolChoice::Auto => json!("auto"),
                ToolChoice::None => json!("none"),
                ToolChoice::Required => json!("required"),
                ToolChoice::Named(name) => json!({"type": "function", "function": {"name": name}}),
            };
        }
        if !request.stop.is_empty() {
            body["stop"] = json!(request.stop);
        }
        if let Some(effort) = &request.reasoning_effort {
            // 官方兼容网关多数接受 reasoning_effort；不支持的会忽略或报参数错误。
            body["reasoning_effort"] = json!(effort);
        }
        body
    }

    /// 单条消息编码。纯文本用字符串，含图片用片段数组。
    fn encode_message(message: &Message) -> Value {
        let role = match message.role.normalized() {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
            Role::Developer => "system",
        };
        let has_image = message.has_image();
        let content = if has_image {
            json!(message
                .content
                .iter()
                .map(|part| match part {
                    ContentPart::Text { text } => json!({"type": "text", "text": text}),
                    ContentPart::ImageUrl { url, detail } => match detail {
                        Some(detail) => json!({"type": "image_url", "image_url": {"url": url, "detail": detail}}),
                        None => json!({"type": "image_url", "image_url": {"url": url}}),
                    },
                })
                .collect::<Vec<_>>())
        } else {
            json!(message.text_content())
        };
        let mut encoded = json!({"role": role, "content": content});
        if !message.tool_calls.is_empty() {
            encoded["tool_calls"] = json!(
                message
                    .tool_calls
                    .iter()
                    .map(|call| json!({
                        "id": call.id,
                        "type": "function",
                        "function": {"name": call.name, "arguments": call.arguments},
                    }))
                    .collect::<Vec<_>>()
            );
        }
        if let Some(tool_call_id) = &message.tool_call_id {
            encoded["tool_call_id"] = json!(tool_call_id);
        }
        if let Some(name) = &message.name {
            encoded["name"] = json!(name);
        }
        encoded
    }

    /// 构造带鉴权与自定义头的请求。
    fn request_builder(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut builder = self.client.request(method, self.endpoint(path));
        if !self.api_key.is_empty() {
            builder = builder.bearer_auth(&self.api_key);
        }
        for (name, value) in &self.options.headers {
            builder = builder.header(name, value);
        }
        builder
    }
}

#[async_trait]
impl Backend for OpenAiCompatBackend {
    /// 返回配置中的后端标识。
    fn id(&self) -> &str {
        &self.id
    }

    /// 返回配置中的展示名。
    fn name(&self) -> &str {
        &self.name
    }

    /// 返回后端类型。
    fn kind(&self) -> &str {
        &self.kind
    }

    /// 返回静态能力。
    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    /// 返回配置的费用层级。
    fn cost_tier(&self) -> crate::backend::CostTier {
        self.cost_tier
    }

    /// 返回配置的排序优先级。
    fn priority(&self) -> u8 {
        self.priority
    }

    /// 返回配置的每日 token 预算。
    fn daily_token_budget(&self) -> u64 {
        self.daily_token_budget
    }

    /// 返回配置里声明的模型。
    fn static_models(&self) -> Vec<String> {
        self.models.clone()
    }

    /// 从上游读取模型参数；配置仅作白名单，目录不支持时保留静态预算。
    async fn list_models(&self) -> Result<Vec<ModelInfo>, AppError> {
        let configured = self
            .models
            .iter()
            .map(|model| ModelInfo {
                id: model.clone(),
                display_name: Some(model.clone()),
                price_factor: None,
                original_price_factor: None,
                price_valid_until: None,
                capabilities: self.capabilities.clone(),
                token_limits: crate::core::TokenLimits {
                    context_window: self.capabilities.context_window,
                    ..Default::default()
                },
            })
            .collect::<Vec<_>>();
        let result = async {
            let response = self
                .request_builder(reqwest::Method::GET, self.models_path())
                .timeout(std::time::Duration::from_secs(15))
                .send()
                .await?;
            let status = response.status();
            let body = response.text().await?;
            if !status.is_success() {
                return Err(AppError::Upstream(UpstreamError::status(
                    status.as_u16(),
                    body,
                )));
            }
            let parsed: Value =
                serde_json::from_str(&body).map_err(|error| AppError::Internal {
                    message: format!("invalid models response: {error}"),
                })?;
            let models = parsed
                .get("data")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| {
                            Some(ModelInfo {
                                id: item.get("id")?.as_str()?.to_string(),
                                display_name: Some(item.get("id")?.as_str()?.to_string()),
                                price_factor: None,
                                original_price_factor: None,
                                price_valid_until: None,
                                capabilities: crate::backend::catalog::capabilities(
                                    item,
                                    &self.capabilities,
                                ),
                                token_limits: crate::backend::catalog::token_limits(item),
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            Ok::<_, AppError>(models)
        }
        .await;
        match result {
            Ok(mut models) if !configured.is_empty() => {
                models.retain(|model| self.models.contains(&model.id));
                for fallback in configured {
                    if let Some(model) = models.iter_mut().find(|model| model.id == fallback.id) {
                        model.token_limits.context_window = model
                            .token_limits
                            .context_window
                            .or(fallback.token_limits.context_window);
                    } else {
                        models.push(fallback);
                    }
                }
                Ok(models)
            }
            Ok(models) => Ok(models),
            Err(_) if !configured.is_empty() => Ok(configured),
            Err(error) => Err(error),
        }
    }

    /// 发起生成。流式与流一次性都统一产出事件流。
    async fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, AppError> {
        let upstream_model = self
            .model_suffix(&request.model)
            .unwrap_or(request.model.clone());
        let body = Self::build_profile_body(&request, &upstream_model, self.wire_profile());
        let stream_requested = request.stream;
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return Err(AppError::BadRequest {
                    code: "request_cancelled".to_string(),
                    message: "client cancelled before upstream response".to_string(),
                });
            }
            result = self.request_builder(reqwest::Method::POST, self.chat_path()).json(&body).send() => result?,
        };
        let status = response.status();
        if !status.is_success() {
            crate::gateway::retry::capture_retry_after(&response);
            let text = response.text().await.unwrap_or_default();
            return Err(AppError::Upstream(UpstreamError::status(
                status.as_u16(),
                text,
            )));
        }

        let (sender, receiver) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        if stream_requested {
            tokio::spawn(async move {
                pump_sse(response, sender, cancel).await;
            });
        } else {
            let cancel_for_task = cancel.clone();
            tokio::spawn(async move {
                pump_json(response, sender, cancel_for_task).await;
            });
        }
        Ok(Box::pin(ReceiverStream::new(receiver)))
    }
}

impl OpenAiCompatBackend {
    /// 从 `backend/model` 地址里取出上游模型名；纯模型名原样返回。
    fn model_suffix(&self, model: &str) -> Option<String> {
        model
            .strip_prefix(&format!("{}/", self.id))
            .map(ToOwned::to_owned)
    }
}

/// 读取上游 SSE 并转成统一事件。
///
/// 供协议相同的其他后端复用（WorkBuddy 的 `/v2/chat/completions` 即 OpenAI 形态）。
/// 同一终态可在用量尾帧中重复；合并相同原因，矛盾终态仍交由网关拒绝。
pub(crate) async fn pump_sse(
    response: reqwest::Response,
    sender: mpsc::Sender<Result<crate::core::Event, AppError>>,
    cancel: CancellationToken,
) {
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::<u8>::new();
    let mut finish_reason = None;
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                send_error(&sender, AppError::BadRequest {
                    code: "request_cancelled".to_string(),
                    message: "client cancelled during stream".to_string(),
                }).await;
                return;
            }
            item = stream.next() => item,
        };
        let Some(item) = chunk else { break };
        let bytes = match item {
            Ok(bytes) => bytes,
            Err(error) => {
                send_error(
                    &sender,
                    AppError::Upstream(UpstreamError::transport(error.to_string())),
                )
                .await;
                return;
            }
        };
        buffer.extend_from_slice(&bytes);
        // SSE 以空行分帧；保留未完整的尾部继续等待。
        while let Some(index) = find_frame_end(&buffer) {
            let frame = buffer.drain(..index).collect::<Vec<u8>>();
            let text = String::from_utf8_lossy(&frame);
            if text.trim().is_empty() {
                continue;
            }
            match parse_sse_frame(&text) {
                Ok(Some(events)) => {
                    for event in events {
                        if let crate::core::Event::Finish(reason) = &event {
                            if finish_reason.as_ref() == Some(reason) {
                                continue;
                            }
                            finish_reason = Some(*reason);
                        }
                        if sender.send(Ok(event)).await.is_err() {
                            return;
                        }
                    }
                }
                Ok(None) => {
                    // [DONE] 之后不再有数据。
                    return;
                }
                Err(error) => {
                    send_error(&sender, error).await;
                    return;
                }
            }
        }
    }
    // EOF 不代表生成完成；由审计事件流检查真实 finish，不能补造 Stop。
}

/// 读取上游一次性 JSON 并转成事件序列。
async fn pump_json(
    response: reqwest::Response,
    sender: mpsc::Sender<Result<crate::core::Event, AppError>>,
    cancel: CancellationToken,
) {
    let payload = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            send_error(&sender, AppError::BadRequest {
                code: "request_cancelled".to_string(),
                message: "client cancelled before upstream body completed".to_string(),
            }).await;
            return;
        }
        result = response.text() => result,
    };
    let text = match payload {
        Ok(text) => text,
        Err(error) => {
            send_error(
                &sender,
                AppError::Upstream(UpstreamError::transport(error.to_string())),
            )
            .await;
            return;
        }
    };
    let parsed: Value = match serde_json::from_str(&text) {
        Ok(parsed) => parsed,
        Err(error) => {
            send_error(
                &sender,
                AppError::Upstream(UpstreamError {
                    status: None,
                    code: "invalid_upstream_response".to_string(),
                    message: format!("invalid JSON from upstream: {error}"),
                    retryable: false,
                    execution_unknown: false,
                }),
            )
            .await;
            return;
        }
    };
    if let Some(error) = upstream_payload_error(&parsed) {
        send_error(&sender, error).await;
        return;
    }
    for event in decode_completion(&parsed) {
        if sender.send(Ok(event)).await.is_err() {
            return;
        }
    }
}

/// 发送一个流内错误。
async fn send_error(sender: &mpsc::Sender<Result<crate::core::Event, AppError>>, error: AppError) {
    let _ = sender.send(Err(error)).await;
}

/// 找到一帧结束位置（`\n\n` 或 `\r\n\r\n`），返回帧尾下标。
fn find_frame_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| index + 2)
        .into_iter()
        .chain(
            buffer
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|index| index + 4),
        )
        .min()
}

/// 解析单个 SSE 帧。返回 `None` 表示收到 `[DONE]`。
fn parse_sse_frame(frame: &str) -> Result<Option<Vec<crate::core::Event>>, AppError> {
    let mut events = Vec::new();
    for line in frame.lines() {
        let line = line.trim_start();
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() {
            continue;
        }
        if data == "[DONE]" {
            return Ok(None);
        }
        let parsed: Value = match serde_json::from_str(data) {
            Ok(parsed) => parsed,
            Err(error) => {
                return Err(AppError::Upstream(UpstreamError {
                    status: None,
                    code: "invalid_upstream_chunk".to_string(),
                    message: format!("invalid SSE chunk: {error}"),
                    retryable: false,
                    execution_unknown: false,
                }));
            }
        };
        if let Some(error) = upstream_payload_error(&parsed) {
            return Err(error);
        }
        events.extend(decode_chunk(&parsed));
    }
    Ok(Some(events))
}

/// 解码流式 chunk。
fn decode_chunk(chunk: &Value) -> Vec<crate::core::Event> {
    let mut events = Vec::new();
    if let Some(choices) = chunk.get("choices").and_then(Value::as_array) {
        for choice in choices {
            if let Some(delta) = choice.get("delta") {
                if let Some(text) = delta.get("content").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    events.push(crate::core::Event::Text {
                        text: text.to_string(),
                    });
                }
                if let Some(refusal) = delta.get("refusal").and_then(Value::as_str)
                    && !refusal.is_empty()
                {
                    // 统一事件暂不区分拒绝类型；保留可见正文，不能产生空成功。
                    events.push(crate::core::Event::Text {
                        text: refusal.into(),
                    });
                }
                if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str)
                    && !reasoning.is_empty()
                {
                    events.push(crate::core::Event::Reasoning {
                        text: reasoning.to_string(),
                    });
                }
                if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                    for (position, call) in calls.iter().enumerate() {
                        let index = call
                            .get("index")
                            .and_then(Value::as_u64)
                            .map(|value| value as u32)
                            .unwrap_or(position as u32);
                        let function = call.get("function");
                        events.push(crate::core::Event::ToolCall(crate::core::ToolCallDelta {
                            index,
                            id: call
                                .get("id")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned),
                            name: function
                                .and_then(|value| value.get("name"))
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned),
                            arguments: function
                                .and_then(|value| value.get("arguments"))
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned),
                        }));
                    }
                }
            }
            if let Some(reason) = choice
                .get("finish_reason")
                .and_then(Value::as_str)
                .and_then(map_finish_reason)
            {
                events.push(crate::core::Event::Finish(reason));
            }
        }
    }
    if let Some(usage) = chunk.get("usage").filter(|value| !value.is_null()) {
        events.push(crate::core::Event::Usage(decode_usage(usage)));
    }
    events
}

/// HTTP 200 内嵌的业务错误仍是失败，不能映射成正常的内容过滤终态。
fn upstream_payload_error(payload: &Value) -> Option<AppError> {
    let error = payload.get("error").filter(|error| !error.is_null())?;
    Some(AppError::Upstream(UpstreamError {
        status: error
            .get("status")
            .or_else(|| error.get("status_code"))
            .and_then(Value::as_u64)
            .and_then(|value| u16::try_from(value).ok())
            .filter(|value| (400..600).contains(value)),
        code: error
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("upstream_error")
            .into(),
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("upstream returned an error")
            .into(),
        retryable: false,
        execution_unknown: true,
    }))
}

/// 解码一次性响应。
fn decode_completion(payload: &Value) -> Vec<crate::core::Event> {
    let mut events = Vec::new();
    if let Some(choices) = payload.get("choices").and_then(Value::as_array) {
        for choice in choices {
            if let Some(message) = choice.get("message") {
                if let Some(text) = message.get("content").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    events.push(crate::core::Event::Text {
                        text: text.to_string(),
                    });
                }
                if let Some(refusal) = message.get("refusal").and_then(Value::as_str)
                    && !refusal.is_empty()
                {
                    events.push(crate::core::Event::Text {
                        text: refusal.into(),
                    });
                }
                if let Some(reasoning) = message.get("reasoning_content").and_then(Value::as_str)
                    && !reasoning.is_empty()
                {
                    events.push(crate::core::Event::Reasoning {
                        text: reasoning.to_string(),
                    });
                }
                if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
                    for (index, call) in calls.iter().enumerate() {
                        let function = call.get("function");
                        events.push(crate::core::Event::ToolCall(crate::core::ToolCallDelta {
                            index: index as u32,
                            id: call
                                .get("id")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned),
                            name: function
                                .and_then(|value| value.get("name"))
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned),
                            arguments: function
                                .and_then(|value| value.get("arguments"))
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned),
                        }));
                    }
                }
            }
            if let Some(reason) = choice
                .get("finish_reason")
                .and_then(Value::as_str)
                .and_then(map_finish_reason)
            {
                events.push(crate::core::Event::Finish(reason));
            }
        }
    }
    if let Some(usage) = payload.get("usage").filter(|value| !value.is_null()) {
        events.push(crate::core::Event::Usage(decode_usage(usage)));
    }
    events
}

/// 空结束字段表示仍在生成，不可补成 Stop；非空字段映射为统一终态。
fn map_finish_reason(reason: &str) -> Option<FinishReason> {
    let reason = reason.trim();
    if reason.is_empty() {
        return None;
    }
    Some(match reason {
        "tool_calls" | "function_call" => FinishReason::ToolCalls,
        "length" => FinishReason::Length,
        "content_filter" => FinishReason::ContentFilter,
        _ => FinishReason::Stop,
    })
}

/// 解码用量，缺失字段保持 None。
fn decode_usage(usage: &Value) -> Usage {
    Usage {
        input_tokens: usage
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .map(|value| value as u32),
        output_tokens: usage
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .map(|value| value as u32),
        total_tokens: usage
            .get("total_tokens")
            .and_then(Value::as_u64)
            .map(|value| value as u32),
    }
}

#[cfg(test)]
mod client_alignment_tests {
    use super::*;

    /// 原生画像保留 OpenAI 的开发者角色和新输出上限，兼容模式不受影响。
    #[test]
    fn native_profile_preserves_developer_and_completion_limit() {
        let request = crate::ingress::chat::parse_chat_request(&json!({
            "model":"o3","max_completion_tokens":32,
            "messages":[{"role":"system","content":"system"},{"role":"developer","content":"developer"},
                {"role":"user","content":"hello"}]
        }), None).unwrap();
        let native = OpenAiCompatBackend::build_profile_body(&request, "o3", WireProfile::Openai);
        assert_eq!(native["max_completion_tokens"], 32);
        assert!(native.get("max_tokens").is_none());
        assert_eq!(native["messages"][0]["role"], "system");
        assert_eq!(native["messages"][1]["role"], "developer");
        let compatible =
            OpenAiCompatBackend::build_profile_body(&request, "o3", WireProfile::Compatible);
        assert_eq!(compatible["max_tokens"], 32);
        assert_eq!(compatible["messages"][1]["role"], "system");
    }

    /// 原生 API 自动选择正确画像，第三方网关需要显式选择。
    #[test]
    fn native_profile_is_scoped_to_official_origin_or_explicit_option() {
        for (base, options, expected) in [
            ("https://api.openai.com/v1", "", WireProfile::Openai),
            (
                "https://api-inference.modelscope.cn/v1",
                "",
                WireProfile::Compatible,
            ),
            (
                "http://localhost/v1",
                "{\"wire_profile\":\"openai\"}",
                WireProfile::Openai,
            ),
        ] {
            let backend = OpenAiCompatBackend::new(&BackendRecord {
                base_url: base.into(),
                options: options.into(),
                ..Default::default()
            })
            .unwrap();
            assert_eq!(backend.wire_profile(), expected);
        }
    }

    /// CRLF 数据帧在 LF DONE 之前，必须先解出文本和尾随用量。
    #[test]
    fn mixed_line_endings_preserve_frames_before_done() {
        for (first, second) in [("\r\n\r\n", "\n\n"), ("\n\n", "\r\n\r\n")] {
            let usage = json!({"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":7}});
            let mut bytes = format!("data: {usage}{first}data: [DONE]{second}").into_bytes();
            let end = find_frame_end(&bytes).unwrap();
            let frame = String::from_utf8(bytes.drain(..end).collect()).unwrap();
            let events = parse_sse_frame(&frame).unwrap().unwrap();
            assert!(
                matches!(&events[0], crate::core::Event::Usage(usage) if usage.input_tokens==Some(12))
            );
            let end = find_frame_end(&bytes).unwrap();
            let frame = String::from_utf8(bytes.drain(..end).collect()).unwrap();
            assert!(parse_sse_frame(&frame).unwrap().is_none());
        }
    }

    /// 仅含拒绝正文的合法响应在流式和一次性入口都不能变成空成功。
    #[test]
    fn refusal_text_is_visible_without_changing_finish_reason() {
        let events = decode_chunk(
            &json!({"choices":[{"delta":{"refusal":"不能帮助此请求"},"finish_reason":"stop"}]}),
        );
        assert!(matches!(&events[0], crate::core::Event::Text {text} if text=="不能帮助此请求"));
        assert!(matches!(
            &events[1],
            crate::core::Event::Finish(FinishReason::Stop)
        ));
        let events = decode_completion(
            &json!({"choices":[{"message":{"content":null,"refusal":"不能帮助此请求"},"finish_reason":"stop"}]}),
        );
        assert!(matches!(&events[0], crate::core::Event::Text {text} if text=="不能帮助此请求"));
    }

    /// 当前无法保真的约束必须在入站被拒绝，不以成功响应掩盖参数丢失。
    #[test]
    fn unsupported_constraints_and_conflicting_limits_are_explicit() {
        for constraints in [
            json!({"parallel_tool_calls":false}),
            json!({"response_format":{"type":"json_schema","json_schema":{"name":"result","schema":{"type":"object"}}}}),
            json!({"tools":[{"type":"function","function":{"name":"f","strict":true}}]}),
            json!({"n":2}),
            json!({"max_tokens":10,"max_completion_tokens":20}),
        ] {
            let mut body = json!({"model":"demo","messages":[{"role":"user","content":"hello"}]});
            body.as_object_mut()
                .unwrap()
                .extend(constraints.as_object().unwrap().clone());
            assert!(crate::ingress::chat::parse_chat_request(&body, None).is_err());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 纯文本消息应编码为字符串 content。
    #[test]
    fn encodes_plain_text_message() {
        let encoded = OpenAiCompatBackend::encode_message(&Message::text(Role::User, "hi"));
        assert_eq!(encoded["role"], "user");
        assert_eq!(encoded["content"], "hi");
    }

    /// 工具消息必须保留 tool_call_id，否则上游无法配对结果。
    #[test]
    fn encodes_tool_result_with_call_id() {
        let message = Message {
            role: Role::Tool,
            content: vec![ContentPart::text("done")],
            tool_calls: Vec::new(),
            tool_call_id: Some("call_1".into()),
            name: None,
        };
        let encoded = OpenAiCompatBackend::encode_message(&message);
        assert_eq!(encoded["tool_call_id"], "call_1");
    }

    /// 工具定义与选择策略要转成 OpenAI 形状。
    #[test]
    fn encodes_tools_and_choice() {
        let request = ChatRequest {
            protocol: crate::core::Protocol::Chat,
            model: "demo/model-a".into(),
            messages: vec![Message::text(Role::User, "hi")],
            tools: vec![crate::core::ToolDef {
                name: "echo".into(),
                description: "echo back".into(),
                parameters: json!({"type": "object", "properties": {"text": {"type": "string"}}}),
            }],
            tool_choice: Some(ToolChoice::Named("echo".into())),
            max_tokens: Some(128),
            temperature: None,
            top_p: None,
            stop: Vec::new(),
            reasoning_effort: None,
            stream: true,
            session: None,
        };
        let body = OpenAiCompatBackend::build_body(&request, "model-a");
        assert_eq!(body["model"], "model-a");
        assert_eq!(body["tools"][0]["function"]["name"], "echo");
        assert_eq!(body["tool_choice"]["function"]["name"], "echo");
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    /// SSE 帧解析要能取出文本与终止原因。
    #[test]
    fn parses_sse_frame() {
        let frame = "data: {\"choices\":[{\"delta\":{\"content\":\"he\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
        let events = parse_sse_frame(frame).expect("parse").expect("not done");
        assert!(matches!(events[0], crate::core::Event::Text { .. }));
        assert!(matches!(
            events[1],
            crate::core::Event::Finish(FinishReason::Stop)
        ));
    }

    /// `[DONE]` 必须终止解析。
    #[test]
    fn stops_on_done_marker() {
        let frame = "data: [DONE]\n\n";
        assert!(parse_sse_frame(frame).expect("parse").is_none());
    }

    /// 空或空白结束字段在 SSE 和 JSON 中都不能冒充真实终态。
    #[test]
    fn empty_finish_fields_do_not_end_a_generation() {
        for reason in ["", " ", "\t"] {
            let chunk = json!({"choices":[{"delta":{"content":"partial"},"finish_reason":reason}]});
            let events = decode_chunk(&chunk);
            assert_eq!(events.len(), 1);
            assert!(matches!(&events[0], crate::core::Event::Text { text } if text == "partial"));
            let completion =
                json!({"choices":[{"message":{"content":"partial"},"finish_reason":reason}]});
            assert_eq!(decode_completion(&completion), events);
        }
        assert_eq!(map_finish_reason("length"), Some(FinishReason::Length));
    }

    /// 一次性响应要展开成文本、工具与终止事件。
    #[test]
    fn decodes_non_stream_completion() {
        let payload = json!({
            "choices": [{
                "message": {
                    "content": "hello",
                    "tool_calls": [{
                        "id": "call_1",
                        "function": {"name": "echo", "arguments": "{\"text\":\"hi\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
        });
        let events = decode_completion(&payload);
        assert!(matches!(events[0], crate::core::Event::Text { .. }));
        assert!(matches!(events[1], crate::core::Event::ToolCall(_)));
        assert!(matches!(
            events[2],
            crate::core::Event::Finish(FinishReason::ToolCalls)
        ));
        assert!(matches!(events[3], crate::core::Event::Usage(_)));
    }

    /// 合法地址才能构造后端。
    #[test]
    fn rejects_invalid_base_url() {
        let record = BackendRecord {
            id: "demo".into(),
            name: "Demo".into(),
            kind: "openai_compat".into(),
            base_url: "ftp://example.com".into(),
            api_key: String::new(),
            models: String::new(),
            enabled: true,
            ..BackendRecord::default()
        };
        assert!(OpenAiCompatBackend::new(&record).is_err());
    }
}
