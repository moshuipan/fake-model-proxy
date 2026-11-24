//! 魔粒真实管理链路回归；所有网络请求和凭证仅使用本机假上游。

use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, Uri},
    response::Response,
};
use fakemodelproxy::{
    accounts::{
        Account, quota_refresh,
        remote::{QuotaConfidence, QuotaReader},
    },
    backend::modelscope::{ModelScopeBackend, ModelScopeCheckin},
    store::{BackendRecord, Store},
    tasks::{self, TaskProvider, TaskStatus, scheduler},
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

const KEY: &str = "fake-access-token";

/// 记录假上游请求的方法、路径、鉴权及登录体，供协议断言使用。
struct Mock {
    responses: Mutex<VecDeque<Value>>,
    paths: Mutex<Vec<String>>,
}
/// 顺序发送官方格式信封，登录响应发出仅供本测试使用的 Cookie。
async fn upstream(
    State(mock): State<Arc<Mock>>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = uri.path();
    mock.paths.lock().unwrap().push(path.into());
    if path == "/api/v1/login" {
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({"AccessToken":KEY})
        );
        assert!(headers.get("cookie").is_none());
    } else if path.starts_with("/openapi/") {
        assert_eq!(headers["authorization"], format!("Bearer {KEY}"));
        assert!(headers.get("cookie").is_none());
    } else {
        assert_eq!(
            headers["cookie"],
            "m_session_id=fake-session; csrf_token=fake-csrf"
        );
        assert!(headers.get("authorization").is_none());
    }
    let value = mock
        .responses
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or(json!({"success":false}));
    let mut response = Response::builder().header("content-type", "application/json");
    if path == "/api/v1/login" {
        response = response
            .header("set-cookie", "m_session_id=fake-session; Path=/; HttpOnly")
            .header("set-cookie", "csrf_token=fake-csrf; Path=/");
    }
    response.body(Body::from(value.to_string())).unwrap()
}

