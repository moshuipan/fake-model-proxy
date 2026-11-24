//! AutoGLM 假上游闭环：签名、短信、令牌旋转、三协议工具流及账号级签到。

use axum::Router;
use axum::body::Body;
use axum::extract::{Json, State};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use fakemodelproxy::accounts::Account;
use fakemodelproxy::backend::Registry;
use fakemodelproxy::backend::autoglm::credential::{Credential, Region};
use fakemodelproxy::config::Config;
use fakemodelproxy::state::AppState;
use fakemodelproxy::store::{BackendRecord, Store};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tower::ServiceExt;

const DEVICE: &str = "f176e9b3-6542-46b6-b7cb-d2f0e83140b1";

/// 所有测试令牌均为本地构造，无有效签名。
fn token(marker: &str, user: &str, device: &str, exp: u64) -> String {
    let payload = json!({"source_id":"agent-autoclaw","user_id":user,"device_id":device,"exp":exp,"marker":marker});
    format!(
        "test.{}.test-only",
        URL_SAFE_NO_PAD.encode(payload.to_string())
    )
}

/// 从假令牌读取标记，供上游断言刷新与账号分流。
fn token_claims(headers: &HeaderMap) -> Value {
    let authorization = headers["authorization"].to_str().unwrap();
    assert!(authorization.starts_with("Bearer "));
    assert!(!authorization.contains("Bearer Bearer"));
    assert_eq!(headers["authorization"], headers["x-authorization"]);
    let payload = authorization.split('.').nth(1).unwrap();
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap()
}

/// 验证模型、账号、登录共用的客户端身份头。
fn assert_identity(headers: &HeaderMap) {
    assert_eq!(headers["x-auth-appid"], "100003");
    assert_eq!(headers["x-auth-sign"].len(), 32);
    assert!(
        headers["x-auth-timestamp"]
            .to_str()
            .unwrap()
            .parse::<u64>()
            .is_ok()
    );
    assert_eq!(headers["x-product"], "autoclaw");
    assert_eq!(headers["x-version"], "2.5.3");
    assert_eq!(headers["x-channel"], "official");
    assert_eq!(headers["x-client-type"], "pc");
    assert_eq!(headers["user-agent"], "AutoClaw/2.5.3");
    assert!(!headers.contains_key("x-harness-type"));
}

/// 上游状态只保存假凭证与请求证据。
#[derive(Default)]
struct Mock {
    refreshes: AtomicUsize,
    reject_old: AtomicBool,
    fail_claim: AtomicBool,
    claims: Mutex<Vec<String>>,
    chats: Mutex<Vec<Value>>,
    sms_devices: Mutex<Vec<String>>,
    wallet_missing: AtomicBool,
    wallet_balance: Mutex<Option<Value>>,
    catalog: Mutex<Value>,
    catalog_reads: AtomicUsize,
    catalog_rejection: AtomicUsize,
    chat_rejection: AtomicUsize,
}

/// 假续期接口验证稳定设备字段，返回旋转的刷新令牌。
async fn refresh(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    assert_identity(&headers);
    assert!(!headers.contains_key("authorization"));
    assert_eq!(body["source_id"], "autoclaw");
    assert_eq!(body["device_id"], DEVICE);
    assert!(body["refresh_token"].as_str().unwrap().starts_with("test."));
    mock.refreshes.fetch_add(1, Ordering::SeqCst);
    Json(json!({"code":0,"data":{
        "access_token":token("new","a",DEVICE,fakemodelproxy::records::now_seconds()+3600),
        "refresh_token":token("rotated","a",DEVICE,fakemodelproxy::records::now_seconds()+7200),
        "refresh":true
    }}))
}

/// 假短信接口记录设备 ID，登录接口必须复用它。
async fn send_code(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    assert_identity(&headers);
    assert_eq!(body["source_id"], "autoclaw");
    mock.sms_devices
        .lock()
        .unwrap()
        .push(body["device_id"].as_str().unwrap().into());
    Json(json!({"code":0,"data":{}}))
}

