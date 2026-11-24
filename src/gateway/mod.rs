//! 统一生成生命周期：会话调度、安全恢复、启动检查和完整流负载。

pub mod balancer;
pub mod retry;
mod settings;

use crate::backend::Registry;
use crate::core::stream::EventStream;
use crate::core::{AppError, ChatRequest, Event};
use crate::routing::{Candidate, Decision, RoutePolicy, Router};
use balancer::{Balancer, Lease, RatePermit, ReserveError};
use futures_util::{Stream, StreamExt};
use retry::{AttemptContext, FailureScope, gateway_error};
pub use settings::{GatewaySettings, MAX_SERVICE_QPS, TargetSettings};
use std::collections::HashSet;
use std::pin::Pin;
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicUsize, Ordering},
};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::{Instant, Sleep};
use tokio_util::sync::CancellationToken;

/// 独立于后端实例生命周期的运行时。
pub struct Gateway {
    settings: RwLock<GatewaySettings>,
    pub balancer: Arc<Balancer>,
    waiting: AtomicUsize,
}

/// 一次执行允许的路由范围；显式地址与自动策略使用相同生命周期。
pub struct DispatchOptions {
    pub policy: RoutePolicy,
    pub free_only: bool,
    pub explicit: Option<(Arc<dyn crate::backend::Backend>, String)>,
    pub max_attempts: usize,
}

impl Default for Gateway {
    /// 构造默认运行时，配置加载仅替换设置。
    fn default() -> Self {
        let settings = GatewaySettings::default();
        Self {
            settings: RwLock::new(settings),
            balancer: Default::default(),
            waiting: AtomicUsize::new(0),
        }
    }
}

