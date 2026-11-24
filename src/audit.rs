//! 个人排障用请求审计：W3C traceparent、上游子 span 和响应生命周期。
//! 不引入采样器、Collector、SDK 或后台写入队列。

use crate::client_plugins::{HistoryIndex, HistorySnapshot, Identification, ReplyFingerprint};
use crate::core::error::AppError;
use crate::core::stream::{EventStream, ToolCallAccumulator};
use crate::core::{ChatRequest, Event, UpstreamError};
use crate::records::{Attempt, RecordLog, RequestRecord, new_record_id, now_seconds};
use crate::state::AppState;
use axum::body::{Body, BodyDataStream, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use axum::response::sse::Event as SseEvent;
use futures_util::{Stream, StreamExt};
use rand_core::{OsRng, RngCore};
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

tokio::task_local! {
    static CURRENT: Arc<Audit>;
}

struct Progress {
    record: RequestRecord,
    upstream_complete: bool,
    finalized: bool,
    history: Option<HistorySnapshot>,
    reply: Option<crate::client_plugins::ReplyEvidence>,
    retry_after: Option<u64>,
}

/// 单个入站请求拥有的上下文；流任务通过 Arc 显式继承。
pub struct Audit {
    log: Arc<RecordLog>,
    started: Instant,
    progress: Mutex<Progress>,
    client: Identification,
    sessions: Arc<HistoryIndex>,
    protocol_terminal_ready: AtomicBool,
}

/// 协议编码器携带完成标记；入队不等于终止帧已交给 HTTP 传输层。
pub(crate) struct SseFrame {
    event: SseEvent,
    terminal: bool,
}

impl SseFrame {
    /// 标记正常完成的协议终止事件，流中错误不使用此标记。
    pub(crate) fn terminal(event: SseEvent) -> Self {
        Self {
            event,
            terminal: true,
        }
    }
}

impl From<SseEvent> for SseFrame {
    /// 普通帧不提供完成证据，客户端仅读部分输出仍视为取消。
    fn from(event: SseEvent) -> Self {
        Self {
            event,
            terminal: false,
        }
    }
}

/// 终止帧被 HTTP body 拉取时标记完成，避免后台生成或仅入队就发布历史。
pub(crate) fn sse_stream(
    receiver: mpsc::Receiver<Result<SseFrame, Infallible>>,
) -> impl Stream<Item = Result<SseEvent, Infallible>> {
    let audit = current();
    ReceiverStream::new(receiver).map(move |frame| {
        frame.map(|frame| {
            if frame.terminal
                && let Some(audit) = &audit
            {
                audit.protocol_terminal_ready.store(true, Ordering::Release);
            }
            frame.event
        })
    })
}

/// 生成非零的随机 span ID，不复用请求 ID。
fn span_id() -> String {
    loop {
        let mut bytes = [0u8; 8];
        OsRng.fill_bytes(&mut bytes);
        if bytes != [0; 8] {
            return bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        }
    }
}

/// 校验版本 00 的 W3C 头；非法、重复或全零标识均忽略并创建新 trace。
fn trace_context(headers: &HeaderMap) -> (String, Option<String>, String) {
    let value = headers
        .get("traceparent")
        .and_then(|value| value.to_str().ok());
    if headers.get_all("traceparent").iter().count() == 1
        && let Some(value) = value
    {
        let fields: Vec<_> = value.split('-').collect();
        let hex = |value: &str, size| {
            value.len() == size
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        };
        if fields.len() == 4
            && fields[0] == "00"
            && hex(fields[1], 32)
            && fields[1].bytes().any(|byte| byte != b'0')
            && hex(fields[2], 16)
            && fields[2].bytes().any(|byte| byte != b'0')
            && hex(fields[3], 2)
        {
            return (fields[1].into(), Some(fields[2].into()), fields[3].into());
        }
    }
    (uuid::Uuid::new_v4().simple().to_string(), None, "00".into())
}

/// 当前请求上下文只在入站 handler 中存在，不修改协议模型结构。
pub fn current() -> Option<Arc<Audit>> {
    CURRENT.try_with(Arc::clone).ok()
}

/// 在解析失败前记录客户端声明的模型和流式标记；不复制正文。
pub fn payload(value: &serde_json::Value) {
    if let Some(audit) = current()
        && let Ok(mut progress) = audit.progress.lock()
    {
        progress.record.requested_model = value["model"]
            .as_str()
            .unwrap_or_default()
            .chars()
            .take(256)
            .collect();
        progress.record.stream = value["stream"].as_bool().unwrap_or(false);
    }
}

impl Audit {
    /// 返回明确会话键；推断分组不产生厂商侧会话参数。
    pub fn routing_session(&self) -> Option<String> {
        self.client
            .session
            .as_ref()
            .and_then(|session| session.routing_session())
    }

    /// 明确或推断组均可调度账号，不改变厂商侧会话参数。
    pub fn routing_group(&self) -> Option<String> {
        self.progress.lock().ok().and_then(|progress| {
            progress.record.session.as_ref().map(|session| {
                format!(
                    "{}:{}",
                    session.client.map_or("generic", |client| client.as_str()),
                    session.id
                )
            })
        })
    }

    /// 请求通过归一解析后补齐模型与规模，不存工具定义或消息。
    pub fn request(&self, request: &ChatRequest) {
        if let Ok(mut progress) = self.progress.lock() {
            progress.record.requested_model = request.model.chars().take(256).collect();
            progress.record.stream = request.stream;
            progress.record.tools = request.tools.len();
            progress.record.input_chars = crate::ingress::input_chars(request);
            progress.record.message_count = Some(request.messages.len());
            let mut images = 0usize;
            let mut image_bytes = 0usize;
            for part in request.messages.iter().flat_map(|message| &message.content) {
                if let crate::core::ContentPart::ImageUrl { url, .. } = part {
                    images += 1;
                    image_bytes = image_bytes.saturating_add(url.len());
                }
            }
            progress.record.image_count = Some(images);
            progress.record.image_url_bytes = Some(image_bytes);
            if self.client.infer_history
                && progress.record.session.is_none()
                && let Some(client) = self.client.client
                && let Some(history) = self.sessions.snapshot_for(request, client)
            {
                let session = self.sessions.resolve(client, &history);
                progress.record.session_id = Some(session.id.clone());
                progress.record.session = Some(session);
                progress.history = Some(history);
            }
        }
    }

    /// 无可用目标时保存有界的本地准入原因及预算，不复制请求或上游错误。
    pub fn rejected_routes(&self, decision: &crate::routing::Decision) {
        if let Ok(mut progress) = self.progress.lock() {
            progress.record.routing_rejections = decision
                .rejected
                .iter()
                .take(128)
                .map(|rejection| crate::routing::Rejection {
                    model: rejection.model.chars().take(256).collect(),
                    reason: rejection.reason.chars().take(256).collect(),
                })
                .collect();
        }
    }

    /// 为真实上游调用创建子 span，并在调用前落盘。
    pub fn begin_attempt(&self, model: &str) -> usize {
        let mut progress = self.progress.lock().expect("audit lock poisoned");
        let index = progress.record.attempts.len();
        let parent_span_id = progress.record.span_id.clone();
        progress.record.attempts.push(Attempt {
            routing: None,
            span_id: span_id(),
            parent_span_id,
            model: model.chars().take(256).collect(),
            offset_ms: self.started.elapsed().as_millis() as u64,
            duration_ms: 0,
            outcome: "running".into(),
            upstream_status: None,
            error: None,
        });
        self.log.update(progress.record.clone());
        index
    }

    /// 记录 Auto 的预算与排序依据，不包含消息或账号凭证。
    pub fn route_attempt(
        &self,
        index: usize,
        candidate: &crate::routing::Candidate,
        policy: crate::routing::RoutePolicy,
        lease: &crate::gateway::balancer::Lease,
    ) {
        if let Ok(mut progress) = self.progress.lock() {
            if let Some(attempt) = progress.record.attempts.get_mut(index) {
                attempt.routing = Some(serde_json::json!({
                    "strategy": policy.as_str(), "account_id": candidate.account_id,
                    "pool": candidate.pool(), "quota_known": candidate.quota_known(),
                    "routing_rank": candidate.auto_rank(),
                    "priority": candidate.priority, "price_factor": candidate.price_factor,
                    "comparison_scope": candidate.comparison_scope, "quota": candidate.quota,
                    "limits": candidate.limits, "affinity_hit": lease.affinity_hit,
                    "reservation": "capacity_acquired", "binding_version": lease.version,
                    "migrated": lease.migrated,
                    "admission": lease.admission,
                }));
            }
            self.log.update(progress.record.clone());
        }
    }

    /// 记录总生成预算、启动时长与错误作用域，厂商提示仅在最终错误中作为响应头。
    pub fn gateway_attempt(
        &self,
        index: usize,
        submissions: usize,
        startup_ms: u64,
        failure: Option<crate::gateway::retry::FailureScope>,
        retry_after: Option<std::time::Duration>,
    ) {
        if let Ok(mut progress) = self.progress.lock() {
            progress.retry_after = retry_after.map(|value| {
                value
                    .as_secs()
                    .saturating_add(u64::from(value.subsec_nanos() > 0))
            });
            let retry_after_seconds = progress.retry_after;
            if let Some(attempt) = progress.record.attempts.get_mut(index)
                && let Some(routing) = &mut attempt.routing
            {
                routing["generation_budget_used"] = serde_json::json!(submissions);
                routing["startup_ms"] = serde_json::json!(startup_ms);
                routing["failure_scope"] = serde_json::json!(failure);
                routing["retry_after_seconds"] = serde_json::json!(retry_after_seconds);
            }
            self.log.update(progress.record.clone());
        }
    }

    /// 本地拒收没有生成尝试，仍返回目标的恢复提示。
    pub fn gateway_retry_after(&self, delay: std::time::Duration) {
        if let Ok(mut progress) = self.progress.lock() {
            progress.retry_after = Some(
                delay
                    .as_secs()
                    .saturating_add(u64::from(delay.subsec_nanos() > 0)),
            );
        }
    }

    /// 捕获上游建立流失败；auto 可继续尝试下一候选，不污染最终成功状态。
    pub fn failed_attempt(&self, index: usize, error: &AppError) {
        if let Ok(mut progress) = self.progress.lock() {
            if progress.finalized {
                return;
            }
            if let Some(attempt) = progress.record.attempts.get_mut(index) {
                attempt.duration_ms =
                    (self.started.elapsed().as_millis() as u64).saturating_sub(attempt.offset_ms);
                attempt.outcome = "error".into();
                attempt.upstream_status = upstream_status(error);
                attempt.error = Some(error_label(error));
            }
            self.log.update(progress.record.clone());
        }
    }

    /// 保持真实事件和取消语义，观察用量、终止原因与流中错误。
    pub fn observe(
        self: &Arc<Self>,
        stream: EventStream,
        index: usize,
        model: &str,
    ) -> EventStream {
        if let Ok(mut progress) = self.progress.lock() {
            progress.record.resolved_model = model.chars().take(256).collect();
        }
        Box::pin(ObservedEvents {
            stream,
            audit: self.clone(),
            index,
            ended: false,
            tools: ToolCallAccumulator::new(),
            reply: self.client.infer_history.then(|| {
                self.sessions.reply_for(
                    self.client
                        .client
                        .unwrap_or(crate::client_plugins::ClientKind::Generic),
                )
            }),
        })
    }

    /// 只保存错误码和状态；原始错误可能回显对话、URL 或凭证。
    fn error(&self, error: &AppError) {
        if let Ok(mut progress) = self.progress.lock() {
            if progress.finalized {
                return;
            }
            progress.record.error = Some(error_label(error));
            progress.record.upstream_status = upstream_status(error);
        }
    }

    /// 协议终止帧交付、HTTP body 耗尽或被丢弃时写终态；终态不被较晚取消覆盖。
    fn finish(&self, delivered: bool) {
        if let Ok(mut progress) = self.progress.lock() {
            if progress.finalized {
                return;
            }
            progress.finalized = true;
            progress.record.duration_ms = self.started.elapsed().as_millis() as u64;
            progress.record.outcome =
                if progress.record.error.is_some() || progress.record.status >= 400 {
                    "error"
                } else if !delivered {
                    "cancelled"
                } else if !progress.upstream_complete {
                    progress.record.error = Some("incomplete_response".into());
                    "error"
                } else {
                    "ok"
                }
                .into();
            let duration = progress.record.duration_ms;
            let outcome = progress.record.outcome.clone();
            for attempt in &mut progress.record.attempts {
                if attempt.outcome == "running" {
                    attempt.outcome = outcome.clone();
                    attempt.duration_ms = duration.saturating_sub(attempt.offset_ms);
                }
            }
            if progress.record.outcome == "ok"
                && let Some(history) = progress.history.take()
                && let Some(reply) = progress.reply.take()
                && let Some(session) = progress.record.session.clone()
            {
                self.sessions
                    .remember(history, reply, session, progress.record.id.clone());
            }
            self.log.update(progress.record.clone());
        }
    }
}

/// 取上游原始状态，保持与入站 HTTP 状态的区别。
fn upstream_status(error: &AppError) -> Option<u16> {
    match error {
        AppError::Upstream(error) => error.status,
        _ => None,
    }
}

/// 有界错误分类适合持久化，不保存来源不明的原始消息。
fn error_label(error: &AppError) -> String {
    let code: String = error
        .code()
        .chars()
        .filter(|char| char.is_ascii_alphanumeric() || "_-.".contains(*char))
        .take(128)
        .collect();
    format!("{code} (HTTP {})", error.status().as_u16())
}

struct ObservedEvents {
    stream: EventStream,
    audit: Arc<Audit>,
    index: usize,
    ended: bool,
    tools: ToolCallAccumulator,
    reply: Option<ReplyFingerprint>,
}

impl Stream for ObservedEvents {
    type Item = Result<Event, AppError>;

    /// 观察归一事件，并把缺终止标记或坏工具参数转换成明确的流错误。
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.ended {
            return Poll::Ready(None);
        }
        let result = self.stream.as_mut().poll_next(cx);
        match &result {
            Poll::Ready(Some(Ok(event))) => {
                if let Event::ToolCall(delta) = event {
                    self.tools.push(delta.clone());
                }
                if let Event::Text { text } = event
                    && let Some(reply) = &mut self.reply
                {
                    reply.text(text);
                }
                if let Ok(mut progress) = self.audit.progress.lock() {
                    if progress.finalized {
                        return result;
                    }
                    let elapsed = self.audit.started.elapsed().as_millis() as u64;
                    progress.record.first_event_ms.get_or_insert(elapsed);
                    match event {
                        Event::Usage(usage) => progress.record.usage = usage.clone(),
                        Event::Finish(reason) => progress.record.finish_reason = Some(*reason),
                        _ => {}
                    }
                }
            }
            Poll::Ready(Some(Err(error))) => {
                self.audit.error(error);
                self.audit.failed_attempt(self.index, error);
                self.ended = true;
            }
            Poll::Ready(None) => {
                self.ended = true;
                let validation = self.tools.finish();
                let finish = self
                    .audit
                    .progress
                    .lock()
                    .ok()
                    .and_then(|progress| progress.record.finish_reason);
                if finish.is_none() || validation.is_err() {
                    let error = AppError::Upstream(UpstreamError {
                        status: None,
                        code: if finish.is_none() {
                            "incomplete_stream"
                        } else {
                            "invalid_tool_call"
                        }
                        .into(),
                        message: "upstream response ended without a valid completion".into(),
                        retryable: false,
                        execution_unknown: true,
                    });
                    self.audit.error(&error);
                    self.audit.failed_attempt(self.index, &error);
                    return Poll::Ready(Some(Err(error)));
                }
                if let Ok(mut progress) = self.audit.progress.lock()
                    && !progress.finalized
                {
                    progress.upstream_complete = true;
                    if let (Some(reply), Ok(calls)) = (&self.reply, &validation) {
                        progress.reply = Some(self.audit.sessions.reply_digest(reply, calls));
                    }
                    if let Some(attempt) = progress.record.attempts.get_mut(self.index) {
                        attempt.duration_ms = (self.audit.started.elapsed().as_millis() as u64)
                            .saturating_sub(attempt.offset_ms);
                        attempt.outcome = "ok".into();
                    }
                }
            }
            Poll::Pending => {}
        }
        result
    }
}

