//! MonkeyCode 官方协议的离线集成验证：认证、签名、目录、三种出站和账号管理。

use axum::{
    Json, Router,
    body::Body,
    extract::{Query, State},
    http::{HeaderMap, Request, StatusCode},
    routing::{get, post},
};
use fakemodelproxy::{
    backend::Registry,
    config::Config,
    state::AppState,
    store::{BackendRecord, Store},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, RwLock,
        atomic::{AtomicUsize, Ordering},
    },
};
use tower::ServiceExt;

#[derive(Default)]
struct Mock {
    calls: AtomicUsize,
    broken: AtomicUsize,
}

/// 模型目录使用配置 UUID 和真实模型名，第二页补充 Responses 模型。
async fn models(headers: HeaderMap, Query(query): Query<HashMap<String, String>>) -> Json<Value> {
    assert_eq!(headers["cookie"], "session=test-session");
    assert!(!headers.contains_key("authorization"));
    let items = if query.get("cursor").map(String::as_str) == Some("page-two") {
        vec![
            json!({"id":"config-r","model":"monkeycode-basic/responses","interface_type":"openai_responses","context_limit":200000,"output_limit":32000,"support_image":true}),
        ]
    } else {
        vec![
            json!({"id":"config-c","model":"monkeycode-basic/chat","remark":"Chat Test","interface_type":"openai_chat","context_limit":128000,"output_limit":8192}),
            json!({"id":"config-m","model":"monkeycode-basic/messages","interface_type":"anthropic","context_limit":200000,"output_limit":16384}),
            json!({"model":"monkeycode-pro/locked","interface_type":"anthropic"}),
            json!({"model":"MonkeyCode-Pro/locked","interface_type":"anthropic"}),
            json!({"model":"monkeycode-basic","interface_type":"anthropic"}),
            json!({"model":"MonkeyCode-Basic","interface_type":"anthropic"}),
            json!({"model":"hidden","interface_type":"anthropic","is_hidden":true}),
            json!({"model":"alien","interface_type":"unknown"}),
        ]
    };
    Json(
        json!({"code":0,"data":{"models":items,"page":{"has_next_page":!query.contains_key("cursor"),"cursor":"page-two"}}}),
    )
}

/// 按路径返回三个原生 SSE 协议，签名使用 Python hmac 预先计算的独立向量。
async fn generate(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (HeaderMap, String) {
    assert_eq!(headers["x-api-key"], "test-api-key");
    assert_eq!(headers["authorization"], "Bearer test-api-key");
    assert!(!headers.contains_key("cookie"));
    assert_eq!(body["stream"], true);
    let model = body["model"].as_str().unwrap();
    let mut events = Vec::new();
    if model.ends_with("/chat") {
        let expected = match body["messages"][0]["content"].as_str().unwrap() {
            "system one" => "v1=a2b5ac42d6b43dc7dd6cfcf659f8ca91531a2d5cbcdc07ad66ef1585d93a19f0",
            "system one\nsystem two" => {
                "v1=aac9a99a310d504ad22087806b4e9ed0b517993d454e57b4cac2df066ec15825"
            }
            _ => panic!("unexpected system prompt"),
        };
        assert_eq!(headers["X-OhMyAgent-Signature"], expected);
        assert_eq!(body["tools"][0]["function"]["name"], "lookup");
        events.push(
            json!({"choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":null}]}),
        );
        events.push(json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"lookup","arguments":"{\"x\":1}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":12,"completion_tokens":7,"total_tokens":19}}));
    } else if model.ends_with("/messages") {
        assert_eq!(
            headers["X-OhMyAgent-Signature"],
            "v1=aac9a99a310d504ad22087806b4e9ed0b517993d454e57b4cac2df066ec15825"
        );
        assert_eq!(body["system"], "system one\nsystem two");
        assert_eq!(body["tools"][0]["name"], "lookup");
        events.extend([
            json!({"type":"message_start","message":{"usage":{"input_tokens":12,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call-1","name":"lookup","input":{}}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"x\":1}"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}),
            json!({"type":"message_stop"}),
        ]);
    } else {
        assert_eq!(
            headers["X-OhMyAgent-Signature"],
            "v1=aac9a99a310d504ad22087806b4e9ed0b517993d454e57b4cac2df066ec15825"
        );
        assert_eq!(body["instructions"], "system one\nsystem two");
        assert_eq!(body["tools"][0]["name"], "lookup");
        assert_eq!(body["max_output_tokens"], 100);
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        let first = json!({"type":"function_call","id":"fc-1","call_id":"call-1","name":"lookup","arguments":"{\"x\":1}"});
        let second = json!({"type":"function_call","id":"fc-2","call_id":"call-2","name":"lookup","arguments":"{\"x\":2}"});
        events.extend([
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg-1"}}),
            json!({"type":"response.output_text.delta","output_index":0,"delta":"hello"}),
            json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc-1","call_id":"call-1","name":"lookup","arguments":""}}),
            json!({"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"fc-2","call_id":"call-2","name":"lookup","arguments":""}}),
            json!({"type":"response.function_call_arguments.delta","output_index":2,"delta":"{\"x\":"}),
            json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"x\":1}"}),
            json!({"type":"response.function_call_arguments.delta","output_index":2,"delta":"2}"}),
            json!({"type":"response.output_item.done","output_index":1,"item":first}),
            json!({"type":"response.output_item.done","output_index":2,"item":second}),
        ]);
        if mock.broken.load(Ordering::Relaxed) == 0 {
            events.push(json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"hello"}]},first,second],"usage":{"input_tokens":12,"output_tokens":7,"total_tokens":19}}}));
        }
    }
    mock.calls.fetch_add(1, Ordering::Relaxed);
    let mut headers = HeaderMap::new();
    headers.insert("content-type", "text/event-stream".parse().unwrap());
    (
        headers,
        events
            .iter()
            .map(|event| format!("data: {event}\r\n\r\n"))
            .collect(),
    )
}