impl Gateway {
    /// 管理界面只读取运行计数，不包含可重建会话指纹。
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "waiting": self.waiting.load(Ordering::SeqCst),
            "balancer": self.balancer.snapshot(),
        })
    }
    /// 读取本次请求一致的设置快照。
    pub fn settings(&self) -> GatewaySettings {
        self.settings
            .read()
            .map(|settings| settings.clone())
            .unwrap_or_default()
    }

    /// 热更新不清除绑定、容量与局部冷却。
    pub fn configure(&self, settings: GatewaySettings) -> Result<(), AppError> {
        settings.validate()?;
        self.publish_settings(settings);
        Ok(())
    }

    /// 配置服务已预校验后的无失败发布，保留长期绑定与健康状态。
    pub(crate) fn publish_settings(&self, settings: GatewaySettings) {
        *self.settings.write().unwrap_or_else(|p| p.into_inner()) = settings;
        self.balancer.changed.notify_waiters();
    }

    /// 显式和自动模型共用调度；显式请求只使用指定后端与模型的账号。
    pub async fn execute(
        self: &Arc<Self>,
        router: &Router,
        registry: &Registry,
        request: ChatRequest,
        options: DispatchOptions,
        cancel: CancellationToken,
    ) -> Result<(EventStream, String), AppError> {
        self.execute_with_settings(router, registry, request, options, cancel, self.settings())
            .await
    }

    /// 应用请求使用与注册表同一 generation 捕获的预算配置。
    pub async fn execute_with_settings(
        self: &Arc<Self>,
        router: &Router,
        registry: &Registry,
        request: ChatRequest,
        options: DispatchOptions,
        cancel: CancellationToken,
        settings: GatewaySettings,
    ) -> Result<(EventStream, String), AppError> {
        let DispatchOptions {
            policy,
            free_only,
            explicit,
            max_attempts,
        } = options;
        let free_only = free_only || (explicit.is_none() && !settings.auto_allow_paid_fallback);
        let context = AttemptContext::new(
            &settings,
            max_attempts.min(settings.max_attempts).max(1),
            self.clone(),
        );
        let audit = crate::audit::current();
        let group = audit
            .as_ref()
            .and_then(|audit| audit.routing_group())
            .or_else(|| request.session.clone())
            .unwrap_or_else(|| format!("new:{}", uuid::Uuid::new_v4().simple()));
        // 兼容名称共享同一个调度身份，切换别名不拆散同一会话。
        let group_model = if explicit.is_none() {
            "auto"
        } else {
            &request.model
        };
        let group = serde_json::to_string(&(group, group_model, policy.as_str(), free_only))
            .unwrap_or_default();
        // 元数据等待也包含在请求 deadline；取消后不继续刷新或分配。
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(cancelled()),
            _ = tokio::time::sleep_until(context.deadline) => return Err(deadline()),
            _ = router.metadata.refresh(registry) => {}
        }
        let mut excluded = HashSet::new();
        let mut attempted = 0usize;
        let mut last = None;
        loop {
            if cancel.is_cancelled() {
                return Err(cancelled());
            }
            if attempted > 0 {
                let delay = retry::backoff(&settings, attempted - 1);
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(cancelled()),
                    _ = tokio::time::sleep_until(context.deadline) => return Err(deadline()),
                    _ = tokio::time::sleep(delay) => {}
                }
            }
            let decision = if let Some((backend, model)) = &explicit {
                explicit_plan(router, backend.clone(), model, &request)
            } else {
                router.plan(registry, &request, policy, free_only)
            };
            if decision.candidates.is_empty() {
                if let Some(audit) = &audit {
                    audit.rejected_routes(&decision);
                }
                if last.is_none()
                    && let Some((error, hint)) = self.balancer.blocked_error(&decision.blocked)
                {
                    if let Some(audit) = &audit {
                        audit.gateway_retry_after(hint);
                    }
                    return Err(error);
                }
                return Err(last
                    .map(|error| exhausted(Some(&error)))
                    .unwrap_or_else(|| no_target(&decision)));
            }
            let queue_deadline = (Instant::now()
                + Duration::from_millis(settings.queue_timeout_ms))
            .min(context.deadline);
            let (index, lease) = loop {
                // 注册通知后再检查条件，避免在释放容量与等待之间丢失唤醒。
                let changed = self.balancer.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                match self.balancer.reserve(
                    &decision.candidates,
                    &group,
                    policy,
                    &settings,
                    &excluded,
                ) {
                    Ok(value) => break value,
                    Err(ReserveError::Unavailable) => {
                        if last.is_none()
                            && let Some((error, hint)) =
                                self.balancer.blocked_error(&decision.candidates)
                        {
                            if let Some(audit) = &audit {
                                audit.gateway_retry_after(hint);
                            }
                            return Err(error);
                        }
                        return Err(exhausted(last.as_ref()));
                    }
                    Err(ReserveError::GroupLimit) => {
                        return Err(gateway_error(
                            503,
                            "gateway_group_capacity",
                            "gateway session group capacity reached",
                            false,
                        ));
                    }
                    Err(error @ (ReserveError::Busy | ReserveError::RateLimited(_))) => {
                        let rate_deadline = match error {
                            ReserveError::RateLimited(until) => Some(until),
                            _ => None,
                        };
                        self.wait_for_slot(
                            &settings,
                            queue_deadline,
                            context.deadline,
                            rate_deadline,
                            &cancel,
                            &mut changed,
                        )
                        .await?;
                    }
                }
            };
            if let Err(error) = context.spend() {
                lease.abandoned();
                // 预算拒绝没有再次提交生成，保留已确认的限流；deadline 仍独立返回超时。
                return Err(
                    if error.code() == "gateway_attempts_exhausted"
                        && last
                            .as_ref()
                            .is_some_and(|error| error.status().as_u16() == 429)
                    {
                        exhausted(last.as_ref())
                    } else {
                        error
                    },
                );
            }
            lease.submitted();
            context.reset();
            attempted += 1;
            let candidate = &decision.candidates[index];
            context.target(candidate.backend.id(), candidate.backend.qps());
            let address = candidate.address();
            let attempt_index = audit.as_ref().map(|audit| {
                let index = audit.begin_attempt(&address);
                audit.route_attempt(index, candidate, policy, &lease);
                index
            });
            let started = Instant::now();
            let mut input = request.clone();
            input.model = candidate.model.clone();
            if explicit.is_none() && candidate.limits.output_enforced {
                input.max_tokens = Some(candidate.limits.output_reserve);
            }
            // 客户端 ID 只用于分组；厂商句柄跟随绑定，迁移或 TTL 后使用新 UUID。
            // 推断组仍使用完整历史，不建立厂商侧会话亲和。
            input.session = request
                .session
                .as_ref()
                .map(|_| lease.upstream_session.clone());
            let attempt_cancel = cancel.child_token();
            let attempt_guard = attempt_cancel.clone().drop_guard();
            let attempt_deadline = (started + Duration::from_millis(settings.startup_timeout_ms))
                .min(context.deadline);
            let result = retry::ATTEMPT.scope(context.clone(), async {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => Err(cancelled()),
                    _ = tokio::time::sleep_until(attempt_deadline) => Err(gateway_error(504,
                        "upstream_startup_timeout", "upstream startup timed out; execution unknown", true)),
                    result = async {
                        let stream = candidate.backend.chat_on_account(input, attempt_cancel.clone(), candidate.account_id.as_deref()).await?;
                        prime(stream).await
                    } => result,
                }
            }).await;
            match result {
                Ok(stream) => {
                    attempt_guard.disarm();
                    if let (Some(audit), Some(index)) = (&audit, attempt_index) {
                        audit.gateway_attempt(
                            index,
                            context.used(),
                            started.elapsed().as_millis() as u64,
                            None,
                            None,
                        );
                    }
                    if let Some(key) =
                        crate::routing::planner::affinity_key(&request, policy, free_only, 0)
                    {
                        router.affinity.bind(&key, &candidate.binding());
                    }
                    let stream: EventStream = Box::pin(TrackedStream {
                        stream,
                        lease: Some(lease),
                        balancer: self.balancer.clone(),
                        candidate: candidate.clone(),
                        health: router.health.clone(),
                        startup_latency: started.elapsed(),
                        deadline: context.deadline,
                        timer: Box::pin(tokio::time::sleep_until(
                            (Instant::now() + Duration::from_millis(settings.idle_timeout_ms))
                                .min(context.deadline),
                        )),
                        idle: Duration::from_millis(settings.idle_timeout_ms),
                        cancelled: Box::pin(attempt_cancel.clone().cancelled_owned()),
                        cancel: attempt_cancel,
                        finish: None,
                        ended: false,
                    });
                    let stream = match &router.usage {
                        Some(usage) => crate::usage::metered(
                            stream,
                            usage.clone(),
                            candidate.backend.id().into(),
                        ),
                        None => stream,
                    };
                    let stream = match (&audit, attempt_index) {
                        (Some(audit), Some(index)) => audit.observe(stream, index, &address),
                        _ => stream,
                    };
                    return Ok((stream, address));
                }
                Err(error) => {
                    attempt_cancel.cancel();
                    if let (Some(audit), Some(index)) = (&audit, attempt_index) {
                        audit.failed_attempt(index, &error);
                    }
                    let (scope, safe) = retry::classify(&error);
                    let hint = context.take_retry_after().or_else(|| {
                        crate::backend::qoder::queue_retry_after(&error).map(Duration::from_secs)
                    });
                    if let (Some(audit), Some(index)) = (&audit, attempt_index) {
                        audit.gateway_attempt(
                            index,
                            context.used(),
                            started.elapsed().as_millis() as u64,
                            Some(scope),
                            hint,
                        );
                    }
                    if !cancel.is_cancelled() {
                        let code = error.code().to_ascii_lowercase();
                        if code.contains("quota")
                            || code.contains("insufficient")
                            || error.status().as_u16() == 402
                        {
                            tokio::select! {
                                _ = cancel.cancelled() => return Err(cancelled()),
                                _ = tokio::time::sleep_until(context.deadline) => return Err(deadline()),
                                _ = router.metadata.invalidate_quota(candidate) => {},
                            }
                        }
                        self.balancer.failure_with_status(
                            candidate,
                            scope,
                            hint.unwrap_or(Duration::from_secs(
                                if scope == FailureScope::Account {
                                    60
                                } else {
                                    15
                                },
                            )),
                            &error.code(),
                            Some(error.status().as_u16()),
                        );
                        // 保留旧目标作为迁移证据；下一请求不能把厂商会话句柄带到新账号。
                    }
                    drop(lease);
                    if !safe {
                        return Err(error);
                    }
                    excluded.insert(balancer::scope_key(candidate, scope));
                    // 本次请求不在限流账号上换模型连发；持久冷却仍使用上游的故障范围。
                    if error.status().as_u16() == 429 {
                        excluded.insert(balancer::scope_key(candidate, FailureScope::Account));
                    }
                    if attempted >= max_attempts.min(settings.max_attempts) {
                        return Err(exhausted(Some(&error)));
                    }
                    last = Some(error);
                }
            }
        }
    }

    /// 容量和限频共用有限队列；通知已在原子检查前注册，限频另有定时唤醒。
    async fn wait_for_slot(
        &self,
        settings: &GatewaySettings,
        queue_deadline: Instant,
        hard_deadline: Instant,
        rate_deadline: Option<Instant>,
        cancel: &CancellationToken,
        changed: impl std::future::Future<Output = ()>,
    ) -> Result<(), AppError> {
        if self.waiting.fetch_add(1, Ordering::SeqCst) >= settings.queue_capacity {
            self.waiting.fetch_sub(1, Ordering::SeqCst);
            return Err(gateway_error(
                503,
                "gateway_queue_full",
                "gateway admission queue is full",
                false,
            ));
        }
        let _guard = Waiting(&self.waiting);
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(cancelled()),
            _ = tokio::time::sleep_until(queue_deadline) => Err(if queue_deadline == hard_deadline {
                deadline()
            } else if let Some(until) = rate_deadline {
                qps_timeout(until)
            } else {
                gateway_error(503, "gateway_capacity_timeout", "gateway capacity wait timed out", false)
            }),
            _ = changed => Ok(()),
            _ = tokio::time::sleep_until(rate_deadline.unwrap_or(queue_deadline)) => Ok(()),
        }
    }

    /// 内部生成重试只获取服务限频预占，流租约仍持有原来的容量。
    pub(super) async fn submission_permit(
        self: &Arc<Self>,
        backend: &str,
        qps: u32,
        settings: &GatewaySettings,
        deadline: Instant,
    ) -> Result<RatePermit, AppError> {
        let queue_deadline =
            (Instant::now() + Duration::from_millis(settings.queue_timeout_ms)).min(deadline);
        let cancel = CancellationToken::new();
        loop {
            let changed = self.balancer.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            match self.balancer.reserve_submission(backend, qps) {
                Ok(permit) => return Ok(permit),
                Err(ReserveError::RateLimited(until)) => {
                    self.wait_for_slot(
                        settings,
                        queue_deadline,
                        deadline,
                        Some(until),
                        &cancel,
                        &mut changed,
                    )
                    .await?;
                }
                Err(_) => {
                    return Err(gateway_error(
                        503,
                        "gateway_unavailable",
                        "gateway admission unavailable",
                        false,
                    ));
                }
            }
        }
    }
}

