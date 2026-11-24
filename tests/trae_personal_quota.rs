//! 个人版额度离线闭环：官方余额信封、十进制精度、续期与账号隔离。

use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, Request, StatusCode, Uri},
    response::Response,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use fakemodelproxy::{
    accounts::{
        Account, quota_refresh,
        remote::{QuotaConfidence, QuotaReader},
    },
    backend::{Backend, Registry, trae_personal::TraePersonalBackend},
    config::Config,
    state::AppState,
    store::{BackendRecord, Store},
    tasks::{TaskKind, TaskRegistry, TaskStatus, scheduler},
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, RwLock},
};
use tokio::sync::Mutex;
use tower::ServiceExt;

const USAGE: &str = "/trae/api/v2/pay/ide_user_ent_usage";
const EXCHANGE: &str = "/cloudide/api/v3/trae/oauth/ExchangeToken";
const CHECKIN: &str = "/trae/api/v2/ug/checkin_credits/status";
type Reply = (&'static str, u16, String);

struct Call {
    path: String,
    authorization: String,
    device: String,
}

struct Mock {
    replies: Mutex<VecDeque<Reply>>,
    calls: Mutex<Vec<Call>>,
    replace_after_response: Mutex<Option<(Store, Account)>>,
}

/// 合成 JWT 模拟个人身份，不读取本机客户端或真实令牌。
fn token(subject: &str, generation: usize) -> String {
    format!(
        "e30.{}.test{generation}",
        URL_SAFE_NO_PAD.encode(json!({"data":{"id":subject},"exp":4_000_000_000u64}).to_string())
    )
}

/// 官方余额响应故意包含不一致的积分包和累计用量，防止实现推算余额。
fn balance(amount: Value) -> Value {
    json!({
        "is_credits_billing":true,
        "total_balance":amount,
        "balance_summary":[{"source":1,"total_balance":amount}],
        "usage_summary":{"consumed_amount":5172.96,"total_amount":5517},
        "user_entitlement_pack_list":[{"credits_amount":55.9604},{"credits_amount":null}]
    })
}

/// 顺序指定响应状态和正文，同时支持坏 JSON。
fn reply(path: &'static str, status: u16, value: Value) -> Reply {
    (path, status, value.to_string())
}

/// 校验生产请求路径、积分作用域和稳定设备身份，禁止访问外部上游。
async fn upstream(
    State(mock): State<Arc<Mock>>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let body: Value = serde_json::from_slice(&body).unwrap();
    let authorization = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let device = headers
        .get("x-device-id")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if uri.path() == EXCHANGE {
        assert_eq!(body["ClientID"], "ono9krqynydwx5");
        assert_eq!(body["UserID"], "personal-user");
        assert_eq!(body["RefreshToken"], "fake-refresh");
    } else {
        assert!(authorization.starts_with("Cloud-IDE-JWT e30."));
        assert_eq!(device, "1234567890123456");
        assert_eq!(headers["x-device-brand"], "TestMac1,2");
        assert_eq!(headers["x-device-type"], "mac");
        assert_eq!(headers["x-os-version"], "macOS 14.0");
        assert_eq!(headers["x-app-version"], "3.4.20");
        assert_eq!(
            body,
            if uri.path() == USAGE {
                json!({"require_usage":true,"req_source":1})
            } else {
                json!({"req_source":1})
            }
        );
    }
    mock.calls.lock().await.push(Call {
        path: uri.path().into(),
        authorization: authorization.into(),
        device: device.into(),
    });
    let (expected, status, raw) = mock
        .replies
        .lock()
        .await
        .pop_front()
        .expect("unexpected upstream request");
    assert_eq!(uri.path(), expected);
    if let Some((store, account)) = mock.replace_after_response.lock().await.take() {
        store.upsert_account(&account).await.unwrap();
    }
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(raw.into())
        .unwrap()
}

struct Fixture {
    state: AppState,
    backend: BackendRecord,
    account: Account,
    mock: Arc<Mock>,
    server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    /// 本机假服务器和内存数据库复用生产服务注册与额度读取器。
    async fn new(replies: Vec<Reply>) -> Self {
        let mock = Arc::new(Mock {
            replies: Mutex::new(replies.into()),
            calls: Mutex::new(Vec::new()),
            replace_after_response: Mutex::new(None),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = BackendRecord {
            id: "trae-personal-test".into(),
            name: "TRAE 个人测试".into(),
            kind: "trae_personal".into(),
            base_url: format!("http://{}", listener.local_addr().unwrap()),
            options: json!({"client_device":{
                "device_id":"1234567890123456","device_brand":"TestMac1,2",
                "device_type":"mac","os_version":"macOS 14.0","app_version":"3.4.20"
            }})
            .to_string(),
            ..Default::default()
        };
        let app = Router::new().fallback(upstream).with_state(mock.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let store = Store::open(std::path::Path::new(":memory:")).await.unwrap();
        store.upsert_backend(&backend).await.unwrap();
        let account = Account {
            id: "personal-account".into(),
            backend_id: backend.id.clone(),
            name: "测试个人账号".into(),
            enabled: true,
            credential: json!({"Token":token("personal-user",0),"RefreshToken":"fake-refresh"})
                .to_string(),
        };
        store.upsert_account(&account).await.unwrap();
        let state = AppState::new(
            Arc::new(Config::default()),
            store.clone(),
            Arc::new(Registry::new()),
            Arc::new(fakemodelproxy::records::RecordLog::default()),
            Arc::new(RwLock::new(TaskRegistry::new())),
            Arc::new(fakemodelproxy::routing::Router::new()),
            Arc::new(fakemodelproxy::usage::UsageLedger::new(store)),
        );
        state.reload_backends().await.unwrap();
        Self {
            state,
            backend,
            account,
            mock,
            server,
        }
    }

    /// 生产后端分别用于路由余额契约和账号额度契约。
    fn reader(&self) -> TraePersonalBackend {
        TraePersonalBackend::new(
            &self.backend,
            vec![self.account.clone()],
            self.state.store.clone(),
        )
        .unwrap()
    }
}

impl Drop for Fixture {
    /// 用例结束立即释放本机监听。
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// 官方无码响应保留数字及字符串小数，零余额明确可用，且不推算总量。
#[tokio::test]
async fn official_balance_preserves_decimal_precision_and_verified_zero() {
    for (amount, expected) in [
        (json!(344.04), "344.04"),
        (json!("181.19999988"), "181.19999988"),
        (json!("0.000000000000000001"), "0.000000000000000001"),
        (json!(0), "0"),
    ] {
        let fixture = Fixture::new(vec![reply(USAGE, 200, balance(amount))]).await;
        let info = fixture.reader().read(&fixture.account).await.unwrap();
        assert_eq!(info.confidence, QuotaConfidence::Verified);
        assert_eq!(info.unit.as_deref(), Some("积分"));
        assert!(info.total.is_none());
        assert!(info.used.is_none());
        assert!(info.reset_at.is_none());
        let snapshot = info.routing.unwrap();
        assert_eq!(snapshot.remaining.as_deref(), Some(expected));
        assert!(snapshot.amount().is_some());
        assert!(!snapshot.unlimited);
        assert_eq!(
            snapshot.quota_scope_id,
            "trae-personal-test/personal-account"
        );
        assert_eq!(info.remaining, if expected == "0" { Some(0) } else { None });
    }
}

/// 坏金额、缺失字段和显式业务错误不得通过积分包或用量补零。
#[tokio::test]
async fn malformed_or_rejected_balances_remain_unknown_in_production_cache() {
    for value in [
        json!([]),
        json!({"is_credits_billing":true,"user_entitlement_pack_list":[{"credits_amount":100}]}),
        json!({"total_balance":344.04}),
        balance(Value::Null),
        balance(json!(-1)),
        balance(json!("NaN")),
        balance(json!("1.0000000000000000001")),
        json!({"code":9074,"is_credits_billing":true,"total_balance":344.04}),
        json!({"code":null,"is_credits_billing":true,"total_balance":344.04}),
    ] {
        let fixture = Fixture::new(vec![reply(USAGE, 200, value)]).await;
        let view =
            quota_refresh::refresh(&fixture.state, true, fakemodelproxy::records::now_seconds())
                .await
                .unwrap();
        assert_eq!(view.quota.len(), 1);
        let info = &view.quota[0];
        assert_eq!(info.confidence, QuotaConfidence::Unknown);
        assert!(info.remaining.is_none());
        assert!(info.routing.is_none());
        assert!(info.note.is_some());
        assert_eq!(fixture.mock.calls.lock().await.len(), 1);
    }
    let fixture = Fixture::new(vec![(USAGE, 200, "bad-json".into())]).await;
    assert!(fixture.reader().read(&fixture.account).await.is_err());
}

/// 非积分计费不能将美元余额标记为积分；显式成功业务码可以读取。
#[tokio::test]
async fn non_credit_billing_is_unknown_and_explicit_success_is_supported() {
    let fixture = Fixture::new(vec![
        reply(
            USAGE,
            200,
            json!({"is_credits_billing":false,"is_dollar_usage_billing":true,"total_balance":99}),
        ),
        reply(
            USAGE,
            200,
            json!({"code":"0","is_credits_billing":true,"total_balance":344.04}),
        ),
    ])
    .await;
    let unknown = fixture.reader().read(&fixture.account).await.unwrap();
    assert_eq!(unknown.confidence, QuotaConfidence::Unknown);
    assert!(unknown.unit.is_none());
    let info = fixture
        .reader()
        .routing_quota(&fixture.account.id)
        .await
        .unwrap();
    assert_eq!(info.confidence, QuotaConfidence::Verified);
    assert_eq!(info.routing.unwrap().remaining.as_deref(), Some("344.04"));
}

/// 明确 401 只续期一次，续期票据保存且额度和签到共用稳定设备。
#[tokio::test]
async fn unauthorized_balance_refreshes_once_and_keeps_checkin_device() {
    let fixture = Fixture::new(vec![
        reply(USAGE, 401, json!({})),
        reply(
            EXCHANGE,
            200,
            json!({"Result":{"Token":token("personal-user",1),"RefreshToken":"next-refresh"}}),
        ),
        reply(USAGE, 200, balance(json!(344.04))),
        reply(
            CHECKIN,
            200,
            json!({"code":0,"enable":true,"checked_in":true}),
        ),
    ])
    .await;
    let info = quota_refresh::refresh_account(
        &fixture.state.store,
        &fixture.state.quota_refresh,
        &fixture.backend,
        &fixture.account,
    )
    .await
    .unwrap();
    assert_eq!(info.confidence, QuotaConfidence::Verified);
    let cached = fixture.state.store.cached_quota().await.unwrap();
    assert_eq!(cached.len(), 1);
    assert_eq!(
        cached[0].routing.as_ref().unwrap().remaining.as_deref(),
        Some("344.04")
    );
    let stored = fixture
        .state
        .store
        .get_account(&fixture.account.id)
        .await
        .unwrap()
        .unwrap();
    assert!(stored.credential.contains("next-refresh"));
    let provider = fixture
        .state
        .task_providers()
        .into_iter()
        .find(|provider| {
            provider.kind() == TaskKind::Checkin && provider.backend_id() == fixture.backend.id
        })
        .unwrap();
    let outcome = provider
        .run_for_account(&fixture.account.id, &scheduler::day_at(scheduler::now()))
        .await
        .unwrap();
    assert_eq!(outcome.status, TaskStatus::AlreadyDone);
    let calls = fixture.mock.calls.lock().await;
    assert_eq!(calls.iter().filter(|call| call.path == EXCHANGE).count(), 1);
    assert_ne!(calls[0].authorization, calls[2].authorization);
    assert_eq!(calls[0].device, calls[2].device);
    assert_eq!(calls[2].device, calls[3].device);
}

/// 第二次 401、限流、禁止访问和重定向均停止，不重放额度请求。
#[tokio::test]
async fn rejected_requests_do_not_loop_or_follow_redirects() {
    for http in [403, 429, 500, 302] {
        let fixture = Fixture::new(vec![reply(USAGE, http, json!({}))]).await;
        assert!(fixture.reader().read(&fixture.account).await.is_err());
        assert_eq!(fixture.mock.calls.lock().await.len(), 1);
    }
    let fixture = Fixture::new(vec![
        reply(USAGE, 401, json!({})),
        reply(
            EXCHANGE,
            200,
            json!({"Result":{"Token":token("personal-user",1),"RefreshToken":"next-refresh"}}),
        ),
        reply(USAGE, 401, json!({})),
    ])
    .await;
    assert!(fixture.reader().read(&fixture.account).await.is_err());
    assert_eq!(fixture.mock.calls.lock().await.len(), 3);
}

/// 账号、服务停用或换身份时，不读取或发布原账号的余额。
#[tokio::test]
async fn quota_validates_account_ownership_and_rejects_identity_changes() {
    let fixture = Fixture::new(vec![reply(USAGE, 200, balance(json!(344.04)))]).await;
    let reader = fixture.reader();
    let mut account = fixture.account.clone();
    account.backend_id = "different-backend".into();
    fixture.state.store.upsert_account(&account).await.unwrap();
    assert!(reader.read(&fixture.account).await.is_err());
    account.backend_id = fixture.backend.id.clone();
    account.enabled = false;
    fixture.state.store.upsert_account(&account).await.unwrap();
    assert!(reader.read(&fixture.account).await.is_err());
    account.enabled = true;
    fixture.state.store.upsert_account(&account).await.unwrap();
    let mut backend = fixture.backend.clone();
    backend.enabled = false;
    fixture.state.store.upsert_backend(&backend).await.unwrap();
    assert!(reader.read(&fixture.account).await.is_err());
    assert!(fixture.mock.calls.lock().await.is_empty());
    backend.enabled = true;
    fixture.state.store.upsert_backend(&backend).await.unwrap();
    account.credential = json!({"Token":token("different-user",0)}).to_string();
    *fixture.mock.replace_after_response.lock().await =
        Some((fixture.state.store.clone(), account));
    assert!(
        quota_refresh::refresh_account(
            &fixture.state.store,
            &fixture.state.quota_refresh,
            &fixture.backend,
            &fixture.account,
        )
        .await
        .is_err()
    );
    assert!(fixture.state.store.cached_quota().await.unwrap().is_empty());
}

/// 多账号余额和身份分别存储，同服务显式共享同一客户端设备。
#[tokio::test]
async fn account_balances_and_device_identities_are_isolated() {
    let fixture = Fixture::new(vec![
        reply(USAGE, 200, balance(json!(344.04))),
        reply(USAGE, 200, balance(json!(12.01))),
    ])
    .await;
    let mut second = fixture.account.clone();
    second.id = "second-account".into();
    second.credential = json!({"Token":token("second-user",0)}).to_string();
    fixture.state.store.upsert_account(&second).await.unwrap();
    for account in [&fixture.account, &second] {
        let info = quota_refresh::refresh_account(
            &fixture.state.store,
            &fixture.state.quota_refresh,
            &fixture.backend,
            account,
        )
        .await
        .unwrap();
        assert_eq!(info.account_id, account.id);
    }
    let cached = fixture.state.store.cached_quota().await.unwrap();
    assert_eq!(cached.len(), 2);
    for (id, expected) in [("personal-account", "344.04"), ("second-account", "12.01")] {
        let info = cached.iter().find(|info| info.account_id == id).unwrap();
        assert_eq!(
            info.routing.as_ref().unwrap().remaining.as_deref(),
            Some(expected)
        );
    }
    let calls = fixture.mock.calls.lock().await;
    assert_ne!(calls[0].authorization, calls[1].authorization);
    assert_eq!(calls[0].device, calls[1].device);
}

/// 退役的两种客户端导入请求均为 404，账号记录和凭证不发生写入。
#[tokio::test]
async fn local_trae_and_solo_import_route_is_removed() {
    let fixture = Fixture::new(Vec::new()).await;
    let before = fixture.state.store.list_accounts().await.unwrap();
    for edition in ["trae_cn", "solo_cn"] {
        let response = fakemodelproxy::build_router(fixture.state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/oauth/trae_personal/import")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"backend_id":fixture.backend.id,"edition":edition}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    let after = fixture.state.store.list_accounts().await.unwrap();
    assert_eq!(before.len(), after.len());
    assert_eq!(before[0].credential, after[0].credential);
    assert!(fixture.mock.calls.lock().await.is_empty());
}