/// 启动真实 HTTP 假上游，再通过 FMP 的公开管理和推理路由验证。
async fn setup() -> (AppState, Arc<Mock>, tokio::task::JoinHandle<()>) {
    let mock = Arc::new(Mock::default());
    let upstream = Router::new()
        .route("/api/v1/users/models", get(models))
        .route("/api/v1/users/subscription", get(|| async { Json(json!({"code":0,"data":{"plan":"basic"}})) }))
        .route("/api/v1/users/wallet", get(|| async { Json(json!({"code":0,"data":{"balance":"181.19999988","daily_token_balance":700,"daily_token_limit":1000}})) }))
        .route("/v1/chat/completions", post(generate))
        .route("/v1/messages", post(generate))
        .route("/v1/responses", post(generate))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let job = tokio::spawn(async move {
        axum::serve(listener, upstream).await.unwrap();
    });
    let store = Store::open(std::path::Path::new(":memory:")).await.unwrap();
    store
        .upsert_backend(&BackendRecord {
            id: "mc".into(),
            name: "MonkeyCode".into(),
            kind: "monkeycode".into(),
            base_url: format!("{base}/v1"),
            ..Default::default()
        })
        .await
        .unwrap();
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
    (state, mock, job)
}

/// 公开 API 请求并收集状态与 JSON 或 SSE。
async fn send(state: &AppState, path: &str, body: Option<Value>) -> (StatusCode, String) {
    let request = Request::builder()
        .uri(path)
        .method(if body.is_some() { "POST" } else { "GET" })
        .header("Content-Type", "application/json")
        .body(Body::from(
            body.map(|body| body.to_string()).unwrap_or_default(),
        ))
        .unwrap();
    let response = fakemodelproxy::build_router(state.clone())
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status();
    let text = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    (status, text)
}

