//! 十二小时额度更新回归：到期、重启、失败隔离、并发与缓存读取。

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::IntoResponse;
use axum::{Json, Router, routing::get};
use fakemodelproxy::accounts::Account;
use fakemodelproxy::accounts::quota_refresh::{INTERVAL_SECONDS, refresh};
use fakemodelproxy::backend::Registry;
use fakemodelproxy::config::Config;
use fakemodelproxy::records::RecordLog;
use fakemodelproxy::state::AppState;
use fakemodelproxy::store::{BackendRecord, Store};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::{Notify, Semaphore};
use tower::ServiceExt;

/// 假上游提供可观测查询次数及可控制的并发等待。
struct Probe {
    calls: AtomicUsize,
    blocked: AtomicBool,
    entered: Notify,
    release: Semaphore,
    tokens: Mutex<HashSet<String>>,
}

/// 返回精确小数额度，特定假令牌用于模拟一个账号失败。
async fn quota(State(probe): State<Arc<Probe>>, headers: HeaderMap) -> impl IntoResponse {
    probe.calls.fetch_add(1, Ordering::SeqCst);
    probe.tokens.lock().unwrap().insert(
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .into(),
    );
    probe.entered.notify_one();
    if probe.blocked.load(Ordering::SeqCst) {
        probe.release.acquire().await.unwrap().forget();
    }
    if headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        == Some("Bearer bad")
    {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})));
    }
    (
        StatusCode::OK,
        Json(json!({"userQuota":{"total":100.12,"used":2.01,"unit":"credits"}})),
    )
}

/// 装配隔离状态，不启动自动签到或访问真实厂商。
fn app(store: Store) -> AppState {
    AppState::new(
        Arc::new(Config {
            listen: "127.0.0.1:0".into(),
            data_dir: ".".into(),
            ..Default::default()
        }),
        store.clone(),
        Arc::new(Registry::new()),
        Arc::new(RecordLog::default()),
        Arc::new(RwLock::new(fakemodelproxy::tasks::TaskRegistry::new())),
        Arc::new(fakemodelproxy::routing::Router::new()),
        Arc::new(fakemodelproxy::usage::UsageLedger::new(store)),
    )
}

