//! 用量账本与日预算。
//!
//! 单进程内存计数 + SQLite 持久化。个人部署下足够防止单日超支；
//! 多副本部署时各副本独立计数，不保证全局一致（文档已标注）。

use crate::core::stream::EventStream;
use crate::core::{AppError, Event};
use crate::store::Store;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

/// UTC 归属日与服务计数在同一临界区内切换。
struct DailyUsage {
    day: String,
    totals: HashMap<String, u64>,
}

/// 进程内用量账本。
pub struct UsageLedger {
    today: Mutex<DailyUsage>,
    store: Store,
}

impl UsageLedger {
    /// 创建账本。启动时应调用 `load_today` 恢复当天数据。
    pub fn new(store: Store) -> Self {
        UsageLedger {
            today: Mutex::new(DailyUsage {
                day: today(),
                totals: HashMap::new(),
            }),
            store,
        }
    }

    /// 启动时恢复当日用量，数据库查询跨午夜时重读新日期。
    pub async fn load_today(&self) -> Result<(), AppError> {
        loop {
            let day = today();
            let rows = self.store.load_usage(&day).await?;
            {
                let mut current = self.lock_today();
                if current.day == day {
                    current.totals = rows.into_iter().collect();
                    return Ok(());
                }
            }
        }
    }

    /// 取得账本后读取日期，避免旧日期的排队读写回退已经切换的计数。
    fn lock_today(&self) -> MutexGuard<'_, DailyUsage> {
        let mut current = self.today.lock().unwrap_or_else(|p| p.into_inner());
        let day = today();
        if current.day != day {
            current.day = day;
            current.totals.clear();
        }
        current
    }

    /// 记录一次用量。内存立即生效，落库异步进行。
    pub fn record(&self, backend_id: &str, tokens: u64) {
        if tokens == 0 {
            return;
        }
        let day = {
            let mut current = self.lock_today();
            *current.totals.entry(backend_id.to_string()).or_insert(0) += tokens;
            current.day.clone()
        };
        let store = self.store.clone();
        let backend_id = backend_id.to_string();
        // 落库失败只影响重启后的统计，不阻塞请求。
        tokio::spawn(async move {
            if let Err(error) = store.add_usage(&backend_id, &day, tokens).await {
                tracing::warn!(%error, backend = %backend_id, "persist usage failed");
            }
        });
    }

    /// 当日已用 token。
    pub fn used_today(&self, backend_id: &str) -> u64 {
        self.lock_today()
            .totals
            .get(backend_id)
            .copied()
            .unwrap_or(0)
    }

    /// 是否超过日预算。预算为 0 或 None 表示不限制。
    pub fn over_budget(&self, backend_id: &str, budget: Option<u64>) -> bool {
        match budget {
            Some(limit) if limit > 0 => self.used_today(backend_id) >= limit,
            _ => false,
        }
    }

    /// 导出快照，供管理页展示。
    pub fn snapshot(&self) -> Vec<(String, u64)> {
        let mut items = self
            .lock_today()
            .totals
            .iter()
            .map(|(backend_id, tokens)| (backend_id.clone(), *tokens))
            .collect::<Vec<_>>();
        items.sort_by(|left, right| left.0.cmp(&right.0));
        items
    }
}

/// 包装事件流：累计用量并写入账本。
pub fn metered(stream: EventStream, ledger: Arc<UsageLedger>, backend_id: String) -> EventStream {
    Box::pin(stream.map(move |item| {
        if let Ok(Event::Usage(usage)) = &item {
            // 优先 total，缺失时用输入+输出。
            let tokens = usage
                .total_tokens
                .map(u64::from)
                .or_else(|| {
                    usage
                        .input_tokens
                        .zip(usage.output_tokens)
                        .map(|(input, output)| u64::from(input) + u64::from(output))
                })
                .unwrap_or(0);
            ledger.record(&backend_id, tokens);
        }
        item
    }))
}

/// 当前 UTC 日期 YYYY-MM-DD。与任务周期使用同一换算。
pub fn today() -> String {
    let days = (crate::records::now_seconds() / 86_400) as i64;
    let (year, month, day) = crate::backend::qoder::civil_from_days_public(days);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// 记录后内存计数立即生效。
    #[tokio::test]
    async fn records_in_memory() {
        let store = Store::open(Path::new(":memory:")).await.expect("store");
        let ledger = UsageLedger::new(store);
        ledger.record("b", 100);
        ledger.record("b", 50);
        assert_eq!(ledger.used_today("b"), 150);
    }

    /// 预算为 0 或 None 视为不限制。
    #[tokio::test]
    async fn zero_budget_means_unlimited() {
        let store = Store::open(Path::new(":memory:")).await.expect("store");
        let ledger = UsageLedger::new(store);
        ledger.record("b", 10_000);
        assert!(!ledger.over_budget("b", None));
        assert!(!ledger.over_budget("b", Some(0)));
    }

    /// 超过预算应被识别。
    #[tokio::test]
    async fn detects_over_budget() {
        let store = Store::open(Path::new(":memory:")).await.expect("store");
        let ledger = UsageLedger::new(store);
        ledger.record("b", 600);
        assert!(ledger.over_budget("b", Some(500)));
        assert!(!ledger.over_budget("b", Some(1000)));
    }

    /// 所有公开读取和记录入口均切换归属日，不让前日耗尽阻断新日。
    #[tokio::test]
    async fn previous_day_usage_expires_on_reads_and_writes() {
        let store = Store::open(Path::new(":memory:")).await.expect("store");
        for entry in ["budget", "snapshot", "record"] {
            let ledger = UsageLedger::new(store.clone());
            {
                let mut previous = ledger.today.lock().unwrap();
                previous.day = "1970-01-01".into();
                previous.totals.insert("b".into(), 100);
            }
            match entry {
                "budget" => {
                    assert!(!ledger.over_budget("b", Some(100)));
                    assert_eq!(ledger.used_today("b"), 0);
                }
                "snapshot" => assert!(ledger.snapshot().is_empty()),
                _ => {
                    ledger.record("b", 7);
                    assert_eq!(ledger.used_today("b"), 7);
                    assert_eq!(ledger.snapshot(), vec![("b".into(), 7)]);
                }
            }
            assert_eq!(ledger.today.lock().unwrap().day, today());
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if store.load_usage(&today()).await.unwrap() == vec![("b".into(), 7)] {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("new-day usage is persisted to the same day as memory");
    }
}