/// 通过通用账号管理接口保存离线凭证，不依赖已移除的客户端导入接口。
async fn save_account(state: &AppState) -> String {
    let (status, text) = send(
        state,
        "/api/accounts",
        Some(json!({
            "id":"mc-test","backend_id":"mc","name":"离线账号",
            "credential":json!({"api_key":"test-api-key","signing_secret":"test-signing-secret",
                "cookie":"session=test-session"}).to_string(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    serde_json::from_str::<Value>(&text).unwrap()["id"]
        .as_str()
        .unwrap()
        .into()
}

/// 三种出站协议均支持三种非流式入站，工具参数和文本不重复。
#[tokio::test]
async fn three_upstream_protocols_support_all_ingress_formats() {
    let (state, mock, job) = setup().await;
    let id = save_account(&state).await;
    assert_eq!(save_account(&state).await, id);
    assert_eq!(state.store.list_accounts().await.unwrap().len(), 1);
    let (status, catalog) = send(&state, "/api/backends/mc/models/sync", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{catalog}");
    let catalog: Value = serde_json::from_str(&catalog).unwrap();
    assert_eq!(catalog["models"].as_array().unwrap().len(), 3);
    for model in ["chat", "messages", "responses"] {
        for path in ["/v1/chat/completions", "/v1/messages", "/v1/responses"] {
            let system = json!([{"role":"system","content":"system one"},{"role":"system","content":"system two"},{"role":"user","content":"hello"}]);
            let body = if path == "/v1/messages" {
                json!({"model":format!("mc/monkeycode-basic/{model}"),"system":"system one\nsystem two","messages":[{"role":"user","content":"hello"}],"max_tokens":100,"tools":[{"name":"lookup","input_schema":{"type":"object"}}]})
            } else if path == "/v1/responses" {
                json!({"model":format!("mc/monkeycode-basic/{model}"),"input":system,"max_output_tokens":100,"tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}}]})
            } else {
                json!({"model":format!("mc/monkeycode-basic/{model}"),"messages":system,"max_tokens":100,"tools":[{"type":"function","function":{"name":"lookup","parameters":{"type":"object"}}}]})
            };
            let (status, text) = send(&state, path, Some(body)).await;
            assert_eq!(status, StatusCode::OK, "{model} {path}: {text}");
            let result: Value = serde_json::from_str(&text).unwrap();
            assert!(
                text.contains("hello") && text.contains("lookup") && text.contains("call-1"),
                "{text}"
            );
            if path == "/v1/chat/completions" && model == "responses" {
                assert_eq!(result["choices"][0]["message"]["content"], "hello");
                assert_eq!(
                    result["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
                    "{\"x\":1}"
                );
                assert_eq!(
                    result["choices"][0]["message"]["tool_calls"][1]["function"]["arguments"],
                    "{\"x\":2}"
                );
            }
        }
    }
    assert_eq!(mock.calls.load(Ordering::Relaxed), 9);
    job.abort();
}

/// 钱包积分保持小数，未知每日计费规则不把独立 Token 余额加到积分。
#[tokio::test]
async fn quota_precision_and_secret_redaction_survive_management_api() {
    let (state, _, job) = setup().await;
    save_account(&state).await;
    let (status, quota) = send(&state, "/api/quota", None).await;
    assert_eq!(status, StatusCode::OK, "{quota}");
    let quota: Value = serde_json::from_str(&quota).unwrap();
    assert_eq!(quota["quota"][0]["routing"]["remaining"], "181.19999988");
    assert!(quota["quota"][0]["remaining"].is_null());
    assert!(
        quota["quota"][0]["note"]
            .as_str()
            .unwrap()
            .contains("每日 Token 剩余 700")
    );
    for path in ["/api/accounts", "/api/config/export"] {
        let (_, text) = send(&state, path, None).await;
        for secret in ["test-api-key", "test-signing-secret", "test-session"] {
            assert!(!text.contains(secret));
        }
    }
    job.abort();
}

/// Responses 缺少完成事件必须报截断错误，不能当作生成成功。
#[tokio::test]
async fn truncated_responses_stream_is_rejected() {
    let (state, mock, job) = setup().await;
    save_account(&state).await;
    mock.broken.store(1, Ordering::Relaxed);
    let (status, text) = send(&state, "/v1/chat/completions", Some(json!({
        "model":"mc/monkeycode-basic/responses","messages":[{"role":"system","content":"system one"},{"role":"system","content":"system two"},{"role":"user","content":"hello"}],
        "max_tokens":100,"tools":[{"type":"function","function":{"name":"lookup","parameters":{"type":"object"}}}]
    }))).await;
    assert!(!status.is_success(), "{text}");
    assert!(text.contains("截断"), "{text}");
    job.abort();
}

/// 通用账号接口拒绝无效凭证，已删除的客户端导入接口不能再保存账号。
#[tokio::test]
async fn invalid_credentials_and_removed_client_import_do_not_persist_accounts() {
    let (state, _, job) = setup().await;
    for key in [
        json!({"api_key":"test-api-key"}),
        json!({"api_key":"test-api-key","signing_secret":"test-signing-secret","server":"https://other.example.com"}),
    ] {
        let (status, _) = send(
            &state,
            "/api/accounts",
            Some(json!({"backend_id":"mc","name":"无效账号","credential":key.to_string()})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    let (status, _) = send(
        &state,
        "/api/monkeycode/import-client",
        Some(json!({"backend_id":"mc","key":{"api_key":"test-api-key","signing_secret":"test-signing-secret"},
            "cookies":[{"name":"session","value":"test-session","domain":"127.0.0.1","path":"/","host_only":true}]})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(state.store.list_accounts().await.unwrap().is_empty());
    job.abort();
}
