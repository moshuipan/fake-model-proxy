//! 模型目录同步回归：显示名持久化，调用始终使用稳定模型 ID。

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use fakemodelproxy::accounts::Account;
use fakemodelproxy::backend::Registry;
use fakemodelproxy::config::Config;
use fakemodelproxy::state::AppState;
use fakemodelproxy::store::{BackendRecord, Store};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tower::ServiceExt;

/// 假上游目录可在测试中改名，读取次数用来识别页面是否重复访问上游。
struct Mock {
    name: Mutex<String>,
    price_factor: Mutex<Value>,
    original_price_factor: Mutex<Value>,
    token_limits: Mutex<Value>,
    reads: AtomicUsize,
    requested_models: Mutex<Vec<String>>,
    bodies: Mutex<Vec<Value>>,
}

/// 返回带显示名的目录，禁用项不得进入白名单。
async fn catalog(State(mock): State<Arc<Mock>>) -> axum::Json<Value> {
    mock.reads.fetch_add(1, Ordering::SeqCst);
    let mut model = json!({"key":"qmodel_latest","display_name":*mock.name.lock().unwrap(),
        "price_factor":*mock.price_factor.lock().unwrap(),
        "original_price_factor":*mock.original_price_factor.lock().unwrap(),"enable":true});
    if let Some(limits) = mock.token_limits.lock().unwrap().as_object() {
        model.as_object_mut().unwrap().extend(limits.clone());
    }
    axum::Json(json!({"assistant":[
        model,
        {"key":"raw_model","enable":true},
        {"key":"disabled_model","display_name":"Disabled","enable":false},
    ]}))
}

/// 解码实际 Qoder 请求，记录路由 ID 并返回一次成功响应。
async fn chat(State(mock): State<Arc<Mock>>, headers: HeaderMap, body: String) -> Response {
    mock.requested_models
        .lock()
        .unwrap()
        .push(headers["x-model-key"].to_str().unwrap().into());
    let decoded = fakemodelproxy::backend::qoder::cosy::decode_body(&body).unwrap();
    let decoded: Value = serde_json::from_slice(&decoded).unwrap();
    assert_eq!(decoded["model_config"]["key"], "qmodel_latest");
    mock.bodies.lock().unwrap().push(decoded);
    let frame = json!({"statusCodeValue":200,"body":json!({
        "choices":[{"delta":{"content":"OK"},"finish_reason":"stop"}],
    }).to_string()});
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from(format!("data: {frame}\n\n")))
        .unwrap()
}

/// 持有本地 SQLite 与假上游，所有测试凭证均为假数据。
struct Fixture {
    state: AppState,
    mock: Arc<Mock>,
}

