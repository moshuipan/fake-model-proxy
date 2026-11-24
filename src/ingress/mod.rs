//! 入站协议适配层。
//!
//! 每个入口只做两件事：把协议请求解析成 [`crate::core::ChatRequest`]，
//! 把归一事件编码回协议响应。

pub mod chat;
pub mod messages;
pub mod models;
pub mod responses;

use crate::core::ChatRequest;
use crate::core::error::AppError;
use crate::core::stream::EventStream;
use crate::routing::RoutePolicy;
use crate::state::AppState;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::http::{HeaderMap, StatusCode};
use tokio_util::sync::CancellationToken;

/// 把框架缓冲拒收转成稳定协议错误，保持 413 与其他读取失败的语义区别。
pub(crate) fn buffered_body(
    state: &AppState,
    body: Result<Bytes, BytesRejection>,
) -> Result<Bytes, AppError> {
    body.map_err(|error| {
        if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
            AppError::PayloadTooLarge {
                limit_bytes: state.config.max_request_body_bytes,
            }
        } else {
            AppError::bad_request("failed to read request body")
        }
    })
}

/// 这里只返回明确的厂商会话参数；推断组另由网关调度读取。
pub(crate) fn session_id(headers: &HeaderMap) -> Option<String> {
    if let Some(audit) = crate::audit::current() {
        return audit.routing_session();
    }
    crate::client_plugins::identify(headers, None)
        .session
        .and_then(|session| session.routing_session())
}

/// 统计请求输入字符数，用于记录规模。
pub fn input_chars(request: &ChatRequest) -> usize {
    request
        .messages
        .iter()
        .map(|message| message.text_content().chars().count())
        .sum()
}

/// 启动一次生成。
///
/// 显式模型走精确寻址；`auto` 系列走规则路由。返回事件流与实际模型地址，
/// 后者用于响应里的 model 字段，便于客户端与日志对齐。
pub async fn start_generation(
    state: &AppState,
    request: ChatRequest,
    cancel: CancellationToken,
) -> Result<(EventStream, String), AppError> {
    let audit = crate::audit::current();
    if let Some(audit) = &audit {
        audit.request(&request);
    }
    crate::core::history::validate_tool_history(&request.messages)?;
    let started = tokio::time::Instant::now();
    let initial_budget =
        std::time::Duration::from_millis(state.router.gateway.settings().total_timeout_ms);
    tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(crate::gateway::retry::gateway_error(499, "request_cancelled", "request cancelled", false)),
        _ = tokio::time::sleep(initial_budget) => return Err(crate::gateway::retry::gateway_error(504, "gateway_deadline", "gateway request deadline exceeded", false)),
        _ = state.router.metadata.refresh(&state.registry) => {}
    }
    let (registry, router, mut settings) = {
        let _guard = state
            .generation_lock
            .read()
            .unwrap_or_else(|p| p.into_inner());
        (
            state.registry.fork(),
            state.router.fork(),
            state.router.gateway.settings(),
        )
    };
    // 元数据读取计入同一总预算；后续快照捕获不扩大已给定的时限。
    settings.total_timeout_ms = settings
        .total_timeout_ms
        .min(initial_budget.as_millis() as u64)
        .saturating_sub(started.elapsed().as_millis() as u64);
    if settings.total_timeout_ms == 0 {
        return Err(crate::gateway::retry::gateway_error(
            504,
            "gateway_deadline",
            "gateway request deadline exceeded",
            false,
        ));
    }
    if let Some((policy, free_only)) = RoutePolicy::parse(&request.model) {
        let (events, address) = router
            .gateway
            .execute_with_settings(
                &router,
                &registry,
                request,
                crate::gateway::DispatchOptions {
                    policy,
                    free_only,
                    explicit: None,
                    max_attempts: settings.max_attempts,
                },
                cancel,
                settings,
            )
            .await?;
        return Ok((events, registry.public_model_id(&address)));
    }
    let (backend, upstream_model) = registry.resolve(&request.model)?;
    let (events, address) = router
        .gateway
        .execute_with_settings(
            &router,
            &registry,
            request,
            crate::gateway::DispatchOptions {
                policy: RoutePolicy::Spread,
                free_only: false,
                explicit: Some((backend, upstream_model)),
                max_attempts: settings.max_attempts,
            },
            cancel,
            settings,
        )
        .await?;
    Ok((events, registry.public_model_id(&address)))
}