/// 本地限频超时使用 429 并给出下一窗口恢复提示。
fn qps_timeout(until: Instant) -> AppError {
    if let Some(audit) = crate::audit::current() {
        audit.gateway_retry_after(
            until
                .saturating_duration_since(Instant::now())
                .max(Duration::from_secs(1)),
        );
    }
    gateway_error(
        429,
        "gateway_qps_timeout",
        "gateway service QPS wait timed out",
        false,
    )
}

/// 显式寻址保持原有模型参数，仅扩展账号选择与运行容量。
fn explicit_plan(
    router: &Router,
    backend: Arc<dyn crate::backend::Backend>,
    model: &str,
    request: &ChatRequest,
) -> Decision {
    let mut decision = Decision::default();
    if let Err(reason) = backend.validate_request(request) {
        decision.rejected.push(crate::routing::Rejection {
            model: format!("{}/{}", backend.id(), model),
            reason,
        });
        return decision;
    }
    let accounts = backend
        .routing_accounts()
        .map(|accounts| {
            accounts
                .into_iter()
                .filter(|account| account.enabled && !account.cooling)
                .map(|account| Some(account.id))
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| vec![None]);
    if router
        .usage
        .as_ref()
        .is_some_and(|usage| usage.over_budget(backend.id(), Some(backend.daily_token_budget())))
    {
        return decision;
    }
    for (account_order, account_id) in accounts.into_iter().enumerate() {
        decision.candidates.push(Candidate {
            backend: backend.clone(),
            model: model.into(),
            account_id,
            account_order,
            price_factor: None,
            comparison_scope: backend.id().into(),
            quota: Default::default(),
            cost_tier: backend.cost_tier(),
            priority: backend.priority(),
            latency_ms: router.health.latency_ms(backend.id()),
            daily_token_budget: backend.daily_token_budget(),
            limits: crate::routing::planner::EffectiveLimits {
                input_limit: None,
                context_limit: None,
                output_limit: None,
                output_reserve: request.max_tokens.unwrap_or(8192),
                output_enforced: backend.output_enforced(),
                estimated_input: crate::routing::estimate_tokens(request),
                input_margin: 0,
                estimator: "explicit",
                limit_source: "provider",
            },
        });
    }
    decision
}

/// 只缓存有界启动事件；HTTP 200、Usage 或空事件不冒充生成成功。
async fn prime(mut stream: EventStream) -> Result<EventStream, AppError> {
    let mut buffered = Vec::new();
    let mut bytes = 0usize;
    while let Some(item) = stream.next().await {
        let event = item?;
        bytes = bytes.saturating_add(serde_json::to_vec(&event).map_or(0, |value| value.len()));
        let ready = match &event {
            Event::Text { text } | Event::Reasoning { text } => !text.is_empty(),
            Event::ToolCall(delta) => {
                delta.id.is_some() || delta.name.is_some() || delta.arguments.is_some()
            }
            Event::Finish(_) => true,
            Event::Usage(_) => false,
        };
        buffered.push(Ok(event));
        if bytes > 1_048_576 || buffered.len() > 128 {
            return Err(gateway_error(
                502,
                "upstream_startup_buffer_limit",
                "upstream startup buffer exceeded",
                true,
            ));
        }
        if ready {
            return Ok(Box::pin(futures_util::stream::iter(buffered).chain(stream)));
        }
    }
    Err(gateway_error(
        502,
        "upstream_incomplete_response",
        "upstream ended without a generation event",
        true,
    ))
}

struct Waiting<'a>(&'a AtomicUsize);
impl Drop for Waiting<'_> {
    /// Future 取消时也撤销排队计数。
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct TrackedStream {
    stream: EventStream,
    lease: Option<Lease>,
    balancer: Arc<Balancer>,
    candidate: Candidate,
    health: Arc<crate::routing::HealthTracker>,
    startup_latency: Duration,
    deadline: Instant,
    timer: Pin<Box<Sleep>>,
    idle: Duration,
    cancel: CancellationToken,
    cancelled: Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
    finish: Option<Event>,
    ended: bool,
}

