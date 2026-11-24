//! 独立 MiniMax OAuth 与原生 Messages 的离线集成验证。

use axum::{
    Form, Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    response::Response,
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use fakemodelproxy::tasks::{TaskProvider, TaskStatus};
use fakemodelproxy::{
    accounts::Account,
    accounts::remote::{CredentialRefresher, QuotaReader},
    backend::{
        Backend, Registry,
        minimax_code::{MiniMaxBackend, oauth::Credential},
    },
    config::Config,
    state::AppState,
    store::{BackendRecord, Store},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{
        Arc, RwLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Mutex;
use tower::ServiceExt;

#[derive(Default)]
struct Mock {
    challenges: Mutex<HashMap<String, String>>,
    polls: AtomicUsize,
    refreshes: AtomicUsize,
    claims: AtomicUsize,
    mode: Mutex<String>,
}

/// 假授权服务器验证独立 PKCE，而非桌面凭证或 API key。
async fn device(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    Form(fields): Form<HashMap<String, String>>,
) -> Json<Value> {
    assert!(!headers.contains_key("authorization"));
    assert_eq!(fields["client_id"], "mcode-public");
    assert_eq!(fields["scope"], "agent.default");
    assert_eq!(fields["audience"], "agent-backend");
    assert_eq!(fields["code_challenge_method"], "S256");
    let code = uuid::Uuid::new_v4().to_string();
    mock.challenges
        .lock()
        .await
        .insert(code.clone(), fields["code_challenge"].clone());
    if mock.mode.lock().await.as_str() == "account-mode" {
        return Json(
            json!({"user_code":code,"verification_url":"https://account.minimax.cn/oauth/verify","expired_in":fakemodelproxy::records::now_seconds()*1000+60000,"interval":1000}),
        );
    }
    Json(
        json!({"device_code":code,"user_code":"ABCD-EFGH","verification_uri_complete":"https://account.minimax.cn/oauth/verify","expires_in":60,"interval":1}),
    )
}

/// 验证取票与刷新表单；可模拟等待、拒绝或结果未知。
async fn token(
    State(mock): State<Arc<Mock>>,
    Form(fields): Form<HashMap<String, String>>,
) -> (StatusCode, Json<Value>) {
    assert_eq!(fields["client_id"], "mcode-public");
    if fields["grant_type"] == "refresh_token" {
        mock.refreshes.fetch_add(1, Ordering::SeqCst);
        assert_eq!(fields["refresh_token"], "fake-refresh");
        if mock.mode.lock().await.as_str() == "refresh-unknown" {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error":"server_error"})),
            );
        }
        return (
            StatusCode::OK,
            Json(
                json!({"access_token":"fresh-access","refresh_token":"fresh-refresh","token_type":"Bearer","expires_in":3600,"scope":"agent.default","user_id":"test-user"}),
            ),
        );
    }
    let count = mock.polls.fetch_add(1, Ordering::SeqCst);
    assert_eq!(
        fields["grant_type"],
        "urn:ietf:params:oauth:grant-type:device_code"
    );
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(fields["code_verifier"].as_bytes()));
    let device_code = fields
        .get("device_code")
        .or(fields.get("user_code"))
        .unwrap();
    assert_eq!(mock.challenges.lock().await[device_code], challenge);
    let mode = mock.mode.lock().await.clone();
    if mode == "pending" || (mode == "once-pending" && count == 0) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"authorization_pending"})),
        );
    }
    (
        StatusCode::OK,
        Json(
            json!({"access_token":"fake-access","refresh_token":"fake-refresh","token_type":"Bearer","expires_in":3600,"scope":"agent.default","user_id":device_code}),
        ),
    )
}

/// 返回动态模型能力，不使用生产模型硬编码。
async fn models() -> Json<Value> {
    Json(
        json!({"ttlSeconds":10,"providers":[{"providerId":"minimax","config":{"models":{"test-model":{"name":"Test","tool_call":true,"reasoning":true,"modalities":{"input":["text","image"]},"limit":{"context":512000,"output":128000},"context_window_options":[512000,1000000]}}}}]}),
    )
}

