//! 国内短信网页登录回归：只访问本地假上游，不发送真实短信。

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode, Uri};
use axum::{Json, Router};
use fakemodelproxy::backend::Registry;
use fakemodelproxy::config::Config;
use fakemodelproxy::state::AppState;
use fakemodelproxy::store::{BackendRecord, Store};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use tokio::sync::{Mutex, Notify};
use tower::ServiceExt;

/// 假上游记录协议字段，可控制配置、业务失败和登录结果延迟。
struct Mock {
    calls: Mutex<Vec<(String, HeaderMap, Value)>>,
    config: RwLock<Value>,
    send_ok: AtomicBool,
    partial_login: AtomicBool,
    block_login: AtomicBool,
    login_started: Notify,
    login_release: Notify,
    login_returned: Notify,
}

/// 官方信封结构由本地模拟返回，令牌只是无效测试文本。
async fn upstream(
    State(mock): State<Arc<Mock>>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    let path = uri.path().to_string();
    mock.calls.lock().await.push((path.clone(), headers, body));
    let data = match path.as_str() {
        "/userapi/v1/agent-send-code-captcha-config" => mock.config.read().unwrap().clone(),
        "/userapi/v1/agent-send-code" => json!({"result":mock.send_ok.load(Ordering::SeqCst)}),
        "/userapi/v1/agent-login" => {
            if mock.block_login.load(Ordering::SeqCst) {
                mock.login_started.notify_one();
                mock.login_release.notified().await;
            }
            if mock.partial_login.load(Ordering::SeqCst) {
                json!({"access_token":"mock-access"})
            } else {
                json!({"access_token":"mock-access","refresh_token":"mock-refresh","user_id":"mock-user"})
            }
        }
        _ => return Json(json!({"code":630000})),
    };
    if path == "/userapi/v1/agent-login" {
        mock.login_returned.notify_one();
    }
    Json(json!({"code":0,"data":data}))
}

struct Fixture {
    state: AppState,
    mock: Arc<Mock>,
    upstream: tokio::task::JoinHandle<()>,
}