/// 假短信登录要求数字验证码，并给回设备身份相同的假令牌。
async fn login(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    assert_identity(&headers);
    assert_eq!(body["code"].as_u64(), Some(123456));
    let device = body["device_id"].as_str().unwrap();
    assert_eq!(mock.sms_devices.lock().unwrap().last().unwrap(), device);
    Json(
        json!({"code":0,"data":{"access_token":token("sms","sms-user",device,fakemodelproxy::records::now_seconds()+3600),
        "refresh_token":token("sms-refresh","sms-user",device,fakemodelproxy::records::now_seconds()+7200),
        "user_id":"sms-user"}}),
    )
}

/// 假目录包含名称、倍率和重复项，校验登录头并记录上游读取次数。
async fn models(State(mock): State<Arc<Mock>>, headers: HeaderMap) -> Response {
    assert_identity(&headers);
    token_claims(&headers);
    mock.catalog_reads.fetch_add(1, Ordering::SeqCst);
    let rejected = mock.catalog_rejection.load(Ordering::SeqCst);
    if rejected != 0 {
        return Response::builder()
            .status(rejected as u16)
            .body(Body::from("private-token-do-not-echo"))
            .unwrap();
    }
    Json(mock.catalog.lock().unwrap().clone()).into_response()
}

/// 假聊天返回原生工具调用或工具结果确认，覆盖非流式入口聚合。
async fn chat(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    assert_identity(&headers);
    assert_eq!(headers["x-request-model"], "zai_auto");
    assert_eq!(body["model"], "auto");
    assert_eq!(body["stream"], true);
    assert!(body.get("max_tokens").is_none());
    let claims = token_claims(&headers);
    let rejected = mock.chat_rejection.load(Ordering::SeqCst);
    if rejected != 0 {
        return Response::builder()
            .status(rejected as u16)
            .body(Body::from("private-token-do-not-echo"))
            .unwrap();
    }
    if mock.reject_old.load(Ordering::SeqCst) && claims["marker"] == "old" {
        return Response::builder().status(401).body(Body::empty()).unwrap();
    }
    assert!(
        body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("You are AutoClaw.")
    );
    let has_tool = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| message["role"] == "tool");
    mock.chats.lock().unwrap().push(body);
    let delta = if has_tool {
        json!({"content":"工具结果已接收"})
    } else {
        json!({"tool_calls":[{"index":0,"id":"call_echo","type":"function","function":{"name":"echo","arguments":"{\"text\":\"ping\"}"}}]})
    };
    let finish = if has_tool { "stop" } else { "tool_calls" };
    let frames = [
        json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}]}),
        json!({"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":3,"total_tokens":14}}),
    ];
    let payload = frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n";
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from(payload))
        .unwrap()
}

/// 假钱包区分余额零与字段缺失。
async fn wallet(State(mock): State<Arc<Mock>>, headers: HeaderMap) -> Json<Value> {
    assert_identity(&headers);
    token_claims(&headers);
    Json(if mock.wallet_missing.load(Ordering::SeqCst) {
        json!({"code":0,"data":{}})
    } else {
        json!({"code":0,"data":{"total_balance":mock.wallet_balance.lock().unwrap().clone().unwrap_or(json!(0)),"wallets":[]}})
    })
}

/// 假任务目录只允许每日签到，其他任务不进入领取请求。
async fn tasks(headers: HeaderMap) -> Json<Value> {
    assert_identity(&headers);
    token_claims(&headers);
    Json(
        json!({"data":[{"task_id":"daily_signin","client_triggered":true},{"task_id":"invite_friends","client_triggered":false}]}),
    )
}

