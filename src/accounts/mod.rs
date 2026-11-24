//! 多账号池、健康冷却与厂商侧远程能力。
//!
//! 账号选择与冷却策略独立于具体后端：后端正向持有池，路由只看见后端的
//! 聚合可用性。这样新增一个需要账号的后端不需要改路由。

pub mod quota;
pub mod quota_refresh;
pub mod remote;

tokio::task_local! {
    /// Auto 在发起上游请求期间固定的账号，不向入站协议暴露。
    pub static ROUTED_ACCOUNT: String;
}

use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// 账号记录。凭证以 JSON 字符串保存，由各后端解释结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub backend_id: String,
    pub name: String,
    /// 凭证 JSON。禁止写入日志。
    pub credential: String,
    pub enabled: bool,
}

/// 失败类型，决定冷却时长。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// 触发限流，短冷却。
    RateLimit,
    /// 额度耗尽，长冷却。
    QuotaExhausted,
    /// 凭证失效，需要重新授权。
    AuthExpired,
    /// 瞬时网络故障，极短冷却。
    Transient,
    /// 未知失败，保守短冷却并累计降权。
    Unknown,
}

impl FailureKind {
    /// 返回该失败类型对应的冷却时长。
    pub fn cooldown(self) -> Duration {
        match self {
            FailureKind::RateLimit => Duration::from_secs(60),
            FailureKind::QuotaExhausted => Duration::from_secs(12 * 60 * 60),
            FailureKind::AuthExpired => Duration::from_secs(30 * 60),
            FailureKind::Transient => Duration::from_secs(15),
            FailureKind::Unknown => Duration::from_secs(30),
        }
    }

    /// 根据 HTTP 状态与错误码推断失败类型。
    pub fn classify(status: Option<u16>, code: &str) -> FailureKind {
        match status {
            Some(429) => FailureKind::RateLimit,
            Some(401) | Some(403) => FailureKind::AuthExpired,
            Some(500..=599) => FailureKind::Transient,
            _ => {
                let code = code.to_lowercase();
                if code.contains("quota") || code.contains("insufficient") {
                    FailureKind::QuotaExhausted
                } else if code.contains("auth") || code.contains("expired") {
                    FailureKind::AuthExpired
                } else {
                    FailureKind::Unknown
                }
            }
        }
    }
}

/// 账号运行时状态，供管理页展示。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountState {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub cooling: bool,
    pub cooldown_remaining_secs: u64,
    pub consecutive_failures: u32,
}

/// 池内槽位。
struct Slot {
    account: Account,
    cooldown_until: Option<Instant>,
    failures: u32,
}

/// 账号池。轮询选择，跳过冷却与禁用账号。
pub struct AccountPool {
    slots: Mutex<Vec<Slot>>,
    cursor: AtomicUsize,
}

impl AccountPool {
    /// 用账号列表构造池。
    pub fn new(accounts: Vec<Account>) -> Self {
        AccountPool {
            slots: Mutex::new(
                accounts
                    .into_iter()
                    .map(|account| Slot {
                        account,
                        cooldown_until: None,
                        failures: 0,
                    })
                    .collect(),
            ),
            cursor: AtomicUsize::new(0),
        }
    }

    /// 池是否为空。
    pub fn is_empty(&self) -> bool {
        self.slots
            .lock()
            .map(|slots| slots.is_empty())
            .unwrap_or(true)
    }

    /// 选择下一个可用账号。全部冷却时返回 None。
    pub fn select(&self) -> Option<Account> {
        if let Ok(id) = ROUTED_ACCOUNT.try_with(Clone::clone) {
            let account = self.get_available(&id)?;
            let slots = self.slots.lock().ok()?;
            if let Some(index) = slots.iter().position(|slot| slot.account.id == id) {
                self.cursor.store(index + 1, Ordering::Relaxed);
            }
            return Some(account);
        }
        let slots = self.slots.lock().ok()?;
        if slots.is_empty() {
            return None;
        }
        let now = Instant::now();
        let start = self.cursor.fetch_add(1, Ordering::Relaxed);
        for offset in 0..slots.len() {
            let index = (start + offset) % slots.len();
            let slot = &slots[index];
            if !slot.account.enabled {
                continue;
            }
            if slot
                .cooldown_until
                .map(|until| until > now)
                .unwrap_or(false)
            {
                continue;
            }
            return Some(slot.account.clone());
        }
        None
    }

