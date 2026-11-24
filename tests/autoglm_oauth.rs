//! 假上游验证官方网页授权完整闭环，不访问厂商或真实账号。

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use fakemodelproxy::backend::Registry;
use fakemodelproxy::backend::autoglm::credential::{Credential, Region};
use fakemodelproxy::config::Config;
use fakemodelproxy::state::AppState;
use fakemodelproxy::store::{BackendRecord, Store};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use tokio::sync::{Mutex, Notify};
use tower::ServiceExt;

/// 保存匿名请求与回调地址，不在测试失败时打印令牌。
#[derive(Default)]
struct Mock {
    requests: Mutex<Vec<Value>>,
    config_devices: Mutex<Vec<String>>,
    exchanges: AtomicUsize,
    mode: Mutex<String>,
    entered: Notify,
    release: Notify,
}

/// 获取授权链接并检查国际客户端身份，保持 state 与设备独立。
async fn url(
    State(mock): State<Arc<Mock>>,
    Path(provider): Path<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    let provider = provider.trim_end_matches("-oauth-url");
    assert_eq!(headers["x-channel"], "official");
    assert_eq!(headers["x-version"], "2.4.0");
    assert!(!headers.contains_key("authorization"));
    assert_eq!(body["source_id"], "autoclaw");
    assert!(uuid::Uuid::parse_str(body["device_id"].as_str().unwrap()).is_ok());
    let callback = reqwest::Url::parse(body["navigate_uri"].as_str().unwrap()).unwrap();
    assert_eq!(callback.host_str(), Some("localhost"));
    assert!([18432, 19654, 19723, 53699].contains(&callback.port().unwrap()));
    assert_eq!(callback.path(), format!("/auth/callback-{provider}"));
    assert_eq!(headers["origin"], callback.origin().ascii_serialization());
    let host = if provider == "google" {
        "accounts.google.com"
    } else {
        "chat.z.ai"
    };
    let state = body["device_id"].as_str().unwrap();
    if mock.mode.lock().await.as_str() == "proof" {
        assert_eq!(body["ali_captcha_verify_param"], "official-sdk-proof");
        assert!(
            mock.config_devices
                .lock()
                .await
                .iter()
                .any(|id| id == state)
        );
    }
    let login_url = match mock.mode.lock().await.as_str() {
        "host" => format!("https://example.com/login?state={state}"),
        "state" => format!("https://{host}/login"),
        "port" => format!("https://{host}:443/login?state={state}"),
        _ => format!("https://{host}/login?state={state}"),
    };
    mock.requests.lock().await.push(body);
    let accepted = mock.mode.lock().await.as_str() != "unaccepted";
    Json(
        json!({"code":0,"captcha_result":accepted,"biz_result":accepted,"data":{"oauth_url":login_url}}),
    )
}

/// 前置配置也使用同一个国际身份，设备 UUID 在后续授权中原样使用。
async fn captcha_config(State(mock): State<Arc<Mock>>, Json(body): Json<Value>) -> Json<Value> {
    assert_eq!(body["source_id"], "autoclaw");
    mock.config_devices
        .lock()
        .await
        .push(body["device_id"].as_str().unwrap().into());
    Json(
        json!({"code":0,"data":{"enabled":true,"captcha_supplier":"aliyun","region":"ga","prefix":"public-prefix","scene_id":"public-scene"}}),
    )
}

/// 一次性兑换，校验与第一步一致的设备和回调地址，并可模拟迟到结果。
async fn exchange(State(mock): State<Arc<Mock>>, Json(body): Json<Value>) -> Json<Value> {
    mock.exchanges.fetch_add(1, Ordering::SeqCst);
    let requests = mock.requests.lock().await;
    let previous = requests
        .iter()
        .find(|request| request["device_id"] == body["device_id"])
        .unwrap();
    assert_eq!(body["source_id"], "autoclaw");
    assert_eq!(body["navigate_uri"], previous["navigate_uri"]);
    assert_eq!(body["state"], body["device_id"]);
    drop(requests);
    let mode = mock.mode.lock().await.clone();
    if mode == "hold" {
        mock.entered.notify_one();
        mock.release.notified().await;
    }
    Json(json!({"code":0,"data":{
        "access_token":"fake-access",
        "refresh_token":if mode == "incomplete" { "" } else { "fake-refresh" },
        "user_id":body["device_id"]
    }}))
}

/// 同一真实路由通过管理密钥授权，假上游只监听本机。
struct Fixture {
    state: AppState,
    mock: Arc<Mock>,
    upstream: tokio::task::JoinHandle<()>,
}