struct IncomingBody {
    stream: BodyDataStream,
    audit: Arc<Audit>,
}

impl Stream for IncomingBody {
    type Item = Result<Bytes, axum::Error>;

    /// 按 handler 实际拉取的分块计数，413 后停止读取，不缓存或记录正文。
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let result = Pin::new(&mut self.stream).poll_next(cx);
        if let Ok(mut progress) = self.audit.progress.lock() {
            match &result {
                Poll::Ready(Some(Ok(bytes))) => {
                    let count = progress.record.request_body_bytes.unwrap_or(0);
                    progress.record.request_body_bytes = Some(count.saturating_add(bytes.len()));
                }
                Poll::Ready(None) => progress.record.request_body_complete = Some(true),
                _ => {}
            }
        }
        result
    }
}

struct AuditedBody {
    stream: BodyDataStream,
    audit: Arc<Audit>,
}

impl Stream for AuditedBody {
    type Item = Result<Bytes, axum::Error>;

    /// 协议终止帧或 body EOF 表示完整响应已交给传输层，不要求客户端继续读 EOF。
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let result = Pin::new(&mut self.stream).poll_next(cx);
        match &result {
            Poll::Ready(None) => self.audit.finish(true),
            Poll::Ready(Some(Ok(_)))
                if self
                    .audit
                    .protocol_terminal_ready
                    .swap(false, Ordering::AcqRel) =>
            {
                self.audit.finish(true);
            }
            Poll::Ready(Some(Err(_))) => {
                self.audit.error(&AppError::Internal {
                    message: "response body error".into(),
                });
                self.audit.finish(false);
            }
            _ => {}
        }
        result
    }
}

