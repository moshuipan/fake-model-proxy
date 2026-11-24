//! 端到端协议测试：用可控的假上游验证工具往返与流式编码。
//!
//! 这些用例不访问任何真实厂商，也不需要凭证。

use axum::Router;
use axum::body::Body;
use axum::extract::Json;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use axum::routing::post;
use fakemodelproxy::backend::Registry;
use fakemodelproxy::config::Config;
use fakemodelproxy::state::AppState;
use fakemodelproxy::store::{BackendRecord, Store};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

/// 启动假上游并返回 base_url。上游根据消息里是否有工具结果决定回复内容。
async fn spawn_mock_upstream() -> String {
    let app = Router::new()
        .route("/v1/chat/completions", post(mock_chat))
        .route(
            "/v1/models",
            axum::routing::get(|| async { Json(json!({"data": [{"id": "mock-model"}]})) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock upstream");
    let address = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{address}/v1")
}

/// 假上游：首轮返回工具调用，后续返回文本。
async fn mock_chat(Json(body): Json<Value>) -> Response {
    let has_tool_result = body
        .get("messages")
        .and_then(Value::as_array)
        .map(|messages| {
            messages
                .iter()
                .any(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
        })
        .unwrap_or(false);
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);

    if streaming {
        let frames = if has_tool_result {
            vec![
                json!({"choices":[{"index":0,"delta":{"content":"tool result received"},"finish_reason":null}]}),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
                json!({"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":3,"total_tokens":14}}),
            ]
        } else {
            vec![
                json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"echo","arguments":"{\"text\":\"ping\"}"}}]},"finish_reason":null}]}),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
            ]
        };
        let mut payload = String::new();
        for frame in frames {
            payload.push_str(&format!("data: {frame}\n\n"));
        }
        payload.push_str("data: [DONE]\n\n");
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from(payload))
            .expect("build sse response");
    }

    let body = if has_tool_result {
        json!({
            "choices": [{
                "message": {"role": "assistant", "content": "tool result received"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12}
        })
    } else {
        json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "echo", "arguments": "{\"text\":\"ping\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 8, "completion_tokens": 4, "total_tokens": 12}
        })
    };
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("build json response")
}

/// 构造指向假上游的代理应用与内存数据库。
async fn build_app(base_url: &str) -> (Router, Store) {
    let store = Store::open(std::path::Path::new(":memory:"))
        .await
        .expect("open store");
    store
        .upsert_backend(&BackendRecord {
            id: "mock".into(),
            name: "Mock".into(),
            kind: "openai_compat".into(),
            base_url: base_url.to_string(),
            api_key: "sk-test".into(),
            models: "mock-model".into(),
            enabled: true,
            cost_tier: "free".into(),
            ..BackendRecord::default()
        })
        .await
        .expect("insert backend");
    let registry = Arc::new(Registry::new());
    registry
        .reload(
            &store.list_backends().await.expect("list"),
            &store.list_accounts().await.expect("accounts"),
            &store,
        )
        .expect("reload");
    let config = Arc::new(Config {
        listen: "127.0.0.1:0".into(),
        data_dir: std::path::PathBuf::from("."),
        api_key: String::new(),
        admin_key: String::new(),
        ..Config::default()
    });
    let state = AppState::new(
        config,
        store.clone(),
        registry,
        std::sync::Arc::new(fakemodelproxy::records::RecordLog::default()),
        std::sync::Arc::new(std::sync::RwLock::new(
            fakemodelproxy::tasks::TaskRegistry::new(),
        )),
        std::sync::Arc::new(fakemodelproxy::routing::Router::new()),
        std::sync::Arc::new(fakemodelproxy::usage::UsageLedger::new(store.clone())),
    );
    (fakemodelproxy::build_router(state), store)
}

/// 发送聊天请求并返回状态码与响应体。
async fn send_chat(app: Router, body: Value) -> (StatusCode, String) {
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("build request");
    let response = app.oneshot(request).await.expect("call proxy");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// 首轮流式响应必须产出工具调用与 tool_calls 终止原因。
#[tokio::test]
async fn streaming_tool_call_round_trip() {
    let base_url = spawn_mock_upstream().await;
    let (app, _store) = build_app(&base_url).await;

    let (status, body) = send_chat(
        app,
        json!({
            "model": "mock/mock-model",
            "stream": true,
            "messages": [{"role": "user", "content": "please echo ping"}],
            "tools": [{
                "type": "function",
                "function": {"name": "echo", "description": "echo", "parameters": {"type": "object"}}
            }]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(
        body.contains("tool_calls"),
        "expected tool_calls, got: {body}"
    );
    assert!(
        body.contains("\"name\":\"echo\""),
        "expected tool name, got: {body}"
    );
    assert!(
        body.contains("tool_calls"),
        "expected finish reason, got: {body}"
    );
    assert!(
        body.trim_end().ends_with("[DONE]"),
        "expected DONE marker, got: {body}"
    );
}

/// 带上工具结果后，第二轮应返回文本并正常结束。
#[tokio::test]
async fn streaming_follow_up_after_tool_result() {
    let base_url = spawn_mock_upstream().await;
    let (app, _store) = build_app(&base_url).await;

    let (status, body) = send_chat(
        app,
        json!({
            "model": "mock/mock-model",
            "stream": true,
            "messages": [
                {"role": "user", "content": "please echo ping"},
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "echo", "arguments": "{\"text\":\"ping\"}"}
                }]},
                {"role": "tool", "tool_call_id": "call_1", "content": "ping"}
            ]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(
        body.contains("tool result received"),
        "expected follow-up text, got: {body}"
    );
    assert!(body.contains("\"finish_reason\":\"stop\""), "body: {body}");
}

/// 非流式请求应返回完整 chat.completion 对象。
#[tokio::test]
async fn non_stream_completion_shape() {
    let base_url = spawn_mock_upstream().await;
    let (app, _store) = build_app(&base_url).await;

    let (status, body) = send_chat(
        app,
        json!({
            "model": "mock/mock-model",
            "messages": [{"role": "user", "content": "hi"}]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body: {body}");
    let parsed: Value = serde_json::from_str(&body).expect("json response");
    assert_eq!(parsed["object"], "chat.completion");
    assert_eq!(parsed["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        parsed["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "echo"
    );
    assert_eq!(parsed["usage"]["total_tokens"], 12);
}

/// 未声明的模型必须返回 400 与可读原因，而不是回退到任意后端。
#[tokio::test]
async fn unknown_model_is_rejected() {
    let base_url = spawn_mock_upstream().await;
    let (app, _store) = build_app(&base_url).await;

    let (status, body) = send_chat(
        app,
        json!({
            "model": "mock/missing-model",
            "messages": [{"role": "user", "content": "hi"}]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert!(body.contains("unknown model"), "body: {body}");
}

/// 重启后配置仍可从数据库恢复，注册表可正常重建。
#[tokio::test]
async fn backend_config_survives_registry_reload() {
    let base_url = spawn_mock_upstream().await;
    let (app, store) = build_app(&base_url).await;
    drop(app);

    // 模拟重启：用同一存储重建注册表。
    let registry = Arc::new(Registry::new());
    registry
        .reload(
            &store.list_backends().await.expect("list"),
            &store.list_accounts().await.expect("accounts"),
            &store,
        )
        .expect("reload");
    assert!(registry.get("mock").is_some(), "backend should reload");
    assert!(
        registry.resolve("mock/mock-model").is_ok(),
        "model address should resolve after reload"
    );
}
