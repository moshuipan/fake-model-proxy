//! 手动批量签到：同渠道串行并随机错开，页面通过短请求读取后台进度。

use super::{TaskKind, TaskOutcome, TaskStatus};
use crate::core::AppError;
use crate::state::AppState;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::task::JoinSet;

pub const MIN_DELAY_MS: i64 = 5_000;
pub const MAX_DELAY_MS: i64 = 15_000;

/// 批次内的账号结果，不保留账号凭证或执行器快照。
#[derive(Debug, Clone, Serialize)]
pub struct BatchAccount {
    pub backend_id: String,
    pub backend_name: String,
    pub account_id: String,
    pub account_name: String,
    pub channel: String,
    pub status: String,
    pub message: String,
    pub period: Option<String>,
    pub amount: Option<i64>,
    pub delay_ms: Option<i64>,
    pub wait_until: Option<i64>,
}

/// 只保留最近一批的进度；实际执行结果同时写入已有任务记录。
#[derive(Debug, Clone, Serialize)]
pub struct BatchRun {
    pub id: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub version: u64,
    pub status: String,
    pub last_error: Option<String>,
    pub accounts: Vec<BatchAccount>,
}

/// 后台批次状态跨 Axum 请求共享，同一进程最多启动一批。
#[derive(Default)]
pub struct BatchControl {
    run: Mutex<Option<BatchRun>>,
}

/// 管理 API 的启动和进度查询使用相同响应。
#[derive(Debug, Clone, Serialize)]
pub struct BatchView {
    pub run: Option<BatchRun>,
    pub delay_min_seconds: i64,
    pub delay_max_seconds: i64,
}

impl BatchControl {
    /// 复制脱敏进度；读取过程不持有网络请求锁。
    pub fn view(&self) -> BatchView {
        BatchView {
            run: self.run.lock().unwrap().clone(),
            delay_min_seconds: MIN_DELAY_MS / 1000,
            delay_max_seconds: MAX_DELAY_MS / 1000,
        }
    }

    /// 只更新仍属于当前批次的账号，每次修改递增版本以防前端响应倒序。
    fn update_account(&self, id: &str, index: usize, update: impl FnOnce(&mut BatchAccount)) {
        let mut guard = self.run.lock().unwrap();
        if let Some(run) = guard.as_mut().filter(|run| run.id == id) {
            update(&mut run.accounts[index]);
            run.version += 1;
        }
    }
}

/// 毫秒时间戳仅用于展示等待时间，实际等待使用 Tokio 单调时钟。
fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// 按渠道类型分组，相同厂商的多个服务配置也共用串行队列。
fn channel_groups(accounts: &[BatchAccount]) -> Vec<Vec<usize>> {
    let mut groups = BTreeMap::<String, Vec<usize>>::new();
    for (index, account) in accounts.iter().enumerate() {
        groups
            .entry(account.channel.clone())
            .or_default()
            .push(index);
    }
    groups.into_values().collect()
}

/// 对启用且支持签到的账号生成批次；重复启动返回正在运行的批次。
pub async fn start(state: &AppState) -> Result<BatchView, AppError> {
    let providers = state.task_providers();
    let backends = state.store.list_backends().await?;
    let accounts = state.store.list_accounts().await?;
    let mut targets = Vec::new();
    for backend in backends.iter().filter(|backend| backend.enabled) {
        if !providers.iter().any(|provider| {
            provider.kind() == TaskKind::Checkin && provider.backend_id() == backend.id
        }) {
            continue;
        }
        for account in accounts
            .iter()
            .filter(|account| account.enabled && account.backend_id == backend.id)
        {
            targets.push(BatchAccount {
                backend_id: backend.id.clone(),
                backend_name: backend.name.clone(),
                account_id: account.id.clone(),
                account_name: account.name.clone(),
                channel: backend.kind.clone(),
                status: "pending".into(),
                message: "等待执行".into(),
                period: None,
                amount: None,
                delay_ms: None,
                wait_until: None,
            });
        }
    }
    targets.sort_by(|left, right| {
        (&left.channel, &left.backend_id, &left.account_id).cmp(&(
            &right.channel,
            &right.backend_id,
            &right.account_id,
        ))
    });
    let run = {
        let mut guard = state.checkin_batch.run.lock().unwrap();
        if guard.as_ref().is_some_and(|run| run.status == "running") {
            drop(guard);
            return Ok(state.checkin_batch.view());
        }
        if targets.is_empty() {
            return Err(AppError::bad_request("没有已启用且支持签到的账号"));
        }
        let run = BatchRun {
            id: uuid::Uuid::new_v4().to_string(),
            started_at: now_millis(),
            finished_at: None,
            version: 1,
            status: "running".into(),
            last_error: None,
            accounts: targets,
        };
        *guard = Some(run.clone());
        run
    };
    let view = state.checkin_batch.view();
    let state = state.clone();
    tokio::spawn(async move { execute_batch(state, run).await });
    Ok(view)
}

