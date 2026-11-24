//! 北京时间每日抽签调度：配置与执行计划以 SQLite 为事实源。

use crate::core::AppError;
use crate::state::AppState;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

const UTC_OFFSET: i64 = 8 * 3600;

/// 页面可编辑配置；不接收客户端提供的配置版本或时区。
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScheduleInput {
    pub enabled: bool,
    pub time: String,
    pub random_minutes: i64,
}

impl ScheduleInput {
    /// 限制为当日 HH:mm 窗口，0 分钟表示固定时间。
    pub fn validate(&self) -> Result<i64, AppError> {
        let bytes = self.time.as_bytes();
        if bytes.len() != 5
            || bytes[2] != b':'
            || ![bytes[0], bytes[1], bytes[3], bytes[4]]
                .iter()
                .all(u8::is_ascii_digit)
        {
            return Err(AppError::bad_request("每日时间必须是 HH:mm"));
        }
        let hour = (bytes[0] - b'0') as i64 * 10 + (bytes[1] - b'0') as i64;
        let minute = (bytes[3] - b'0') as i64 * 10 + (bytes[4] - b'0') as i64;
        let start = hour * 60 + minute;
        if hour >= 24 || minute >= 60 {
            return Err(AppError::bad_request("每日时间超出 00:00–23:59"));
        }
        if !(0..=1440 - start).contains(&self.random_minutes) {
            return Err(AppError::bad_request(
                "随机范围必须为非负整数分钟，且结束时间不能超过当天 24:00",
            ));
        }
        Ok(start * 60)
    }
}

/// 持久配置；修改版本用于废弃未执行的旧计划。
#[derive(Debug, Clone, Serialize)]
pub struct CheckinSettings {
    pub enabled: bool,
    pub time: String,
    pub random_minutes: i64,
    pub revision: i64,
}

impl CheckinSettings {
    /// 校验数据库配置并计算窗口开始的日内秒数。
    fn start_seconds(&self) -> Result<i64, AppError> {
        ScheduleInput {
            enabled: self.enabled,
            time: self.time.clone(),
            random_minutes: self.random_minutes,
        }
        .validate()
    }
}

/// 每账号每天的一次自动尝试，抽签时间跨进程重启保持不变。
#[derive(Debug, Clone, Serialize)]
pub struct CheckinPlan {
    pub backend_id: String,
    pub account_id: String,
    pub day: String,
    pub scheduled_at: i64,
    pub window_end: i64,
    pub revision: i64,
    pub status: String,
}

/// 控制状态跨页面保存与后端重载共享；不参与领取结果持久化。
#[derive(Default)]
pub struct SchedulerControl {
    pub changed: Notify,
    active: AtomicBool,
    last_error: Mutex<Option<String>>,
    planning: tokio::sync::Mutex<()>,
}

/// 管理页状态和每个启用账号的下一次抽签计划。
#[derive(Serialize)]
pub struct ScheduleView {
    pub settings: CheckinSettings,
    pub timezone: &'static str,
    pub running: bool,
    pub last_error: Option<String>,
    pub next_runs: Vec<CheckinPlan>,
}

