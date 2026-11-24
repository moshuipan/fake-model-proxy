//! 后端级健康与延迟统计。
//!
//! 与账号冷却互补：账号池解决「同一后端里的某个凭证坏了」，这里解决
//! 「整个后端不可用或很慢」，避免 auto 反复选中同一个坏后端。

use crate::accounts::FailureKind;
use crate::backend::CostTier;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 单个后端的健康状态。
#[derive(Debug, Clone, Default)]
struct BackendHealth {
    cooldown_until: Option<Instant>,
    consecutive_failures: u32,
    /// 指数加权平均延迟（毫秒）。
    latency_ewma_ms: Option<f64>,
    /// 最近一次成功时间。
    last_success: Option<Instant>,
}

/// 健康快照，供管理页展示。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthSnapshot {
    pub backend_id: String,
    pub cooling: bool,
    pub cooldown_remaining_secs: u64,
    pub consecutive_failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
}

/// 健康追踪器。按后端 id 记录，读写都在短临界区内完成。
#[derive(Default)]
pub struct HealthTracker {
    states: Mutex<HashMap<String, BackendHealth>>,
}

impl HealthTracker {
    /// 创建空追踪器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 后端当前是否可参与路由。
    pub fn is_available(&self, backend_id: &str) -> bool {
        let Ok(states) = self.states.lock() else {
            return true;
        };
        let Some(state) = states.get(backend_id) else {
            return true;
        };
        state
            .cooldown_until
            .map(|until| until <= Instant::now())
            .unwrap_or(true)
    }

    /// 记录一次成功，更新延迟并清除冷却。
    pub fn report_success(&self, backend_id: &str, latency: Duration) {
        let Ok(mut states) = self.states.lock() else {
            return;
        };
        let state = states.entry(backend_id.to_string()).or_default();
        state.cooldown_until = None;
        state.consecutive_failures = 0;
        state.last_success = Some(Instant::now());
        let sample = latency.as_millis() as f64;
        // 0.3 的权重让近期样本有影响，又不被单次抖动主导。
        state.latency_ewma_ms = Some(match state.latency_ewma_ms {
            Some(previous) => previous * 0.7 + sample * 0.3,
            None => sample,
        });
    }

    /// 记录一次失败，按类型进入冷却。
    pub fn report_failure(&self, backend_id: &str, kind: FailureKind) {
        let Ok(mut states) = self.states.lock() else {
            return;
        };
        let state = states.entry(backend_id.to_string()).or_default();
        state.consecutive_failures += 1;
        let multiplier = state.consecutive_failures.min(4);
        state.cooldown_until = Some(Instant::now() + kind.cooldown() * multiplier);
    }

    /// 返回后端延迟估计（毫秒）。没有样本时返回 None。
    pub fn latency_ms(&self, backend_id: &str) -> Option<u64> {
        let states = self.states.lock().ok()?;
        states
            .get(backend_id)
            .and_then(|state| state.latency_ewma_ms)
            .map(|value| value.round() as u64)
    }

    /// 导出全部后端状态快照。
    pub fn snapshot(&self) -> Vec<HealthSnapshot> {
        let Ok(states) = self.states.lock() else {
            return Vec::new();
        };
        let now = Instant::now();
        let mut result = states
            .iter()
            .map(|(backend_id, state)| {
                let remaining = state
                    .cooldown_until
                    .filter(|until| *until > now)
                    .map(|until| until.duration_since(now).as_secs())
                    .unwrap_or(0);
                HealthSnapshot {
                    backend_id: backend_id.clone(),
                    cooling: remaining > 0,
                    cooldown_remaining_secs: remaining,
                    consecutive_failures: state.consecutive_failures,
                    latency_ms: state.latency_ewma_ms.map(|value| value.round() as u64),
                }
            })
            .collect::<Vec<_>>();
        result.sort_by(|left, right| left.backend_id.cmp(&right.backend_id));
        result
    }
}

/// 按费用与延迟综合排序时的比较键。
///
/// 未知延迟排在已有样本之后，避免「没有数据」被误判为最快。
pub fn latency_key(latency_ms: Option<u64>) -> u64 {
    latency_ms.unwrap_or(u64::MAX)
}

/// 判断费用层级是否允许。
pub fn cost_allowed(tier: CostTier, free_only: bool) -> bool {
    if !free_only {
        return true;
    }
    matches!(tier, CostTier::Free | CostTier::Subscription)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 成功一次后应记录延迟样本。
    #[test]
    fn success_records_latency() {
        let tracker = HealthTracker::new();
        tracker.report_success("b", Duration::from_millis(120));
        assert_eq!(tracker.latency_ms("b"), Some(120));
        assert!(tracker.is_available("b"));
    }

    /// 失败后进入冷却，期间不可用；成功恢复。
    #[test]
    fn failure_cools_down_then_recovers() {
        let tracker = HealthTracker::new();
        tracker.report_failure("b", FailureKind::RateLimit);
        assert!(!tracker.is_available("b"));
        tracker.report_success("b", Duration::from_millis(50));
        assert!(tracker.is_available("b"));
    }

    /// 无样本的后端延迟视为未知，排序时排最后。
    #[test]
    fn unknown_latency_sorts_last() {
        assert!(latency_key(None) > latency_key(Some(10_000)));
    }

    /// 免费筛选只接受免费与套餐内。
    #[test]
    fn free_only_filter() {
        assert!(cost_allowed(CostTier::Free, true));
        assert!(cost_allowed(CostTier::Subscription, true));
        assert!(!cost_allowed(CostTier::Paid, true));
        assert!(!cost_allowed(CostTier::Unknown, true));
        assert!(cost_allowed(CostTier::Paid, false));
    }
}