impl Fixture {
    /// 国内、默认区域和海外配置均指向隔离的本地假上游。
    async fn new() -> Self {
        let mock = Arc::new(Mock {
            calls: Mutex::new(Vec::new()),
            config: RwLock::new(json!({"enabled":false})),
            send_ok: AtomicBool::new(true),
            partial_login: AtomicBool::new(false),
            block_login: AtomicBool::new(false),
            login_started: Notify::new(),
            login_release: Notify::new(),
            login_returned: Notify::new(),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = Router::new().fallback(upstream).with_state(mock.clone());
        let upstream = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let store = Store::open(std::path::Path::new(":memory:")).await.unwrap();
        for (id, region) in [
            ("cn", Some("cn")),
            ("default", None),
            ("global", Some("global")),
        ] {
            let mut options = json!({"account_base":base});
            if let Some(region) = region {
                options["region"] = json!(region);
            }
            store
                .upsert_backend(&BackendRecord {
                    id: id.into(),
                    name: id.into(),
                    kind: "autoglm".into(),
                    base_url: base.clone(),
                    options: options.to_string(),
                    ..Default::default()
                })
                .await
                .unwrap();
        }
        let state = AppState::new(
            Arc::new(Config {
                admin_key: "admin-test".into(),
                ..Default::default()
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

    /// 调用实际路由，管理身份与短信网页的同源身份分别控制。
    async fn send(
        &self,
        method: &str,
        path: &str,
        value: Value,
        admin: bool,
        origin: Option<&str>,
    ) -> (StatusCode, HeaderMap, String) {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .header("host", "127.0.0.1:8317");
        if admin {
            request = request.header("authorization", "Bearer admin-test");
        }
        if let Some(origin) = origin {
            request = request
                .header("origin", origin)
                .header("sec-fetch-site", "same-origin");
        }
        let response = fakemodelproxy::build_router(self.state.clone())
            .oneshot(request.body(Body::from(value.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
    }

    /// 管理员启动一次短信会话，创建过程不访问上游。
    async fn start(&self, backend: &str) -> Value {
        let (status, _, text) = self
            .send(
                "POST",
                "/api/oauth/autoglm/start",
                json!({"backend_id":backend}),
                true,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        serde_json::from_str(&text).unwrap()
    }

    /// 登录网页使用浏览器同源头，不携带管理密钥。
    async fn browser(&self, flow: &Value, action: &str, body: Value) -> (StatusCode, Value) {
        let path = format!("{}/{action}", flow["login_url"].as_str().unwrap());
        let (status, _, text) = self
            .send("POST", &path, body, false, Some("http://127.0.0.1:8317"))
            .await;
        (status, serde_json::from_str(&text).unwrap())
    }

    /// 加载公开配置并成功发码，为登录测试准备已绑定的手机号。
    async fn prepare(&self, flow: &Value) {
        assert_eq!(
            self.browser(flow, "captcha-config", json!({})).await.0,
            StatusCode::OK
        );
        assert_eq!(
            self.browser(flow, "send-code", json!({"phone":"13800138000"}))
                .await
                .0,
            StatusCode::OK
        );
    }
}

impl Drop for Fixture {
    /// 清理本地监听器，不遗留测试服务。
    fn drop(&mut self) {
        self.upstream.abort();
    }
}

/// 国内与默认区域创建独立网页；页面加载本身不会发短信或创建账号。
#[tokio::test]
async fn domestic_sessions_have_isolated_local_pages_and_no_initial_sms() {
    let fixture = Fixture::new().await;
    let first = fixture.start("cn").await;
    let second = fixture.start("default").await;
    assert_ne!(first["session_id"], second["session_id"]);
    for flow in [&first, &second] {
        assert_eq!(flow["kind"], "sms_browser");
        assert_eq!(flow["automatic_account_save"], true);
        assert_eq!(flow["expires_in"], 600);
        let (status, headers, page) = fixture
            .send(
                "GET",
                flow["login_url"].as_str().unwrap(),
                json!({}),
                false,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["cache-control"], "no-store");
        assert_eq!(headers["referrer-policy"], "no-referrer");
        assert!(
            headers["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("frame-ancestors 'none'")
        );
        assert!(page.contains("登录并保存账号"));
        assert!(!page.contains("adminKey") && !page.contains("localStorage"));
    }
    assert!(fixture.mock.calls.lock().await.is_empty());
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

/// 成功只兑换一次；轮询和重试不回显令牌，重登录保留人工账号名称。
#[tokio::test]
async fn sms_login_saves_once_with_stable_device_and_preserves_name() {
    let fixture = Fixture::new().await;
    let flow = fixture.start("cn").await;
    fixture.prepare(&flow).await;
    let input = json!({"phone":"13800138000","code":"001234"});
    let (status, result) = fixture.browser(&flow, "login", input.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["status"], "done");
    assert_eq!(result["name"], "AutoGLM · 138****8000");
    assert!(!result.to_string().contains("mock-access"));
    assert_eq!(
        fixture.browser(&flow, "login", input.clone()).await.1,
        result
    );
    assert_eq!(fixture.browser(&flow, "status", json!({})).await.1, result);
    let account_id = result["account_id"].as_str().unwrap();
    let mut account = fixture
        .state
        .store
        .get_account(account_id)
        .await
        .unwrap()
        .unwrap();
    let credential: Value = serde_json::from_str(&account.credential).unwrap();
    assert_eq!(credential["region"], "cn");
    let calls = fixture.mock.calls.lock().await;
    assert_eq!(calls.len(), 3);
    for (_, headers, body) in calls.iter() {
        assert_eq!(body["source_id"], "autoclaw");
        assert_eq!(body["device_id"], credential["device_id"]);
        assert_eq!(headers["x-version"], "2.5.3");
        assert_eq!(headers["x-product"], "autoclaw");
        assert!(headers.contains_key("x-auth-sign"));
    }
    assert_eq!(calls[2].2["code"], 1234);
    drop(calls);
    account.name = "我的 GLM 账号".into();
    fixture.state.store.upsert_account(&account).await.unwrap();
    let second = fixture.start("cn").await;
    fixture.prepare(&second).await;
    assert_eq!(
        fixture.browser(&second, "login", input).await.1["name"],
        "我的 GLM 账号"
    );
    assert_eq!(fixture.state.store.list_accounts().await.unwrap().len(), 1);
}

/// 配置未知、缺少回执和上游未确认发码都不能解锁登录；成功后禁止立即重发。
#[tokio::test]
async fn captcha_and_sms_confirmation_are_required_before_login() {
    let fixture = Fixture::new().await;
    let flow = fixture.start("cn").await;
    let phone = json!({"phone":"13800138000"});
    assert_eq!(
        fixture.browser(&flow, "send-code", phone.clone()).await.0,
        StatusCode::BAD_REQUEST
    );
    *fixture.mock.config.write().unwrap() = json!({"enabled":true,"captcha_supplier":"other"});
    assert_eq!(
        fixture.browser(&flow, "captcha-config", json!({})).await.0,
        StatusCode::BAD_GATEWAY
    );
    *fixture.mock.config.write().unwrap() = json!({
        "enabled":true,"captcha_supplier":"aliyun","region":"cn","prefix":"mock","scene_id":"mock-scene"
    });
    assert_eq!(
        fixture.browser(&flow, "captcha-config", json!({})).await.0,
        StatusCode::OK
    );
    assert_eq!(
        fixture.browser(&flow, "send-code", phone).await.0,
        StatusCode::BAD_REQUEST
    );
    fixture.mock.send_ok.store(false, Ordering::SeqCst);
    let proof = json!({"phone":"13800138000","ali_captcha_verify_param":"mock-proof"});
    assert_eq!(
        fixture.browser(&flow, "send-code", proof.clone()).await.0,
        StatusCode::BAD_GATEWAY
    );
    assert_eq!(
        fixture
            .browser(
                &flow,
                "login",
                json!({"phone":"13800138000","code":"123456"})
            )
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    fixture.mock.send_ok.store(true, Ordering::SeqCst);
    assert_eq!(
        fixture.browser(&flow, "send-code", proof.clone()).await.0,
        StatusCode::OK
    );
    assert_eq!(
        fixture.browser(&flow, "send-code", proof).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        fixture
            .browser(
                &flow,
                "login",
                json!({"phone":"13900139000","code":"123456"})
            )
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        fixture
            .browser(&flow, "login", json!({"phone":"13800138000","code":"abc"}))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert!(
        fixture
            .mock
            .calls
            .lock()
            .await
            .iter()
            .all(|(path, _, _)| !path.ends_with("agent-login"))
    );
}

/// 外站、无来源、错误服务及未知会话不能访问短信操作；海外不会静默变成短信登录。
#[tokio::test]
async fn browser_origin_admin_identity_and_backend_boundaries_are_enforced() {
    let fixture = Fixture::new().await;
    assert_eq!(
        fixture
            .send(
                "POST",
                "/api/oauth/autoglm/start",
                json!({"backend_id":"cn"}),
                false,
                None
            )
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        fixture
            .send(
                "POST",
                "/api/oauth/autoglm/start",
                json!({"backend_id":"global"}),
                true,
                None
            )
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let flow = fixture.start("cn").await;
    let path = format!("{}/captcha-config", flow["login_url"].as_str().unwrap());
    for origin in [
        None,
        Some("https://other.example"),
        Some("http://127.0.0.1:9999"),
    ] {
        assert_eq!(
            fixture.send("POST", &path, json!({}), true, origin).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        fixture
            .send(
                "POST",
                "/api/oauth/autoglm/poll",
                json!({"backend_id":"default","session_id":flow["session_id"]}),
                true,
                None,
            )
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        fixture
            .send("GET", "/auth/autoglm/unknown", json!({}), false, None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    for path in ["/api/oauth/autoglm/send-code", "/api/oauth/autoglm/login"] {
        assert_eq!(
            fixture.send("POST", path, json!({}), true, None).await.0,
            StatusCode::NOT_FOUND
        );
    }
    assert!(fixture.mock.calls.lock().await.is_empty());
}

/// 不完整凭证拒收；取消一个窗口不会影响其他会话。
#[tokio::test]
async fn incomplete_login_is_rejected_and_cancellation_is_isolated() {
    let fixture = Fixture::new().await;
    let first = fixture.start("cn").await;
    let second = fixture.start("cn").await;
    fixture.prepare(&first).await;
    fixture.mock.partial_login.store(true, Ordering::SeqCst);
    assert_eq!(
        fixture
            .browser(
                &first,
                "login",
                json!({"phone":"13800138000","code":"123456"})
            )
            .await
            .0,
        StatusCode::BAD_GATEWAY
    );
    assert!(
        fixture
            .state
            .store
            .list_accounts()
            .await
            .unwrap()
            .is_empty()
    );
    fixture.browser(&first, "cancel", json!({})).await;
    assert_eq!(
        fixture
            .browser(&first, "send-code", json!({"phone":"13800138000"}))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        fixture.browser(&second, "status", json!({})).await.1["status"],
        "pending"
    );
    let backend = fixture
        .state
        .store
        .get_backend("cn")
        .await
        .unwrap()
        .unwrap();
    fixture
        .state
        .store
        .upsert_backend(&BackendRecord {
            enabled: false,
            ..backend
        })
        .await
        .unwrap();
    assert_eq!(
        fixture.browser(&second, "status", json!({})).await.0,
        StatusCode::BAD_REQUEST
    );
}

/// 登录交换期间取消或改区域，迟到令牌不能写入；不依赖固定等待时间。
#[tokio::test]
async fn late_login_is_discarded_after_cancel_or_backend_change() {
    for change_backend in [false, true] {
        let fixture = Fixture::new().await;
        let flow = fixture.start("cn").await;
        fixture.prepare(&flow).await;
        fixture.mock.block_login.store(true, Ordering::SeqCst);
        let login = fixture.browser(
            &flow,
            "login",
            json!({"phone":"13800138000","code":"123456"}),
        );
        let change = async {
            fixture.mock.login_started.notified().await;
            if change_backend {
                let mut backend = fixture
                    .state
                    .store
                    .get_backend("cn")
                    .await
                    .unwrap()
                    .unwrap();
                backend.options = json!({"region":"global"}).to_string();
                fixture.state.store.upsert_backend(&backend).await.unwrap();
            } else {
                fixture.browser(&flow, "cancel", json!({})).await;
            }
            fixture.mock.login_release.notify_one();
        };
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(login, change)
        })
        .await
        .unwrap();
        assert_eq!(result.0, StatusCode::BAD_REQUEST);
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

/// 登录完成后等待配置锁时仍能取消，迟到凭证不会保存为账号。
#[tokio::test]
async fn completed_login_waiting_for_configuration_can_be_cancelled() {
    let fixture = Fixture::new().await;
    let flow = fixture.start("cn").await;
    fixture.prepare(&flow).await;
    let service = fixture.state.config_service();
    let guard = service.lock_write().await;
    let login = fixture.browser(
        &flow,
        "login",
        json!({"phone":"13800138000","code":"123456"}),
    );
    tokio::pin!(login);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::select! {
            result = &mut login => panic!("login completed while configuration was locked: {result:?}"),
            _ = fixture.mock.login_returned.notified() => {}
        }
    }).await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut login)
            .await
            .is_err()
    );
    let cancelled = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        fixture.browser(&flow, "cancel", json!({})),
    )
    .await
    .expect("cancellation must not wait for the configuration lock");
    assert_eq!(cancelled.1["status"], "cancelled");
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), &mut login)
        .await
        .expect("cancelled login releases its wait before the config lock is released");
    assert_eq!(result.0, StatusCode::BAD_REQUEST);
    drop(guard);
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

/// 实际配置提交与完成登录并发时不死锁，停用同时更新数据库与运行快照。
#[tokio::test]
async fn disabling_service_publishes_while_completed_login_waits_to_save() {
    let fixture = Fixture::new().await;
    let flow = fixture.start("cn").await;
    fixture.prepare(&flow).await;
    let service = fixture.state.config_service();
    let before = service.current().await.unwrap().desired_revision;
    let mut disabled = fixture
        .state
        .store
        .get_backend("cn")
        .await
        .unwrap()
        .unwrap();
    disabled.enabled = false;

    let credentials = fixture.state.store.lock_credentials().await;
    let edit = service.save_backend(&disabled);
    tokio::pin!(edit);
    assert!(futures_util::poll!(&mut edit).is_pending());
    let login = fixture.browser(
        &flow,
        "login",
        json!({"phone":"13800138000","code":"123456"}),
    );
    tokio::pin!(login);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::select! {
            result = &mut login => panic!("login completed before configuration: {result:?}"),
            _ = fixture.mock.login_returned.notified() => {}
        }
    })
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut login)
            .await
            .is_err()
    );
    drop(credentials);

    let (edit_result, login_result) =
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            tokio::join!(edit, login)
        })
        .await
        .expect("configuration publication and authorization cancellation must complete");
    edit_result.unwrap();
    assert_eq!(login_result.0, StatusCode::BAD_REQUEST);
    assert!(
        !fixture
            .state
            .store
            .get_backend("cn")
            .await
            .unwrap()
            .unwrap()
            .enabled
    );
    assert!(fixture.state.registry.get("cn").is_none());
    assert!(
        fixture
            .state
            .store
            .list_accounts()
            .await
            .unwrap()
            .is_empty()
    );
    let after = service.current().await.unwrap();
    assert_eq!(after.desired_revision, before + 1);
    assert_eq!(after.active_revision, after.desired_revision);
}
