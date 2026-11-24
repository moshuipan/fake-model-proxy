//! AutoClaw 2.5.3 的离线契约场景：续期回落边界、完整令牌对和每日签到卡片。

use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    response::IntoResponse,
};
use fakemodelproxy::{
    accounts::Account,
    backend::autoglm::{
        AutoGlmBackend,
        client::Client,
        credential::{Credential, Region},
    },
    store::{BackendRecord, Store},
    tasks::{TaskProvider, TaskStatus, run_and_record, scheduler},
};
use serde_json::{Value, json};
use std::{collections::VecDeque, sync::Arc};
use tokio::sync::Mutex;

struct Mock {
    replies: Mutex<VecDeque<(&'static str, u16, Value)>>,
    calls: Mutex<Vec<(Method, String, Value)>>,
}

struct Server {
    mock: Arc<Mock>,
    record: BackendRecord,
    handle: tokio::task::JoinHandle<()>,
}

/// 严格核对请求顺序与接口；只使用无效凭证和动态回环端口。
async fn upstream(
    State(mock): State<Arc<Mock>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    assert_eq!(headers["x-version"], "2.5.3");
    let payload = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    let (path, status, value) = mock
        .replies
        .lock()
        .await
        .pop_front()
        .expect("unexpected call");
    assert_eq!(uri.path(), path);
    if path.contains("refresh") {
        assert!(!headers.contains_key("authorization"));
        assert_eq!(payload["device_id"], "test-device");
        assert_eq!(payload["source_id"], "autoclaw");
    } else {
        assert_eq!(headers["authorization"], "Bearer fake-access");
    }
    mock.calls.lock().await.push((method, path.into(), payload));
    (StatusCode::from_u16(status).unwrap(), Json(value))
}

impl Server {
    /// 同时覆盖账号域和模型域，禁止测试访问外网。
    async fn new(replies: Vec<(&'static str, u16, Value)>) -> Self {
        let mock = Arc::new(Mock {
            replies: Mutex::new(replies.into()),
            calls: Mutex::new(Vec::new()),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().fallback(upstream).with_state(mock.clone());
        let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            record: BackendRecord {
                id: "autoglm-test".into(),
                kind: "autoglm".into(),
                base_url: base.clone(),
                options: json!({"region":"cn","account_base":base}).to_string(),
                ..Default::default()
            },
            mock,
            handle,
        }
    }

    /// 构造生产后端和任务存储，凭证不具备官方授权效力。
    async fn backend(&self) -> (AutoGlmBackend, Store) {
        let store = Store::open(std::path::Path::new(":memory:")).await.unwrap();
        store.upsert_backend(&self.record).await.unwrap();
        let account = Account {
            id: "test-account".into(),
            backend_id: self.record.id.clone(),
            name: "test".into(),
            enabled: true,
            credential: credential().serialize().unwrap(),
        };
        store.upsert_account(&account).await.unwrap();
        let backend = AutoGlmBackend::new(&self.record, vec![account], store.clone()).unwrap();
        (backend, store)
    }
}

impl Drop for Server {
    /// 场景结束释放监听器，不残留后台请求。
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// 令牌没有 JWT 签名，设备值只用于回环请求断言。
fn credential() -> Credential {
    Credential {
        access_token: "fake-access".into(),
        refresh_token: "fake-refresh".into(),
        device_id: "test-device".into(),
        region: Some(Region::Cn),
        ..Default::default()
    }
}

/// 官方指定业务码才回落；网络、坏结构和其他拒绝不进入第二刷新通道。
#[tokio::test]
async fn refresh_falls_back_only_for_explicit_business_code() {
    for (status, response, fallback) in [
        (200, json!({"code":400002}), true),
        (503, json!({"code":510002}), false),
        (200, json!({"code":400000}), false),
        (
            200,
            json!({"code":0,"data":{"access_token":"incomplete"}}),
            false,
        ),
    ] {
        let mut replies = vec![("/userapi/v1/refresh", status, response)];
        if fallback {
            replies.push((
                "/userapi/v1/agent-refresh",
                200,
                json!({"code":0,"data":{"access_token":"new-access","refresh_token":"new-refresh"}}),
            ));
        }
        let server = Server::new(replies).await;
        let result = Client::new(&server.record)
            .unwrap()
            .refresh(&credential())
            .await;
        assert_eq!(result.is_ok(), fallback);
        if let Ok(updated) = result {
            assert_eq!(updated.refresh_token, "new-refresh");
        }
        assert_eq!(
            server.mock.calls.lock().await.len(),
            if fallback { 2 } else { 1 }
        );
        assert!(server.mock.replies.lock().await.is_empty());
    }
}

/// 卡片包含官方日期、可见性和领取状态，不能使用旧任务列表形状替代。
fn card(day: &str, visible: bool, status: &str) -> Value {
    json!({"code":0,"data":{"biz_date":day,"card_visible":visible,"claim_status":status}})
}

/// 一次领取仅携带 client_request_id，随后核对卡片，重复调度不再次提交。
#[tokio::test]
async fn card_claim_uses_one_idempotency_key_and_records_reward_once() {
    let day = scheduler::day_at(scheduler::now());
    let server = Server::new(vec![
        ("/asset-operation/api/v1/checkin/card", 200, card(&day, true, "unclaimed")),
        ("/asset-operation/api/v1/checkin/claim", 200,
            json!({"code":0,"data":{"biz_date":day,"result":"success","claimed_amount":37,"items":[{"replayed":false}]}})),
        ("/asset-operation/api/v1/checkin/card", 200, card(&day, true, "claimed")),
    ]).await;
    let (backend, store) = server.backend().await;
    let result = run_and_record(&store, &backend, "test-account", &day)
        .await
        .unwrap();
    assert_eq!(result.status, TaskStatus::Succeeded);
    assert_eq!(result.amount, Some(37));
    let repeated = run_and_record(&store, &backend, "test-account", &day)
        .await
        .unwrap();
    assert_eq!(repeated.status, TaskStatus::AlreadyDone);
    let calls = server.mock.calls.lock().await;
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[1].0, Method::POST);
    assert_eq!(calls[1].2.as_object().unwrap().len(), 1);
    assert!(uuid::Uuid::parse_str(calls[1].2["client_request_id"].as_str().unwrap()).is_ok());
}

/// 未开放、已领取与处理中均只查询，不提交领取或跨协议回落。
#[tokio::test]
async fn unavailable_card_states_never_submit_claim() {
    let day = scheduler::day_at(scheduler::now());
    for (visible, status, expected) in [
        (false, "unclaimed", TaskStatus::Skipped),
        (true, "claimed", TaskStatus::AlreadyDone),
        (true, "processing", TaskStatus::Unknown),
    ] {
        let server = Server::new(vec![(
            "/asset-operation/api/v1/checkin/card",
            200,
            card(&day, visible, status),
        )])
        .await;
        let (backend, _) = server.backend().await;
        assert_eq!(
            backend
                .run_for_account("test-account", &day)
                .await
                .unwrap()
                .status,
            expected
        );
        assert_eq!(server.mock.calls.lock().await.len(), 1);
    }
}

/// 服务器失败和部分成功都保留未知，不能再次领取；并发已领取不能计成本次奖励。
#[tokio::test]
async fn claim_uncertainty_and_replay_preserve_official_result_semantics() {
    let day = scheduler::day_at(scheduler::now());
    for (status, response, expected, verify) in [
        (503, json!({}), TaskStatus::Unknown, false),
        (200, json!({"code":650304}), TaskStatus::AlreadyDone, false),
        (
            200,
            json!({"code":0,"data":{"biz_date":day,"result":"partial_success","claimed_amount":3}}),
            TaskStatus::Unknown,
            true,
        ),
        (
            200,
            json!({"code":0,"data":{"biz_date":day,"result":"success","claimed_amount":37,"items":[{"replayed":true}]}}),
            TaskStatus::AlreadyDone,
            true,
        ),
    ] {
        let mut replies = vec![
            (
                "/asset-operation/api/v1/checkin/card",
                200,
                card(&day, true, "unclaimed"),
            ),
            ("/asset-operation/api/v1/checkin/claim", status, response),
        ];
        if verify {
            replies.push((
                "/asset-operation/api/v1/checkin/card",
                200,
                card(&day, true, "claimed"),
            ));
        }
        let server = Server::new(replies).await;
        let (backend, store) = server.backend().await;
        let result = run_and_record(&store, &backend, "test-account", &day)
            .await
            .unwrap();
        assert_eq!(result.status, expected);
        assert_eq!(result.amount, None);
        let _ = run_and_record(&store, &backend, "test-account", &day)
            .await
            .unwrap();
        assert_eq!(
            server.mock.calls.lock().await.len(),
            if verify { 3 } else { 2 }
        );
    }
}