/// 假领取按访问令牌的用户归因，可模拟结果未知。
async fn claim(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    assert_identity(&headers);
    assert_eq!(body["task_id"], "daily_signin");
    mock.claims
        .lock()
        .unwrap()
        .push(token_claims(&headers)["user_id"].as_str().unwrap().into());
    if mock.fail_claim.load(Ordering::SeqCst) {
        return Response::builder()
            .status(503)
            .body(Body::from("{}"))
            .unwrap();
    }
    Response::builder()
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"data":{"success":true,"reward_points":37}}).to_string(),
        ))
        .unwrap()
}

/// 持有隔离代理及上游证据，测试不会修改用户配置。
struct Fixture {
    state: AppState,
    mock: Arc<Mock>,
}

impl Fixture {
    /// 启动本地假上游，统一注入聊天与账号地址。
    async fn new(with_account: bool, expired: bool) -> Self {
        let mock = Arc::new(Mock {
            catalog: Mutex::new(json!({"data":{"groups":[{"id":"group","models":[
                {"model_id":"zai_auto"},
                {"model":"zai_glm-5","displayName":"GLM-5","credits":"0.2x"},
                {"model_id":"zai_auto","name":"Auto","price_factor":"0.1","original_price_factor":0.5}
            ]}],"request":{"id":"request-id"},"promotions":[{"id":"promotion-id"}]}})),
            ..Default::default()
        });
        let upstream = Router::new()
            .route("/userapi/v1/agent-send-code", post(send_code))
            .route("/userapi/v1/agent-login/", post(login))
            .route("/userapi/v1/refresh", post(refresh))
            .route("/autoclaw-proxy/proxy/autoclaw-model-config", get(models))
            .route(
                "/autoclaw-proxy/proxy/autoclaw/chat/completions",
                post(chat),
            )
            .route("/agent-assetmgr/api/v2/wallets", get(wallet))
            .route("/autoclaw-proxy/proxy/autoclaw-task-list", get(tasks))
            .route("/autoclaw-proxy/proxy/autoclaw-task-complete", post(claim))
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });
        let store = Store::open(std::path::Path::new(":memory:")).await.unwrap();
        store
            .upsert_backend(&BackendRecord {
                id: "glm".into(),
                name: "AutoGLM".into(),
                kind: "autoglm".into(),
                base_url: base.clone(),
                models: "zai_auto".into(),
                options: json!({"region":"cn","account_base":base,"checkin_protocol":"task"})
                    .to_string(),
                cost_tier: "free".into(),
                priority: 30,
                ..Default::default()
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
        let fixture = Self { state, mock };
        if with_account {
            fixture.add_account("a", expired).await;
        }
        fixture.state.reload_backends().await.unwrap();
        fixture
    }

    /// 添加带设备信息的本地假账号。
    async fn add_account(&self, user: &str, expired: bool) {
        let now = fakemodelproxy::records::now_seconds();
        self.state.store.upsert_account(&Account {
            id:user.into(),backend_id:"glm".into(),name:user.into(),enabled:true,
            credential:json!({"accessToken":format!("Bearer {}",token("old",user,DEVICE,if expired { now-1 } else { now+3600 })),
                "refreshToken":token("refresh",user,DEVICE,now+7200)}).to_string(),
        }).await.unwrap();
    }

    /// 经真实管理与协议路由执行请求，读取完整响应。
    async fn send(&self, method: &str, path: &str, body: Value) -> (StatusCode, String) {
        let response = fakemodelproxy::build_router(self.state.clone())
            .oneshot(
                Request::builder()
                    .method(method)
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

/// 无凭证时目录与生成均明确要求登录，不返回虚构目录。
#[tokio::test]
async fn missing_account_requires_login() {
    let fixture = Fixture::new(false, false).await;
    let (status, body) = fixture
        .send("POST", "/v1/chat/completions", chat_request(false))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("gateway_no_eligible_target"));
    assert!(fixture.mock.chats.lock().unwrap().is_empty());
    let (status, body) = fixture
        .send("POST", "/api/backends/glm/models/sync", Value::Null)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("尚无可用账号"));
    assert_eq!(fixture.mock.catalog_reads.load(Ordering::SeqCst), 0);
}

/// 同步持久化官方名称与倍率；本地重载不读上游，调用仍使用完整路由 ID。
#[tokio::test]
async fn catalog_metadata_survives_reload_without_changing_route() {
    let fixture = Fixture::new(true, false).await;
    let (status, _) = fixture
        .send("POST", "/api/backends/glm/models/sync", Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK);
    fixture.state.reload_backends().await.unwrap();
    let (_, body) = fixture.send("GET", "/api/backends", Value::Null).await;
    let backends: Value = serde_json::from_str(&body).unwrap();
    let backend = &backends["backends"][0];
    assert_eq!(backend["models"], "zai_auto,zai_glm-5");
    assert_eq!(backend["model_catalog"][0]["display_name"], "Auto");
    assert_eq!(backend["model_catalog"][0]["price_factor"], 0.1);
    assert_eq!(backend["model_catalog"][0]["original_price_factor"], 0.5);
    assert_eq!(backend["model_catalog"][1]["display_name"], "GLM-5");
    assert_eq!(backend["model_catalog"][1]["price_factor"], 0.2);
    assert_eq!(fixture.mock.catalog_reads.load(Ordering::SeqCst), 1);
    let (status, _) = fixture
        .send("POST", "/v1/chat/completions", chat_request(false))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fixture.mock.chats.lock().unwrap().len(), 1);
    assert!(fixture.state.registry.resolve("glm/Auto").is_err());
    let (status, body) = fixture.send("GET", "/v1/models", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let models: Value = serde_json::from_str(&body).unwrap();
    let model = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["id"] == "autoglm-glm-5")
        .unwrap();
    assert_eq!(model["display_name"], "GLM-5");
    assert_eq!(model["price_factor"], 0.2);
}

