//! 小浣熊离线 HTTP 闭环：一次性授权、取消、续期 CAS、积分和三协议工具流。

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    response::Response,
    routing::{get, post},
};
use fakemodelproxy::{
    accounts::{
        Account,
        remote::{CredentialRefresher, QuotaReader},
    },
    backend::{
        Backend, Registry,
        raccoon::{RaccoonBackend, client, credential::Credential},
    },
    config::Config,
    state::AppState,
    store::{BackendRecord, Store},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, Notify};
use tower::ServiceExt;

#[derive(Default)]
struct Mock {
    exchanges: AtomicUsize,
    refreshes: AtomicUsize,
    chats: AtomicUsize,
    reject_old: AtomicBool,
    mode: Mutex<String>,
    entered: Notify,
    release: Notify,
    balance: Mutex<Value>,
}

/// 换票接口只接受一次性授权码，不能附带旧账号 Bearer。
async fn exchange(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    assert!(!headers.contains_key("authorization"));
    assert_eq!(body.as_object().unwrap().len(), 1);
    let code = body["authorization_code"].as_str().unwrap();
    mock.exchanges.fetch_add(1, Ordering::SeqCst);
    Json(
        json!({"code":0,"data":{"access_token":format!("fake-access-{code}"),
        "refresh_token":"fake-refresh","expires_in":3600}}),
    )
}

/// 身份来自服务器而非令牌 claims；可暂停以测试迟到响应。
async fn user(State(mock): State<Arc<Mock>>, headers: HeaderMap) -> Json<Value> {
    if mock.mode.lock().await.as_str() == "hold-user" {
        mock.entered.notify_one();
        mock.release.notified().await;
    }
    let identity = headers["authorization"]
        .to_str()
        .unwrap()
        .strip_prefix("Bearer fake-access-")
        .unwrap();
    Json(json!({"code":0,"data":{"id":identity,"name":"测试小浣熊"}}))
}

/// 续期可暂停，验证并发请求只刷新一次及人工编辑不会被覆盖。
async fn refresh(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    assert!(!headers.contains_key("authorization"));
    assert_eq!(body["refresh_token"], "fake-refresh");
    mock.refreshes.fetch_add(1, Ordering::SeqCst);
    if mock.mode.lock().await.as_str() == "hold-refresh" {
        mock.entered.notify_one();
        mock.release.notified().await;
    }
    // 官方允许续期省略 refresh_token；不能因此清空已保存的刷新令牌。
    Json(json!({"code":0,"data":{"access_token":"fresh-access","expires_in":3600}}))
}

/// 动态模型 name 是调用 ID，uuid 是内部目录标识。
async fn models(State(mock): State<Arc<Mock>>, headers: HeaderMap) -> Json<Value> {
    assert!(
        headers["authorization"]
            .to_str()
            .unwrap()
            .starts_with("Bearer ")
    );
    if mock.mode.lock().await.as_str() == "bad-catalog" {
        return Json(json!({"code":0,"data":{"id":"envelope","models":[{"id":"wrong"}]}}));
    }
    Json(
        json!({"code":0,"data":{"categories":[{"id":"group","models":[
            {"id":"uuid-only","name":"test-model","display_name":"测试模型",
                "billing_multiplier":0.5,"params":{"context_window":200000,"max_tokens":8192},
                "tags":["vision","reasoning"]},
            {"id":"dup","name":"test-model","billing_multiplier":1},
            {"name":"free-model","billing_multiplier":0},
            {"name":"unknown-model","credits":10,"params":{"context_window":-1,"max_tokens":1.5}},
            {"name":"disabled","enabled":false}
        ]}]}}),
    )
}

/// 小数积分不经过浮点或取整；可模拟缺字段。
async fn balance(State(mock): State<Arc<Mock>>, headers: HeaderMap) -> Response {
    if mock.reject_old.load(Ordering::SeqCst) && headers["authorization"] == "Bearer fake-access" {
        return Response::builder().status(401).body(Body::empty()).unwrap();
    }
    Json(json!({"code":0,"data":mock.balance.lock().await.clone()})).into_response()
}

use axum::response::IntoResponse;

