//! 一次入站调用的总预算，适配器内部重新提交也使用同一上下文。

use super::GatewaySettings;
use crate::core::{AppError, UpstreamError};
use rand_core::{OsRng, RngCore};
use serde::Serialize;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, SystemTime};
use tokio::time::Instant;

tokio::task_local! {
    pub static ATTEMPT: Arc<AttemptContext>;
}

/// 每请求尝试上限与 deadline；生成提交前消耗预算。
pub struct AttemptContext {
    remaining: AtomicUsize,
    initial_budget: usize,
    gateway: Arc<super::Gateway>,
    settings: GatewaySettings,
    pub deadline: Instant,
    retry_after: Mutex<Option<Duration>>,
    target: Mutex<Option<(String, u32)>>,
}

impl AttemptContext {
    /// 初始化共享预算；辅助元数据和鉴权请求仍受外层 deadline 约束。
    pub fn new(
        settings: &GatewaySettings,
        max_attempts: usize,
        gateway: Arc<super::Gateway>,
    ) -> Arc<Self> {
        Arc::new(Self {
            remaining: AtomicUsize::new(max_attempts),
            initial_budget: max_attempts,
            gateway,
            settings: settings.clone(),
            deadline: Instant::now() + Duration::from_millis(settings.total_timeout_ms),
            retry_after: Mutex::new(None),
            target: Mutex::new(None),
        })
    }

    /// 初次调用和适配器重新生成共享原子计数。
    pub fn spend(&self) -> Result<(), AppError> {
        if Instant::now() >= self.deadline {
            return Err(gateway_error(
                504,
                "gateway_deadline",
                "gateway request deadline exceeded",
                false,
            ));
        }
        self.remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .map_err(|_| {
                gateway_error(
                    503,
                    "gateway_attempts_exhausted",
                    "gateway generation attempt budget exhausted",
                    false,
                )
            })?;
        Ok(())
    }

    /// 审计同时记录内部重新提交，避免仅用目标切换数代表生成次数。
    pub fn used(&self) -> usize {
        self.initial_budget
            .saturating_sub(self.remaining.load(Ordering::SeqCst))
    }

    /// 取出最近一次拒绝的恢复时间，避免其污染下一目标。
    pub fn take_retry_after(&self) -> Option<Duration> {
        self.retry_after
            .lock()
            .ok()
            .and_then(|mut value| value.take())
    }

    /// 每个新目标开始前清除上一次响应的恢复提示。
    pub fn reset(&self) {
        self.take_retry_after();
    }

    /// 适配器重新提交沿用当前服务及其 generation 的限频配置。
    pub fn target(&self, backend: &str, qps: u32) {
        *self.target.lock().unwrap_or_else(|p| p.into_inner()) = Some((backend.into(), qps));
    }

    /// 等待限频期间不消耗生成或重试预算，拒绝预算时退回预占。
    async fn additional(&self) -> Result<(), AppError> {
        let target = self
            .target
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let permit = if let Some((backend, qps)) = target {
            Some(
                self.gateway
                    .submission_permit(&backend, qps, &self.settings, self.deadline)
                    .await?,
            )
        } else {
            None
        };
        self.spend()?;
        if let Some(permit) = permit {
            permit.commit();
        }
        Ok(())
    }
}

/// 适配器内部再次提交生成前调用；不在网关内时维持直接调用兼容。
pub async fn additional_submission() -> Result<(), AppError> {
    match ATTEMPT.try_with(Arc::clone) {
        Ok(context) => context.additional().await,
        Err(_) => Ok(()),
    }
}

/// 保存 Retry-After 秒数或 HTTP 日期，不改变公共错误结构。
pub fn capture_retry_after(response: &reqwest::Response) {
    let value = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok());
    let delay = value.and_then(|value| {
        value
            .parse::<u64>()
            .ok()
            .map(Duration::from_secs)
            .or_else(|| {
                let format = time::format_description::parse_borrowed::<2>(
                "[weekday repr:short], [day] [month repr:short] [year] [hour]:[minute]:[second] GMT"
            ).ok()?;
                let date = time::PrimitiveDateTime::parse(value, &format)
                    .ok()?
                    .assume_utc();
                let now = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .ok()?
                    .as_secs();
                Some(Duration::from_secs(
                    (date.unix_timestamp().max(0) as u64).saturating_sub(now),
                ))
            })
    });
    if let Some(delay) = delay {
        let _ = ATTEMPT.try_with(|context| {
            if let Ok(mut value) = context.retry_after.lock() {
                *value = Some(delay);
            }
        });
    }
}

/// 使用统一错误形状返回网关失败，执行未知时禁止适配器重放。
pub fn gateway_error(status: u16, code: &str, message: &str, unknown: bool) -> AppError {
    AppError::Upstream(UpstreamError {
        status: Some(status),
        code: code.into(),
        message: message.into(),
        retryable: false,
        execution_unknown: unknown,
    })
}

/// 故障隔离作用域；状态码不足以推断 403 的实际影响范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureScope {
    Caller,
    Account,
    AccountModel,
    BackendModel,
    Backend,
}

/// 判定失败是否可以换目标；未知执行与一般请求错误永不重放。
pub fn classify(error: &AppError) -> (FailureScope, bool) {
    let AppError::Upstream(upstream) = error else {
        return (FailureScope::Caller, false);
    };
    let code = upstream.code.to_ascii_lowercase();
    if code == "gateway_qps_timeout" {
        return (FailureScope::Caller, false);
    }
    let scope = if code.contains("account_model")
        || code.contains("model_permission")
        || code == "qoder_resource_limit"
    {
        FailureScope::AccountModel
    } else if code.contains("queued") || code.contains("model_overload") {
        FailureScope::BackendModel
    } else if code.contains("quota")
        || code.contains("insufficient")
        || matches!(upstream.status, Some(401 | 402 | 429))
    {
        FailureScope::Account
    } else if upstream.status == Some(403) {
        if code.contains("auth") || code.contains("credential") {
            FailureScope::Account
        } else {
            FailureScope::Caller
        }
    } else {
        FailureScope::Backend
    };
    let retry = !upstream.execution_unknown
        && scope != FailureScope::Caller
        && (upstream.retryable
            || scope == FailureScope::AccountModel
            || upstream.status == Some(401)
            || code.contains("quota"));
    (scope, retry)
}

/// 有界 full jitter；只在需要下一次安全尝试时使用。
pub fn backoff(settings: &GatewaySettings, retry: usize) -> Duration {
    let cap = settings
        .backoff_base_ms
        .saturating_mul(1u64 << retry.min(16))
        .min(settings.backoff_cap_ms);
    Duration::from_millis(OsRng.next_u64() % cap.saturating_add(1))
}