/// 假 Messages 返回 CRLF 分隔的文本、推理、工具和缓存用量。
async fn chat(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    assert_eq!(headers["authorization"], "Bearer fake-access");
    assert_eq!(headers["x-mavis-agent-id"], "main");
    assert!(headers.contains_key("x-mavis-session-id"));
    assert_eq!(body["model"], "test-model");
    assert_eq!(body["stream"], true);
    let mut frames = vec![
        json!({"type":"message_start","message":{"usage":{"input_tokens":3,"cache_read_input_tokens":2,"cache_creation_input_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"你好"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":""}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"分析"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"call-test","name":"echo","input":{}}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"text\":\"hi\"}"}}),
        json!({"type":"content_block_stop","index":2}),
    ];
    if mock.mode.lock().await.as_str() != "truncated" {
        frames.push(json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4}}));
        frames.push(json!({"type":"message_stop"}));
    }
    let bytes = frames
        .into_iter()
        .map(|v| format!("data: {v}\r\n\r\n"))
        .collect::<String>()
        .into_bytes();
    // 每个字节独立分片，覆盖中文 UTF-8 跨网络块边界。
    let stream =
        futures_util::stream::iter(bytes.into_iter().map(|v| Ok::<_, std::io::Error>(vec![v])));
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// 未过期小数积分与已过期批次共同返回，检查余额计算口径。
async fn quota() -> Json<Value> {
    let now = fakemodelproxy::records::now_seconds() * 1000;
    Json(json!({"details":[
        {"granted_amount":"12.25","consumed_amount":"2.10","remaining_amount":"10.15","expire_at_ms":now+3600000},
        {"granted_amount":100,"consumed_amount":0,"remaining_amount":100,"expire_at_ms":now-1000}
    ]}))
}

/// 假签到面板只有明确可领取状态触发 POST。
async fn signin(State(mock): State<Arc<Mock>>) -> Json<Value> {
    Json(
        json!({"base_resp":{"status_code":0},"data":{"days":[{"is_today":true,"status":if mock.mode.lock().await.as_str()=="not-claimable" {3} else {2}}]}}),
    )
}

/// 领取计数用于证明坏响应不重放，也不会伪造成功。
async fn claim(State(mock): State<Arc<Mock>>) -> Json<Value> {
    mock.claims.fetch_add(1, Ordering::SeqCst);
    let mode = mock.mode.lock().await.clone();
    Json(if mode == "empty-claim" {
        json!({"base_resp":{"status_code":0},"data":{}})
    } else {
        let panel = json!({"scene":2,"days":(1..=7).map(|day|json!({
            "day_no":day,"points":12,"is_today":day==1,"status":if day==1 {3} else {1}
        })).collect::<Vec<_>>()});
        let mut data = json!({
            "claim_id":"test-claim","claim_result":if mode=="already-claim" {2} else {1},
            "day_no":1,"points":12,"expire_at_ms":1890000000000u64,"panel":panel
        });
        if mode == "bad-claim" {
            data["claim_id"] = Value::Null;
        }
        json!({"base_resp":{"status_code":0},"data":data})
    })
}

struct Fixture {
    state: AppState,
    mock: Arc<Mock>,
    backend: BackendRecord,
    upstream: tokio::task::JoinHandle<()>,
}

impl Fixture {
    /// 装配真实管理路由与本机假上游，数据库按测试独立。
    async fn new() -> Self {
        let mock = Arc::new(Mock::default());
        let upstream_app = Router::new()
            .route("/oauth2/device/code", post(device))
            .route("/oauth2/token", post(token))
            .route("/mavis/api/v1/models", get(models))
            .route("/mavis/api/v1/llm/v1/messages", post(chat))
            .route("/minimax-cloud/api/v1/credit/details", get(quota))
            .route("/minimax-cloud/api/v1/signin/status", get(signin))
            .route("/minimax-cloud/api/v1/signin/claim", post(claim))
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let upstream = tokio::spawn(async move {
            axum::serve(listener, upstream_app).await.unwrap();
        });
        let store = Store::open(std::path::Path::new(":memory:")).await.unwrap();
        let backend = BackendRecord {
            id: "mm".into(),
            name: "MiniMax Test".into(),
            kind: "minimax_code".into(),
            base_url: base,
            models: "test-model".into(),
            options: json!({"region":"cn"}).to_string(),
            ..Default::default()
        };
        store.upsert_backend(&backend).await.unwrap();
        let state = AppState::new(
            Arc::new(Config::default()),
            store.clone(),
            Arc::new(Registry::new()),
            Arc::new(fakemodelproxy::records::RecordLog::default()),
            Arc::new(RwLock::new(fakemodelproxy::tasks::TaskRegistry::new())),
            Arc::new(fakemodelproxy::routing::Router::new()),
            Arc::new(fakemodelproxy::usage::UsageLedger::new(store)),
        );
        state.reload_backends().await.unwrap();
        Self {
            state,
            mock,
            backend,
            upstream,
        }
    }