impl Stream for TrackedStream {
    type Item = Result<Event, AppError>;
    /// 保持容量直到流耗尽，拒绝无终止事件 EOF，并把超时作为流错误输出。
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.ended {
            return Poll::Ready(None);
        }
        if self.cancelled.as_mut().poll(cx).is_ready() {
            self.ended = true;
            self.lease.take();
            return Poll::Ready(Some(Err(cancelled())));
        }
        if std::future::Future::poll(self.timer.as_mut(), cx).is_ready() {
            let candidate = self.candidate.clone();
            self.balancer.failure(
                &candidate,
                FailureScope::BackendModel,
                Duration::from_secs(15),
                "upstream_stream_timeout",
            );
            self.ended = true;
            self.cancel.cancel();
            self.lease.take();
            return Poll::Ready(Some(Err(gateway_error(
                504,
                "upstream_stream_timeout",
                "upstream stream timed out after response started",
                true,
            ))));
        }
        loop {
            match self.stream.as_mut().poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Ok(event))) => {
                    if matches!(&event, Event::Finish(_)) {
                        if self.finish.is_some() {
                            self.ended = true;
                            self.cancel.cancel();
                            self.lease.take();
                            return Poll::Ready(Some(Err(gateway_error(
                                502,
                                "upstream_duplicate_finish",
                                "upstream emitted multiple finish events",
                                true,
                            ))));
                        }
                        self.finish = Some(event);
                        let next = (Instant::now() + self.idle).min(self.deadline);
                        self.timer.as_mut().reset(next);
                        continue;
                    }
                    let next = (Instant::now() + self.idle).min(self.deadline);
                    self.timer.as_mut().reset(next);
                    return Poll::Ready(Some(Ok(event)));
                }
                Poll::Ready(Some(Err(error))) => {
                    self.ended = true;
                    let (scope, _) = retry::classify(&error);
                    self.balancer.failure(
                        &self.candidate,
                        scope,
                        Duration::from_secs(15),
                        &error.code(),
                    );
                    self.cancel.cancel();
                    self.lease.take();
                    return Poll::Ready(Some(Err(error)));
                }
                Poll::Ready(None) => {
                    self.ended = true;
                    if let Some(finish) = self.finish.take() {
                        if let Some(lease) = &self.lease {
                            lease.completed();
                        }
                        self.lease.take();
                        self.health
                            .report_success(self.candidate.backend.id(), self.startup_latency);
                        return Poll::Ready(Some(Ok(finish)));
                    } else {
                        self.lease.take();
                        self.balancer.failure(
                            &self.candidate,
                            FailureScope::BackendModel,
                            Duration::from_secs(15),
                            "upstream_incomplete_response",
                        );
                        return Poll::Ready(Some(Err(gateway_error(
                            502,
                            "upstream_incomplete_response",
                            "upstream ended without a finish event",
                            true,
                        ))));
                    }
                }
            }
        }
    }
}