/// 每次领取前重新检查账号、渠道与执行器，延迟期间的停用、删除或渠道修改立即生效。
async fn execute_account(
    state: &AppState,
    account: &BatchAccount,
    period: &str,
) -> Result<TaskOutcome, AppError> {
    let backend = state.store.get_backend(&account.backend_id).await?;
    let stored = state.store.get_account(&account.account_id).await?;
    if backend.is_none_or(|backend| !backend.enabled || backend.kind != account.channel)
        || stored.is_none_or(|stored| !stored.enabled || stored.backend_id != account.backend_id)
    {
        return Ok(TaskOutcome::skipped(
            "账号或服务已停用、删除或渠道已变更，跳过签到",
        ));
    }
    let Some(provider) = state.task_providers().into_iter().find(|provider| {
        provider.kind() == TaskKind::Checkin && provider.backend_id() == account.backend_id
    }) else {
        return Ok(TaskOutcome::skipped("服务当前没有签到能力"));
    };
    super::run_and_record(&state.store, provider.as_ref(), &account.account_id, period).await
}

/// 同渠道下一账号在上一账号结束后等待随机间隔，不在等待期间持有领取锁。
async fn execute_channel(
    state: AppState,
    run: BatchRun,
    indices: Vec<usize>,
) -> Result<(), AppError> {
    for (position, index) in indices.into_iter().enumerate() {
        if position > 0 {
            let delay_ms = super::scheduler::random_between(MIN_DELAY_MS, MAX_DELAY_MS)?;
            state
                .checkin_batch
                .update_account(&run.id, index, |account| {
                    account.status = "waiting".into();
                    account.message = format!("同渠道随机等待 {:.1} 秒", delay_ms as f64 / 1000.0);
                    account.delay_ms = Some(delay_ms);
                    account.wait_until = Some(now_millis() + delay_ms);
                });
            tokio::time::sleep(Duration::from_millis(delay_ms as u64)).await;
        }
        let period = super::scheduler::day_at(super::scheduler::now());
        state
            .checkin_batch
            .update_account(&run.id, index, |account| {
                account.status = "running".into();
                account.message = "正在查询并执行签到".into();
                account.period = Some(period.clone());
                account.wait_until = None;
            });
        let outcome = execute_account(&state, &run.accounts[index], &period)
            .await
            .unwrap_or_else(|error| TaskOutcome {
                status: TaskStatus::Unknown,
                message: format!("无法确认执行结果或记录：{}", error.message()),
                amount: None,
            });
        state
            .checkin_batch
            .update_account(&run.id, index, |account| {
                account.status = outcome.status.as_str().into();
                account.message = outcome.message;
                account.amount = outcome.amount;
            });
    }
    Ok(())
}