    /// 通过公开路由调用，不绕过 OAuth 会话或对话协议转换。
    async fn send(&self, path: &str, body: Value) -> (StatusCode, String) {
        let response = fakemodelproxy::build_router(self.state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    /// 发起授权并读取独立会话 ID。
    async fn start(&self) -> Value {
        let (status, body) = self
            .send("/api/oauth/minimax_code/start", json!({"backend_id":"mm"}))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let result: Value = serde_json::from_str(&body).unwrap();
        assert!(result["user_code"].as_str().is_some_and(|v| !v.is_empty()));
        assert!(!body.contains("device_code"));
        assert!(!body.contains("code_verifier"));
        result
    }

    /// 授权后台有真实 interval；等待设明确截止，避免无界轮询。
    async fn done(&self, flow: &Value) -> Value {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let (status, body) = self
                    .send(
                        "/api/oauth/minimax_code/poll",
                        json!({"backend_id":"mm","session_id":flow["session_id"]}),
                    )
                    .await;
                assert_eq!(status, StatusCode::OK, "{body}");
                let value: Value = serde_json::from_str(&body).unwrap();
                if value["status"] != "pending" {
                    return value;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }

    /// 保存代理自有测试凭证，以独立账号身份测试自动刷新。
    async fn account(&self, expires_at: u64) -> Account {
        let account = Account {
            id: "test".into(),
            backend_id: "mm".into(),
            name: "Test".into(),
            enabled: true,
            credential: Credential {
                access_token: "fake-access".into(),
                refresh_token: "fake-refresh".into(),
                expires_at,
                region: "cn".into(),
                subject: "test-user".into(),
            }
            .serialize()
            .unwrap(),
        };
        self.state
            .config_service()
            .save_account(&account)
            .await
            .unwrap();
        self.state.reload_backends().await.unwrap();
        account
    }
}

impl Drop for Fixture {
    /// 停止假上游，不操作真实服务或厂商账号。
    fn drop(&mut self) {
        self.upstream.abort();
    }
}

/// 多窗口不能混领结果；取消与服务变更不产生迟到账号。
#[tokio::test]
async fn independent_oauth_sessions_and_cancellation() {
    let fixture = Fixture::new().await;
    *fixture.mock.mode.lock().await = "once-pending".into();
    let first = fixture.start().await;
    let second = fixture.start().await;
    assert_ne!(first["session_id"], second["session_id"]);
    let (status, _) = fixture
        .send(
            "/api/oauth/minimax_code/poll",
            json!({"backend_id":"other","session_id":first["session_id"]}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    for flow in [&first, &second] {
        let done = fixture.done(flow).await;
        assert_eq!(done["status"], "done");
        assert_eq!(done, fixture.done(flow).await);
        assert!(!done.to_string().contains("fake-access"));
    }
    assert_eq!(fixture.state.store.list_accounts().await.unwrap().len(), 2);
    *fixture.mock.mode.lock().await = "pending".into();
    let cancelled = fixture.start().await;
    fixture
        .send(
            "/api/oauth/minimax_code/cancel",
            json!({"backend_id":"mm","session_id":cancelled["session_id"]}),
        )
        .await;
    let (status, _) = fixture
        .send(
            "/api/oauth/minimax_code/poll",
            json!({"backend_id":"mm","session_id":cancelled["session_id"]}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let changed = fixture.start().await;
    let mut backend = fixture.backend.clone();
    backend.enabled = false;
    fixture.state.store.upsert_backend(&backend).await.unwrap();
    *fixture.mock.mode.lock().await = String::new();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let (status, _) = fixture
        .send(
            "/api/oauth/minimax_code/poll",
            json!({"backend_id":"mm","session_id":changed["session_id"]}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(fixture.state.store.list_accounts().await.unwrap().len(), 2);
}

/// 三协议保留文本、工具、用量；现有 Messages/Chat 契约另保留思考。
#[tokio::test]
async fn three_protocols_and_truncated_stream() {
    let fixture = Fixture::new().await;
    fixture
        .account(fakemodelproxy::records::now_seconds() + 3600)
        .await;
    for (path, body) in [
        (
            "/v1/chat/completions",
            json!({"model":"mm/test-model","messages":[{"role":"user","content":"hi"}]}),
        ),
        (
            "/v1/messages",
            json!({"model":"mm/test-model","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}),
        ),
        (
            "/v1/responses",
            json!({"model":"mm/test-model","input":"hi"}),
        ),
    ] {
        let (status, text) = fixture.send(path, body).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert!(text.contains("你好"), "{text}");
        assert!(text.contains("echo"), "{text}");
        if path != "/v1/responses" {
            assert!(text.contains("分析"), "{text}");
        }
        let result: Value = serde_json::from_str(&text).unwrap();
        let usage = &result["usage"];
        assert_eq!(
            usage["input_tokens"]
                .as_u64()
                .or(usage["prompt_tokens"].as_u64()),
            Some(6)
        );
    }
    *fixture.mock.mode.lock().await = "truncated".into();
    let (status, _) = fixture
        .send(
            "/v1/chat/completions",
            json!({"model":"mm/test-model","messages":[{"role":"user","content":"hi"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

/// 自动刷新并发只换一次；未知结果不能再次花旧刷新令牌。
#[tokio::test]
async fn refresh_single_rotation_quota_and_unknown_fence() {
    let fixture = Fixture::new().await;
    let account = fixture.account(0).await;
    let backend = Arc::new(
        MiniMaxBackend::new(
            &fixture.backend,
            vec![account.clone()],
            fixture.state.store.clone(),
        )
        .unwrap(),
    );
    let models = backend.list_models().await.unwrap();
    assert_eq!(models[0].token_limits.context_window, Some(512000));
    assert_eq!(models[0].token_limits.max_output_tokens, Some(128000));
    assert_eq!(models[0].token_limits.context_tiers.len(), 2);
    let (first, second) = tokio::join!(backend.read(&account), backend.read(&account));
    let quota = first.unwrap();
    second.unwrap();
    assert_eq!(fixture.mock.refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(quota.remaining, Some(1015));
    assert_eq!(quota.total, Some(1225));
    assert!(quota.reset_at.unwrap().ends_with('Z'));
    let stored = fixture
        .state
        .store
        .get_account("test")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        Credential::parse(&stored.credential).unwrap().access_token,
        "fresh-access"
    );
    let account = fixture.account(0).await;
    *fixture.mock.mode.lock().await = "refresh-unknown".into();
    assert!(backend.refresh(&account).await.is_err());
    let count = fixture.mock.refreshes.load(Ordering::SeqCst);
    assert!(backend.refresh(&account).await.is_err());
    assert_eq!(fixture.mock.refreshes.load(Ordering::SeqCst), count);
}

/// 官方 user_code、毫秒 interval 和绝对过期时间格式也可独立完成授权。
#[tokio::test]
async fn account_device_response_variant() {
    let fixture = Fixture::new().await;
    *fixture.mock.mode.lock().await = "account-mode".into();
    let flow = fixture.start().await;
    assert_eq!(fixture.done(&flow).await["status"], "done");
    assert_eq!(fixture.state.store.list_accounts().await.unwrap().len(), 1);
}

/// 刷新锁被其他网络请求占用时，取消仍结束等待并阻止保存。
#[tokio::test]
async fn cancellation_while_waiting_for_credential_lock() {
    let fixture = Fixture::new().await;
    let guard = fixture.state.store.lock_credentials().await;
    let flow = fixture.start().await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while fixture.mock.polls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let (status, _) = tokio::time::timeout(
        Duration::from_secs(1),
        fixture.send(
            "/api/oauth/minimax_code/cancel",
            json!({"backend_id":"mm","session_id":flow["session_id"]}),
        ),
    )
    .await
    .expect("取消不应等待凭证刷新锁");
    assert_eq!(status, StatusCode::OK);
    drop(guard);
    let (status, _) = fixture
        .send(
            "/api/oauth/minimax_code/poll",
            json!({"backend_id":"mm","session_id":flow["session_id"]}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        fixture
            .state
            .store
            .list_accounts()
            .await
            .unwrap()
            .is_empty()
    );
}

/// 领取响应缺奖励返回未知；未知签到枚举不假设为已经签到。
#[tokio::test]
async fn checkin_requires_claimable_and_confirmed_reward() {
    let fixture = Fixture::new().await;
    let account = fixture
        .account(fakemodelproxy::records::now_seconds() + 3600)
        .await;
    let backend =
        MiniMaxBackend::new(&fixture.backend, vec![account], fixture.state.store.clone()).unwrap();
    *fixture.mock.mode.lock().await = "not-claimable".into();
    assert_eq!(
        backend
            .run_for_account("test", "today")
            .await
            .unwrap()
            .status,
        TaskStatus::AlreadyDone
    );
    assert_eq!(fixture.mock.claims.load(Ordering::SeqCst), 0);
    *fixture.mock.mode.lock().await = "empty-claim".into();
    assert_eq!(
        backend
            .run_for_account("test", "today")
            .await
            .unwrap()
            .status,
        TaskStatus::Unknown
    );
    assert_eq!(fixture.mock.claims.load(Ordering::SeqCst), 1);
    *fixture.mock.mode.lock().await = String::new();
    let result = backend
        .run_for_account("test", "another-day")
        .await
        .unwrap();
    assert_eq!(result.status, TaskStatus::Succeeded);
    assert_eq!(result.amount, Some(12));
    for (mode, expected) in [
        ("already-claim", TaskStatus::AlreadyDone),
        ("bad-claim", TaskStatus::Unknown),
    ] {
        *fixture.mock.mode.lock().await = mode.into();
        let result = backend
            .run_for_account("test", "another-day")
            .await
            .unwrap();
        assert_eq!(result.status, expected);
        assert_eq!(result.amount, None);
    }
}

/// 在自动清理的临时目录验证重启恢复，不能重新消费旧刷新令牌。
#[tokio::test]
async fn staged_refresh_survives_database_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("refresh.db");
    let store = Store::open(&path).await.unwrap();
    let account = Account {
        id: "staged".into(),
        backend_id: "mm".into(),
        name: "Test".into(),
        enabled: true,
        credential: "old".into(),
    };
    store.upsert_account(&account).await.unwrap();
    store
        .begin_credential_refresh("staged", "old")
        .await
        .unwrap();
    // 模拟已经持久暂存、但原子替换账号前崩溃的窗口。
    let options = sqlx::sqlite::SqliteConnectOptions::new().filename(&path);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .connect_with(options)
        .await
        .unwrap();
    sqlx::query("UPDATE credential_refresh SET next='new' WHERE account_id='staged'")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    drop(store);
    let reopened = Store::open(&path).await.unwrap();
    reopened
        .recover_credential_refresh("staged", "old")
        .await
        .unwrap();
    assert_eq!(
        reopened
            .get_account("staged")
            .await
            .unwrap()
            .unwrap()
            .credential,
        "new"
    );
    reopened
        .begin_credential_refresh("staged", "new")
        .await
        .unwrap();
    drop(reopened);
    let reopened = Store::open(&path).await.unwrap();
    assert!(
        reopened
            .recover_credential_refresh("staged", "new")
            .await
            .is_err()
    );
    drop(reopened);
}

/// 已暂存的换票结果可提交，账号删除后恢复不能复活。
#[tokio::test]
async fn persisted_refresh_recovery_and_deleted_account() {
    let fixture = Fixture::new().await;
    let account = fixture.account(0).await;
    fixture
        .state
        .store
        .begin_credential_refresh("test", &account.credential)
        .await
        .unwrap();
    fixture
        .state
        .store
        .finish_credential_refresh("test", &account.credential, "new-credential")
        .await
        .unwrap();
    fixture
        .state
        .store
        .recover_credential_refresh("test", "new-credential")
        .await
        .unwrap();
    assert_eq!(
        fixture
            .state
            .store
            .get_account("test")
            .await
            .unwrap()
            .unwrap()
            .credential,
        "new-credential"
    );
    fixture
        .state
        .store
        .begin_credential_refresh("test", "new-credential")
        .await
        .unwrap();
    fixture.state.store.delete_account("test").await.unwrap();
    fixture
        .state
        .store
        .finish_credential_refresh("test", "new-credential", "late-credential")
        .await
        .unwrap();
    assert!(
        fixture
            .state
            .store
            .get_account("test")
            .await
            .unwrap()
            .is_none()
    );
}
