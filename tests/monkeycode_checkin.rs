//! MonkeyCode 签到真实 HTTP 离线验证，覆盖验证码、幂等、配置隔离和额度发布。

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    routing::{get, post},
};
use fakemodelproxy::{
    accounts::Account,
    backend::Registry,
    config::Config,
    records::RecordLog,
    state::AppState,
    store::{BackendRecord, Store},
    tasks::{TaskProvider, TaskStatus, run_and_record},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::sync::{Notify, Semaphore};
use tower::ServiceExt;

/// 可控制状态、领取响应和质询等待，不接触真实服务。
struct Probe {
    checked: AtomicBool,
    mode: AtomicUsize,
    statuses: AtomicUsize,
    challenges: AtomicUsize,
    redeems: AtomicUsize,
    claims: AtomicUsize,
    wallets: AtomicUsize,
    blocked: AtomicBool,
    entered: Notify,
    release: Semaphore,
}

impl Default for Probe {
    /// 创建关闭的等待门，测试显式释放上游请求。
    fn default() -> Self {
        Self {
            checked: AtomicBool::new(false),
            mode: AtomicUsize::new(0),
            statuses: AtomicUsize::new(0),
            challenges: AtomicUsize::new(0),
            redeems: AtomicUsize::new(0),
            claims: AtomicUsize::new(0),
            wallets: AtomicUsize::new(0),
            blocked: AtomicBool::new(false),
            entered: Notify::new(),
            release: Semaphore::new(0),
        }
    }
}

/// 所有账号 API 只允许测试 Cookie，模型密钥不能发送到签到接口。
fn authentication(headers: &HeaderMap) {
    assert_eq!(headers["cookie"], "session=test");
    assert!(!headers.contains_key("authorization"));
    assert!(!headers.contains_key("x-api-key"));
}

/// 构造毫秒有效期，独立于生产求解器的时钟函数。
fn expires() -> u64 {
    (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
        + 60_000) as u64
}

/// 关键状态缺失时模拟上游协议变化。
async fn status(State(probe): State<Arc<Probe>>, headers: HeaderMap) -> Json<Value> {
    authentication(&headers);
    probe.statuses.fetch_add(1, Ordering::SeqCst);
    if probe.mode.load(Ordering::SeqCst) == 4 {
        return Json(json!({"code":0,"data":{}}));
    }
    Json(json!({"code":0,"data":{"checked_in":probe.checked.load(Ordering::SeqCst)}}))
}

/// 质询按官方裸 JSON 和 201 返回，等待门用于模拟配置在计算期间变化。
async fn challenge(
    State(probe): State<Arc<Probe>>,
    headers: HeaderMap,
    body: String,
) -> (StatusCode, Json<Value>) {
    authentication(&headers);
    assert!(body.is_empty());
    probe.challenges.fetch_add(1, Ordering::SeqCst);
    probe.entered.notify_one();
    if probe.blocked.load(Ordering::SeqCst) {
        probe.release.acquire().await.unwrap().forget();
    }
    let mode = probe.mode.load(Ordering::SeqCst);
    if mode == 5 {
        return (StatusCode::FORBIDDEN, Json(json!({"message":"rejected"})));
    }
    (
        StatusCode::CREATED,
        Json(json!({
            "token":"fixture-token", "challenge":{"c":if mode == 8 {129} else {3},"s":16,"d":2},
            "expires":if mode == 6 {1} else {expires()},
        })),
    )
}

/// 独立 Python hashlib 预先求出的最小 nonce，校验真实求解器的请求。
async fn redeem(
    State(probe): State<Arc<Probe>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    authentication(&headers);
    assert_eq!(
        body,
        json!({"token":"fixture-token","solutions":[121,481,362]})
    );
    probe.redeems.fetch_add(1, Ordering::SeqCst);
    (
        StatusCode::CREATED,
        Json(json!({
            "success":true,"token":"redeemed-token",
            "expires":if probe.mode.load(Ordering::SeqCst) == 7 {1} else {expires()},
        })),
    )
}

/// 领取可模拟服务已处理但响应失败，以及完全不能确认的失败。
async fn claim(
    State(probe): State<Arc<Probe>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    authentication(&headers);
    assert_eq!(body, json!({"captcha_token":"redeemed-token"}));
    probe.claims.fetch_add(1, Ordering::SeqCst);
    let mode = probe.mode.load(Ordering::SeqCst);
    if mode != 2 {
        probe.checked.store(true, Ordering::SeqCst);
    }
    if mode == 2 || mode == 3 {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})));
    }
    if mode == 9 {
        return (StatusCode::OK, Json(json!({"code":0,"data":{}})));
    }
    (
        StatusCode::OK,
        Json(json!({"code":0,"data":{"checked_in":true}})),
    )
}