/// 重同步更新名称和零倍率，优惠字段移除及无单位金额不能留下旧倍率。
#[tokio::test]
async fn catalog_resync_replaces_prices_and_drops_missing_metadata() {
    let fixture = Fixture::new(true, false).await;
    fixture
        .send("POST", "/api/backends/glm/models/sync", Value::Null)
        .await;
    *fixture.mock.catalog.lock().unwrap() = json!({"models":[
        {"model_id":"zai_auto","name":"自动选择","credits":"0x"},
        {"model":"zai_glm-5","display_name":"GLM-5 更新","price":0.2,"credits":0.2}
    ]});
    let (status, _) = fixture
        .send("POST", "/api/backends/glm/models/sync", Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, body) = fixture.send("GET", "/api/backends", Value::Null).await;
    let backends: Value = serde_json::from_str(&body).unwrap();
    let catalog = &backends["backends"][0]["model_catalog"];
    assert_eq!(catalog[0]["display_name"], "自动选择");
    assert_eq!(catalog[0]["price_factor"], 0.0);
    assert!(catalog[0].get("original_price_factor").is_none());
    assert_eq!(catalog[1]["display_name"], "GLM-5 更新");
    assert!(catalog[1].get("price_factor").is_none());
    assert_eq!(catalog[1]["id"], "zai_glm-5");
}