/// 各渠道分别推进，单个账号的失败不阻断其余账号；未完成项明确保留错误。
async fn execute_batch(state: AppState, run: BatchRun) {
    let mut workers = JoinSet::new();
    for indices in channel_groups(&run.accounts) {
        workers.spawn(execute_channel(state.clone(), run.clone(), indices));
    }
    let mut error = None;
    while let Some(result) = workers.join_next().await {
        match result {
            Ok(Ok(())) => {}
            Ok(Err(failure)) => error = Some(failure.message()),
            Err(_) => error = Some("签到执行任务中断，请核对已执行账号".into()),
        }
    }
    let mut guard = state.checkin_batch.run.lock().unwrap();
    if let Some(current) = guard.as_mut().filter(|current| current.id == run.id) {
        for account in &mut current.accounts {
            if matches!(account.status.as_str(), "pending" | "waiting" | "running") {
                account.status = if account.status == "running" {
                    "unknown"
                } else {
                    "failed"
                }
                .into();
                account.message = error.clone().unwrap_or_else(|| "批次未能完成此账号".into());
                account.wait_until = None;
            }
        }
        current.status = "completed".into();
        current.last_error = error;
        current.finished_at = Some(now_millis());
        current.version += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::Account;
    use crate::backend::Registry;
    use crate::config::Config;
    use crate::store::{BackendRecord, Store};
    use crate::tasks::{TaskProvider, TaskRegistry};
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, RwLock};
    use tokio::sync::Notify;
    use tokio::time::Instant;
    use tower::ServiceExt;

    /// 记录假上游动作的起止单调时刻，不读取真实凭证。
    struct Call {
        account: String,
        start: Instant,
        end: Instant,
    }

    /// 假执行器模拟短请求、一个明确失败账号，以及多个独立渠道。
    struct FakeProvider {
        id: String,
        calls: Arc<Mutex<Vec<Call>>>,
    }

    #[async_trait]
    impl TaskProvider for FakeProvider {
        /// 仅模拟签到能力。
        fn kind(&self) -> TaskKind {
            TaskKind::Checkin
        }
        /// 返回测试服务 ID。
        fn backend_id(&self) -> &str {
            &self.id
        }
        /// 测试要求精确指定账号，不使用账号池自动挑选。
        async fn run(&self, _period: &str) -> Result<TaskOutcome, AppError> {
            Err(AppError::bad_request("test requires account"))
        }
        /// 记录实际执行账号与时刻，模拟失败但不访问网络。
        async fn run_for_account(
            &self,
            account_id: &str,
            _period: &str,
        ) -> Result<TaskOutcome, AppError> {
            let start = Instant::now();
            tokio::time::sleep(Duration::from_millis(40)).await;
            self.calls.lock().unwrap().push(Call {
                account: account_id.into(),
                start,
                end: Instant::now(),
            });
            if account_id == "fail" {
                Err(AppError::bad_request("fake rejection"))
            } else {
                Ok(TaskOutcome::succeeded("fake reward", Some(1)))
            }
        }
    }

    /// 创建不同服务但同渠道的账号、另一个渠道，以及需要排除的账号。
    async fn fixture() -> (AppState, Arc<Mutex<Vec<Call>>>) {
        let store = Store::open(Path::new(":memory:")).await.unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = TaskRegistry::new();
        for (id, kind, enabled, registered) in [
            ("q1", "qoder", true, true),
            ("q2", "qoder", true, true),
            ("w1", "workbuddy", true, true),
            ("off", "qoder", false, true),
            ("plain", "openai_compat", true, false),
        ] {
            store
                .upsert_backend(&BackendRecord {
                    id: id.into(),
                    name: id.into(),
                    kind: kind.into(),
                    enabled,
                    ..Default::default()
                })
                .await
                .unwrap();
            if registered {
                tasks.register(Arc::new(FakeProvider {
                    id: id.into(),
                    calls: calls.clone(),
                }));
            }
        }
        for (id, backend, enabled) in [
            ("fail", "q1", true),
            ("q-next", "q2", true),
            ("w-first", "w1", true),
            ("w-next", "w1", true),
            ("disabled", "q1", false),
            ("off-account", "off", true),
            ("unsupported", "plain", true),
        ] {
            store
                .upsert_account(&Account {
                    id: id.into(),
                    backend_id: backend.into(),
                    name: id.into(),
                    credential: "{}".into(),
                    enabled,
                })
                .await
                .unwrap();
        }
        let state = AppState::new(
            Arc::new(Config {
                admin_key: "test-admin".into(),
                ..Default::default()
            }),
            store.clone(),
            Arc::new(Registry::new()),
            Arc::new(crate::records::RecordLog::default()),
            Arc::new(RwLock::new(tasks)),
            Arc::new(crate::routing::Router::new()),
            Arc::new(crate::usage::UsageLedger::new(store)),
        );
        (state, calls)
    }

    /// 用真实单调时钟验证 5–15 秒间隔、渠道独立推进、失败续行和重复点击复用。
    #[tokio::test]
    async fn batch_staggers_same_channel_and_keeps_other_channels_moving() {
        let (state, calls) = fixture().await;
        let started = Instant::now();
        let app = crate::build_router(state.clone());
        let request = || {
            Request::builder()
                .method("POST")
                .uri("/api/tasks/checkin-all")
                .header("x-admin-key", "test-admin")
                .body(Body::empty())
                .unwrap()
        };
        let (first, second) = tokio::join!(
            app.clone().oneshot(request()),
            app.clone().oneshot(request()),
        );
        let first = first.unwrap();
        let second = second.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(second.status(), StatusCode::OK);
        let first: serde_json::Value =
            serde_json::from_slice(&first.into_body().collect().await.unwrap().to_bytes()).unwrap();
        let second: serde_json::Value =
            serde_json::from_slice(&second.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(first["run"]["id"], second["run"]["id"]);
        assert_eq!(first["run"]["accounts"].as_array().unwrap().len(), 4);
        assert_eq!(first["delay_min_seconds"], 5);
        assert_eq!(first["delay_max_seconds"], 15);
        let completed = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let view = state.checkin_batch.view();
                if view.run.as_ref().unwrap().status == "completed" {
                    break view.run.unwrap();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            completed
                .accounts
                .iter()
                .filter(|account| account.status == "succeeded")
                .count(),
            3
        );
        assert_eq!(
            completed
                .accounts
                .iter()
                .filter(|account| account.status == "failed")
                .count(),
            1
        );
        assert_eq!(state.store.list_tasks(20).await.unwrap().len(), 4);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/tasks/checkin-all")
                    .header("x-admin-key", "test-admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(status["run"]["id"], first["run"]["id"]);
        assert_eq!(status["run"]["status"], "completed");
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 4);
        for (first_id, next_id) in [("fail", "q-next"), ("w-first", "w-next")] {
            let first = calls.iter().find(|call| call.account == first_id).unwrap();
            let next = calls.iter().find(|call| call.account == next_id).unwrap();
            assert!(first.start.duration_since(started) < Duration::from_secs(1));
            let delay = completed
                .accounts
                .iter()
                .find(|account| account.account_id == next_id)
                .unwrap()
                .delay_ms
                .unwrap();
            assert!((MIN_DELAY_MS..=MAX_DELAY_MS).contains(&delay));
            let gap = next.start.duration_since(first.end);
            assert!(gap >= Duration::from_millis(delay as u64));
            assert!(gap < Duration::from_millis(delay as u64 + 1000));
        }
        assert_eq!(
            completed
                .accounts
                .iter()
                .filter(|account| account.delay_ms.is_none())
                .count(),
            2
        );
    }

    /// 等待期间停用账号或修改渠道，实际执行时均会跳过。
    #[tokio::test]
    async fn account_changes_are_rechecked_before_execution() {
        let (state, calls) = fixture().await;
        let mut stored = state.store.get_account("q-next").await.unwrap().unwrap();
        stored.enabled = false;
        state.store.upsert_account(&stored).await.unwrap();
        let target = BatchAccount {
            backend_id: "q2".into(),
            backend_name: "Q".into(),
            account_id: "q-next".into(),
            account_name: "A".into(),
            channel: "qoder".into(),
            status: "pending".into(),
            message: String::new(),
            period: None,
            amount: None,
            delay_ms: None,
            wait_until: None,
        };
        let outcome = execute_account(&state, &target, "2026-11-19")
            .await
            .unwrap();
        assert_eq!(outcome.status, TaskStatus::Skipped);
        assert!(calls.lock().unwrap().is_empty());
        stored.enabled = true;
        state.store.upsert_account(&stored).await.unwrap();
        let mut backend = state.store.get_backend("q2").await.unwrap().unwrap();
        backend.kind = "workbuddy".into();
        state.store.upsert_backend(&backend).await.unwrap();
        let outcome = execute_account(&state, &target, "2026-11-19")
            .await
            .unwrap();
        assert_eq!(outcome.status, TaskStatus::Skipped);
        assert!(calls.lock().unwrap().is_empty());
    }

    /// 完整管理路由不允许未鉴权启动，且无可执行账号时不给出虚假的运行批次。
    #[tokio::test]
    async fn api_rejects_unauthorized_and_empty_batches() {
        let (state, calls) = fixture().await;
        let app = crate::build_router(state.clone());
        for method in ["GET", "POST"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri("/api/tasks/checkin-all")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        for mut account in state.store.list_accounts().await.unwrap() {
            account.enabled = false;
            state.store.upsert_account(&account).await.unwrap();
        }
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/tasks/checkin-all")
                    .header("x-admin-key", "test-admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(state.checkin_batch.view().run.is_none());
        assert!(calls.lock().unwrap().is_empty());
    }

    /// 阻塞执行器模拟已经开始上游动作、尚未得到结果的进程中断。
    struct BlockedProvider {
        started: Notify,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl TaskProvider for BlockedProvider {
        /// 测试签到任务。
        fn kind(&self) -> TaskKind {
            TaskKind::Checkin
        }
        /// 与测试账号所属服务保持一致。
        fn backend_id(&self) -> &str {
            "q1"
        }
        /// 发出开始通知后保持挂起，交给测试取消任务。
        async fn run(&self, _period: &str) -> Result<TaskOutcome, AppError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            std::future::pending().await
        }
    }

    /// 外部动作开始前已落盘未知标记，中断后再次触发不会发出第二次领取。
    #[tokio::test]
    async fn interrupted_execution_is_recorded_before_any_replay() {
        let (state, _) = fixture().await;
        let provider = Arc::new(BlockedProvider {
            started: Notify::new(),
            calls: AtomicUsize::new(0),
        });
        let store = state.store.clone();
        let worker = provider.clone();
        let handle = tokio::spawn(async move {
            crate::tasks::run_and_record(&store, worker.as_ref(), "fail", "2026-11-19").await
        });
        provider.started.notified().await;
        let record = state
            .store
            .get_task("q1", "fail", "checkin", "2026-11-19")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.status, "unknown");
        handle.abort();
        assert!(handle.await.unwrap_err().is_cancelled());
        let outcome =
            crate::tasks::run_and_record(&state.store, provider.as_ref(), "fail", "2026-11-19")
                .await
                .unwrap();
        assert_eq!(outcome.status, TaskStatus::Unknown);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    }
}