impl Drop for AuditedBody {
    /// 下游断开会丢弃 body，原有 CancelOnDrop 随内部 SSE 一同触发。
    fn drop(&mut self) {
        self.audit.finish(false);
    }
}

struct HandlerGuard(Option<Arc<Audit>>);

impl Drop for HandlerGuard {
    /// handler 等待上游期间被取消也必须留下终态。
    fn drop(&mut self) {
        if let Some(audit) = &self.0 {
            audit.finish(false);
        }
    }
}

/// 所有推理协议共用审计入口，覆盖鉴权、JSON/413 拒收与流取消。
pub async fn middleware(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let started = Instant::now();
    let (trace_id, parent_span_id, flags) = trace_context(request.headers());
    let span_id = span_id();
    let traceparent = format!("00-{trace_id}-{span_id}-{flags}");
    let client = crate::client_plugins::identify(
        request.headers(),
        request
            .extensions()
            .get::<crate::client_plugins::ClientKind>()
            .copied(),
    );
    let record = RequestRecord {
        id: new_record_id(),
        at: now_seconds(),
        protocol: match request.uri().path() {
            "/v1/messages" => "messages",
            "/v1/responses" => "responses",
            _ => "chat",
        }
        .into(),
        trace_id,
        span_id,
        parent_span_id,
        client: client.client,
        session_id: client.session.as_ref().map(|session| session.id.clone()),
        session: client.session.clone(),
        outcome: "running".into(),
        request_body_bytes: Some(0),
        request_body_complete: Some(false),
        request_content_length: request
            .headers()
            .get("content-length")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok()),
        request_body_limit_bytes: Some(state.config.max_request_body_bytes),
        ..Default::default()
    };
    let id = record.id.clone();
    state.records.push(record.clone());
    let audit = Arc::new(Audit {
        log: state.records,
        started,
        client,
        sessions: state.client_sessions,
        protocol_terminal_ready: AtomicBool::new(false),
        progress: Mutex::new(Progress {
            record,
            upstream_complete: false,
            finalized: false,
            history: None,
            reply: None,
            retry_after: None,
        }),
    });
    let mut guard = HandlerGuard(Some(audit.clone()));
    let (parts, body) = request.into_parts();
    let request = Request::from_parts(
        parts,
        Body::from_stream(IncomingBody {
            stream: body.into_data_stream(),
            audit: audit.clone(),
        }),
    );
    let mut response = CURRENT.scope(audit.clone(), next.run(request)).await;
    if let Ok(mut progress) = audit.progress.lock() {
        progress.record.status = response.status().as_u16();
    }
    if let Some(error) = response.extensions().get::<AppError>() {
        audit.error(error);
    }
    if (response.status().is_client_error() || response.status().is_server_error())
        && let Some(seconds) = audit
            .progress
            .lock()
            .ok()
            .and_then(|progress| progress.retry_after)
        && let Ok(value) = HeaderValue::from_str(&seconds.to_string())
    {
        response.headers_mut().insert("retry-after", value);
    }
    response.headers_mut().insert(
        "x-request-id",
        HeaderValue::from_str(&id).expect("generated request id"),
    );
    response.headers_mut().insert(
        "traceparent",
        HeaderValue::from_str(&traceparent).expect("generated traceparent"),
    );
    let (parts, body) = response.into_parts();
    let response = Response::from_parts(
        parts,
        Body::from_stream(AuditedBody {
            stream: body.into_data_stream(),
            audit,
        }),
    );
    // 生命周期已交给 body，防止 handler 返回时误记取消。
    guard.0.take();
    response
}
