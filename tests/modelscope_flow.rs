//! 魔搭接入回归：组织前缀、令牌保留、匿名目录字段及三种入站协议。

use axum::Router;
use axum::body::Body;
use axum::extract::{Json, Path, State};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use fakemodelproxy::backend::Registry;
use fakemodelproxy::config::Config;
use fakemodelproxy::state::AppState;
use fakemodelproxy::store::Store;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex, RwLock};
use tower::ServiceExt;

const MODEL: &str = "Qwen/Qwen3.5-35B-A3B";
const KEY: &str = "fake-modelscope-access-token";

/// 只记录假上游收到的模型名，确认组织前缀完整保留。
#[derive(Default)]
struct Mock {
    models: Mutex<Vec<String>>,
    details: Mutex<Vec<String>>,
}

/// 模拟已观察到的公开目录格式，不补造价格或窗口信息。
async fn catalog(headers: HeaderMap) -> Json<Value> {
    assert_eq!(headers["authorization"], format!("Bearer {KEY}"));
    Json(json!({"object":"list","data":[
        {"id":MODEL,"object":"","owned_by":"system"},
        {"id":"Qwen/Qwen-Image-Edit","object":"","owned_by":"system"}
    ]}))
}

/// 官方视觉理解任务允许图片，图像编辑任务不能成为聊天候选，公开详情不带鉴权。
async fn detail(
    State(mock): State<Arc<Mock>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Json<Value> {
    assert!(headers.get("authorization").is_none());
    assert!(headers.get("cookie").is_none());
    mock.details.lock().unwrap().push(id.clone());
    Json(json!({"Success":true,"Code":200,"Data":{"Tasks":[
        {"Name":if id == MODEL {"image-text-to-text"} else {"image-to-image"}}
    ]}}))
}

/// 使用假令牌验证鉴权，并分别生成 OpenAI JSON 或 SSE 响应。
async fn chat(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    assert_eq!(headers["authorization"], format!("Bearer {KEY}"));
    mock.models
        .lock()
        .unwrap()
        .push(body["model"].as_str().unwrap().into());
    let (content_type, payload) = if body["stream"] == true {
        (
            "text/event-stream",
            format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"index":0,"delta":{"content":"OK"},"finish_reason":null}]}),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
                    "usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}})
            ),
        )
    } else {
        (
            "application/json",
            json!({"id":"fake-completion","choices":[{"index":0,
                "message":{"role":"assistant","content":"OK"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}})
            .to_string(),
        )
    };
    Response::builder()
        .header("content-type", content_type)
        .body(Body::from(payload))
        .unwrap()
}

/// 构造空配置与假上游，所有凭证和数据都仅存在于内存。
async fn fixture() -> (Router, Arc<Mock>, String) {
    let mock = Arc::new(Mock::default());
    let upstream = Router::new()
        .route("/v1/models", get(catalog))
        .route("/api/v1/models/{*id}", get(detail))
        .route("/v1/chat/completions", post(chat))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, upstream).await.unwrap();
    });
    let store = Store::open(std::path::Path::new(":memory:")).await.unwrap();
    let state = AppState::new(
        Arc::new(Config::default()),
        store.clone(),
        Arc::new(Registry::new()),
        Arc::new(fakemodelproxy::records::RecordLog::default()),
        Arc::new(RwLock::new(fakemodelproxy::tasks::TaskRegistry::new())),
        Arc::new(fakemodelproxy::routing::Router::new()),
        Arc::new(fakemodelproxy::usage::UsageLedger::new(store)),
    );
    (fakemodelproxy::build_router(state), mock, base)
}

/// 通过真实管理和协议路由完成请求，保留原始响应供 SSE 断言。
async fn send(app: &Router, method: &str, path: &str, body: Value) -> (StatusCode, String) {
    let response = app
        .clone()
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

/// 从预设创建、编辑及同步后，三种协议的流式和非流式都保留令牌与组织前缀。
#[tokio::test]
async fn official_preset_catalog_and_all_protocols_preserve_model_and_token() {
    let (app, mock, base) = fixture().await;
    let (status, body) = send(&app, "GET", "/api/presets", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let presets: Value = serde_json::from_str(&body).unwrap();
    let preset = presets["presets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|preset| preset["id"] == "modelscope")
        .unwrap();
    assert_eq!(preset["kind"], "modelscope");
    assert_eq!(preset["base_url"], "https://api-inference.modelscope.cn/v1");
    let mut input = json!({"id":"modelscope","name":"ModelScope","kind":preset["kind"],
        "base_url":base,"api_key":KEY,"models":"","cost_tier":preset["cost_tier"]});
    assert_eq!(
        send(&app, "POST", "/api/backends", input.clone()).await.0,
        StatusCode::OK
    );
    input["api_key"] = json!("");
    input["priority"] = json!(41);
    assert_eq!(
        send(&app, "POST", "/api/backends", input).await.0,
        StatusCode::OK
    );
    let (status, body) = send(
        &app,
        "POST",
        "/api/backends/modelscope/models/sync",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let result: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["models"][0]["id"], MODEL);
    assert_eq!(result["models"][0]["token_limits"], json!({}));
    assert_eq!(result["models"].as_array().unwrap().len(), 1);
    assert_eq!(result["models"][0]["capabilities"]["images"], true);
    assert!(result["models"][0].get("price_factor").is_none());
    let (_, body) = send(&app, "GET", "/api/backends", Value::Null).await;
    assert!(!body.contains(KEY));
    let address = format!("modelscope/{MODEL}");
    for stream in [false, true] {
        for (path, mut body) in [
            (
                "/v1/chat/completions",
                json!({"messages":[{"role":"user","content":"hi"}]}),
            ),
            ("/v1/responses", json!({"input":"hi"})),
            (
                "/v1/messages",
                json!({"messages":[{"role":"user","content":"hi"}],"max_tokens":32}),
            ),
        ] {
            body["model"] = json!(address);
            body["stream"] = json!(stream);
            let (status, response) = send(&app, "POST", path, body).await;
            assert_eq!(status, StatusCode::OK, "{path}: {response}");
            assert!(response.contains("OK"), "{path}: {response}");
            if stream {
                assert!(response.contains("data:"), "{path}: {response}");
            }
        }
    }
    assert_eq!(*mock.models.lock().unwrap(), vec![MODEL; 6]);
    assert_eq!(mock.details.lock().unwrap().len(), 2);
}
