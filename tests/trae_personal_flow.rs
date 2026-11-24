//! 个人版假上游覆盖三协议、授权隔离、工具回传和刷新围栏。

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use fakemodelproxy::{
    accounts::{Account, remote::CredentialRefresher},
    backend::{
        Registry,
        trae_personal::{TraePersonalBackend, credential::Credential},
    },
    config::Config,
    state::AppState,
    store::{BackendRecord, Store},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Mutex;
use tower::ServiceExt;

/// 合成令牌只模拟个人 data.id，不访问真实账号。
fn token(subject: &str, generation: usize) -> String {
    format!(
        "e30.{}.test{generation}",
        URL_SAFE_NO_PAD.encode(
            json!({
                "data":{"id":subject},"exp":4_000_000_000u64
            })
            .to_string()
        )
    )
}

/// 合成个人凭证用于模拟授权和持久化。
fn credential(subject: &str) -> Credential {
    Credential::parse(&json!({"Token":token(subject,0),"RefreshToken":"test-refresh"}).to_string())
        .unwrap()
}

#[derive(Default)]
struct Mock {
    mode: Mutex<String>,
    exchanges: AtomicUsize,
    chats: Mutex<Vec<Value>>,
}

/// 固定官方扩展的设备编码与四种版本职责，避免把 GUI 版本替换为原生协议版本。
fn assert_native_headers(headers: &HeaderMap) {
    assert_eq!(headers["x-device-id"], "1234567890123456");
    assert_eq!(headers["x-device-brand"], "TestMac1%2C2");
    assert_eq!(headers["x-device-type"], "mac");
    assert_eq!(headers["x-os-version"], "macOS%2014.0");
    assert_eq!(headers["x-machine-id"], "test-machine");
    assert_eq!(headers["x-ide-version"], "1.107.1");
    assert_eq!(headers["x-ide-version-code"], "20260212");
    assert_eq!(headers["x-app-version"], "default");
    assert_eq!(headers["x-ide-version-type"], "stable");
    assert_eq!(headers.get_all("x-ide-version-type").iter().count(), 1);
}

/// 隐藏模型和自定义占位配置必须在发布时排除。
async fn catalog(headers: HeaderMap) -> Json<Value> {
    assert_native_headers(&headers);
    assert!(
        headers["authorization"]
            .to_str()
            .unwrap()
            .starts_with("Cloud-IDE-JWT ")
    );
    assert_eq!(headers["x-cloudide-token"], headers["x-ide-token"]);
    Json(json!({"config_info_list":[
        {"config_name":"personal-model","config_switch":true,"usage":"chat_completion",
         "extra_config":"{\"native_function_call\":true,\"parallel_tool_calling\":false}",
         "model_detail_list":[{"model_name":"native__dev","prompt_max_tokens":100000,"max_tokens":16000}]},
        {"config_name":"hidden","config_switch":true,"usage":"chat_completion","is_invisible_to_user":true,
         "model_detail_list":[{"model_name":"hidden__dev"}]},
        {"config_name":"title","config_switch":true,"usage":"title","model_detail_list":[{"model_name":"title__dev"}]},
        {"config_name":"custom_model_123","config_switch":true,"usage":"chat_completion",
         "model_detail_list":[{"model_name":"custom__dev"}]}
    ]}))
}

/// 验证原生工具字段、会话和生成事件；可模拟缺少 done 的断流。
async fn chat(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    assert_native_headers(&headers);
    assert_eq!(body["config_name"], "personal-model");
    assert_eq!(body["model_name"], "native__dev");
    assert_eq!(body["raw_chat_function"], "chat_v3");
    assert_eq!(body["stream"], true);
    assert_eq!(body["session_id"], body["conversation_id"]);
    let tool = !body["tools"].is_null()
        && body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|message| message["role"] != "tool");
    if tool {
        assert!(body["tools"][0]["function"]["parameters"].is_string());
        assert!(body["tool_choice"].is_string());
    }
    mock.chats.lock().await.push(body);
    let mut sse = if tool {
        "event: output\r\ndata: {\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function_call\":{\"name\":\"echo\",\"arguments\":\"{\\\"text\\\":\\\"OK\\\"}\"}}]}\r\n\r\n".to_string()
    } else {
        "event: output\ndata: {\"reasoning_content\":\"思考\"}\n\nevent: output\ndata: {\"response\":\"OK\"}\n\n".into()
    };
    sse.push_str("event: token_usage\ndata: {\"prompt_tokens\":34,\"completion_tokens\":25,\"total_tokens\":59}\n\n");
    if mock.mode.lock().await.as_str() != "eof" {
        sse.push_str(if tool {
            "event: done\ndata: {\"finish_reason\":\"tool_calls\"}\n\n"
        } else {
            "event: done\ndata: {\"finish_reason\":\"stop\"}\n\n"
        });
    }
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        sse,
    )
        .into_response()
}