/// 短信网页启动不发码、不虚构已完成账号；旧直接管理短信接口保持关闭。
#[tokio::test]
async fn browser_sms_start_does_not_send_code_or_fabricate_accounts() {
    let fixture = Fixture::new(false, false).await;
    let (status, body) = fixture
        .send(
            "POST",
            "/api/oauth/autoglm/start",
            json!({"backend_id":"glm"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let result: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["kind"], "sms_browser");
    assert_eq!(result["automatic_account_save"], true);
    assert!(
        result["login_url"]
            .as_str()
            .unwrap()
            .starts_with("/auth/autoglm/")
    );
    assert!(!body.contains("access_token") && !body.contains("refresh_token"));
    assert!(
        fixture
            .state
            .store
            .list_accounts()
            .await
            .unwrap()
            .is_empty()
    );
    for path in ["/api/oauth/autoglm/send-code", "/api/oauth/autoglm/login"] {
        assert_eq!(
            fixture.send("POST", path, json!({})).await.0,
            StatusCode::NOT_FOUND
        );
    }
}

/// 并发请求只续期一次，旋转令牌写回并可在重载后继续使用。
#[tokio::test]
async fn concurrent_expired_requests_share_persistent_refresh() {
    let fixture = Fixture::new(true, true).await;
    let (first, second) = tokio::join!(
        fixture.send("POST", "/v1/chat/completions", chat_request(false)),
        fixture.send("POST", "/v1/chat/completions", chat_request(false))
    );
    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(second.0, StatusCode::OK);
    assert_eq!(fixture.mock.refreshes.load(Ordering::SeqCst), 1);
    let account = fixture.state.store.get_account("a").await.unwrap().unwrap();
    let credential = Credential::parse(&account.credential).unwrap();
    assert!(credential.refresh_token.contains("test."));
    assert_ne!(
        credential.refresh_token,
        json!({"refresh":true}).to_string()
    );
    assert_eq!(credential.user_id, "a");
    assert_eq!(credential.device_id, DEVICE);
    fixture.state.reload_backends().await.unwrap();
    let (status, body) = fixture
        .send("POST", "/api/backends/glm/models/sync", Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("zai_glm-5"));
    assert_eq!(fixture.mock.refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture
            .state
            .store
            .get_backend("glm")
            .await
            .unwrap()
            .unwrap()
            .models,
        "zai_auto,zai_glm-5"
    );
}

/// 尚未到期但被上游拒绝的令牌只续期并重试一次。
#[tokio::test]
async fn unauthorized_response_refreshes_once() {
    let fixture = Fixture::new(true, false).await;
    fixture.mock.reject_old.store(true, Ordering::SeqCst);
    let (status, _) = fixture
        .send("POST", "/v1/chat/completions", chat_request(false))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fixture.mock.refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.mock.chats.lock().unwrap().len(), 1);
}

/// 连通性检查区分 401 与 403；401 最多续期一次，403 不盲目刷新或回显正文。
#[tokio::test]
async fn catalog_auth_rejections_are_bounded_and_report_the_actual_status() {
    for rejected in [401, 403] {
        let fixture = Fixture::new(true, false).await;
        fixture
            .mock
            .catalog_rejection
            .store(rejected, Ordering::SeqCst);
        let (status, body) = fixture
            .send("POST", "/api/backends/glm/test", json!({}))
            .await;
        assert_eq!(status, StatusCode::OK);
        let result: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(result["ok"], false);
        assert_eq!(result["upstream_status"], rejected);
        assert!(
            result["message"]
                .as_str()
                .unwrap()
                .contains(&format!("HTTP {rejected}"))
        );
        assert!(!body.contains("private-token-do-not-echo"));
        assert_eq!(
            fixture.mock.refreshes.load(Ordering::SeqCst),
            usize::from(rejected == 401)
        );
        assert_eq!(
            fixture.mock.catalog_reads.load(Ordering::SeqCst),
            if rejected == 401 { 2 } else { 1 }
        );
    }
}

/// 普通 403 只终止当前生成，不触发续期或让同一账号的后续模型请求失效。
#[tokio::test]
async fn forbidden_generation_does_not_disable_the_account() {
    let fixture = Fixture::new(true, false).await;
    fixture.mock.chat_rejection.store(403, Ordering::SeqCst);
    let (status, body) = fixture
        .send("POST", "/v1/chat/completions", chat_request(false))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.contains("HTTP 403"));
    assert!(!body.contains("private-token-do-not-echo"));
    assert_eq!(fixture.mock.refreshes.load(Ordering::SeqCst), 0);

    fixture.mock.chat_rejection.store(0, Ordering::SeqCst);
    let (status, body) = fixture
        .send("POST", "/v1/chat/completions", chat_request(false))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(fixture.mock.chats.lock().unwrap().len(), 1);
    assert_eq!(fixture.mock.refreshes.load(Ordering::SeqCst), 0);
}