/// 钱包使用小数，验证结果通过现有额度缓存暴露。
async fn wallet(State(probe): State<Arc<Probe>>, headers: HeaderMap) -> (StatusCode, Json<Value>) {
    authentication(&headers);
    probe.wallets.fetch_add(1, Ordering::SeqCst);
    if probe.mode.load(Ordering::SeqCst) == 10 {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})));
    }
    (
        StatusCode::OK,
        Json(json!({"code":0,"data":{"balance":"101.19999988"}})),
    )
}

/// 通过生产注册入口创建一个本地假服务，后台调度默认关闭。
async fn fixture() -> (AppState, Arc<Probe>, tokio::task::JoinHandle<()>) {
    let probe = Arc::new(Probe::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let upstream = Router::new()
        .route("/api/v1/users/wallet/checkin", get(status).post(claim))
        .route("/api/v1/public/captcha/challenge", post(challenge))
        .route("/api/v1/public/captcha/redeem", post(redeem))
        .route("/api/v1/users/wallet", get(wallet))
        .with_state(probe.clone());
    let job = tokio::spawn(async move {
        axum::serve(listener, upstream).await.unwrap();
    });
    let store = Store::open(std::path::Path::new(":memory:")).await.unwrap();
    store
        .upsert_backend(&BackendRecord {
            id: "mc".into(),
            kind: "monkeycode".into(),
            name: "MonkeyCode".into(),
            base_url: format!("{base}/v1"),
            ..Default::default()
        })
        .await
        .unwrap();
    store
        .upsert_account(&Account {
            id: "a".into(),
            backend_id: "mc".into(),
            name: "离线账号".into(),
            enabled: true,
            credential: json!({"api_key":"model-key","signing_secret":"model-secret",
            "cookie":"session=test","server":base,"base_url":format!("{base}/v1")})
            .to_string(),
        })
        .await
        .unwrap();
    let state = AppState::new(
        Arc::new(Config::default()),
        store.clone(),
        Arc::new(Registry::new()),
        Arc::new(RecordLog::default()),
        Arc::new(RwLock::new(fakemodelproxy::tasks::TaskRegistry::new())),
        Arc::new(fakemodelproxy::routing::Router::new()),
        Arc::new(fakemodelproxy::usage::UsageLedger::new(store)),
    );
    state.reload_backends().await.unwrap();
    (state, probe, job)
}

/// 返回生产注册的签到执行器，不在测试中另行替换其网络行为。
fn provider(state: &AppState) -> Arc<dyn TaskProvider> {
    state
        .task_providers()
        .into_iter()
        .find(|provider| provider.backend_id() == "mc")
        .unwrap()
}

/// 请求控制台使用的管理 API 并解析响应。
async fn api(state: &AppState, method: &str, path: &str) -> Value {
    let response = fakemodelproxy::build_router(state.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

/// 并发签到只提交一次，成功后精确余额和版本更新且不推测奖励。
#[tokio::test]
async fn checkin_claims_once_and_updates_only_account_quota() {
    let (state, probe, job) = fixture().await;
    state
        .store
        .finish_quota_refresh(&[], 1000, 44200)
        .await
        .unwrap();
    let provider = provider(&state);
    let (first, second) = tokio::join!(
        run_and_record(&state.store, provider.as_ref(), "a", "2026-11-20"),
        run_and_record(&state.store, provider.as_ref(), "a", "2026-11-20"),
    );
    assert_eq!(first.unwrap().status, TaskStatus::Succeeded);
    assert_eq!(second.unwrap().status, TaskStatus::AlreadyDone);
    assert_eq!(probe.claims.load(Ordering::SeqCst), 1);
    assert_eq!(probe.wallets.load(Ordering::SeqCst), 1);
    let cached = api(&state, "GET", "/api/quota/cached").await;
    assert_eq!(cached["quota"][0]["routing"]["remaining"], "101.19999988");
    assert_eq!(cached["auto_refresh"]["generation"], 2);
    assert_eq!(cached["auto_refresh"]["last_completed_at"], 1000);
    assert_eq!(cached["auto_refresh"]["next_refresh_at"], 44200);
    let record = state
        .store
        .get_task("mc", "a", "checkin", "2026-11-20")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.amount, None);
    assert!(!record.message.contains("100"));
    job.abort();
}

/// 未知结果持久化后，即使重载执行器也不再次提交；只查询一次状态核对。
#[tokio::test]
async fn unknown_claim_is_never_replayed_after_reload() {
    let (state, probe, job) = fixture().await;
    probe.mode.store(2, Ordering::SeqCst);
    let outcome = run_and_record(&state.store, provider(&state).as_ref(), "a", "2026-11-20")
        .await
        .unwrap();
    assert_eq!(outcome.status, TaskStatus::Unknown);
    state.reload_backends().await.unwrap();
    let again = run_and_record(&state.store, provider(&state).as_ref(), "a", "2026-11-20")
        .await
        .unwrap();
    assert_eq!(again.status, TaskStatus::Unknown);
    assert_eq!(probe.statuses.load(Ordering::SeqCst), 2);
    assert_eq!(probe.claims.load(Ordering::SeqCst), 1);
    assert_eq!(probe.wallets.load(Ordering::SeqCst), 0);
    job.abort();
}

/// 缺失领取状态或 5xx 可用 GET 确认，不能用再次 POST 核对。
#[tokio::test]
async fn ambiguous_claim_is_confirmed_by_read_only_status() {
    for mode in [3, 9] {
        let (state, probe, job) = fixture().await;
        probe.mode.store(mode, Ordering::SeqCst);
        let outcome = provider(&state)
            .run_for_account("a", "2026-11-20")
            .await
            .unwrap();
        assert_eq!(outcome.status, TaskStatus::Succeeded);
        assert_eq!(outcome.amount, None);
        assert_eq!(probe.statuses.load(Ordering::SeqCst), 2);
        assert_eq!(probe.claims.load(Ordering::SeqCst), 1);
        job.abort();
    }
}

/// 已签到无需验证码；钱包失败也不能改变已完成的签到状态。
#[tokio::test]
async fn already_checked_and_quota_failure_preserve_completed_status() {
    for checked in [false, true] {
        let (state, probe, job) = fixture().await;
        probe.checked.store(checked, Ordering::SeqCst);
        probe.mode.store(10, Ordering::SeqCst);
        let outcome = provider(&state)
            .run_for_account("a", "2026-11-20")
            .await
            .unwrap();
        assert_eq!(
            outcome.status,
            if checked {
                TaskStatus::AlreadyDone
            } else {
                TaskStatus::Succeeded
            }
        );
        assert!(outcome.message.contains("额度暂未更新"));
        assert_eq!(
            probe.challenges.load(Ordering::SeqCst),
            usize::from(!checked)
        );
        job.abort();
    }
}

/// 状态缺失、验证码拒绝、过期或超大参数都不能提交领取。
#[tokio::test]
async fn invalid_status_or_captcha_cannot_claim() {
    for mode in [4, 5, 6, 7, 8] {
        let (state, probe, job) = fixture().await;
        probe.mode.store(mode, Ordering::SeqCst);
        assert!(
            provider(&state)
                .run_for_account("a", "2026-11-20")
                .await
                .is_err()
        );
        assert_eq!(probe.claims.load(Ordering::SeqCst), 0);
        if mode == 4 {
            assert_eq!(probe.challenges.load(Ordering::SeqCst), 0);
        }
        job.abort();
    }
}

/// 质询期间修改凭证、停用或切换服务时，不能提交兑换或领取。
#[tokio::test]
async fn configuration_changes_during_verification_cancel_claim() {
    for mode in 0..3 {
        let (state, probe, job) = fixture().await;
        probe.blocked.store(true, Ordering::SeqCst);
        let provider = provider(&state);
        let run = tokio::spawn(async move { provider.run_for_account("a", "2026-11-20").await });
        probe.entered.notified().await;
        if mode == 0 {
            let mut account = state.store.get_account("a").await.unwrap().unwrap();
            account.credential = account
                .credential
                .replace("session=test", "session=changed");
            state.store.upsert_account(&account).await.unwrap();
        } else {
            let mut backend = state.store.get_backend("mc").await.unwrap().unwrap();
            if mode == 1 {
                backend.enabled = false;
            } else {
                backend.options = r#"{"region":"global"}"#.into();
            }
            state.store.upsert_backend(&backend).await.unwrap();
        }
        probe.release.add_permits(1);
        assert!(run.await.unwrap().is_err());
        assert_eq!(probe.redeems.load(Ordering::SeqCst), 0);
        assert_eq!(probe.claims.load(Ordering::SeqCst), 0);
        job.abort();
    }
}

/// 错误区域、账号归属及停用账号在首次请求前拒绝，不泄露 Cookie。
#[tokio::test]
async fn origin_ownership_and_enabled_state_are_validated_before_requests() {
    for mode in 0..4 {
        let (state, probe, job) = fixture().await;
        let mut account = state.store.get_account("a").await.unwrap().unwrap();
        match mode {
            0 => account.credential = account.credential.replace("session=test", ""),
            1 => {
                let mut value: Value = serde_json::from_str(&account.credential).unwrap();
                value["server"] = json!("https://monkeycode-ai.net");
                account.credential = value.to_string();
            }
            2 => account.enabled = false,
            _ => {
                let mut backend = state.store.get_backend("mc").await.unwrap().unwrap();
                backend.id = "other".into();
                state.store.upsert_backend(&backend).await.unwrap();
                account.backend_id = "other".into();
            }
        }
        state.store.upsert_account(&account).await.unwrap();
        assert!(
            provider(&state)
                .run_for_account("a", "2026-11-20")
                .await
                .is_err()
        );
        assert_eq!(probe.statuses.load(Ordering::SeqCst), 0);
        job.abort();
    }
}

/// 控制台单服务、批量和每日计划都从注册表发现 MonkeyCode。
#[tokio::test]
async fn manual_batch_and_daily_schedule_discover_monkeycode() {
    let (state, probe, job) = fixture().await;
    probe.checked.store(true, Ordering::SeqCst);
    let runtime = api(&state, "GET", "/api/console/status").await;
    assert!(
        runtime["checkin_backends"]
            .as_array()
            .unwrap()
            .contains(&json!("mc"))
    );
    let result = api(&state, "POST", "/api/tasks/mc/checkin").await;
    assert_eq!(result["results"][0]["status"], "already_done");
    let view = fakemodelproxy::tasks::scheduler::view(&state)
        .await
        .unwrap();
    assert!(
        view.next_runs
            .iter()
            .any(|plan| plan.backend_id == "mc" && plan.account_id == "a")
    );
    api(&state, "POST", "/api/tasks/checkin-all").await;
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let view = state.checkin_batch.view();
            if view.run.as_ref().is_some_and(|run| run.status != "running") {
                let run = view.run.unwrap();
                assert_eq!(run.accounts[0].status, "already_done");
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(probe.claims.load(Ordering::SeqCst), 0);
    assert_eq!(probe.wallets.load(Ordering::SeqCst), 1);
    job.abort();
}