/// 原生 Chat SSE 分字节传输，覆盖中文、推理、工具参数和缺失结束帧。
async fn chat(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    mock.chats.fetch_add(1, Ordering::SeqCst);
    assert!(
        headers["authorization"]
            .to_str()
            .unwrap()
            .starts_with("Bearer ")
    );
    assert!(!headers.contains_key("x-client-type"));
    assert_eq!(body["model"], "test-model");
    assert_eq!(body["stream"], true);
    assert_eq!(body["max_tokens"], 32);
    assert_eq!(body["messages"][0]["content"], "keep system prompt");
    assert_eq!(body["tools"][0]["function"]["name"], "echo");
    let mut frames = vec![
        json!({"choices":[{"index":0,"delta":{"role":"assistant","content":"你好"}}]}),
        json!({"choices":[{"index":0,"delta":{"reasoning_content":"分析"}}]}),
        json!({"choices":[{"index":0,"delta":{"tool_calls":[
            {"index":0,"id":"call-test","type":"function","function":{"name":"echo","arguments":"{\"text\":"}}
        ]}}]}),
        json!({"choices":[{"index":0,"delta":{"tool_calls":[
            {"index":0,"function":{"arguments":"\"hi\"}"}}
        ]}}]}),
    ];
    let truncated = mock.mode.lock().await.as_str() == "truncated";
    if !truncated {
        frames.push(
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],
            "usage":{"prompt_tokens":3,"completion_tokens":4,"total_tokens":7}}),
        );
    }
    let mut data = frames
        .into_iter()
        .map(|frame| format!("data: {frame}\r\n\r\n"))
        .collect::<String>();
    if !truncated {
        data.push_str("data: [DONE]\r\n\r\n");
    }
    let stream = futures_util::stream::iter(
        data.into_bytes()
            .into_iter()
            .map(|byte| Ok::<_, std::io::Error>(vec![byte])),
    );
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

struct Fixture {
    state: AppState,
    backend: BackendRecord,
    mock: Arc<Mock>,
    upstream: tokio::task::JoinHandle<()>,
    _directory: tempfile::TempDir,
}