impl Fixture {
    /// 构建可并行授权的两个后端，共享账号存储而不共享授权槽。
    async fn new() -> Self {
        let mock = Arc::new(Mock::default());
        let app = Router::new()
            .route("/userapi/overseasv1/{provider}", post(url))
            .route(
                "/userapi/overseasv1/oauth-captcha-config",
                post(captcha_config),
            )
            .route("/userapi/overseasv1/zai-oauth-login", post(exchange))
            .route("/userapi/overseasv1/google-oauth-login", post(exchange))
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let upstream = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let store = Store::open(std::path::Path::new(":memory:")).await.unwrap();
        for id in ["glm", "other"] {
            store
                .upsert_backend(&BackendRecord {
                    id: id.into(),
                    name: id.into(),
                    kind: "autoglm".into(),
                    base_url: base.clone(),
                    options: json!({"region":"global","account_base":base}).to_string(),
                    ..Default::default()
                })
                .await
                .unwrap();
        }
        let state = AppState::new(
            Arc::new(Config {
                listen: "127.0.0.1:0".into(),
                data_dir: ".".into(),
                api_key: String::new(),
                admin_key: "admin-test".into(),
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
        Self {
            state,
            mock,
            upstream,
        }
    }

    /// 经完整管理 API 发送请求，不绕过鉴权或会话绑定。
    async fn send(&self, action: &str, body: Value) -> (StatusCode, Value) {
        let response = fakemodelproxy::build_router(self.state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/oauth/autoglm/{action}"))
                    .header("content-type", "application/json")
                    .header("x-admin-key", "admin-test")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    /// 开始流程并读取其唯一回调地址，后续不访问真实官方登录域。
    async fn start(&self, backend: &str, provider: &str) -> (Value, String, String) {
        let (status, result) = self
            .send("start", json!({"backend_id":backend,"provider":provider}))
            .await;
        assert_eq!(status, StatusCode::OK);
        let login = reqwest::Url::parse(result["login_url"].as_str().unwrap()).unwrap();
        let state = login
            .query_pairs()
            .find(|(key, _)| key == "state")
            .unwrap()
            .1
            .to_string();
        let requests = self.mock.requests.lock().await;
        let callback = requests
            .iter()
            .find(|request| request["device_id"] == state)
            .unwrap()["navigate_uri"]
            .as_str()
            .unwrap()
            .to_string();
        (result, callback, state)
    }

    /// 在有明确终止条件的本地测试中等待后台落盘，超时即失败。
    async fn done(&self, backend: &str, id: &Value) -> Value {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let (status, result) = self
                    .send("poll", json!({"backend_id":backend,"session_id":id}))
                    .await;
                if result["status"] != "pending" || status != StatusCode::OK {
                    return result;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("本地授权流程未结束")
    }
}

impl Drop for Fixture {
    /// 结束假上游，取消测试服务任务，不影响真实服务。
    fn drop(&mut self) {
        self.upstream.abort();
    }
}

/// 模拟浏览器 GET 回调，禁止 HTTP 客户端跟随其他地址。
async fn callback(url: &str, state: &str, code: &str) -> StatusCode {
    let mut url = reqwest::Url::parse(url).unwrap();
    url.query_pairs_mut()
        .append_pair("state", state)
        .append_pair("code", code);
    reqwest::Client::new()
        .get(url)
        .send()
        .await
        .unwrap()
        .status()
}

/// 验证授权闭环、多窗口隔离、state 拒收、幂等轮询、取消和服务变更。
#[tokio::test]
async fn browser_oauth_isolated_one_shot_and_persistent() {
    let fixture = Fixture::new().await;
    // 错误来源、缺失 state 和显式端口均拒绝，失败流程随即释放端口。
    for mode in ["host", "state", "port", "unaccepted"] {
        *fixture.mock.mode.lock().await = mode.into();
        let (status, _) = fixture
            .send("start", json!({"backend_id":"glm","provider":"zai"}))
            .await;
        assert_ne!(status, StatusCode::OK);
    }
    *fixture.mock.mode.lock().await = String::new();
    let (first, first_url, first_state) = fixture.start("glm", "zai").await;
    let (second, second_url, second_state) = fixture.start("glm", "google").await;
    let (third, third_url, third_state) = fixture.start("other", "zai").await;
    assert_ne!(first["session_id"], second["session_id"]);
    assert_ne!(first_url, second_url);
    assert_eq!(
        callback(&first_url, "wrong-state", "code").await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(fixture.mock.exchanges.load(Ordering::SeqCst), 0);
    let (status, _) = fixture
        .send(
            "poll",
            json!({"backend_id":"other","session_id":first["session_id"]}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    for (url, state) in [
        (&first_url, &first_state),
        (&second_url, &second_state),
        (&third_url, &third_state),
    ] {
        assert_eq!(callback(url, state, "code").await, StatusCode::OK);
    }
    for (backend, flow) in [("glm", &first), ("glm", &second), ("other", &third)] {
        let done = fixture.done(backend, &flow["session_id"]).await;
        assert_eq!(done["status"], "done");
        let again = fixture.done(backend, &flow["session_id"]).await;
        assert_eq!(done, again);
        assert!(!done.to_string().contains("fake-access"));
        assert!(!done.to_string().contains("fake-refresh"));
    }
    assert_eq!(fixture.mock.exchanges.load(Ordering::SeqCst), 3);
    let accounts = fixture.state.store.list_accounts().await.unwrap();
    assert_eq!(accounts.len(), 3);
    assert_eq!(
        accounts
            .iter()
            .filter(|account| account.backend_id == "glm")
            .count(),
        2
    );
    for account in &accounts {
        let credential = Credential::parse(&account.credential).unwrap();
        assert!(credential.region == Some(Region::Global));
        assert!(uuid::Uuid::parse_str(&credential.device_id).is_ok());
    }
    for account in accounts {
        fixture
            .state
            .store
            .delete_account(&account.id)
            .await
            .unwrap();
    }

    // 拒绝授权没有令牌兑换，完整状态结束后可再开一个流程。
    let (denied, url, state) = fixture.start("glm", "zai").await;
    let mut url = reqwest::Url::parse(&url).unwrap();
    url.query_pairs_mut()
        .append_pair("state", &state)
        .append_pair("error", "access_denied");
    let response = reqwest::Client::new().get(url).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(fixture.done("glm", &denied["session_id"]).await["error"].is_object());
    assert_eq!(fixture.mock.exchanges.load(Ordering::SeqCst), 3);

    // 不完整的成功信封不能保存不可续期的账号。
    *fixture.mock.mode.lock().await = "incomplete".into();
    let (incomplete, url, state) = fixture.start("glm", "zai").await;
    assert_eq!(callback(&url, &state, "code").await, StatusCode::OK);
    assert!(fixture.done("glm", &incomplete["session_id"]).await["error"].is_object());
    assert!(
        fixture
            .state
            .store
            .list_accounts()
            .await
            .unwrap()
            .is_empty()
    );

    // 兑换已经发出时取消，迟到结果不得保存账号。
    *fixture.mock.mode.lock().await = "hold".into();
    let (held, url, state) = fixture.start("glm", "zai").await;
    assert_eq!(callback(&url, &state, "code").await, StatusCode::OK);
    fixture.mock.entered.notified().await;
    assert_eq!(
        callback(&url, &state, "duplicate").await,
        StatusCode::CONFLICT
    );
    let (status, _) = fixture
        .send(
            "cancel",
            json!({"backend_id":"glm","session_id":held["session_id"]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    fixture.mock.release.notify_one();
    assert!(fixture.done("glm", &held["session_id"]).await["error"].is_object());
    assert!(
        fixture
            .state
            .store
            .list_accounts()
            .await
            .unwrap()
            .is_empty()
    );
    *fixture.mock.mode.lock().await = String::new();

    // 服务停用或删除发生在授权过程中，后端条件写回拒绝创建账号。
    for delete in [false, true] {
        let (flow, url, state) = fixture.start("glm", "zai").await;
        if delete {
            fixture.state.store.delete_backend("glm").await.unwrap();
        } else {
            let mut backend = fixture
                .state
                .store
                .get_backend("glm")
                .await
                .unwrap()
                .unwrap();
            backend.enabled = false;
            fixture.state.store.upsert_backend(&backend).await.unwrap();
        }
        assert_eq!(callback(&url, &state, "code").await, StatusCode::OK);
        assert!(fixture.done("glm", &flow["session_id"]).await["error"].is_object());
        assert!(
            fixture
                .state
                .store
                .list_accounts()
                .await
                .unwrap()
                .is_empty()
        );
        if !delete {
            let mut backend = fixture
                .state
                .store
                .get_backend("glm")
                .await
                .unwrap()
                .unwrap();
            backend.enabled = true;
            fixture.state.store.upsert_backend(&backend).await.unwrap();
        }
    }

    // 官方前置配置中的设备与 SDK 回执贯穿授权和保存，不退回手工复制令牌。
    *fixture.mock.mode.lock().await = "proof".into();
    let (status, config) = fixture
        .send("captcha-config", json!({"backend_id":"other"}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(config["config"]["enabled"], true);
    let (status, flow) = fixture
        .send(
            "start",
            json!({"backend_id":"other","provider":"zai",
        "device_id":config["device_id"],"ali_captcha_verify_param":"official-sdk-proof"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let requests = fixture.mock.requests.lock().await;
    let body = requests.last().unwrap();
    let url = body["navigate_uri"].as_str().unwrap().to_string();
    let state = body["device_id"].as_str().unwrap().to_string();
    drop(requests);
    assert_eq!(callback(&url, &state, "code").await, StatusCode::OK);
    assert_eq!(
        fixture.done("other", &flow["session_id"]).await["status"],
        "done"
    );
}