impl Fixture {
    /// 将 Qoder 目录和聊天端点指向本地上游。
    async fn new() -> Self {
        let mock = Arc::new(Mock {
            name: Mutex::new("Qwen4-Max".into()),
            price_factor: Mutex::new(json!(0.2)),
            original_price_factor: Mutex::new(Value::Null),
            token_limits: Mutex::new(Value::Null),
            reads: AtomicUsize::new(0),
            requested_models: Mutex::new(Vec::new()),
            bodies: Mutex::new(Vec::new()),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/catalog", get(catalog))
            .route("/chat", post(chat))
            .with_state(mock.clone());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let store = Store::open(std::path::Path::new(":memory:")).await.unwrap();
        store
            .upsert_backend(&BackendRecord {
                id: "qoder".into(),
                name: "Qoder".into(),
                kind: "qoder".into(),
                base_url: base.clone(),
                models: "old_model".into(),
                priority: 17,
                cost_tier: "subscription".into(),
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
                credential: json!({"uid":"fake","device_token":"fake-token",
                "model_list_url":format!("{base}/catalog"),"endpoint":format!("{base}/chat")})
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

    /// 通过完整管理或协议路由请求 JSON。
    async fn send(&self, method: &str, path: &str, payload: Value) -> (StatusCode, Value) {
        let response = fakemodelproxy::build_router(self.state.clone())
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&body).unwrap())
    }
}

/// 含图请求必须把视觉标志及完整图片传给 Qoder，纯文本请求维持关闭标志。
#[tokio::test]
async fn qoder_image_requests_set_visual_mode_and_keep_all_images() {
    let fixture = Fixture::new().await;
    *fixture.mock.token_limits.lock().unwrap() = json!({"is_vl":true});
    fixture
        .send("POST", "/api/backends/qoder/models/sync", Value::Null)
        .await;
    for content in [
        json!([{"type":"image_url","image_url":{"url":"https://example.test/one.png"}},
            {"type":"text","text":"看这两张图"},
            {"type":"image_url","image_url":{"url":"data:image/png;base64,xxx"}}]),
        json!("hi"),
    ] {
        let image = content.is_array();
        let (status, body) = fixture
            .send(
                "POST",
                "/v1/chat/completions",
                json!({
                    "model":"qoder/qmodel_latest","messages":[{"role":"user","content":content}]
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let bodies = fixture.mock.bodies.lock().unwrap();
        let upstream = bodies.last().unwrap();
        assert_eq!(upstream["model_config"]["is_vl"], image);
        assert_eq!(
            upstream["chat_context"]["extra"]["modelConfig"]["is_vl"],
            image
        );
        if image {
            assert_eq!(
                upstream["messages"][0]["contents"][0]["image_url"]["url"],
                "https://example.test/one.png"
            );
            assert_eq!(
                upstream["messages"][0]["contents"][2]["image_url"]["url"],
                "data:image/png;base64,xxx"
            );
        }
    }
}

/// 同步后保存真实名称，重新装配页面仍可读取，不需要再次访问上游。
#[tokio::test]
async fn synced_names_survive_state_reload_and_keep_route_id() {
    let fixture = Fixture::new().await;
    let (status, _) = fixture
        .send("POST", "/api/backends/qoder/models/sync", Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK);
    fixture.state.reload_backends().await.unwrap();
    let (status, backends) = fixture.send("GET", "/api/backends", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let backend = &backends["backends"][0];
    assert_eq!(backend["models"], "qmodel_latest,raw_model");
    assert_eq!(backend["model_catalog"][0]["display_name"], "Qwen4-Max");
    assert_eq!(backend["model_catalog"][0]["id"], "qmodel_latest");
    assert_eq!(backend["model_catalog"][0]["price_factor"], 0.2);
    assert_eq!(backend["priority"], 17);
    assert_eq!(backend["options"], "{\"region\":\"cn\"}");
    assert!(backend["model_catalog"][1].get("display_name").is_none());
    assert!(backend["model_catalog"][1].get("price_factor").is_none());
    assert_eq!(fixture.mock.reads.load(Ordering::SeqCst), 1);
    let (status, _) = fixture
        .send(
            "POST",
            "/v1/chat/completions",
            json!({
                "model":"qoder/qmodel_latest","messages":[{"role":"user","content":"hi"}],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        *fixture.mock.requested_models.lock().unwrap(),
        vec!["qmodel_latest"]
    );
    assert!(fixture.state.registry.resolve("qoder/Qwen4-Max").is_err());
}

/// 上游对同一模型 ID 改名时，同步替换显示名而不改变调用地址。
#[tokio::test]
async fn resync_replaces_old_display_name() {
    let fixture = Fixture::new().await;
    fixture
        .send("POST", "/api/backends/qoder/models/sync", Value::Null)
        .await;
    *fixture.mock.name.lock().unwrap() = "Qwen4-Max-Updated".into();
    fixture
        .send("POST", "/api/backends/qoder/models/sync", Value::Null)
        .await;
    let (_, backends) = fixture.send("GET", "/api/backends", Value::Null).await;
    assert_eq!(
        backends["backends"][0]["model_catalog"][0]["display_name"],
        "Qwen4-Max-Updated"
    );
    assert_eq!(
        backends["backends"][0]["model_catalog"][0]["id"],
        "qmodel_latest"
    );
    assert_eq!(fixture.mock.reads.load(Ordering::SeqCst), 2);
}

/// 倍率允许上游数字或数字字符串，重同步更新数值并保留确认的零倍率。
#[tokio::test]
async fn pricing_sync_preserves_fractional_string_and_zero_factors() {
    let fixture = Fixture::new().await;
    for (raw, expected) in [(json!(0.2), 0.2), (json!("0.05"), 0.05), (json!(0), 0.0)] {
        *fixture.mock.price_factor.lock().unwrap() = raw;
        let (status, _) = fixture
            .send("POST", "/api/backends/qoder/models/sync", Value::Null)
            .await;
        assert_eq!(status, StatusCode::OK);
        fixture.state.reload_backends().await.unwrap();
        let (_, backends) = fixture.send("GET", "/api/backends", Value::Null).await;
        assert_eq!(
            backends["backends"][0]["model_catalog"][0]["price_factor"].as_f64(),
            Some(expected)
        );
        assert_eq!(
            backends["backends"][0]["model_catalog"][0]["id"],
            "qmodel_latest"
        );
    }
}

/// 非法、缺失及非有限倍率为未知，不能落成零倍率。
#[tokio::test]
async fn invalid_pricing_is_unknown_instead_of_free() {
    let fixture = Fixture::new().await;
    for raw in [
        Value::Null,
        json!(-0.2),
        json!("NaN"),
        json!("Infinity"),
        json!("unknown"),
    ] {
        *fixture.mock.price_factor.lock().unwrap() = raw;
        let (status, _) = fixture
            .send("POST", "/api/backends/qoder/models/sync", Value::Null)
            .await;
        assert_eq!(status, StatusCode::OK);
        let (_, backends) = fixture.send("GET", "/api/backends", Value::Null).await;
        assert!(
            backends["backends"][0]["model_catalog"][0]
                .get("price_factor")
                .is_none()
        );
    }
}

/// 汇总模型 API 时同时保留上游倍率与完整路由地址。
#[tokio::test]
async fn models_api_keeps_price_factor() {
    let fixture = Fixture::new().await;
    fixture
        .send("POST", "/api/backends/qoder/models/sync", Value::Null)
        .await;
    let (status, models) = fixture.send("GET", "/v1/models", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let model = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["id"] == "qoder-qwen4-max")
        .unwrap();
    assert_eq!(model["price_factor"], 0.2);
}

/// 同步和重载保留窗口档位、输入与输出；协议 API 同时暴露新字段及旧默认窗口。
#[tokio::test]
async fn token_limits_survive_reload_and_are_exposed_by_models_api() {
    let fixture = Fixture::new().await;
    *fixture.mock.token_limits.lock().unwrap() = json!({
        "context_config":{
            "large":{"token_count":"400000","is_default":false},
            "standard":{"token_count":200000,"is_default":true}
        },
        "max_input_tokens":"180000",
        "max_output_tokens":8192
    });
    let (status, _) = fixture
        .send("POST", "/api/backends/qoder/models/sync", Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK);
    fixture.state.reload_backends().await.unwrap();
    let (_, backends) = fixture.send("GET", "/api/backends", Value::Null).await;
    let expected = json!({
        "context_window":400000,
        "max_input_tokens":180000,
        "max_output_tokens":8192,
        "context_tiers":[
            {"tokens":200000,"is_default":true},
            {"tokens":400000,"is_default":false}
        ]
    });
    assert_eq!(
        backends["backends"][0]["model_catalog"][0]["token_limits"],
        expected
    );
    let (_, models) = fixture.send("GET", "/v1/models", Value::Null).await;
    let model = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["id"] == "qoder-qwen4-max")
        .unwrap();
    assert_eq!(model["token_limits"], expected);
    assert_eq!(model["capabilities"]["context_window"], 200000);
    // 上游撤回限制后，重同步不能继续把陈旧数值当成当前能力。
    *fixture.mock.token_limits.lock().unwrap() = Value::Null;
    fixture
        .send("POST", "/api/backends/qoder/models/sync", Value::Null)
        .await;
    let (_, backends) = fixture.send("GET", "/api/backends", Value::Null).await;
    assert_eq!(
        backends["backends"][0]["model_catalog"][0]["token_limits"],
        json!({})
    );
}

/// 优惠前倍率按上游字段同步，优惠字段移除后不能保留陈旧原价。
#[tokio::test]
async fn discount_factors_refresh_together() {
    let fixture = Fixture::new().await;
    *fixture.mock.price_factor.lock().unwrap() = json!(0.1);
    *fixture.mock.original_price_factor.lock().unwrap() = json!(0.5);
    fixture
        .send("POST", "/api/backends/qoder/models/sync", Value::Null)
        .await;
    let (_, backends) = fixture.send("GET", "/api/backends", Value::Null).await;
    assert_eq!(
        backends["backends"][0]["model_catalog"][0]["price_factor"],
        0.1
    );
    assert_eq!(
        backends["backends"][0]["model_catalog"][0]["original_price_factor"],
        0.5
    );
    *fixture.mock.original_price_factor.lock().unwrap() = Value::Null;
    fixture
        .send("POST", "/api/backends/qoder/models/sync", Value::Null)
        .await;
    let (_, backends) = fixture.send("GET", "/api/backends", Value::Null).await;
    assert!(
        backends["backends"][0]["model_catalog"][0]
            .get("original_price_factor")
            .is_none()
    );
}

/// 删除服务同时删除其目录，避免相同 ID 重建后出现旧服务名称。
#[tokio::test]
async fn deleting_backend_removes_catalog() {
    let fixture = Fixture::new().await;
    fixture
        .send("POST", "/api/backends/qoder/models/sync", Value::Null)
        .await;
    let (status, _) = fixture
        .send("DELETE", "/api/backends/qoder", Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        fixture
            .state
            .store
            .list_model_catalogs()
            .await
            .unwrap()
            .is_empty()
    );
}

/// 同步已删除服务不能创建孤立缓存或覆盖其他服务配置。
#[tokio::test]
async fn catalog_sync_rejects_deleted_backend() {
    let fixture = Fixture::new().await;
    fixture.state.store.delete_backend("qoder").await.unwrap();
    let models = vec![fakemodelproxy::core::ModelInfo {
        id: "qmodel_latest".into(),
        display_name: Some("Qwen4-Max".into()),
        price_factor: None,
        original_price_factor: None,
        price_valid_until: None,
        capabilities: Default::default(),
        token_limits: Default::default(),
    }];
    assert!(
        fixture
            .state
            .store
            .sync_model_catalog("qoder", &models)
            .await
            .is_err()
    );
    assert!(
        fixture
            .state
            .store
            .list_model_catalogs()
            .await
            .unwrap()
            .is_empty()
    );
}