impl Fixture {
    /// 使用真实管理和入站路由，假上游与数据库按测试隔离。
    async fn new() -> Self {
        let mock = Arc::new(Mock::default());
        *mock.balance.lock().await = json!({"available_points":"123.456789012345678901",
            "daily_points":"100.25"});
        let app = Router::new()
            .route(
                "/api/web/auth/v1/login_with_authorization_code",
                post(exchange),
            )
            .route("/api/web/auth/v1/refresh", post(refresh))
            .route(client::USER_PATH, get(user))
            .route(client::MODELS_PATH, get(models))
            .route(client::BALANCE_PATH, get(balance))
            .route(client::CHAT_PATH, post(chat))
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let upstream = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("test.db"))
            .await
            .unwrap();
        let backend = BackendRecord {
            id: "raccoon-test".into(),
            name: "小浣熊测试".into(),
            kind: "raccoon".into(),
            base_url,
            models: "test-model".into(),
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
            backend,
            mock,
            upstream,
            _directory: directory,
        }
    }

    /// 公开接口调用只返回状态和安全响应。
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

    /// 发起授权，确认官方新标签页地址和 loopback 参数。
    async fn start(&self) -> Value {
        let (status, body) = self
            .send(
                "/api/oauth/raccoon/start",
                json!({"backend_id":self.backend.id}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let flow: Value = serde_json::from_str(&body).unwrap();
        let url = reqwest::Url::parse(flow["login_url"].as_str().unwrap()).unwrap();
        assert_eq!(url.origin().ascii_serialization(), client::ORIGIN);
        assert_eq!(url.path(), "/authorize");
        let pairs = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(pairs["desktop_callback"], "loopback");
        assert_eq!(pairs["state"].len(), 64);
        assert_eq!(flow["automatic_account_save"], true);
        flow
    }

    /// 模拟官网实际回调；不访问生产授权服务器。
    async fn callback(&self, flow: &Value, code: &str, wrong_state: bool) -> StatusCode {
        let official = reqwest::Url::parse(flow["login_url"].as_str().unwrap()).unwrap();
        let pairs = official
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        let mut url = reqwest::Url::parse(&pairs["redirect"]).unwrap();
        url.query_pairs_mut().append_pair("code", code).append_pair(
            "state",
            if wrong_state {
                "invalid"
            } else {
                &pairs["state"]
            },
        );
        match reqwest::Client::new().get(url).send().await {
            Ok(response) => response.status(),
            // 一次性回调关闭监听也是明确的拒绝；首次回调仍需断言 OK。
            Err(error) if error.is_connect() => StatusCode::GONE,
            Err(error) => panic!("callback request failed: {error}"),
        }
    }

    /// 后台提交设两秒截止，完成结果重复读取也不重复换票。
    async fn done(&self, flow: &Value) -> (StatusCode, Value) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let (status, body) = self
                    .send(
                        "/api/oauth/raccoon/poll",
                        json!({"backend_id":self.backend.id,"session_id":flow["session_id"]}),
                    )
                    .await;
                let value: Value = serde_json::from_str(&body).unwrap();
                if status != StatusCode::OK || value["status"] == "done" {
                    return (status, value);
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }

    /// 保存虚构账号，测试自动续期和真实入站转换。
    async fn account(&self, expires_at: u64) -> Account {
        let account = Account {
            id: "test".into(),
            backend_id: self.backend.id.clone(),
            name: "Test".into(),
            enabled: true,
            credential: Credential {
                access_token: "fake-access".into(),
                refresh_token: "fake-refresh".into(),
                expires_at,
                obtained_at: fakemodelproxy::records::now_seconds(),
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

    /// 构造可读取当前账号的同一生产适配器。
    fn provider(&self, account: &Account) -> RaccoonBackend {
        RaccoonBackend::new(
            &self.backend,
            vec![account.clone()],
            self.state.store.clone(),
        )
        .unwrap()
    }
}

impl Drop for Fixture {
    /// 终止本机假上游并释放尚未完成的授权。
    fn drop(&mut self) {
        self.upstream.abort();
    }
}

/// 同一回调仅换票一次，多标签页 state 隔离，自动保存目录与积分。
#[tokio::test]
async fn official_callback_is_one_shot_and_results_have_no_credentials() {
    let fixture = Fixture::new().await;
    *fixture.mock.mode.lock().await = "hold-user".into();
    let first = fixture.start().await;
    let second = fixture.start().await;
    assert_ne!(first["session_id"], second["session_id"]);
    assert_ne!(first["login_url"], second["login_url"]);
    assert_eq!(
        fixture.callback(&first, "one", true).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(fixture.mock.exchanges.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.callback(&first, "one", false).await, StatusCode::OK);
    fixture.mock.entered.notified().await;
    assert_eq!(
        fixture.callback(&first, "one", false).await,
        StatusCode::CONFLICT
    );
    let (status, _) = fixture
        .send(
            "/api/oauth/raccoon/poll",
            json!({"backend_id":"other","session_id":first["session_id"]}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    fixture.mock.release.notify_one();
    let (status, done) = fixture.done(&first).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(done["models_synced"], true);
    assert_eq!(done, fixture.done(&first).await.1);
    assert!(!done.to_string().contains("fake-access"));
    *fixture.mock.mode.lock().await = String::new();
    assert_eq!(
        fixture.callback(&second, "two", false).await,
        StatusCode::OK
    );
    assert_eq!(fixture.done(&second).await.0, StatusCode::OK);
    assert_eq!(fixture.mock.exchanges.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.state.store.list_accounts().await.unwrap().len(), 2);
    assert_eq!(fixture.state.store.cached_quota().await.unwrap().len(), 2);
    let catalogs = fixture.state.store.list_model_catalogs().await.unwrap();
    assert_eq!(catalogs[&fixture.backend.id][0].id, "test-model");
}

/// 取消中途授权及服务变更均阻止网络响应复活账号。
#[tokio::test]
async fn cancellation_and_changed_service_reject_late_grants() {
    for cancel in [true, false] {
        let fixture = Fixture::new().await;
        *fixture.mock.mode.lock().await = "hold-user".into();
        let flow = fixture.start().await;
        fixture.callback(&flow, "late", false).await;
        fixture.mock.entered.notified().await;
        if cancel {
            fixture
                .send(
                    "/api/oauth/raccoon/cancel",
                    json!({"backend_id":fixture.backend.id,"session_id":flow["session_id"]}),
                )
                .await;
        } else {
            let mut changed = fixture.backend.clone();
            changed.enabled = false;
            fixture.state.store.upsert_backend(&changed).await.unwrap();
        }
        fixture.mock.release.notify_one();
        assert_ne!(fixture.done(&flow).await.0, StatusCode::OK);
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
}

/// 目录同步失败不能把已保存账号误报为授权失败。
#[tokio::test]
async fn failed_catalog_keeps_account_and_exposes_retryable_sync() {
    let fixture = Fixture::new().await;
    *fixture.mock.mode.lock().await = "bad-catalog".into();
    let flow = fixture.start().await;
    fixture.callback(&flow, "saved", false).await;
    let (status, done) = fixture.done(&flow).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(done["models_synced"], false);
    assert!(done["sync_error"].as_str().unwrap().contains("目录"));
    assert_eq!(fixture.state.store.list_accounts().await.unwrap().len(), 1);
    assert!(fixture.state.registry.get(&fixture.backend.id).is_some());
}

/// 目录倍率保留小数和零，缺失能力或限制不虚构。
#[tokio::test]
async fn catalog_and_exact_balance_keep_their_original_meaning() {
    let fixture = Fixture::new().await;
    let account = fixture
        .account(fakemodelproxy::records::now_seconds() + 3600)
        .await;
    let provider = fixture.provider(&account);
    let models = provider.list_models().await.unwrap();
    assert_eq!(models.len(), 3);
    assert_eq!(models[0].price_factor, Some(0.5));
    assert_eq!(models[0].token_limits.context_window, Some(200000));
    assert_eq!(models[0].token_limits.max_output_tokens, Some(8192));
    assert!(models[0].capabilities.images && models[0].capabilities.reasoning);
    assert_eq!(models[1].price_factor, Some(0.0));
    assert!(models[2].price_factor.is_none());
    assert!(models[2].token_limits.context_window.is_none());
    assert!(models[2].token_limits.max_output_tokens.is_none());
    assert!(!models[2].capabilities.reasoning);
    let info = provider.read(&account).await.unwrap();
    assert_eq!(
        info.routing.unwrap().remaining.as_deref(),
        Some("123.456789012345678901")
    );
    assert!(info.remaining.is_none());
    assert!(info.reset_at.is_none() && info.total.is_none() && info.used.is_none());
    assert!(info.note.unwrap().contains("100.25"));
    *fixture.mock.balance.lock().await = json!({"daily_points":100});
    assert!(provider.read(&account).await.is_err());
    *fixture.mock.balance.lock().await = json!({"available_points":0,"daily_points":0});
    assert_eq!(provider.read(&account).await.unwrap().remaining, Some(0));
    let refreshed = fakemodelproxy::accounts::quota_refresh::refresh(
        &fixture.state,
        true,
        fakemodelproxy::records::now_seconds(),
    )
    .await
    .unwrap();
    assert_eq!(refreshed.quota.len(), 1);
}

/// 并发 401 只续期一次；新令牌和保留的 refresh_token 一起落库。
#[tokio::test]
async fn concurrent_unauthorized_reads_share_one_refresh() {
    let fixture = Fixture::new().await;
    let account = fixture
        .account(fakemodelproxy::records::now_seconds() + 3600)
        .await;
    fixture.mock.reject_old.store(true, Ordering::SeqCst);
    let provider = fixture.provider(&account);
    let (first, second) = tokio::join!(provider.read(&account), provider.read(&account));
    assert!(first.is_ok() && second.is_ok());
    assert_eq!(fixture.mock.refreshes.load(Ordering::SeqCst), 1);
    let updated = fixture
        .state
        .store
        .get_account(&account.id)
        .await
        .unwrap()
        .unwrap();
    let credential = Credential::parse(&updated.credential).unwrap();
    assert_eq!(credential.access_token, "fresh-access");
    assert_eq!(credential.refresh_token, "fake-refresh");
}

/// 手工更换凭证、停用账号或修改服务发生在刷新过程中，旧响应不能覆盖新记录。
#[tokio::test]
async fn refresh_cas_preserves_manual_changes_and_disabled_accounts() {
    for change in ["credential", "disabled", "backend"] {
        let fixture = Fixture::new().await;
        let account = fixture
            .account(fakemodelproxy::records::now_seconds() + 3600)
            .await;
        *fixture.mock.mode.lock().await = "hold-refresh".into();
        let provider = fixture.provider(&account);
        let mut edited = account.clone();
        let task = tokio::spawn(async move { provider.refresh(&account).await });
        fixture.mock.entered.notified().await;
        if change == "disabled" {
            edited.enabled = false;
        } else if change == "credential" {
            let mut credential = Credential::parse(&edited.credential).unwrap();
            credential.access_token = "manual-access".into();
            edited.credential = credential.serialize().unwrap();
        } else {
            let mut changed = fixture.backend.clone();
            changed.options = r#"{"changed":true}"#.into();
            fixture.state.store.upsert_backend(&changed).await.unwrap();
        }
        fixture.state.store.upsert_account(&edited).await.unwrap();
        fixture.mock.release.notify_one();
        let result = task.await.unwrap();
        assert_eq!(result.is_err(), change != "credential");
        let saved = fixture
            .state
            .store
            .get_account(&edited.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.credential, edited.credential);
        assert_eq!(saved.enabled, edited.enabled);
    }
}

/// 三种协议的流式和非流式均复用原生工具流，不替换系统提示。
#[tokio::test]
async fn all_three_ingress_protocols_keep_tools_text_and_usage() {
    let fixture = Fixture::new().await;
    fixture
        .account(fakemodelproxy::records::now_seconds() + 3600)
        .await;
    let model = format!("{}/test-model", fixture.backend.id);
    for stream in [true, false] {
        let requests = [
            (
                "/v1/chat/completions",
                json!({"model":model,"stream":stream,"max_tokens":32,
                "messages":[{"role":"system","content":"keep system prompt"},{"role":"user","content":"你好"}],
                "tools":[{"type":"function","function":{"name":"echo","parameters":{"type":"object"}}}]}),
            ),
            (
                "/v1/messages",
                json!({"model":model,"stream":stream,"max_tokens":32,
                "system":"keep system prompt","messages":[{"role":"user","content":"你好"}],
                "tools":[{"name":"echo","input_schema":{"type":"object"}}]}),
            ),
            (
                "/v1/responses",
                json!({"model":model,"stream":stream,"max_output_tokens":32,
                "instructions":"keep system prompt","input":"你好",
                "tools":[{"type":"function","name":"echo","parameters":{"type":"object"}}]}),
            ),
        ];
        for (path, request) in requests {
            let (status, body) = fixture.send(path, request).await;
            assert_eq!(status, StatusCode::OK, "{path}: {body}");
            assert!(
                body.contains("你好") && body.contains("call-test") && body.contains("echo"),
                "{body}"
            );
            assert!(body.contains("tool") || body.contains("function"));
            assert!(body.contains("usage"), "{body}");
        }
    }
    assert_eq!(fixture.mock.chats.load(Ordering::SeqCst), 6);
}

/// 部分工具输出后断流不能重新生成或假装完成。
#[tokio::test]
async fn truncated_generation_is_not_replayed() {
    let fixture = Fixture::new().await;
    fixture
        .account(fakemodelproxy::records::now_seconds() + 3600)
        .await;
    *fixture.mock.mode.lock().await = "truncated".into();
    let (status,body) = fixture.send("/v1/chat/completions",
        json!({"model":format!("{}/test-model",fixture.backend.id),"stream":true,"max_tokens":32,
            "messages":[{"role":"system","content":"keep system prompt"},{"role":"user","content":"hi"}],
            "tools":[{"type":"function","function":{"name":"echo","parameters":{"type":"object"}}}]})).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("error"), "{body}");
    assert_eq!(fixture.mock.chats.load(Ordering::SeqCst), 1);
}

/// 有效期较长的业务令牌需要滚动续期；未知到期时间不伪造 JWT。
#[test]
fn long_lived_tokens_refresh_before_their_rolling_window_closes() {
    let now = fakemodelproxy::records::now_seconds();
    let mut credential = Credential {
        access_token: "fake".into(),
        refresh_token: "refresh".into(),
        obtained_at: now - 6 * 86400,
        expires_at: now + 24 * 86400,
    };
    assert!(credential.needs_refresh());
    credential.obtained_at = now - 86400;
    assert!(!credential.needs_refresh());
    credential.expires_at = now + 60;
    assert!(credential.needs_refresh());
    assert!(Credential::parse(r#"{"access_token":"cookie-only"}"#).is_err());
}