/// 三协议的流式和一次性入口均保留原生工具调用，结果可回传。
#[tokio::test]
async fn all_protocols_preserve_native_tools_and_result() {
    let fixture = Fixture::new(true, false).await;
    for stream in [false, true] {
        let requests = [
            ("/v1/chat/completions", chat_request(stream)),
            (
                "/v1/messages",
                json!({"model":"glm/zai_auto","max_tokens":128,"stream":stream,
                "messages":[{"role":"user","content":"hi"}],
                "tools":[{"name":"echo","description":"echo","input_schema":{"type":"object"}}]}),
            ),
            (
                "/v1/responses",
                json!({"model":"glm/zai_auto","max_output_tokens":128,"stream":stream,
                "input":"hi","tools":[{"type":"function","name":"echo","description":"echo","parameters":{"type":"object"}}]}),
            ),
        ];
        for (path, request) in requests {
            let (status, body) = fixture.send("POST", path, request).await;
            assert_eq!(status, StatusCode::OK, "{path}: {body}");
            assert!(
                body.contains("echo") && body.contains("call_echo"),
                "{path}: {body}"
            );
        }
    }
    let (status,body) = fixture.send("POST","/v1/chat/completions",json!({
        "model":"glm/zai_auto","messages":[{"role":"assistant","tool_calls":[{"id":"call_echo","type":"function","function":{"name":"echo","arguments":"{\"text\":\"ping\"}"}}]},
            {"role":"tool","tool_call_id":"call_echo","content":"ping"}]
    })).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("工具结果已接收"));
    assert!(
        fixture
            .mock
            .chats
            .lock()
            .unwrap()
            .iter()
            .take(6)
            .all(|body| body["tools"][0]["function"]["name"] == "echo")
    );
}

/// 钱包零余额有证据；字段缺失为未知，不伪造总量或已用量。
#[tokio::test]
async fn wallet_zero_and_unknown_are_distinct() {
    let fixture = Fixture::new(true, false).await;
    let (_, body) = fixture.send("GET", "/api/quota", Value::Null).await;
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["quota"][0]["remaining"], 0);
    assert_eq!(value["quota"][0]["confidence"], "verified");
    assert!(value["quota"][0].get("total").is_none() && value["quota"][0].get("used").is_none());
    fixture.mock.wallet_missing.store(true, Ordering::SeqCst);
    let (_, body) = fixture.send("GET", "/api/quota", Value::Null).await;
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["quota"][0]["confidence"], "unknown");
    assert!(value["quota"][0].get("remaining").is_none());
}

/// 小于一积分的原始余额完整保留，超过十八位精度时不伪造余额。
#[tokio::test]
async fn wallet_preserves_exact_decimal_balance_and_native_unit() {
    let fixture = Fixture::new(true, false).await;
    for balance in [
        json!("0.000000000000000001"),
        serde_json::from_str::<Value>("2118.800000120000000001").unwrap(),
    ] {
        let expected = balance
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| balance.to_string());
        *fixture.mock.wallet_balance.lock().unwrap() = Some(balance);
        let (status, body) = fixture.send("GET", "/api/quota", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        let value: Value = serde_json::from_str(&body).unwrap();
        let quota = &value["quota"][0];
        assert_eq!(quota["routing"]["remaining"], expected);
        assert_eq!(quota["routing"]["unit"], "积分");
        assert_eq!(quota["routing"]["complete"], true);
        assert!(quota.get("remaining").is_none());
    }
    *fixture.mock.wallet_balance.lock().unwrap() = Some(json!("1.0000000000000000001"));
    let (_, body) = fixture.send("GET", "/api/quota", Value::Null).await;
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["quota"][0]["confidence"], "unknown");
    assert!(value["quota"][0].get("routing").is_none());
}

