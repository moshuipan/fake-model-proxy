//! 离线验证每日登录保分：真实 HTTP、账号归属、到期凭据和每日幂等记录。

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::IntoResponse,
};
use fakemodelproxy::{
    accounts::Account,
    backend::{Registry, raccoon::credential::Credential},
    config::Config,
    state::AppState,
    store::{BackendRecord, Store},
    tasks::{TaskKind, TaskOutcome, TaskRegistry, TaskStatus, run_and_record, scheduler},
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, RwLock},
};
use time::{Date, UtcOffset, format_description::well_known::Rfc3339};
use tokio::sync::Mutex;

struct Mock {
    responses: Mutex<VecDeque<(&'static str, u16, Value)>>,
    requests: Mutex<Vec<String>>,
}

/// 逐条校验实际请求，所有凭证均为测试构造，永不访问厂商。
async fn upstream(
    State(mock): State<Arc<Mock>>,
    uri: Uri,
    headers: HeaderMap,
) -> impl IntoResponse {
    assert_eq!(headers["authorization"], "Bearer fake-access");
    assert!(!headers.contains_key("x-client-platform"));
    let mut responses = mock.responses.lock().await;
    let (path, status, value) = responses.pop_front().expect("unexpected request");
    assert_eq!(uri.path(), path);
    mock.requests.lock().await.push(uri.to_string());
    (StatusCode::from_u16(status).unwrap(), Json(value))
}

struct Fixture {
    state: AppState,
    account: Account,
    mock: Arc<Mock>,
    upstream: tokio::task::JoinHandle<()>,
    _directory: tempfile::TempDir,
}

impl Fixture {
    /// 使用生产注册表与 SQLite 任务记录，仅上游响应由场景指定。
    async fn new(responses: Vec<(&'static str, u16, Value)>) -> Self {
        let mock = Arc::new(Mock {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app = Router::new().fallback(upstream).with_state(mock.clone());
        let backend = BackendRecord {
            id: "raccoon-test".into(),
            name: "小浣熊测试".into(),
            kind: "raccoon".into(),
            base_url: format!("http://{}", listener.local_addr().unwrap()),
            ..Default::default()
        };
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("test.db"))
            .await
            .unwrap();
        store.upsert_backend(&backend).await.unwrap();
        let now = fakemodelproxy::records::now_seconds();
        let account = Account {
            id: "test-account".into(),
            backend_id: backend.id.clone(),
            name: "测试账号".into(),
            enabled: true,
            credential: Credential {
                access_token: "fake-access".into(),
                refresh_token: "fake-refresh".into(),
                obtained_at: now,
                expires_at: now + 3600,
            }
            .serialize()
            .unwrap(),
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
            account,
            mock,
            upstream: handle,
            _directory: directory,
        }
    }

    /// 手动和自动调度使用同一个账号级入口与周期去重。
    async fn run(&self, account_id: &str, period: &str) -> TaskOutcome {
        let provider = self
            .state
            .task_providers()
            .into_iter()
            .find(|provider| {
                provider.kind() == TaskKind::Checkin
                    && provider.backend_id() == self.account.backend_id
            })
            .expect("raccoon daily task must be registered");
        run_and_record(&self.state.store, provider.as_ref(), account_id, period)
            .await
            .unwrap()
    }
}

impl Drop for Fixture {
    /// 场景结束即释放假上游，阻止请求逃逸到其他测试。
    fn drop(&mut self) {
        self.upstream.abort();
    }
}

/// 构造真实格式的北京时间 daily_grant；有效期包含发放日。
fn grant(period: &str, days: i64) -> Value {
    let start = Date::parse(
        period,
        &time::format_description::parse_borrowed::<2>("[year]-[month]-[day]").unwrap(),
    )
    .unwrap()
    .midnight()
    .assume_offset(UtcOffset::from_hms(8, 0, 0).unwrap());
    json!({
        "biz_type":"daily_grant", "points":300,
        "grant_at":start.format(&Rfc3339).unwrap(),
        "expire_at":(start + time::Duration::days(days) - time::Duration::seconds(1))
            .format(&Rfc3339).unwrap()
    })
}

/// 账单结构沿用官方 items、has_more 与 next_cursor。
fn bills(items: Vec<Value>) -> (&'static str, u16, Value) {
    (
        "/api/web/points/v2/bills",
        200,
        json!({"code":0,"data":{"items":items,"has_more":false,"next_cursor":""}}),
    )
}

/// 官网初始化的设置响应不含弹窗，成功必须由之后的真实账单确认。
fn login() -> Vec<(&'static str, u16, Value)> {
    vec![
        (
            "/api/web/auth/v1/user_info",
            200,
            json!({"code":0,"data":{"id":"test-user"}}),
        ),
        (
            "/api/web/office/v3/setting_info",
            200,
            json!({"code":0,"data":{"available_points":9999,"point_grant_popups":[]}}),
        ),
    ]
}

/// 已延期时不发起登录请求，重复执行也不产生第二次上游查询或虚构奖励。
#[tokio::test]
async fn already_retained_today_is_recorded_once_without_new_reward() {
    let day = scheduler::day_at(scheduler::now());
    let fixture = Fixture::new(vec![bills(vec![grant(&day, 7)])]).await;
    let first = fixture.run(&fixture.account.id, &day).await;
    assert_eq!(first.status, TaskStatus::AlreadyDone);
    assert!(first.message.contains("23:59:59"));
    assert_eq!(first.amount, None);
    let second = fixture.run(&fixture.account.id, &day).await;
    assert_eq!(second.status, TaskStatus::AlreadyDone);
    assert_eq!(fixture.mock.requests.lock().await.len(), 1);
    assert!(fixture.mock.responses.lock().await.is_empty());
}

/// 首次缺批次与当天失效两种状态，都需初始化后读取七天到期凭据。
#[tokio::test]
async fn login_is_successful_only_after_today_expiry_is_verified() {
    let day = scheduler::day_at(scheduler::now());
    for initial in [vec![], vec![grant(&day, 1)]] {
        let mut responses = vec![bills(initial)];
        responses.extend(login());
        responses.push(bills(vec![grant(&day, 7)]));
        let fixture = Fixture::new(responses).await;
        let result = fixture.run(&fixture.account.id, &day).await;
        assert_eq!(result.status, TaskStatus::Succeeded);
        assert_eq!(result.amount, None);
        let saved = fixture
            .state
            .store
            .get_task(
                &fixture.account.backend_id,
                &fixture.account.id,
                "checkin",
                &day,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.status, "succeeded");
        assert_eq!(saved.amount, None);
        assert_eq!(fixture.mock.requests.lock().await.len(), 4);
        assert!(fixture.mock.responses.lock().await.is_empty());
    }
}

/// 余额增加、旧批次、损坏字段与核对失败均不能冒充成功，未知结果不重放。
#[tokio::test]
async fn unverified_expiry_stays_unknown_and_is_not_replayed() {
    let day = scheduler::day_at(scheduler::now());
    let yesterday = scheduler::day_at(scheduler::now() - 86_400);
    let mut invalid = grant(&day, 7);
    invalid["expire_at"] = json!("not-a-date");
    for verification in [
        bills(vec![grant(&day, 1)]),
        bills(vec![grant(&yesterday, 7)]),
        bills(vec![invalid]),
        ("/api/web/points/v2/bills", 429, json!({"code":200103})),
    ] {
        let mut responses = vec![bills(vec![])];
        responses.extend(login());
        responses.push(verification);
        let fixture = Fixture::new(responses).await;
        let result = fixture.run(&fixture.account.id, &day).await;
        assert_eq!(result.status, TaskStatus::Unknown);
        assert_eq!(result.amount, None);
        assert_eq!(
            fixture.run(&fixture.account.id, &day).await.status,
            TaskStatus::Unknown
        );
        assert_eq!(fixture.mock.requests.lock().await.len(), 4);
    }
}

/// 翻页找到当天批次，不能把前一页的旧奖励当成今天已完成。
#[tokio::test]
async fn pagination_and_account_validation_use_current_account_and_day() {
    let day = scheduler::day_at(scheduler::now());
    let yesterday = scheduler::day_at(scheduler::now() - 86_400);
    let fixture = Fixture::new(vec![
        (
            "/api/web/points/v2/bills",
            200,
            json!({"code":0,"data":{
                "items":[grant(&yesterday,7)],"has_more":true,"next_cursor":"next page&safe"
            }}),
        ),
        bills(vec![grant(&day, 7)]),
    ])
    .await;
    assert_eq!(
        fixture.run(&fixture.account.id, &day).await.status,
        TaskStatus::AlreadyDone
    );
    assert!(fixture.mock.requests.lock().await[1].contains("cursor=next+page%26safe"));
    assert_eq!(
        fixture.run("different-account", &day).await.status,
        TaskStatus::Failed
    );
    assert_eq!(
        fixture.run(&fixture.account.id, &yesterday).await.status,
        TaskStatus::Failed
    );
    let mut disabled = fixture.account.clone();
    disabled.enabled = false;
    fixture.state.store.upsert_account(&disabled).await.unwrap();
    // 直接调用执行器，避免本地已完成记录遮蔽当前账号校验。
    let provider = fixture
        .state
        .task_providers()
        .into_iter()
        .find(|provider| provider.backend_id() == fixture.account.backend_id)
        .unwrap();
    assert!(
        provider
            .run_for_account(&fixture.account.id, &day)
            .await
            .is_err()
    );
    assert_eq!(fixture.mock.requests.lock().await.len(), 2);
}

/// 官网兼容的嵌套分页与 cursor 别名均能推进；游标仍需经过 URL 编码。
#[tokio::test]
async fn official_nested_pagination_shapes_preserve_cursor_progress() {
    let day = scheduler::day_at(scheduler::now());
    for paging in [
        json!({"has_more":true,"cursor":"next&safe"}),
        json!({"paging":{"has_more":true,"next_cursor":"next&safe"}}),
        json!({"next_cursor":null,"has_more":null,"paging":{"has_more":true,"cursor":"next&safe"}}),
    ] {
        let mut data = paging;
        data["items"] = json!([]);
        let fixture = Fixture::new(vec![
            (
                "/api/web/points/v2/bills",
                200,
                json!({"code":0,"data":data}),
            ),
            bills(vec![grant(&day, 7)]),
        ])
        .await;
        assert_eq!(
            fixture.run(&fixture.account.id, &day).await.status,
            TaskStatus::AlreadyDone
        );
        assert!(fixture.mock.requests.lock().await[1].contains("cursor=next%26safe"));
        assert!(fixture.mock.responses.lock().await.is_empty());
    }
}
