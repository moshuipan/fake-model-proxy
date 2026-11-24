//! 多会话并发回归：用户标识不是会话标识，重复上游会话会被假上游拒绝。

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::Response;
use axum::routing::post;
use fakemodelproxy::accounts::Account;
use fakemodelproxy::backend::Registry;
use fakemodelproxy::config::Config;
use fakemodelproxy::state::AppState;
use fakemodelproxy::store::{BackendRecord, Store};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::Notify;
use tower::ServiceExt;

/// 记录同时进入的会话，第二个请求到达后才允许第一个请求继续。
#[derive(Default)]
struct Mock {
    active: Mutex<HashSet<String>>,
    sessions: Mutex<Vec<String>>,
    arrivals: AtomicUsize,
    ready: Notify,
}

/// 重复会话返回明确错误，独立会话返回一个正常 SSE 完成事件。
async fn chat(State(mock): State<Arc<Mock>>, headers: HeaderMap, body: String) -> Response {
    assert_eq!(headers["x-model-key"], "qmodel_latest");
    let body = fakemodelproxy::backend::qoder::cosy::decode_body(&body).unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    let session = body["session_id"].as_str().unwrap().to_string();
    let distinct = mock.active.lock().unwrap().insert(session.clone());
    mock.sessions.lock().unwrap().push(session.clone());
    mock.arrivals.fetch_add(1, Ordering::SeqCst);
    mock.ready.notify_waiters();
    loop {
        let ready = mock.ready.notified();
        tokio::pin!(ready);
        ready.as_mut().enable();
        if mock.arrivals.load(Ordering::SeqCst) >= 2 {
            break;
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), ready)
            .await
            .expect("second request must arrive while the first is active");
    }
    if !distinct {
        return Response::builder()
            .status(StatusCode::CONFLICT)
            .body(Body::from("请求失败，请创建新会话后重试"))
            .unwrap();
    }
    mock.active.lock().unwrap().remove(&session);
    let frame = json!({"statusCodeValue":200,"body":json!({
        "choices":[{"delta":{"content":"OK"},"finish_reason":"stop"}],
    }).to_string()});
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from(format!("data: {frame}\n\n")))
        .unwrap()
}

/// 使用隔离 SQLite 与假凭证，避免消耗真实账号积分。
struct Fixture {
    state: AppState,
    mock: Arc<Mock>,
}

impl Fixture {
    /// 构造一个真实 Qoder 适配器并将其聊天请求指向本地上游。
    async fn new() -> Self {
        let mock = Arc::new(Mock::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/chat", listener.local_addr().unwrap());
        let upstream = Router::new()
            .route("/chat", post(chat))
            .with_state(mock.clone());
        tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });
        let store = Store::open(std::path::Path::new(":memory:")).await.unwrap();
        store
            .upsert_backend(&BackendRecord {
                id: "qoder".into(),
                name: "Qoder".into(),
                kind: "qoder".into(),
                models: "qmodel_latest".into(),
                options: "{\"region\":\"cn\"}".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        store
            .upsert_account(&Account {
                id: "fake".into(),
                backend_id: "qoder".into(),
                name: "Fake".into(),
                enabled: true,
                credential: json!({"uid":"fake","device_token":"fake-token","endpoint":endpoint})
                    .to_string(),
            })
            .await
            .unwrap();
        let state = AppState::new(
            Arc::new(Config {
                listen: "127.0.0.1:0".into(),
                data_dir: ".".into(),
                api_key: String::new(),
                admin_key: String::new(),
                ..Config::default()
            }),
            store.clone(),
            Arc::new(Registry::new()),
            Arc::new(fakemodelproxy::records::RecordLog::default()),
            Arc::new(RwLock::new(fakemodelproxy::tasks::TaskRegistry::new())),
            Arc::new(fakemodelproxy::routing::Router::new()),
            Arc::new(fakemodelproxy::usage::UsageLedger::new(store)),
        );
        state.reload_backends().await.unwrap();
        Self { state, mock }
    }

    /// 经完整协议入口请求响应，支持明确的会话头。
    async fn send(
        &self,
        path: &str,
        payload: &Value,
        session: Option<&str>,
    ) -> (StatusCode, String) {
        let mut request = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json");
        if let Some(session) = session {
            request = request.header("x-fmp-session", session);
        }
        let response = fakemodelproxy::build_router(self.state.clone())
            .oneshot(request.body(Body::from(payload.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }
}

/// 同一用户的独立聊天在三种协议、流式与非流式下均不共享上游会话。
#[tokio::test]
async fn concurrent_chats_with_same_user_are_independent() {
    for stream in [false, true] {
        let requests = [
            (
                "/v1/chat/completions",
                json!({"model":"qoder/qmodel_latest","user":"shared-user",
                "stream":stream,"messages":[{"role":"user","content":"hi"}]}),
            ),
            (
                "/v1/messages",
                json!({"model":"qoder/qmodel_latest","metadata":{"user_id":"shared-user"},
                "max_tokens":32,"stream":stream,"messages":[{"role":"user","content":"hi"}]}),
            ),
            (
                "/v1/responses",
                json!({"model":"qoder/qmodel_latest","metadata":{"user_id":"shared-user"},
                "stream":stream,"input":"hi"}),
            ),
        ];
        for (path, payload) in requests {
            let fixture = Fixture::new().await;
            let (first, second) = tokio::join!(
                fixture.send(path, &payload, None),
                fixture.send(path, &payload, None)
            );
            for response in [first, second] {
                assert_eq!(response.0, StatusCode::OK, "{path}: {}", response.1);
                assert!(response.1.contains("OK"), "{path}: {}", response.1);
            }
            let sessions = fixture.mock.sessions.lock().unwrap();
            assert_eq!(sessions.len(), 2);
            assert_ne!(sessions[0], sessions[1]);
            assert!(sessions.iter().all(|session| session != "shared-user"));
        }
    }
}

/// 明确头优先分组，厂商句柄由绑定独立持有；空头不能合并聊天。
#[tokio::test]
async fn explicit_and_empty_session_headers_are_handled_separately() {
    let payload = json!({"model":"qoder/qmodel_latest","user":"shared-user",
        "messages":[{"role":"user","content":"hi"}]});
    let fixture = Fixture::new().await;
    let (first, second) = tokio::join!(
        fixture.send("/v1/chat/completions", &payload, Some("conversation-a")),
        fixture.send("/v1/chat/completions", &payload, Some("conversation-b"))
    );
    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(second.0, StatusCode::OK);
    let sessions: HashSet<_> = fixture
        .mock
        .sessions
        .lock()
        .unwrap()
        .iter()
        .cloned()
        .collect();
    assert_eq!(sessions.len(), 2);
    assert!(
        sessions
            .iter()
            .all(|session| uuid::Uuid::parse_str(session).is_ok())
    );
    assert!(!sessions.contains("conversation-a") && !sessions.contains("conversation-b"));

    let fixture = Fixture::new().await;
    let (first, second) = tokio::join!(
        fixture.send("/v1/chat/completions", &payload, Some("")),
        fixture.send("/v1/chat/completions", &payload, Some(""))
    );
    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(second.0, StatusCode::OK);
    let sessions = fixture.mock.sessions.lock().unwrap();
    assert_ne!(sessions[0], sessions[1]);
    assert!(sessions.iter().all(|session| !session.is_empty()));
}