/// 启动本机上游，并保存不含真实身份的魔搭服务及自动引用账号。
async fn fixture(
    responses: Vec<Value>,
) -> (
    Store,
    BackendRecord,
    Account,
    Arc<Mock>,
    tokio::task::JoinHandle<()>,
) {
    let mock = Arc::new(Mock {
        responses: Mutex::new(responses.into()),
        paths: Mutex::new(vec![]),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let record = BackendRecord {
        id: "ms".into(),
        name: "ModelScope".into(),
        kind: "modelscope".into(),
        base_url: format!("http://{}/v1", listener.local_addr().unwrap()),
        api_key: KEY.into(),
        ..Default::default()
    };
    let router = Router::new().fallback(upstream).with_state(mock.clone());
    let handle = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let store = Store::open(Path::new(":memory:")).await.unwrap();
    store.upsert_backend(&record).await.unwrap();
    store.sync_modelscope_accounts().await.unwrap();
    let account = store.list_accounts_for("ms").await.unwrap().remove(0);
    (store, record, account, mock, handle)
}

/// 构造官方余额信封，支持小数和冻结余额。
fn balance(available: Value, disabled: bool) -> Value {
    json!({"success":true,"data":{"available_balance":available,"total_balance":"550.125",
        "frozen_amount":"50","is_disabled":disabled}})
}
/// 构造两项独立的今日领取状态，不从固定奖励数推算结果。
fn rules(daily: bool, aliyun: bool, daily_amount: i64, aliyun_amount: i64) -> Value {
    json!({"success":true,"data":[
        {"rule_key":"daily_active","today_used":u64::from(daily),"today_earned_amount":daily_amount},
        {"rule_key":"aliyun_bindlogin","today_used":u64::from(aliyun),"today_earned_amount":aliyun_amount}]})
}
/// 构造 SDK 和网站的大小写业务信封。
fn sdk(data: Value) -> Value {
    json!({"Success":true,"Code":200,"Data":data})
}

/// 小数余额保持精确，冻结量不计入可用量，停用钱包不能进入可用候选。
#[tokio::test]
async fn precise_available_balance_and_disabled_wallet_are_distinct() {
    for disabled in [false, true] {
        let (store, record, account, _, handle) =
            fixture(vec![balance(json!("500.123456789123456789"), disabled)]).await;
        let backend = ModelScopeBackend::new(&record, store, Duration::from_secs(1)).unwrap();
        let info = backend.read(&account).await.unwrap();
        assert_eq!(info.confidence, QuotaConfidence::Verified);
        assert_eq!(
            info.routing.as_ref().unwrap().remaining.as_deref(),
            Some(if disabled {
                "0"
            } else {
                "500.123456789123456789"
            })
        );
        assert!(info.routing.unwrap().valid_until.unwrap() <= info.checked_at + 60);
        assert!(info.used.is_none() && info.total.is_none() && info.remaining.is_none());
        assert!(info.note.unwrap().contains("冻结 50 魔粒"));
        handle.abort();
    }
}

/// 业务失败和缺失字段不能以零余额或成功任务发布。
#[tokio::test]
async fn rejected_and_incomplete_balance_never_become_verified_zero() {
    for value in [
        json!({"success":false,"data":{"available_balance":0}}),
        json!({"success":true,"data":{"available_balance":0}}),
    ] {
        let (store, record, account, _, handle) = fixture(vec![value]).await;
        let backend = ModelScopeBackend::new(&record, store, Duration::from_secs(1)).unwrap();
        assert!(backend.read(&account).await.is_err());
        handle.abort();
    }
}

/// 今日已完成时只查询规则和余额，不再登录；同周期再次执行不访问上游。
#[tokio::test]
async fn already_earned_skips_login_and_persists_idempotency() {
    let (store, record, account, mock, handle) =
        fixture(vec![rules(true, true, 200, 50), balance(json!(500), false)]).await;
    let provider = ModelScopeCheckin::new(
        &record,
        store.clone(),
        Arc::new(quota_refresh::Control::default()),
    )
    .unwrap();
    let period = scheduler::day_at(scheduler::now());
    for _ in 0..2 {
        let result = tasks::run_and_record(&store, &provider, &account.id, &period)
            .await
            .unwrap();
        assert_eq!(result.status, TaskStatus::AlreadyDone);
        assert!(result.amount.is_none());
        assert!(result.message.contains("250 魔粒"));
    }
    assert_eq!(
        *mock.paths.lock().unwrap(),
        vec![
            "/openapi/v1/magicubes/earn/rules",
            "/openapi/v1/magicubes/balance"
        ]
    );
    assert_eq!(
        store.cached_quota().await.unwrap()[0]
            .routing
            .as_ref()
            .unwrap()
            .remaining
            .as_deref(),
        Some("500")
    );
    handle.abort();
}

/// 已绑定和未绑定分别核对实际发放，登录 Cookie 仅发给网站端点。
#[tokio::test]
async fn official_login_confirms_actual_rewards_for_bound_and_unbound_accounts() {
    for bound in [true, false] {
        let (store, record, account, mock, handle) = fixture(vec![
            rules(false, false, 0, 0),
            sdk(json!({})),
            sdk(json!({})),
            sdk(json!({"IsBindingYun":bound})),
            rules(true, bound, 197, if bound { 43 } else { 0 }),
            balance(json!(500), false),
        ])
        .await;
        let provider = ModelScopeCheckin::new(
            &record,
            store.clone(),
            Arc::new(quota_refresh::Control::default()),
        )
        .unwrap();
        let result = tasks::run_and_record(
            &store,
            &provider,
            &account.id,
            &scheduler::day_at(scheduler::now()),
        )
        .await
        .unwrap();
        assert_eq!(result.status, TaskStatus::Succeeded, "{}", result.message);
        assert_eq!(result.amount, Some(if bound { 240 } else { 197 }));
        assert_eq!(
            *mock.paths.lock().unwrap(),
            vec![
                "/openapi/v1/magicubes/earn/rules",
                "/api/v1/login",
                "/api/v1/users/login/info",
                "/api/v1/users/binding/check",
                "/openapi/v1/magicubes/earn/rules",
                "/openapi/v1/magicubes/balance",
            ]
        );
        assert!(
            !store.list_accounts().await.unwrap()[0]
                .credential
                .contains("fake-session")
        );
        handle.abort();
    }
}

/// 登录后仍无法确认奖励时记录未知；重复执行不能重复登录。
#[tokio::test]
async fn uncertain_login_is_not_replayed_in_the_same_period() {
    let (store, record, account, mock, handle) = fixture(vec![
        rules(false, false, 0, 0),
        sdk(json!({})),
        sdk(json!({})),
        sdk(json!({"IsBindingYun":true})),
        rules(true, false, 200, 0),
        balance(json!(200), false),
    ])
    .await;
    let provider = ModelScopeCheckin::new(
        &record,
        store.clone(),
        Arc::new(quota_refresh::Control::default()),
    )
    .unwrap();
    let period = scheduler::day_at(scheduler::now());
    for _ in 0..2 {
        assert_eq!(
            tasks::run_and_record(&store, &provider, &account.id, &period)
                .await
                .unwrap()
                .status,
            TaskStatus::Unknown
        );
    }
    assert_eq!(
        mock.paths
            .lock()
            .unwrap()
            .iter()
            .filter(|path| *path == "/api/v1/login")
            .count(),
        1
    );
    handle.abort();
}

/// 更换令牌立即失效余额，旧执行器和迟到快照不能使用旧身份。
#[tokio::test]
async fn token_change_invalidates_quota_and_rejects_old_provider_and_publication() {
    let (store, record, account, mock, handle) = fixture(vec![balance(json!(500), false)]).await;
    let backend = ModelScopeBackend::new(&record, store.clone(), Duration::from_secs(1)).unwrap();
    let info = backend.read(&account).await.unwrap();
    assert!(
        store
            .cache_account_quota(&record, &account, &info)
            .await
            .unwrap()
    );
    let mut changed = record.clone();
    changed.api_key = "another-fake-token".into();
    store.upsert_backend(&changed).await.unwrap();
    assert!(store.cached_quota().await.unwrap().is_empty());
    assert!(
        !store
            .cache_account_quota(&record, &account, &info)
            .await
            .unwrap()
    );
    assert!(backend.read(&account).await.is_err());
    store.sync_modelscope_accounts().await.unwrap();
    let new_account = store.list_accounts_for("ms").await.unwrap().remove(0);
    assert_ne!(new_account.id, account.id);
    assert!(!new_account.credential.contains("another-fake-token"));
    assert!(store.get_account(&account.id).await.unwrap().is_none());
    assert_eq!(mock.paths.lock().unwrap().len(), 1);
    handle.abort();
}

/// 非今日周期在网络动作前拒绝，避免跨周期领取记录。
#[tokio::test]
async fn stale_period_does_not_send_login_or_read_rules() {
    let (store, record, account, mock, handle) = fixture(vec![]).await;
    let provider =
        ModelScopeCheckin::new(&record, store, Arc::new(quota_refresh::Control::default()))
            .unwrap();
    assert!(
        provider
            .run_for_account(&account.id, "1970-01-01")
            .await
            .is_err()
    );
    assert!(mock.paths.lock().unwrap().is_empty());
    handle.abort();
}

/// 旧官方兼容配置迁移只修改类型，模型及其他配置和外部来源账号继续保留。
#[tokio::test]
async fn legacy_official_configuration_migrates_without_overwriting_fields() {
    let store = Store::open(Path::new(":memory:")).await.unwrap();
    let mut record = BackendRecord {
        id: "legacy".into(),
        name: "旧魔搭".into(),
        kind: "openai_compat".into(),
        base_url: "https://api-inference.modelscope.cn/v1/".into(),
        api_key: KEY.into(),
        models: "Qwen/Qwen3".into(),
        priority: 43,
        options: "{}".into(),
        capabilities: "{\"tools\":false}".into(),
        ..Default::default()
    };
    store.upsert_backend(&record).await.unwrap();
    let external = Account {
        id: "external".into(),
        backend_id: "other".into(),
        name: "其他账号".into(),
        credential: "{}".into(),
        enabled: true,
    };
    store.upsert_account(&external).await.unwrap();
    store.sync_modelscope_accounts().await.unwrap();
    record.kind = "modelscope".into();
    assert_eq!(
        serde_json::to_value(store.get_backend("legacy").await.unwrap().unwrap()).unwrap(),
        serde_json::to_value(&record).unwrap()
    );
    assert_eq!(store.list_accounts_for("legacy").await.unwrap().len(), 1);
    store.sync_modelscope_accounts().await.unwrap();
    assert_eq!(store.list_accounts_for("legacy").await.unwrap().len(), 1);
    record.enabled = false;
    store.upsert_backend(&record).await.unwrap();
    store.sync_modelscope_accounts().await.unwrap();
    assert!(!store.list_accounts_for("legacy").await.unwrap()[0].enabled);
    store.delete_backend("legacy").await.unwrap();
    store.sync_modelscope_accounts().await.unwrap();
    assert!(store.list_accounts_for("legacy").await.unwrap().is_empty());
    assert!(store.get_account("external").await.unwrap().is_some());
}

/// 一次升级清除旧预设默认屏蔽；局部覆盖及升级后新设的完整覆盖仍保留。
#[tokio::test]
async fn legacy_capability_mask_is_removed_without_losing_custom_overrides() {
    let store = Store::open(Path::new(":memory:")).await.unwrap();
    for (id, capabilities) in [
        (
            "preset",
            r#"{"tools":false,"parallel_tools":false,"images":false}"#,
        ),
        (
            "pretty",
            r#"{"tools": false, "parallel_tools": false, "images": false}"#,
        ),
        ("custom", r#"{"images":false}"#),
    ] {
        store
            .upsert_backend(&BackendRecord {
                id: id.into(),
                kind: "modelscope".into(),
                base_url: "https://api-inference.modelscope.cn/v1".into(),
                capabilities: capabilities.into(),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    store.sync_modelscope_accounts().await.unwrap();
    for (id, expected) in [
        ("preset", ""),
        ("pretty", ""),
        ("custom", r#"{"images":false}"#),
    ] {
        assert_eq!(
            store.get_backend(id).await.unwrap().unwrap().capabilities,
            expected
        );
    }
    let mut preset = store.get_backend("preset").await.unwrap().unwrap();
    preset.capabilities = r#"{"tools":false,"parallel_tools":false,"images":false}"#.into();
    store.upsert_backend(&preset).await.unwrap();
    store.sync_modelscope_accounts().await.unwrap();
    assert_eq!(
        store
            .get_backend("preset")
            .await
            .unwrap()
            .unwrap()
            .capabilities,
        preset.capabilities
    );
}