/// 创建本地假上游与一个已启用账号，端点仅存入假凭证。
async fn fixture(store: Store) -> (AppState, Arc<Probe>, String) {
    let probe = Arc::new(Probe {
        calls: AtomicUsize::new(0),
        blocked: AtomicBool::new(false),
        entered: Notify::new(),
        release: Semaphore::new(0),
        tokens: Mutex::new(HashSet::new()),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/quota", listener.local_addr().unwrap());
    let upstream = Router::new()
        .route("/quota", get(quota))
        .with_state(probe.clone());
    tokio::spawn(async move {
        axum::serve(listener, upstream).await.unwrap();
    });
    store
        .upsert_backend(&BackendRecord {
            id: "q".into(),
            name: "Qoder".into(),
            kind: "qoder".into(),
            base_url: "https://gateway.qoder.com.cn".into(),
            models: "m".into(),
            options: r#"{"region":"cn"}"#.into(),
            ..Default::default()
        })
        .await
        .unwrap();
    store
        .upsert_account(&account("a", "q", &url, "good", true))
        .await
        .unwrap();
    (app(store), probe, url)
}

/// 构造无真实凭证的额度账号。
fn account(id: &str, backend: &str, url: &str, token: &str, enabled: bool) -> Account {
    Account {
        id: id.into(),
        backend_id: backend.into(),
        name: id.into(),
        enabled,
        credential: json!({"device_token":token,"quota_url":url}).to_string(),
    }
}

/// 精确到期才再次请求上游，重开数据库后计划和余额仍保留。
#[tokio::test]
async fn twelve_hour_schedule_and_cache_survive_reopening_database() {
    let path = std::env::temp_dir().join(format!("fmp-quota-{}.db", uuid::Uuid::new_v4()));
    let store = Store::open(&path).await.unwrap();
    let (state, probe, _) = fixture(store).await;
    let first = refresh(&state, false, 1000).await.unwrap();
    assert_eq!(
        first.auto_refresh.record.next_refresh_at,
        Some(1000 + INTERVAL_SECONDS)
    );
    assert_eq!(
        first.quota[0]
            .routing
            .as_ref()
            .unwrap()
            .remaining
            .as_deref(),
        Some("98.11")
    );
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    let restarted = app(Store::open(&path).await.unwrap());
    let cached = refresh(&restarted, false, 1000 + INTERVAL_SECONDS - 1)
        .await
        .unwrap();
    assert_eq!(cached.auto_refresh.record.generation, 1);
    assert_eq!(cached.quota.len(), 1);
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    let second = refresh(&restarted, false, 1000 + INTERVAL_SECONDS)
        .await
        .unwrap();
    assert_eq!(second.auto_refresh.record.generation, 2);
    assert_eq!(probe.calls.load(Ordering::SeqCst), 2);
    let manual = refresh(&restarted, true, 1000 + INTERVAL_SECONDS + 10)
        .await
        .unwrap();
    assert_eq!(
        manual.auto_refresh.record.next_refresh_at,
        Some(1000 + 2 * INTERVAL_SECONDS + 10)
    );
}

/// 失败账号和无效配置隔离；停用账号及服务不会发送请求。
#[tokio::test]
async fn failures_are_isolated_and_disabled_accounts_are_skipped() {
    let (state, probe, url) =
        fixture(Store::open(std::path::Path::new(":memory:")).await.unwrap()).await;
    state
        .store
        .upsert_account(&account("b", "q", &url, "bad", true))
        .await
        .unwrap();
    state
        .store
        .upsert_account(&account("c", "q", &url, "disabled-account", false))
        .await
        .unwrap();
    for (id, enabled, options) in [("disabled", false, "{}"), ("invalid", true, "invalid-json")] {
        state
            .store
            .upsert_backend(&BackendRecord {
                id: id.into(),
                name: id.into(),
                kind: "qoder".into(),
                enabled,
                base_url: "https://gateway.qoder.com.cn".into(),
                options: options.into(),
                ..Default::default()
            })
            .await
            .unwrap();
        state
            .store
            .upsert_account(&account(id, id, &url, id, true))
            .await
            .unwrap();
    }
    let result = refresh(&state, false, 1000).await.unwrap();
    assert_eq!(result.quota.len(), 3);
    assert_eq!(
        *probe.tokens.lock().unwrap(),
        HashSet::from(["Bearer good".into(), "Bearer bad".into()])
    );
    assert_eq!(
        result
            .quota
            .iter()
            .filter(|info| info.confidence
                == fakemodelproxy::accounts::remote::QuotaConfidence::Unknown)
            .count(),
        2
    );
    assert_eq!(
        result.auto_refresh.record.last_error.as_deref(),
        Some("2 个账号暂未读取成功")
    );
}

/// 同时进入的手动与自动刷新等待同一批，并发手动更新不重复访问上游。
#[tokio::test]
async fn concurrent_manual_refreshes_share_one_batch() {
    let (state, probe, _) =
        fixture(Store::open(std::path::Path::new(":memory:")).await.unwrap()).await;
    probe.blocked.store(true, Ordering::SeqCst);
    let gate = probe.clone();
    tokio::spawn(async move {
        gate.entered.notified().await;
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        gate.release.add_permits(1);
    });
    let (first, second, automatic) = tokio::join!(
        refresh(&state, true, 1000),
        refresh(&state, true, 1000),
        refresh(&state, false, 1000),
    );
    assert_eq!(first.unwrap().auto_refresh.record.generation, 1);
    assert_eq!(second.unwrap().auto_refresh.record.generation, 1);
    assert_eq!(automatic.unwrap().auto_refresh.record.generation, 1);
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
}

/// 缓存接口只读本地数据库，人工改凭证或停用后不展示旧余额。
#[tokio::test]
async fn cached_endpoint_does_not_query_upstream_and_configuration_invalidates_cache() {
    let (state, probe, url) =
        fixture(Store::open(std::path::Path::new(":memory:")).await.unwrap()).await;
    refresh(&state, false, 1000).await.unwrap();
    for _ in 0..2 {
        let response = fakemodelproxy::build_router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/quota/cached")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["quota"][0]["routing"]["remaining"], "98.11");
    }
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    // 自动续期保持同一钱包，人工编辑凭证则清除旧快照。
    let previous = state.store.get_account("a").await.unwrap().unwrap();
    let rotated = account("a", "q", &url, "rotated", true);
    state
        .store
        .replace_credential("a", &previous.credential, &rotated.credential)
        .await
        .unwrap();
    assert_eq!(state.store.cached_quota().await.unwrap().len(), 1);
    state
        .store
        .upsert_account(&account("a", "q", &url, "manual", true))
        .await
        .unwrap();
    assert!(state.store.cached_quota().await.unwrap().is_empty());
    refresh(&state, true, 1010).await.unwrap();
    let mut backend = state.store.get_backend("q").await.unwrap().unwrap();
    backend.options = r#"{"region":"global"}"#.into();
    state.store.upsert_backend(&backend).await.unwrap();
    assert!(state.store.cached_quota().await.unwrap().is_empty());
}

/// 单账号发布不能让等待中的手动全量刷新误以为整批已完成。
#[tokio::test]
async fn account_refresh_does_not_suppress_a_waiting_full_refresh() {
    let (state, probe, _) =
        fixture(Store::open(std::path::Path::new(":memory:")).await.unwrap()).await;
    probe.blocked.store(true, Ordering::SeqCst);
    let backend = state.store.get_backend("q").await.unwrap().unwrap();
    let account = state.store.get_account("a").await.unwrap().unwrap();
    let single_state = state.clone();
    let single = tokio::spawn(async move {
        fakemodelproxy::accounts::quota_refresh::refresh_account(
            &single_state.store,
            &single_state.quota_refresh,
            &backend,
            &account,
        )
        .await
    });
    probe.entered.notified().await;
    let waiting = Arc::new(Notify::new());
    let entered = waiting.clone();
    let batch_state = state.clone();
    let batch = tokio::spawn(async move {
        entered.notify_one();
        refresh(&batch_state, true, 1000).await
    });
    waiting.notified().await;
    probe.release.add_permits(2);
    single.await.unwrap().unwrap();
    let full = batch.await.unwrap().unwrap();
    assert_eq!(probe.calls.load(Ordering::SeqCst), 2);
    assert_eq!(full.auto_refresh.record.generation, 2);
    assert_eq!(
        full.auto_refresh.record.next_refresh_at,
        Some(1000 + INTERVAL_SECONDS)
    );
}

/// 查询时重新登录后的迟到余额不能回填旧凭证，缓存版本也不能推进。
#[tokio::test]
async fn account_refresh_discards_balance_if_credential_changes_during_read() {
    let (state, probe, url) =
        fixture(Store::open(std::path::Path::new(":memory:")).await.unwrap()).await;
    probe.blocked.store(true, Ordering::SeqCst);
    let backend = state.store.get_backend("q").await.unwrap().unwrap();
    let original = state.store.get_account("a").await.unwrap().unwrap();
    let single_state = state.clone();
    let single = tokio::spawn(async move {
        fakemodelproxy::accounts::quota_refresh::refresh_account(
            &single_state.store,
            &single_state.quota_refresh,
            &backend,
            &original,
        )
        .await
    });
    probe.entered.notified().await;
    state
        .store
        .upsert_account(&account("a", "q", &url, "changed", true))
        .await
        .unwrap();
    probe.release.add_permits(1);
    assert!(single.await.unwrap().is_err());
    assert!(state.store.cached_quota().await.unwrap().is_empty());
    assert_eq!(
        state.store.quota_refresh_record().await.unwrap().generation,
        0
    );
}