    /// 返回指定且仍可用的账号，不推进轮询游标。
    pub fn get_available(&self, id: &str) -> Option<Account> {
        let slots = self.slots.lock().ok()?;
        let now = Instant::now();
        slots
            .iter()
            .find(|slot| {
                slot.account.id == id
                    && slot.account.enabled
                    && slot.cooldown_until.is_none_or(|until| until <= now)
            })
            .map(|slot| slot.account.clone())
    }

    /// 记录一次成功，清除冷却与失败计数。
    pub fn report_success(&self, account_id: &str) {
        if let Ok(mut slots) = self.slots.lock()
            && let Some(slot) = slots.iter_mut().find(|slot| slot.account.id == account_id)
        {
            slot.cooldown_until = None;
            slot.failures = 0;
        }
    }

    /// 记录一次失败，按类型设置冷却。
    pub fn report_failure(&self, account_id: &str, kind: FailureKind) {
        if let Ok(mut slots) = self.slots.lock()
            && let Some(slot) = slots.iter_mut().find(|slot| slot.account.id == account_id)
        {
            slot.failures += 1;
            // 连续失败按次数放大冷却，上限为 4 倍。
            let multiplier = slot.failures.min(4);
            slot.cooldown_until = Some(Instant::now() + kind.cooldown() * multiplier);
        }
    }

    /// 导出状态快照。
    pub fn snapshot(&self) -> Vec<AccountState> {
        let Ok(slots) = self.slots.lock() else {
            return Vec::new();
        };
        let now = Instant::now();
        let mut snapshot: Vec<_> = slots
            .iter()
            .map(|slot| {
                let remaining = slot
                    .cooldown_until
                    .filter(|until| *until > now)
                    .map(|until| until.duration_since(now).as_secs())
                    .unwrap_or(0);
                AccountState {
                    id: slot.account.id.clone(),
                    name: slot.account.name.clone(),
                    enabled: slot.account.enabled,
                    cooling: remaining > 0,
                    cooldown_remaining_secs: remaining,
                    consecutive_failures: slot.failures,
                }
            })
            .collect();
        if !snapshot.is_empty() {
            let start = self.cursor.load(Ordering::Relaxed) % snapshot.len();
            snapshot.rotate_left(start);
        }
        snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造测试账号。
    fn account(id: &str, enabled: bool) -> Account {
        Account {
            id: id.to_string(),
            backend_id: "b".to_string(),
            name: id.to_string(),
            credential: "{}".to_string(),
            enabled,
        }
    }

    /// 选择应按轮询推进，并跳过禁用账号。
    #[test]
    fn select_round_robins_and_skips_disabled() {
        let pool = AccountPool::new(vec![
            account("a", true),
            account("b", false),
            account("c", true),
        ]);
        let first = pool.select().expect("first");
        let second = pool.select().expect("second");
        let third = pool.select().expect("third");
        assert_ne!(first.id, "b");
        assert_ne!(second.id, "b");
        assert_ne!(third.id, "b");
    }

    /// 失败后进入冷却，不再被选中；成功后恢复。
    #[test]
    fn failure_enters_cooldown_and_success_recovers() {
        let pool = AccountPool::new(vec![account("a", true)]);
        pool.report_failure("a", FailureKind::RateLimit);
        assert!(pool.select().is_none(), "cooldown account must be skipped");
        assert!(pool.snapshot()[0].cooling);
        pool.report_success("a");
        assert!(
            pool.select().is_some(),
            "account must recover after success"
        );
    }

    /// 连续失败放大冷却，但最多 4 倍。
    #[test]
    fn repeated_failures_scale_cooldown() {
        let pool = AccountPool::new(vec![account("a", true)]);
        for _ in 0..10 {
            pool.report_failure("a", FailureKind::Transient);
        }
        let state = &pool.snapshot()[0];
        assert_eq!(state.consecutive_failures, 10);
        assert!(state.cooldown_remaining_secs <= FailureKind::Transient.cooldown().as_secs() * 4);
    }

    /// 状态码应映射到正确的失败类型。
    #[test]
    fn classifies_failure_kind() {
        assert_eq!(FailureKind::classify(Some(429), ""), FailureKind::RateLimit);
        assert_eq!(
            FailureKind::classify(Some(401), ""),
            FailureKind::AuthExpired
        );
        assert_eq!(FailureKind::classify(Some(503), ""), FailureKind::Transient);
        assert_eq!(
            FailureKind::classify(None, "insufficient_quota"),
            FailureKind::QuotaExhausted
        );
    }
}