impl Drop for TrackedStream {
    /// 下游放弃读取时停止上游；容量租约由字段析构释放。
    fn drop(&mut self) {
        if !self.ended {
            self.cancel.cancel();
        }
    }
}

/// 取消不是厂商故障，稳定错误码用于审计。
fn cancelled() -> AppError {
    gateway_error(499, "request_cancelled", "request cancelled", false)
}
/// 启动响应前的全请求时限错误。
fn deadline() -> AppError {
    gateway_error(
        504,
        "gateway_deadline",
        "gateway request deadline exceeded",
        true,
    )
}
/// 候选或预算耗尽保持限流语义，其余返回网关不可用。
fn exhausted(last: Option<&AppError>) -> AppError {
    if let Some(AppError::Upstream(error)) = last
        && (error.status == Some(429)
            || error.status == Some(403)
                && retry::classify(&AppError::Upstream(error.clone())).0
                    == FailureScope::AccountModel)
    {
        return AppError::Upstream(error.clone());
    }
    gateway_error(
        503,
        "gateway_attempts_exhausted",
        "all eligible gateway targets failed",
        false,
    )
}

/// 所有候选均被请求能力或窗口排除时返回 400，资源或配置不可用返回 503。
fn no_target(decision: &Decision) -> AppError {
    let caller = !decision.rejected.is_empty()
        && decision.rejected.iter().all(|rejection| {
            [
                "tools_unsupported",
                "images_unsupported",
                "image_role_unsupported",
                "output_limit_invalid",
                "output_limit_exceeded",
                "input_limit_exceeded",
                "context_limit_exceeded",
            ]
            .iter()
            .any(|prefix| rejection.reason.starts_with(prefix))
        });
    if caller {
        AppError::bad_request(decision.summary())
    } else {
        gateway_error(
            503,
            "gateway_no_eligible_target",
            &decision.summary(),
            false,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{FinishReason, UpstreamError};

    /// 空响应没有成功证据，执行状态仍然未知。
    #[tokio::test]
    async fn empty_stream_is_an_unknown_execution_failure() {
        let error = prime(Box::pin(futures_util::stream::empty()))
            .await
            .err()
            .unwrap();
        assert!(matches!(
            error,
            AppError::Upstream(UpstreamError {
                execution_unknown: true,
                ..
            })
        ));
    }

    /// Usage 不表示生成启动，显式拒绝应在交给编码器前返回。
    #[tokio::test]
    async fn startup_error_after_usage_is_not_hidden_by_http_success() {
        let stream = futures_util::stream::iter(vec![
            Ok(Event::Usage(Default::default())),
            Err(AppError::Upstream(UpstreamError::status(429, "rejected"))),
        ]);
        let error = prime(Box::pin(stream)).await.err().unwrap();
        assert_eq!(error.status().as_u16(), 429);
    }

    /// 启动缓冲保持原始事件顺序，包括 Usage 与合法空结果的终止事件。
    #[tokio::test]
    async fn priming_preserves_events_and_accepts_a_valid_empty_completion() {
        let events = vec![
            Ok(Event::Usage(Default::default())),
            Ok(Event::Finish(FinishReason::Stop)),
        ];
        let stream = prime(Box::pin(futures_util::stream::iter(events.clone())))
            .await
            .unwrap();
        assert_eq!(stream.collect::<Vec<_>>().await, events);
    }
}