/// 计算北京时间自然日，手动与自动签到共用同一周期。
pub fn day_at(seconds: i64) -> String {
    let days = (seconds + UTC_OFFSET).div_euclid(86_400);
    let (year, month, day) = crate::backend::qoder::civil_from_days_public(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// 返回当前 Unix 秒，所有时间比较使用同一时钟。
pub fn now() -> i64 {
    crate::records::now_seconds() as i64
}

/// 对闭区间做均匀抽签，拒绝采样避免取模偏差。
pub(super) fn random_between(start: i64, end: i64) -> Result<i64, AppError> {
    let width = (end - start + 1) as u64;
    let limit = u64::MAX - u64::MAX % width;
    loop {
        let mut bytes = [0; 8];
        OsRng
            .try_fill_bytes(&mut bytes)
            .map_err(|_| AppError::Internal {
                message: "无法生成签到随机时间".into(),
            })?;
        let value = u64::from_le_bytes(bytes);
        if value < limit {
            return Ok(start + (value % width) as i64);
        }
    }
}

/// 生成今天和明天的计划；首次在窗口内开启只从剩余时间抽签。
async fn prepare(state: &AppState, now: i64) -> Result<ScheduleView, AppError> {
    let settings = state.store.checkin_settings().await?;
    let start = settings.start_seconds()?;
    let day_start = (now + UTC_OFFSET).div_euclid(86_400) * 86_400 - UTC_OFFSET;
    let today = day_at(now);
    let providers = state.task_providers();
    let accounts = state.store.list_accounts().await?;
    let active: HashSet<_> = accounts
        .iter()
        .filter(|account| {
            account.enabled
                && providers.iter().any(|provider| {
                    provider.kind() == super::TaskKind::Checkin
                        && provider.backend_id() == account.backend_id
                })
        })
        .map(|account| (account.backend_id.clone(), account.id.clone()))
        .collect();
    let existing = state.store.checkin_plans(&today).await?;
    for (backend_id, account_id) in &active {
        for offset in [0, 86_400] {
            let day = day_at(day_start + offset);
            if existing.iter().any(|plan| {
                plan.backend_id == *backend_id
                    && plan.account_id == *account_id
                    && plan.day == day
                    && (plan.revision == settings.revision
                        || !matches!(plan.status.as_str(), "pending" | "missed"))
            }) {
                continue;
            }
            let window_start = day_start + offset + start;
            // 24:00 的结束边界属于当日窗口，不跨到次日周期。
            let window_end =
                (window_start + settings.random_minutes * 60).min(day_start + offset + 86_400 - 1);
            let draw_start = if settings.random_minutes > 0 && now <= window_end {
                window_start.max(now)
            } else {
                window_start
            };
            let plan = CheckinPlan {
                backend_id: backend_id.clone(),
                account_id: account_id.clone(),
                day,
                scheduled_at: random_between(draw_start, window_end)?,
                window_end,
                revision: settings.revision,
                status: if now > window_end + 60 {
                    "missed"
                } else {
                    "pending"
                }
                .into(),
            };
            state.store.save_checkin_plan(&plan).await?;
        }
    }
    state.store.expire_checkin_plans(now).await?;
    let plans = state.store.checkin_plans(&today).await?;
    let mut seen = HashSet::new();
    let next_runs = plans
        .into_iter()
        .filter(|plan| {
            active.contains(&(plan.backend_id.clone(), plan.account_id.clone()))
                && matches!(plan.status.as_str(), "pending" | "running")
                && plan.revision == settings.revision
                && seen.insert((plan.backend_id.clone(), plan.account_id.clone()))
        })
        .collect();
    Ok(ScheduleView {
        settings,
        timezone: "Asia/Shanghai",
        running: state.checkin_scheduler.active.load(Ordering::Relaxed),
        last_error: state.checkin_scheduler.last_error.lock().unwrap().clone(),
        next_runs,
    })
}

/// 页面读与调度准备串行化，重复刷新不会改变抽签结果。
pub async fn view(state: &AppState) -> Result<ScheduleView, AppError> {
    let _guard = state.checkin_scheduler.planning.lock().await;
    prepare(state, now()).await
}

/// 保存配置并返回具体计划，Notify 让休眠的调度器即时重新读取。
pub async fn save(state: &AppState, input: &ScheduleInput) -> Result<ScheduleView, AppError> {
    let _guard = state.checkin_scheduler.planning.lock().await;
    state
        .config_service()
        .mutate("legacy.checkin.schedule", |document, _| {
            document.checkin = input.clone();
            Ok(())
        })
        .await?;
    state.checkin_scheduler.changed.notify_one();
    prepare(state, now()).await
}

/// 检查到期计划，每账号每日最多一次自动尝试；网络执行不阻塞页面保存。
async fn tick(state: &AppState, at: i64) -> Result<(), AppError> {
    let view = {
        let _guard = state.checkin_scheduler.planning.lock().await;
        prepare(state, at).await?
    };
    if !view.settings.enabled {
        return Ok(());
    }
    for plan in view.next_runs.iter().filter(|plan| plan.scheduled_at <= at) {
        let Some(provider) = state.task_providers().into_iter().find(|provider| {
            provider.kind() == super::TaskKind::Checkin && provider.backend_id() == plan.backend_id
        }) else {
            continue;
        };
        if !state.store.claim_checkin_plan(plan, now()).await? {
            continue;
        }
        let outcome =
            super::run_and_record(&state.store, provider.as_ref(), &plan.account_id, &plan.day)
                .await?;
        state
            .store
            .finish_checkin_plan(plan, outcome.status.as_str())
            .await?;
        tracing::info!(
            backend = %plan.backend_id, account = %plan.account_id,
            status = outcome.status.as_str(), "scheduled check-in finished"
        );
    }
    Ok(())
}

/// 始终运行控制循环；关闭时不领取，配置保存与后端重载均可唤醒。
pub fn spawn(state: AppState) {
    if state.checkin_scheduler.active.swap(true, Ordering::Relaxed) {
        return;
    }
    tokio::spawn(async move {
        if let Err(error) = state.store.recover_checkin_plans().await {
            tracing::error!(%error, "recover check-in plans failed");
            *state.checkin_scheduler.last_error.lock().unwrap() = Some(error.message());
            state
                .checkin_scheduler
                .active
                .store(false, Ordering::Relaxed);
            return;
        }
        loop {
            let result = tick(&state, now()).await;
            *state.checkin_scheduler.last_error.lock().unwrap() =
                result.as_ref().err().map(AppError::message);
            if let Err(error) = result {
                tracing::warn!(%error, "check-in scheduler failed");
            }
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(15)) => {},
                _ = state.checkin_scheduler.changed.notified() => {},
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::Account;
    use crate::backend::Registry;
    use crate::config::Config;
    use crate::store::{BackendRecord, Store};
    use crate::tasks::{TaskKind, TaskOutcome, TaskProvider, TaskRegistry};
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use std::path::Path;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, RwLock};
    use tower::ServiceExt;

    /// 假执行器只记录调用，不读取真实凭证、不访问任何厂商。
    struct FakeProvider {
        calls: AtomicUsize,
        fail: AtomicBool,
    }

    #[async_trait]
    impl TaskProvider for FakeProvider {
        /// 模拟每日签到能力。
        fn kind(&self) -> TaskKind {
            TaskKind::Checkin
        }
        /// 返回测试后端。
        fn backend_id(&self) -> &str {
            "fake"
        }
        /// 记录一次领取，供并发测试检查实际调用次数。
        async fn run(&self, _period: &str) -> Result<TaskOutcome, AppError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            if self.fail.load(Ordering::SeqCst) {
                return Err(AppError::bad_request("fake rejection"));
            }
            Ok(TaskOutcome::succeeded("fake reward", Some(1)))
        }
    }

    /// 在独立数据库中构造两个假账号及一个假执行器。
    async fn fixture(path: &Path) -> (AppState, Arc<FakeProvider>) {
        let store = Store::open(path).await.unwrap();
        store
            .upsert_backend(&BackendRecord {
                id: "fake".into(),
                name: "Fake".into(),
                base_url: "http://127.0.0.1:9/v1".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        for id in ["a", "b"] {
            store
                .upsert_account(&Account {
                    id: id.into(),
                    backend_id: "fake".into(),
                    name: id.into(),
                    credential: "{}".into(),
                    enabled: true,
                })
                .await
                .unwrap();
        }
        let provider = Arc::new(FakeProvider {
            calls: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        });
        let mut tasks = TaskRegistry::new();
        tasks.register(provider.clone());
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
        (state, provider)
    }

    /// 构造合法设置。
    fn input(enabled: bool, time: &str, random_minutes: i64) -> ScheduleInput {
        ScheduleInput {
            enabled,
            time: time.into(),
            random_minutes,
        }
    }

    /// 在当前日内生成到期计划，测试时钟无需等待实际签到时间。
    async fn due_plans(state: &AppState) -> Vec<CheckinPlan> {
        due_plans_at(state, now()).await
    }

    /// 用显式测试时间构造到期计划，避免晚间窗口断言依赖真实时钟。
    async fn due_plans_at(state: &AppState, at: i64) -> Vec<CheckinPlan> {
        let settings = state
            .store
            .save_checkin_settings(&input(true, "00:00", 0))
            .await
            .unwrap();
        for id in ["a", "b"] {
            state
                .store
                .save_checkin_plan(&CheckinPlan {
                    backend_id: "fake".into(),
                    account_id: id.into(),
                    day: day_at(at),
                    scheduled_at: at - 1,
                    window_end: at + 60,
                    revision: settings.revision,
                    status: "pending".into(),
                })
                .await
                .unwrap();
        }
        state.store.checkin_plans(&day_at(at)).await.unwrap()
    }

    /// 严格时间、整数范围和北京时间午夜边界都遵守同一约束。
    #[test]
    fn validates_time_window_and_beijing_period() {
        assert_eq!(input(false, "10:00", 60).validate().unwrap(), 36_000);
        assert!(input(true, "23:59", 1).validate().is_ok());
        for (time, range) in [
            ("24:00", 0),
            ("10:60", 0),
            ("1:00", 0),
            ("10:00", -1),
            ("23:59", 2),
            ("10:00", i64::MAX),
        ] {
            assert!(input(true, time, range).validate().is_err());
        }
        assert_eq!(day_at(0), "1970-01-01");
        assert_eq!(day_at(16 * 3600 - 1), "1970-01-01");
        assert_eq!(day_at(16 * 3600), "1970-01-02");
    }

    /// 页面刷新和重新打开磁盘数据库均保留配置与抽签结果。
    #[tokio::test]
    async fn plans_and_settings_survive_refresh_and_restart() {
        let directory = std::env::temp_dir().join(format!("fmp-schedule-{}", uuid::Uuid::new_v4()));
        let path = directory.join("test.db");
        let (state, provider) = fixture(&path).await;
        state
            .store
            .save_checkin_settings(&input(false, "09:00", 120))
            .await
            .unwrap();
        let at = (now() + UTC_OFFSET).div_euclid(86_400) * 86_400 - UTC_OFFSET + 8 * 3600;
        let first = prepare(&state, at).await.unwrap();
        assert_eq!(first.next_runs.len(), 2);
        assert!(first.next_runs.iter().all(|plan| {
            let start = at + 3600;
            (start..=start + 120 * 60).contains(&plan.scheduled_at)
        }));
        let snapshot = serde_json::to_value(&first.next_runs).unwrap();
        let again = prepare(&state, at).await.unwrap();
        assert_eq!(snapshot, serde_json::to_value(&again.next_runs).unwrap());
        state
            .store
            .save_checkin_settings(&input(false, "09:00", 120))
            .await
            .unwrap();
        assert_eq!(
            first.settings.revision,
            state.store.checkin_settings().await.unwrap().revision
        );
        drop(state);
        let (reopened, _) = fixture(&path).await;
        let restored = prepare(&reopened, at).await.unwrap();
        assert_eq!(snapshot, serde_json::to_value(&restored.next_runs).unwrap());
        assert!(!restored.settings.enabled);
        assert_eq!(restored.settings.time, "09:00");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        drop(reopened);
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// 修改窗口重排未执行项，已执行项当天不再自动领取。
    #[tokio::test]
    async fn edits_replan_pending_but_preserve_completed_days() {
        let (state, _) = fixture(Path::new(":memory:")).await;
        let at = (now() + UTC_OFFSET).div_euclid(86_400) * 86_400 - UTC_OFFSET + 12 * 3600;
        let plans = due_plans_at(&state, at).await;
        assert!(state.store.claim_checkin_plan(&plans[0], at).await.unwrap());
        state
            .store
            .finish_checkin_plan(&plans[0], "succeeded")
            .await
            .unwrap();
        let old_revision = plans[0].revision;
        state
            .store
            .save_checkin_settings(&input(true, "23:00", 30))
            .await
            .unwrap();
        let view = prepare(&state, at).await.unwrap();
        assert!(view.settings.revision > old_revision);
        let today = state.store.checkin_plans(&day_at(at)).await.unwrap();
        let completed = today
            .iter()
            .find(|plan| plan.account_id == plans[0].account_id && plan.day == plans[0].day)
            .unwrap();
        assert_eq!(completed.status, "succeeded");
        assert_eq!(completed.revision, old_revision);
        let pending = today
            .iter()
            .find(|plan| plan.account_id == plans[1].account_id && plan.day == plans[1].day)
            .unwrap();
        assert_eq!(pending.revision, view.settings.revision);
        assert!(pending.scheduled_at >= plans[1].scheduled_at);
    }

    /// 两个调度检查并发时，每账号仅一次。
    #[tokio::test]
    async fn concurrent_ticks_claim_each_account_once() {
        let (state, provider) = fixture(Path::new(":memory:")).await;
        due_plans(&state).await;
        let (left, right) = tokio::join!(tick(&state, now()), tick(&state, now()));
        left.unwrap();
        right.unwrap();
        tick(&state, now()).await.unwrap();
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
        assert_eq!(state.store.list_tasks(10).await.unwrap().len(), 2);
    }

    /// 领取失败保留结果，当天再次检查或重启都不自动重试。
    #[tokio::test]
    async fn failed_attempts_are_not_automatically_retried() {
        let (state, provider) = fixture(Path::new(":memory:")).await;
        due_plans(&state).await;
        provider.fail.store(true, Ordering::SeqCst);
        tick(&state, now()).await.unwrap();
        state.store.recover_checkin_plans().await.unwrap();
        provider.fail.store(false, Ordering::SeqCst);
        tick(&state, now()).await.unwrap();
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
        assert!(
            state
                .store
                .list_tasks(10)
                .await
                .unwrap()
                .iter()
                .all(|task| task.status == "failed")
        );
    }

    /// 关闭或账号停用时即使已到期也不能领取；重新开启需要新版本计划。
    #[tokio::test]
    async fn disabled_settings_and_accounts_block_due_claims() {
        let (state, provider) = fixture(Path::new(":memory:")).await;
        let plans = due_plans(&state).await;
        state
            .store
            .save_checkin_settings(&input(false, "00:00", 0))
            .await
            .unwrap();
        assert!(
            !state
                .store
                .claim_checkin_plan(&plans[0], now())
                .await
                .unwrap()
        );
        tick(&state, now()).await.unwrap();
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        let settings = state
            .store
            .save_checkin_settings(&input(true, "00:00", 0))
            .await
            .unwrap();
        let mut plan = plans[0].clone();
        plan.revision = settings.revision;
        state.store.save_checkin_plan(&plan).await.unwrap();
        let mut account = state
            .store
            .list_accounts()
            .await
            .unwrap()
            .into_iter()
            .find(|account| account.id == plan.account_id)
            .unwrap();
        account.enabled = false;
        state.store.upsert_account(&account).await.unwrap();
        assert!(!state.store.claim_checkin_plan(&plan, now()).await.unwrap());
    }

    /// 错过窗口的计划等待次日，固定时间也只容忍一分钟偏差。
    #[tokio::test]
    async fn missed_windows_do_not_replay_on_startup() {
        let (state, provider) = fixture(Path::new(":memory:")).await;
        let plans = due_plans(&state).await;
        let at = plans[0].window_end + 61;
        let view = prepare(&state, at).await.unwrap();
        assert!(view.next_runs.iter().all(|plan| plan.day > plans[0].day));
        tick(&state, at).await.unwrap();
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert!(!state.store.claim_checkin_plan(&plans[0], at).await.unwrap());
    }

    /// 中断后的计划与手动任务均标记未知，禁止领取请求再次发出。
    #[tokio::test]
    async fn interrupted_claim_blocks_automatic_and_manual_replay() {
        let (state, provider) = fixture(Path::new(":memory:")).await;
        let plans = due_plans(&state).await;
        assert!(
            state
                .store
                .claim_checkin_plan(&plans[0], now())
                .await
                .unwrap()
        );
        state.store.recover_checkin_plans().await.unwrap();
        let outcome = crate::tasks::run_and_record(
            &state.store,
            provider.as_ref(),
            &plans[0].account_id,
            &plans[0].day,
        )
        .await
        .unwrap();
        assert_eq!(outcome.status, crate::tasks::TaskStatus::Unknown);
        assert!(
            !state
                .store
                .claim_checkin_plan(&plans[0], now())
                .await
                .unwrap()
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    }

    /// 完整 HTTP 路由验证管理鉴权、非法窗口和页面保存往返。
    #[tokio::test]
    async fn schedule_api_checks_auth_and_persists_settings() {
        let (state, provider) = fixture(Path::new(":memory:")).await;
        let app = crate::build_router(state.clone());
        let denied = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/tasks/checkin-schedule")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
        let error: serde_json::Value =
            serde_json::from_slice(&denied.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(error["error"]["code"], "invalid_admin_key");
        for (payload, expected) in [
            (
                serde_json::json!({"enabled":false,"time":"23:59","random_minutes":2}),
                StatusCode::BAD_REQUEST,
            ),
            (
                serde_json::json!({"enabled":false,"time":"10:30","random_minutes":45}),
                StatusCode::OK,
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("PUT")
                        .uri("/api/tasks/checkin-schedule")
                        .header("x-admin-key", "test-admin")
                        .header("content-type", "application/json")
                        .body(Body::from(payload.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/tasks/checkin-schedule")
                    .header("x-admin-key", "test-admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(value["settings"]["time"], "10:30");
        assert_eq!(value["settings"]["random_minutes"], 45);
        assert_eq!(value["timezone"], "Asia/Shanghai");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    }
}