/// 每个账号只领取一次，记录上游奖励数，不使用固定签到积分。
#[tokio::test]
async fn checkin_is_per_account_and_period() {
    let fixture = Fixture::new(true, false).await;
    fixture.add_account("b", false).await;
    fixture.state.reload_backends().await.unwrap();
    for _ in 0..2 {
        let (status, body) = fixture
            .send("POST", "/api/tasks/glm/checkin", Value::Null)
            .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"amount\":37"));
    }
    assert_eq!(*fixture.mock.claims.lock().unwrap(), vec!["a", "b"]);
    assert_eq!(fixture.state.store.list_tasks(10).await.unwrap().len(), 2);
}

/// 领取结果未知时保留记录，重复按钮或调度不会再次发送领取。
#[tokio::test]
async fn unknown_checkin_is_not_replayed() {
    let fixture = Fixture::new(true, false).await;
    fixture.mock.fail_claim.store(true, Ordering::SeqCst);
    for _ in 0..2 {
        let (_, body) = fixture
            .send("POST", "/api/tasks/glm/checkin", Value::Null)
            .await;
        assert!(body.contains("\"status\":\"unknown\""));
    }
    assert_eq!(fixture.mock.claims.lock().unwrap().len(), 1);
}

/// 条件持久化不能覆盖人工编辑，也不能复活已删除账号。
#[tokio::test]
async fn credential_write_respects_edit_and_delete() {
    let fixture = Fixture::new(true, false).await;
    let original = fixture.state.store.get_account("a").await.unwrap().unwrap();
    let mut edited = original.clone();
    edited.credential = json!({"access_token":"manual-token"}).to_string();
    fixture.state.store.upsert_account(&edited).await.unwrap();
    assert!(
        !fixture
            .state
            .store
            .replace_credential("a", &original.credential, "refreshed")
            .await
            .unwrap()
    );
    fixture.state.store.delete_account("a").await.unwrap();
    assert!(
        !fixture
            .state
            .store
            .replace_credential("a", &edited.credential, "refreshed")
            .await
            .unwrap()
    );
    assert!(
        fixture
            .state
            .store
            .get_account("a")
            .await
            .unwrap()
            .is_none()
    );
}

/// 国内与海外配置分离，JWT 区域也可在未配置时决定账号域。
#[test]
fn region_selects_current_domestic_and_global_origins() {
    let credential = Credential {
        access_token: "test".into(),
        region: Some(Region::Global),
        ..Default::default()
    };
    let client =
        fakemodelproxy::backend::autoglm::client::Client::new(&BackendRecord::default()).unwrap();
    assert_eq!(
        client
            .request(reqwest::Method::GET, "/test", &credential, true)
            .build()
            .unwrap()
            .url()
            .host_str(),
        Some("autoglm-api.autoglm.ai")
    );
    let credential = Credential {
        region: Some(Region::Cn),
        ..credential
    };
    assert_eq!(
        client
            .request(reqwest::Method::GET, "/test", &credential, true)
            .build()
            .unwrap()
            .url()
            .host_str(),
        Some("autoglm-acceleration-api.zhipuai.cn")
    );
    assert_eq!(
        client
            .request(reqwest::Method::GET, "/test", &credential, false)
            .build()
            .unwrap()
            .url()
            .host_str(),
        Some("autoglm-acceleration-api.zhipuai.cn")
    );
}

/// 构造带 Unicode 系统词与工具定义的最小聊天请求。
fn chat_request(stream: bool) -> Value {
    json!({"model":"glm/zai_auto","stream":stream,"max_tokens":128,
        "messages":[{"role":"system","content":"中文提示：You are Codex. 保留其余内容。"},{"role":"user","content":"hi"}],
        "tools":[{"type":"function","function":{"name":"echo","description":"echo","parameters":{"type":"object"}}}]})
}
