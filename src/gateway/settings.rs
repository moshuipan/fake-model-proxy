//! 可持久化的统一网关策略；自动服务默认仅使用免费及已有套餐额度。

use crate::core::AppError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 服务滚动窗口最多保留的请求数，也是可配置的 QPS 上限。
pub const MAX_SERVICE_QPS: u32 = 10_000;

/// 后端或账号的容量与新组分配权重；账号键使用 backend/account。
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct TargetSettings {
    pub capacity: usize,
    pub weight: u32,
}

impl Default for TargetSettings {
    /// 小规模个人账号池的保守并发初值。
    fn default() -> Self {
        Self {
            capacity: 4,
            weight: 1,
        }
    }
}

/// 所有协议共用的执行预算，时间单位为毫秒。
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct GatewaySettings {
    /// 旧 default_auto_spread 字段反序列化时忽略，不能被误迁移为付费授权。
    pub auto_allow_paid_fallback: bool,
    pub max_attempts: usize,
    pub total_timeout_ms: u64,
    pub connect_timeout_ms: u64,
    pub startup_timeout_ms: u64,
    pub idle_timeout_ms: u64,
    pub backoff_base_ms: u64,
    pub backoff_cap_ms: u64,
    pub queue_timeout_ms: u64,
    pub queue_capacity: usize,
    pub group_ttl_ms: u64,
    pub group_capacity: usize,
    pub targets: HashMap<String, TargetSettings>,
}

impl Default for GatewaySettings {
    /// 建议初值，可由管理 API 调整；不是测量得出的服务保证。
    fn default() -> Self {
        Self {
            auto_allow_paid_fallback: false,
            max_attempts: 4,
            total_timeout_ms: 600_000,
            connect_timeout_ms: 10_000,
            startup_timeout_ms: 60_000,
            idle_timeout_ms: 120_000,
            backoff_base_ms: 250,
            backoff_cap_ms: 5_000,
            queue_timeout_ms: 10_000,
            queue_capacity: 128,
            group_ttl_ms: 1_800_000,
            group_capacity: 4096,
            targets: HashMap::new(),
        }
    }
}

impl GatewaySettings {
    /// 拒绝零容量、溢出时限和无界预算，防止配置导致死锁或资源耗尽。
    pub fn validate(&self) -> Result<(), AppError> {
        let times = [
            self.total_timeout_ms,
            self.connect_timeout_ms,
            self.startup_timeout_ms,
            self.idle_timeout_ms,
            self.queue_timeout_ms,
            self.group_ttl_ms,
        ];
        if !(1..=16).contains(&self.max_attempts)
            || times.iter().any(|value| *value == 0 || *value > 86_400_000)
            || self.backoff_base_ms > self.backoff_cap_ms
            || self.backoff_cap_ms > 60_000
            || self.queue_capacity == 0
            || self.queue_capacity > 100_000
            || self.group_capacity == 0
            || self.group_capacity > 100_000
            || self.targets.len() > 10_000
            || self.targets.iter().any(|(key, target)| {
                key.is_empty()
                    || key.len() > 512
                    || key.chars().any(|ch| ch.is_whitespace() || ch.is_control())
                    || target.capacity == 0
                    || target.capacity > 10_000
                    || target.weight == 0
                    || target.weight > 10_000
            })
        {
            return Err(AppError::bad_request(
                "invalid gateway limits or target capacity",
            ));
        }
        Ok(())
    }

    /// 账号缺少覆盖时容量为 4，后端缺少覆盖时容量为 16。
    pub fn target(&self, key: &str, backend: bool) -> TargetSettings {
        self.targets.get(key).cloned().unwrap_or(TargetSettings {
            capacity: if backend { 16 } else { 4 },
            weight: 1,
        })
    }
}
