//! 个人版签到离线闭环：真实注册、续期、账号隔离、每日幂等与未知结果持久化。

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, Uri},
    response::Response,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use fakemodelproxy::{
    accounts::Account,
    backend::{Registry, trae_personal::credential::Credential},
    config::Config,
    state::AppState,
    store::{BackendRecord, Store},
    tasks::{TaskOutcome, TaskProvider, TaskRegistry, TaskStatus, run_and_record, scheduler},
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, RwLock},
};
use tokio::sync::Mutex;

const STATUS: &str = "/trae/api/v2/ug/checkin_credits/status";
const CLAIM: &str = "/trae/api/v2/ug/checkin_credits/claim";
const EXCHANGE: &str = "/cloudide/api/v3/trae/oauth/ExchangeToken";
type Reply = (&'static str, u16, String);

struct Call {
    path: String,
    authorization: String,
    device: String,
}

struct Mock {
    device: Option<Value>,
    replies: Mutex<VecDeque<Reply>>,
    calls: Mutex<Vec<Call>>,
    replace_after_status: Mutex<Option<(Store, Account)>>,
    replace_backend_after_status: Mutex<Option<(Store, BackendRecord)>>,
}

/// 合成设备只供假上游断言，不把本机用户硬件或注册 ID 写成产品默认值。
fn client_device() -> Value {
    json!({
        "device_id":"1234567890123456",
        "device_brand":"TestMac1,2",
        "device_type":"mac",
        "os_version":"macOS 14.0",
        "app_version":"3.4.20"
    })
}

/// 合成 JWT 只模拟个人身份和期限，不包含真实签名或账号信息。
fn token(subject: &str, generation: usize, expires: u64) -> String {
    format!(
        "e30.{}.test{generation}",
        URL_SAFE_NO_PAD.encode(json!({"data":{"id":subject},"exp":expires}).to_string())
    )
}

/// 顺序指定 HTTP 与业务信封，保留坏 JSON 场景。
fn reply(path: &'static str, status: u16, value: Value) -> Reply {
    (path, status, value.to_string())
}

/// 查询积分面额故意大于领取奖励，防止实现混淆两者。
fn status(checked_in: bool) -> Reply {
    reply(
        STATUS,
        200,
        json!({"code":0,"enable":true,"checked_in":checked_in,"credits":999}),
    )
}

/// 逐条校验实际路径与个人身份头，所有请求只访问本机假上游。
async fn upstream(
    State(mock): State<Arc<Mock>>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let body: Value = serde_json::from_slice(&body).unwrap();
    let authorization = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let device = headers
        .get("x-device-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if uri.path() == EXCHANGE {
        assert_eq!(body["ClientID"], "ono9krqynydwx5");
        assert_eq!(body["UserID"], "personal-user");
        assert_eq!(body["RefreshToken"], "fake-refresh");
    } else {
        assert!(authorization.starts_with("Cloud-IDE-JWT e30."));
        assert_eq!(body, json!({"req_source":1}));
        if let Some(profile) = &mock.device {
            for (header, field) in [
                ("x-device-id", "device_id"),
                ("x-device-brand", "device_brand"),
                ("x-device-type", "device_type"),
                ("x-os-version", "os_version"),
                ("x-app-version", "app_version"),
            ] {
                assert_eq!(
                    headers[header].to_str().unwrap(),
                    profile[field].as_str().unwrap()
                );
            }
        } else {
            assert_eq!(device, "0");
            for header in [
                "x-device-brand",
                "x-device-type",
                "x-os-version",
                "x-app-version",
            ] {
                assert!(!headers.contains_key(header));
            }
        }
    }
    mock.calls.lock().await.push(Call {
        path: uri.path().into(),
        authorization: authorization.into(),
        device: device.into(),
    });
    let (expected, http, raw) = mock
        .replies
        .lock()
        .await
        .pop_front()
        .expect("unexpected request");
    assert_eq!(uri.path(), expected);
    if uri.path() == STATUS
        && let Some((store, account)) = mock.replace_after_status.lock().await.take()
    {
        store.upsert_account(&account).await.unwrap();
    }
    if uri.path() == STATUS
        && let Some((store, backend)) = mock.replace_backend_after_status.lock().await.take()
    {
        store.upsert_backend(&backend).await.unwrap();
    }
    Response::builder()
        .status(http)
        .header("content-type", "application/json")
        .body(raw.into())
        .unwrap()
}

/// 复用生产 AppState、后端注册与任务 Store。
fn state(store: Store) -> AppState {
    AppState::new(
        Arc::new(Config::default()),
        store.clone(),
        Arc::new(Registry::new()),
        Arc::new(fakemodelproxy::records::RecordLog::default()),
        Arc::new(RwLock::new(TaskRegistry::new())),
        Arc::new(fakemodelproxy::routing::Router::new()),
        Arc::new(fakemodelproxy::usage::UsageLedger::new(store)),
    )
}

struct Fixture {
    state: AppState,
    backend: BackendRecord,
    account: Account,
    mock: Arc<Mock>,
    server: tokio::task::JoinHandle<()>,
    directory: tempfile::TempDir,
}

impl Fixture {
    /// 临时数据库放系统临时目录，测试不会读取本机真实个人凭证。
    async fn new(replies: Vec<Reply>) -> Self {
        Self::with_device(replies, Some(client_device())).await
    }

    /// 指定服务的客户端配置，兼容用例可明确省略注册设备。
    async fn with_device(replies: Vec<Reply>, device: Option<Value>) -> Self {
        let mock = Arc::new(Mock {
            device: device.clone(),
            replies: Mutex::new(replies.into()),
            calls: Mutex::new(Vec::new()),
            replace_after_status: Mutex::new(None),
            replace_backend_after_status: Mutex::new(None),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = BackendRecord {
            id: "trae-personal-test".into(),
            name: "TRAE 个人测试".into(),
            kind: "trae_personal".into(),
            base_url: format!("http://{}", listener.local_addr().unwrap()),
            options: json!({"client_device":device}).to_string(),
            ..Default::default()
        };
        let app = Router::new().fallback(upstream).with_state(mock.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("test.db"))
            .await
            .unwrap();
        store.upsert_backend(&backend).await.unwrap();
        let account = Account {
            id: "personal-account".into(),
            backend_id: backend.id.clone(),
            name: "测试个人账号".into(),
            enabled: true,
            credential: Credential::parse(
                &json!({
                    "Token":token("personal-user",0,4_000_000_000),"RefreshToken":"fake-refresh"
                })
                .to_string(),
            )
            .unwrap()
            .serialize()
            .unwrap(),
        };
        store.upsert_account(&account).await.unwrap();
        let state = state(store);
        state.reload_backends().await.unwrap();
        Self {
            state,
            backend,
            account,
            mock,
            server,
            directory,
        }
    }

    /// 注册后的个人服务只提供一个账号级每日任务。
    fn provider(&self) -> Arc<dyn TaskProvider> {
        let providers = self.state.task_providers();
        assert_eq!(providers.len(), 1);
        providers[0].clone()
    }

    /// 手动、批量与自动调度经过同一生产幂等入口。
    async fn run(&self) -> TaskOutcome {
        run_and_record(
            &self.state.store,
            self.provider().as_ref(),
            &self.account.id,
            &scheduler::day_at(scheduler::now()),
        )
        .await
        .unwrap()
    }

    /// 重新打开数据库和重新注册，模拟重启后的任务与凭证读取。
    async fn reopen(&self) -> AppState {
        let store = Store::open(&self.directory.path().join("test.db"))
            .await
            .unwrap();
        let state = state(store);
        state.reload_backends().await.unwrap();
        state
    }
}

impl Drop for Fixture {
    /// 用例结束释放假上游。
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// 成功领取只记录实发数量，重复执行与重新注册都不再次提交。
#[tokio::test]
async fn successful_claim_is_idempotent_across_restart() {
    let fixture = Fixture::new(vec![
        status(false),
        reply(CLAIM, 200, json!({"code":0,"credits":100})),
    ])
    .await;
    let first = fixture.run().await;
    assert_eq!(first.status, TaskStatus::Succeeded);
    assert_eq!(first.amount, Some(100));
    let restarted = fixture.reopen().await;
    let again = run_and_record(
        &restarted.store,
        restarted.task_providers()[0].as_ref(),
        &fixture.account.id,
        &scheduler::day_at(scheduler::now()),
    )
    .await
    .unwrap();
    assert_eq!(again.status, TaskStatus::AlreadyDone);
    assert_eq!(again.amount, Some(100));
    assert_eq!(fixture.mock.calls.lock().await.len(), 2);
    assert!(fixture.mock.replies.lock().await.is_empty());
}

/// 已签到或活动关闭都不领取，查询面额也不计为新奖励。
#[tokio::test]
async fn already_done_and_disabled_do_not_claim() {
    for (response, expected) in [
        (status(true), TaskStatus::AlreadyDone),
        (
            reply(STATUS, 200, json!({"code":0,"enable":false})),
            TaskStatus::Skipped,
        ),
    ] {
        let fixture = Fixture::new(vec![response]).await;
        let result = fixture.run().await;
        assert_eq!(result.status, expected);
        assert_eq!(result.amount, None);
        assert_eq!(fixture.mock.calls.lock().await.len(), 1);
    }
}

/// 领取成功但没给数量时保持未知，不能套用状态查询中的 999 积分。
#[tokio::test]
async fn successful_claim_without_amount_keeps_amount_unknown() {
    let fixture = Fixture::new(vec![
        status(false),
        reply(CLAIM, 200, json!({"code":"0","message":"success"})),
    ])
    .await;
    let result = fixture.run().await;
    assert_eq!(result.status, TaskStatus::Succeeded);
    assert_eq!(result.amount, None);
}

/// 限流停止本次操作，不额外查询、重试领取或轮换设备。
#[tokio::test]
async fn rate_limit_stops_after_one_claim() {
    let fixture = Fixture::new(vec![status(false), reply(CLAIM, 200, json!({"code":9074}))]).await;
    let result = fixture.run().await;
    assert_eq!(result.status, TaskStatus::Failed);
    assert!(result.message.contains("9074"));
    assert!(result.message.contains("积分领取"));
    assert_eq!(fixture.mock.calls.lock().await.len(), 2);
}

/// 状态查询的 9074 必须明确标记查询阶段，不能让用户误认为已提交领取。
#[tokio::test]
async fn status_rate_limit_identifies_query_without_claim() {
    let fixture = Fixture::new(vec![reply(
        STATUS,
        200,
        json!({"code":9074,"enable":true,"checked_in":false}),
    )])
    .await;
    let result = fixture.run().await;
    assert_eq!(result.status, TaskStatus::Failed);
    assert!(result.message.contains("状态查询"));
    assert!(result.message.contains("9074"));
    let calls = fixture.mock.calls.lock().await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].path, STATUS);
}

/// 设备已领取必须核对本账号状态，不把设备限制当成个人签到成功。
#[tokio::test]
async fn device_already_claimed_requires_account_confirmation() {
    for (verification, expected) in [
        (status(false), TaskStatus::Failed),
        (status(true), TaskStatus::AlreadyDone),
        (reply(STATUS, 500, json!({})), TaskStatus::Unknown),
    ] {
        let fixture = Fixture::new(vec![
            status(false),
            reply(CLAIM, 200, json!({"code":9095})),
            verification,
        ])
        .await;
        let result = fixture.run().await;
        assert_eq!(result.status, expected);
        assert_eq!(result.amount, None);
        let calls = fixture.mock.calls.lock().await;
        assert_eq!(calls.iter().filter(|call| call.path == CLAIM).count(), 1);
        assert_eq!(calls.len(), 3);
        assert!(calls.iter().all(|call| call.device == calls[0].device));
    }
}

/// HTTP 500、坏 JSON 和缺业务码都属于领取结果未知，重开数据库后也禁止重放。
#[tokio::test]
async fn ambiguous_claims_are_fenced_across_instances() {
    for response in [
        reply(CLAIM, 500, json!({"code":0})),
        (CLAIM, 200, "bad-json".into()),
        reply(CLAIM, 200, json!({"message":"success"})),
        reply(CLAIM, 408, json!({})),
    ] {
        let fixture = Fixture::new(vec![status(false), response]).await;
        assert_eq!(fixture.run().await.status, TaskStatus::Unknown);
        let restarted = fixture.reopen().await;
        let again = run_and_record(
            &restarted.store,
            restarted.task_providers()[0].as_ref(),
            &fixture.account.id,
            &scheduler::day_at(scheduler::now()),
        )
        .await
        .unwrap();
        assert_eq!(again.status, TaskStatus::Unknown);
        assert_eq!(fixture.mock.calls.lock().await.len(), 2);
    }
}

/// 状态业务失败、关键字段损坏和非 JSON 都不能被误解为尚未签到。
#[tokio::test]
async fn invalid_status_never_triggers_claim() {
    for response in [
        reply(
            STATUS,
            200,
            json!({"code":9074,"enable":true,"checked_in":false}),
        ),
        reply(STATUS, 200, json!({"enable":true,"checked_in":false})),
        reply(STATUS, 200, json!({"code":0,"enable":true})),
        reply(
            STATUS,
            200,
            json!({"code":0,"enable":true,"checked_in":"false"}),
        ),
        (STATUS, 200, "bad-json".into()),
    ] {
        let fixture = Fixture::new(vec![response]).await;
        assert_eq!(fixture.run().await.status, TaskStatus::Failed);
        assert_eq!(fixture.mock.calls.lock().await.len(), 1);
    }
}

/// 状态或领取的明确 401 可续期一次，之后的请求使用新票据和原设备 ID。
#[tokio::test]
async fn explicit_401_refreshes_once_and_preserves_device_identity() {
    for rejected_path in [STATUS, CLAIM] {
        let exchange = reply(
            EXCHANGE,
            200,
            json!({"Result":{
                "Token":token("personal-user",1,4_000_000_000),"RefreshToken":"next-refresh"
            }}),
        );
        let replies = if rejected_path == STATUS {
            vec![
                reply(STATUS, 401, json!({})),
                exchange,
                status(false),
                reply(CLAIM, 200, json!({"code":0})),
            ]
        } else {
            vec![
                status(false),
                reply(CLAIM, 401, json!({})),
                exchange,
                reply(CLAIM, 200, json!({"code":0})),
            ]
        };
        let fixture = Fixture::new(replies).await;
        assert_eq!(fixture.run().await.status, TaskStatus::Succeeded);
        let saved = fixture
            .state
            .store
            .get_account(&fixture.account.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            Credential::parse(&saved.credential).unwrap().refresh_token,
            "next-refresh"
        );
        assert_eq!(
            fixture
                .state
                .store
                .get_backend(&fixture.backend.id)
                .await
                .unwrap()
                .unwrap()
                .options,
            fixture.backend.options
        );
        let calls = fixture.mock.calls.lock().await;
        assert_eq!(calls.iter().filter(|call| call.path == EXCHANGE).count(), 1);
        let personal: Vec<_> = calls.iter().filter(|call| call.path != EXCHANGE).collect();
        assert!(
            personal
                .iter()
                .all(|call| call.device == personal[0].device)
        );
        assert_ne!(
            personal.first().unwrap().authorization,
            personal.last().unwrap().authorization
        );
    }
}

/// 二次 401 停止操作，过期票据在状态查询前主动续期。
#[tokio::test]
async fn second_401_stops_and_expired_credentials_refresh_before_status() {
    let exchange = reply(
        EXCHANGE,
        200,
        json!({"Result":{
            "Token":token("personal-user",1,4_000_000_000),"RefreshToken":"next-refresh"
        }}),
    );
    let fixture = Fixture::new(vec![
        reply(STATUS, 401, json!({})),
        exchange.clone(),
        reply(STATUS, 401, json!({})),
    ])
    .await;
    assert_eq!(fixture.run().await.status, TaskStatus::Failed);
    assert_eq!(fixture.mock.calls.lock().await.len(), 3);
    let fixture = Fixture::new(vec![exchange, status(true)]).await;
    let mut expired = fixture.account.clone();
    expired.credential = Credential::parse(
        &json!({
            "Token":token("personal-user",0,1),"RefreshToken":"fake-refresh"
        })
        .to_string(),
    )
    .unwrap()
    .serialize()
    .unwrap();
    fixture.state.store.upsert_account(&expired).await.unwrap();
    assert_eq!(fixture.run().await.status, TaskStatus::AlreadyDone);
    assert_eq!(fixture.mock.calls.lock().await[0].path, EXCHANGE);
}

/// 不存在、其他服务、停用账号和旧日期都不能借用当前个人账号领取。
#[tokio::test]
async fn account_ownership_and_day_are_checked_before_network() {
    let fixture = Fixture::new(vec![]).await;
    let provider = fixture.provider();
    let day = scheduler::day_at(scheduler::now());
    assert!(provider.run_for_account("missing", &day).await.is_err());
    assert!(
        provider
            .run_for_account(
                &fixture.account.id,
                &scheduler::day_at(scheduler::now() - 86_400)
            )
            .await
            .is_err()
    );
    for (backend_id, enabled) in [
        ("other-service", true),
        (fixture.backend.id.as_str(), false),
    ] {
        let mut invalid = fixture.account.clone();
        invalid.backend_id = backend_id.into();
        invalid.enabled = enabled;
        fixture.state.store.upsert_account(&invalid).await.unwrap();
        assert!(provider.run_for_account(&invalid.id, &day).await.is_err());
    }
    fixture
        .state
        .store
        .upsert_account(&fixture.account)
        .await
        .unwrap();
    let mut disabled_backend = fixture.backend.clone();
    disabled_backend.enabled = false;
    fixture
        .state
        .store
        .upsert_backend(&disabled_backend)
        .await
        .unwrap();
    assert!(
        provider
            .run_for_account(&fixture.account.id, &day)
            .await
            .is_err()
    );
    assert!(fixture.mock.calls.lock().await.is_empty());
}

/// 查询途中账号被替换时必须停止，不把旧账号状态与新账号领取拼在一起。
#[tokio::test]
async fn replacing_account_during_status_prevents_claim() {
    let fixture = Fixture::new(vec![status(false)]).await;
    let mut replacement = fixture.account.clone();
    replacement.credential = Credential::parse(
        &json!({
            "Token":token("another-person",0,4_000_000_000),"RefreshToken":"other-refresh"
        })
        .to_string(),
    )
    .unwrap()
    .serialize()
    .unwrap();
    *fixture.mock.replace_after_status.lock().await =
        Some((fixture.state.store.clone(), replacement));
    assert_eq!(fixture.run().await.status, TaskStatus::Failed);
    assert_eq!(fixture.mock.calls.lock().await.len(), 1);
}

/// 统一调度自动发现个人账号，关闭总开关仍提供计划预览且不发网络请求。
#[tokio::test]
async fn scheduler_includes_personal_account_without_enabling_global_setting() {
    let fixture = Fixture::new(vec![]).await;
    let view = scheduler::view(&fixture.state).await.unwrap();
    assert!(!view.settings.enabled);
    assert_eq!(view.timezone, "Asia/Shanghai");
    assert!(
        view.next_runs
            .iter()
            .any(|plan| plan.backend_id == fixture.backend.id
                && plan.account_id == fixture.account.id)
    );
    assert!(fixture.mock.calls.lock().await.is_empty());
}

/// 同服务多个账号共享显式物理设备，账号凭证和每日记录保持隔离。
#[tokio::test]
async fn multiple_accounts_have_separate_identity_and_daily_records() {
    let fixture = Fixture::new(vec![status(true), status(true), status(true)]).await;
    let mut second = fixture.account.clone();
    second.id = "second-account".into();
    second.credential = Credential::parse(
        &json!({"Token":token("second-user",0,4_000_000_000),"RefreshToken":"second-refresh"})
            .to_string(),
    )
    .unwrap()
    .serialize()
    .unwrap();
    fixture.state.store.upsert_account(&second).await.unwrap();
    assert_eq!(fixture.run().await.status, TaskStatus::AlreadyDone);
    let day = scheduler::day_at(scheduler::now());
    let outcome = run_and_record(
        &fixture.state.store,
        fixture.provider().as_ref(),
        &second.id,
        &day,
    )
    .await
    .unwrap();
    assert_eq!(outcome.status, TaskStatus::AlreadyDone);
    let restarted = fixture.reopen().await;
    restarted.task_providers()[0]
        .run_for_account(&fixture.account.id, &day)
        .await
        .unwrap();
    let calls = fixture.mock.calls.lock().await;
    assert_ne!(calls[0].authorization, calls[1].authorization);
    assert_eq!(calls[0].device, calls[1].device);
    assert_eq!(calls[0].device, calls[2].device);
    assert!(
        fixture
            .state
            .store
            .get_task(&fixture.backend.id, &second.id, "checkin", &day)
            .await
            .unwrap()
            .is_some()
    );
}

/// 旧配置仍能查询状态，但不会把账号哈希或固定样例当作官方注册设备。
#[tokio::test]
async fn missing_device_uses_unregistered_fallback_without_fabricated_metadata() {
    let fixture = Fixture::with_device(vec![status(true)], None).await;
    assert_eq!(fixture.run().await.status, TaskStatus::AlreadyDone);
    assert_eq!(fixture.mock.calls.lock().await.len(), 1);
}

/// 不同服务可绑定不同设备，不能隐式使用另一服务或本机客户端的 ID。
#[tokio::test]
async fn separate_services_use_their_configured_devices() {
    let first = Fixture::new(vec![status(true)]).await;
    let mut other = client_device();
    other["device_id"] = json!("9876543210987654");
    other["device_brand"] = json!("TestMac9,9");
    other["os_version"] = json!("macOS 15.0");
    other["app_version"] = json!("4.0.0");
    let second = Fixture::with_device(vec![status(true)], Some(other)).await;
    assert_eq!(first.run().await.status, TaskStatus::AlreadyDone);
    assert_eq!(second.run().await.status, TaskStatus::AlreadyDone);
    assert_ne!(
        first.mock.calls.lock().await[0].device,
        second.mock.calls.lock().await[0].device
    );
}

/// 查询途中修改设备使旧实例失效，禁止用旧查询继续提交领取。
#[tokio::test]
async fn changing_device_during_status_prevents_claim() {
    let fixture = Fixture::new(vec![status(false)]).await;
    let mut changed = fixture.backend.clone();
    let mut device = client_device();
    device["device_id"] = json!("9876543210987654");
    changed.options = json!({"client_device":device}).to_string();
    *fixture.mock.replace_backend_after_status.lock().await =
        Some((fixture.state.store.clone(), changed));
    assert_eq!(fixture.run().await.status, TaskStatus::Failed);
    assert_eq!(fixture.mock.calls.lock().await.len(), 1);
}