/// 坏响应不能解除一次性刷新围栏。
async fn exchange(State(mock): State<Arc<Mock>>, Json(body): Json<Value>) -> Response {
    mock.exchanges.fetch_add(1, Ordering::SeqCst);
    assert_eq!(body["ClientID"], "ono9krqynydwx5");
    assert_eq!(body["UserID"], "personal-user");
    if mock.mode.lock().await.as_str() == "refresh-unknown" {
        return (StatusCode::OK, "bad-json").into_response();
    }
    Json(json!({"Result":{"Token":token("personal-user",1),"RefreshToken":"next-refresh"}}))
        .into_response()
}

struct Fixture {
    state: AppState,
    backend: BackendRecord,
    mock: Arc<Mock>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    /// 临时服务器不跨用例存活。
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Fixture {
    /// 在本机模拟个人模型与授权服务，数据库仅驻内存。
    async fn new() -> Self {
        let mock = Arc::new(Mock::default());
        let app = Router::new()
            .route("/api/ide/v1/get_detail_param", post(catalog))
            .route("/api/ide/v2/llm_raw_chat", post(chat))
            .route("/cloudide/api/v3/trae/oauth/ExchangeToken", post(exchange))
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let store = Store::open(std::path::Path::new(":memory:")).await.unwrap();
        let backend = BackendRecord {
            id: "personal".into(),
            name: "个人版".into(),
            kind: "trae_personal".into(),
            base_url,
            models: "native__dev".into(),
            options: json!({"client_device":{
                "device_id":"1234567890123456","device_brand":"TestMac1,2",
                "device_type":"mac","os_version":"macOS 14.0","app_version":"3.4.20",
                "ide_version":"1.107.1",
                "machine_id":"test-machine","plugin_version":"2.3.100000","app_type":"stable"
            }})
            .to_string(),
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
            server,
        }
    }
    /// 添加已授权合成个人账号。
    async fn account(&self) -> Account {
        let account = Account {
            id: "account".into(),
            backend_id: self.backend.id.clone(),
            name: "个人账号".into(),
            credential: credential("personal-user").serialize().unwrap(),
            enabled: true,
        };
        self.state
            .config_service()
            .save_account(&account)
            .await
            .unwrap();
        self.state.reload_backends().await.unwrap();
        account
    }
    /// 经过真实入站路由读取完整响应。
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
}

/// 三种入站协议的流式和非流式都经同一原生个人上游。
#[tokio::test]
async fn six_protocol_modes_and_native_tool_result_roundtrip() {
    let fixture = Fixture::new().await;
    fixture.account().await;
    for stream in [false, true] {
        for (path, body) in [
            (
                "/v1/chat/completions",
                json!({"model":"personal/native__dev","messages":[{"role":"user","content":"OK"}],"max_tokens":128,"stream":stream}),
            ),
            (
                "/v1/messages",
                json!({"model":"personal/native__dev","messages":[{"role":"user","content":"OK"}],"max_tokens":128,"stream":stream}),
            ),
            (
                "/v1/responses",
                json!({"model":"personal/native__dev","input":"OK","max_output_tokens":128,"stream":stream}),
            ),
        ] {
            let (status, response) = fixture.send(path, body).await;
            assert_eq!(status, StatusCode::OK, "{response}");
            assert!(response.contains("OK"));
        }
    }
    let tools = json!([{"type":"function","function":{"name":"echo","parameters":{"type":"object","properties":{"text":{"type":"string"}}}}}]);
    let (status,response) = fixture.send("/v1/chat/completions",json!({"model":"personal/native__dev",
        "messages":[{"role":"user","content":"echo"}],"tools":tools,"tool_choice":{"type":"function","function":{"name":"echo"}}})).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let value: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(value["choices"][0]["finish_reason"], "tool_calls");
    let assistant = value["choices"][0]["message"].clone();
    let (status,response) = fixture.send("/v1/chat/completions",json!({"model":"personal/native__dev",
        "messages":[{"role":"user","content":"echo"},assistant,{"role":"tool","tool_call_id":"call_1","content":"OK"}],"tools":tools})).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let chats = fixture.mock.chats.lock().await;
    let last = chats.last().unwrap();
    assert_eq!(
        last["messages"][1]["tool_calls"][0]["function_call"]["name"],
        "echo"
    );
    assert_eq!(last["messages"][2]["role"], "tool");
    assert_eq!(last["messages"][2]["tool_call_id"], "call_1");
}

/// 官网回传的随机标识阻止会话替换，成功登录不向管理端返回凭证。
#[tokio::test]
async fn browser_callback_is_once_only_and_saves_personal_catalog() {
    let fixture = Fixture::new().await;
    let (status, response) = fixture
        .send(
            "/api/oauth/trae_personal/start",
            json!({"backend_id":"personal"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let flow: Value = serde_json::from_str(&response).unwrap();
    let login = reqwest::Url::parse(flow["login_url"].as_str().unwrap()).unwrap();
    assert_eq!(login.host_str(), Some("www.trae.cn"));
    let params: std::collections::HashMap<_, _> = login.query_pairs().collect();
    for (field, expected) in [
        ("device_id", "1234567890123456"),
        ("x_device_id", "1234567890123456"),
        ("machine_id", "test-machine"),
        ("x_machine_id", "test-machine"),
        ("x_device_brand", "TestMac1,2"),
        ("x_device_type", "mac"),
        ("x_os_version", "macOS 14.0"),
        ("x_app_version", "3.4.20"),
        ("plugin_version", "2.3.100000"),
        ("x_app_type", "stable"),
    ] {
        assert_eq!(params[field], expected);
    }
    let mut callback = reqwest::Url::parse(
        &login
            .query_pairs()
            .find(|(key, _)| key == "auth_callback_url")
            .unwrap()
            .1,
    )
    .unwrap();
    assert_eq!(callback.path(), "/authorize");
    let nonce = login
        .query_pairs()
        .find(|(key, _)| key == "login_trace_id")
        .unwrap()
        .1
        .into_owned();
    let jwt = json!({"Token":token("personal-user",0),"RefreshToken":"test-refresh"}).to_string();
    callback
        .query_pairs_mut()
        .append_pair("userJwt", &jwt)
        .append_pair("loginTraceID", &nonce);
    let mut wrong = callback.clone();
    wrong.set_query(None);
    wrong
        .query_pairs_mut()
        .append_pair("userJwt", &jwt)
        .append_pair("loginTraceID", &"b".repeat(64));
    let client = reqwest::Client::new();
    assert_eq!(
        client.get(wrong).send().await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        client.get(callback.clone()).send().await.unwrap().status(),
        StatusCode::OK
    );
    let mut done = false;
    for _ in 0..100 {
        let (_, body) = fixture
            .send(
                "/api/oauth/trae_personal/poll",
                json!({"backend_id":"personal","session_id":flow["session_id"]}),
            )
            .await;
        assert!(!body.contains("test-refresh"));
        let value: Value = serde_json::from_str(&body).unwrap();
        if value["status"] == "done" {
            assert_eq!(value["models_synced"], true);
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(done);
    assert_eq!(fixture.state.store.list_accounts().await.unwrap().len(), 1);
    let catalogs = fixture.state.store.list_model_catalogs().await.unwrap();
    assert_eq!(catalogs["personal"].len(), 1);
    assert_eq!(
        catalogs["personal"][0].token_limits.context_window,
        Some(116000)
    );
    let providers = fixture.state.task_providers();
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0].backend_id(), "personal");
    assert_eq!(
        providers[0].kind(),
        fakemodelproxy::tasks::TaskKind::Checkin
    );
    let duplicate = client.get(callback).send().await;
    assert!(duplicate.is_err() || duplicate.unwrap().status() != StatusCode::OK);
    let (status, _) = fixture
        .send(
            "/api/oauth/trae_personal/poll",
            json!({"backend_id":"other","session_id":flow["session_id"]}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// 取消授权关闭监听，不保存个人账号。
#[tokio::test]
async fn cancel_and_configuration_change_release_authorization() {
    let fixture = Fixture::new().await;
    let mut previous_device = None;
    let mut previous_nonce = None;
    for changed in [false, true] {
        let (_, body) = fixture
            .send(
                "/api/oauth/trae_personal/start",
                json!({"backend_id":"personal"}),
            )
            .await;
        let flow: Value = serde_json::from_str(&body).unwrap();
        let url = reqwest::Url::parse(flow["login_url"].as_str().unwrap()).unwrap();
        let params: std::collections::HashMap<_, _> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        if let Some(device) = &previous_device {
            assert_eq!(&params["device_id"], device);
            assert_ne!(&params["login_trace_id"], previous_nonce.as_ref().unwrap());
        }
        previous_device = Some(params["device_id"].clone());
        previous_nonce = Some(params["login_trace_id"].clone());
        if changed {
            fixture
                .state
                .auth
                .sessions::<fakemodelproxy::admin::trae_personal::Sessions>()
                .cancel_backend("personal")
                .await;
        } else {
            fixture
                .send(
                    "/api/oauth/trae_personal/cancel",
                    json!({"backend_id":"personal","session_id":flow["session_id"]}),
                )
                .await;
        }
        let (status, _) = fixture
            .send(
                "/api/oauth/trae_personal/poll",
                json!({"backend_id":"personal","session_id":flow["session_id"]}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
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

use std::time::Duration;

/// 坏续期响应保留围栏，另一个实例也不能重新提交同一刷新票据。
#[tokio::test]
async fn unknown_refresh_is_fenced_across_instances() {
    let fixture = Fixture::new().await;
    let account = fixture.account().await;
    *fixture.mock.mode.lock().await = "refresh-unknown".into();
    for _ in 0..2 {
        let provider = TraePersonalBackend::new(
            &fixture.backend,
            vec![account.clone()],
            fixture.state.store.clone(),
        )
        .unwrap();
        assert!(
            CredentialRefresher::refresh(&provider, &account)
                .await
                .is_err()
        );
    }
    assert_eq!(fixture.mock.exchanges.load(Ordering::SeqCst), 1);
}

/// 正常刷新保持个人 subject 并写回最新票据。
#[tokio::test]
async fn refresh_rotates_personal_credential() {
    let fixture = Fixture::new().await;
    let account = fixture.account().await;
    let provider = TraePersonalBackend::new(
        &fixture.backend,
        vec![account.clone()],
        fixture.state.store.clone(),
    )
    .unwrap();
    let next = CredentialRefresher::refresh(&provider, &account)
        .await
        .unwrap();
    assert_eq!(
        Credential::parse(&next.credential).unwrap().refresh_token,
        "next-refresh"
    );
    assert_eq!(fixture.mock.exchanges.load(Ordering::SeqCst), 1);
}

/// 缺少个人终态必须传播错误，不能返回成功的正文结果。
#[tokio::test]
async fn truncated_generation_does_not_report_success() {
    let fixture = Fixture::new().await;
    fixture.account().await;
    *fixture.mock.mode.lock().await = "eof".into();
    let (status, response) = fixture
        .send(
            "/v1/chat/completions",
            json!({"model":"personal/native__dev",
        "messages":[{"role":"user","content":"OK"}]}),
        )
        .await;
    assert_ne!(status, StatusCode::OK, "{response}");
}
