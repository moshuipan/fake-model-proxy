//! 多模态完整链路回归：目录、FMP Auto、后端 Auto 与三种入站协议。

use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Json, State},
    http::{Request, StatusCode},
    response::Response,
    routing::{get, post},
};
use fakemodelproxy::{
    accounts::Account,
    backend::Registry,
    config::Config,
    records::RecordLog,
    state::AppState,
    store::{BackendRecord, Store},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex, RwLock};
use tower::ServiceExt;

const IMAGE: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAAB";

/// 保存实际送到上游的请求，验证选模及图片传输。
#[derive(Default)]
struct Mock {
    bodies: Mutex<Vec<Value>>,
    qoder_bodies: Mutex<Vec<Value>>,
}

/// 使用明确的输入模态和窗口声明区分文本、视觉及后端原生 Auto。
async fn catalog() -> Json<Value> {
    Json(json!({"data":[
        {"id":"text","input_modalities":["text"],"capabilities":{"tools":true},"context_window":200000,"max_output_tokens":32000},
        {"id":"vision","input_modalities":["text","image"],"capabilities":{"tools":true},"context_window":200000,"max_output_tokens":32000},
        {"id":"auto","capabilities":{"images":true,"tools":true},"context_window":200000,"max_output_tokens":32000}
    ]}))
}

/// 返回 OpenAI JSON 或 SSE，图片模型选择错误时立即使测试失败。
async fn chat(State(mock): State<Arc<Mock>>, Json(body): Json<Value>) -> Response {
    if image_count(&body) > 0 {
        assert_ne!(body["model"], "text", "图片不可路由到纯文本模型");
    }
    mock.bodies.lock().unwrap().push(body.clone());
    let (content_type, payload) = if body["stream"] == true {
        (
            "text/event-stream",
            format!(
                "data: {}\n\ndata: [DONE]\n\n",
                json!({
                    "choices":[{"index":0,"delta":{"content":"OK"},"finish_reason":"stop"}]
                })
            ),
        )
    } else {
        ("application/json", json!({
            "id":"mock","choices":[{"index":0,"message":{"role":"assistant","content":"OK"},"finish_reason":"stop"}]
        }).to_string())
    };
    Response::builder()
        .header("content-type", content_type)
        .body(Body::from(payload))
        .unwrap()
}

/// 解码真实 Qoder 适配器的封包，只由本地假账号请求，不连接官方服务。
async fn qoder_chat(State(mock): State<Arc<Mock>>, body: Bytes) -> Response {
    let encoded = std::str::from_utf8(&body).unwrap();
    let decoded = fakemodelproxy::backend::qoder::cosy::decode_body(encoded).unwrap();
    let body: Value = serde_json::from_slice(&decoded).unwrap();
    mock.qoder_bodies.lock().unwrap().push(body);
    let envelope = json!({"statusCodeValue":200,"body":json!({
        "choices":[{"delta":{"content":"OK"},"finish_reason":"stop"}]
    }).to_string()});
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from(format!("data: {envelope}\n\n")))
        .unwrap()
}

/// 统计已编码的图片片段，不把 data URL 当文本输入。
fn image_count(body: &Value) -> usize {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .filter(|part| part["type"] == "image_url")
        .count()
}

/// 通过真实协议和管理路由调用应用，保留 JSON 或 SSE 原文。
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
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

/// 构造只有本机假上游的独立应用，同步模型后生成只读取本地目录。
async fn fixture() -> (Router, Arc<Mock>, Arc<RecordLog>) {
    fixture_with_qoder(false).await
}

/// 可选增加图片能力已开启的 Qoder，验证私有传输约束不能被配置覆盖。
async fn fixture_with_qoder(with_qoder: bool) -> (Router, Arc<Mock>, Arc<RecordLog>) {
    let mock = Arc::new(Mock::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let upstream = Router::new()
        .route("/v1/models", get(catalog))
        .route("/v1/chat/completions", post(chat))
        .route("/qoder/chat", post(qoder_chat))
        .with_state(mock.clone());
    tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let store = Store::open(std::path::Path::new(":memory:")).await.unwrap();
    store
        .upsert_backend(&BackendRecord {
            id: "local".into(),
            name: "Local".into(),
            kind: "openai_compat".into(),
            base_url: base.clone(),
            cost_tier: "free".into(),
            options: r#"{"routing":{"model_priority":{"text":1,"auto":2,"vision":3}}}"#.into(),
            ..Default::default()
        })
        .await
        .unwrap();
    if with_qoder {
        store
            .upsert_backend(&BackendRecord {
                id: "qoder".into(),
                name: "Qoder".into(),
                kind: "qoder".into(),
                models: "auto".into(),
                cost_tier: "free".into(),
                priority: 1,
                capabilities: r#"{"images":true,"tools":true,"context_window":200000}"#.into(),
                ..Default::default()
            })
            .await
            .unwrap();
        store
            .upsert_account(&Account {
                id: "fake-qoder".into(),
                name: "Fake Qoder".into(),
                backend_id: "qoder".into(),
                enabled: true,
                credential: json!({
                    "uid":"fake-user","device_token":"fake-token",
                    "endpoint":format!("{}/qoder/chat", base.trim_end_matches("/v1")),
                })
                .to_string(),
            })
            .await
            .unwrap();
    }
    let log = Arc::new(RecordLog::default());
    let state = AppState::new(
        Arc::new(Config::default()),
        store.clone(),
        Arc::new(Registry::new()),
        log.clone(),
        Arc::new(RwLock::new(fakemodelproxy::tasks::TaskRegistry::new())),
        Arc::new(fakemodelproxy::routing::Router::new()),
        Arc::new(fakemodelproxy::usage::UsageLedger::new(store)),
    );
    state.reload_backends().await.unwrap();
    let app = fakemodelproxy::build_router(state);
    let (status, body) = send(&app, "POST", "/api/backends/local/models/sync", Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let models: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(models["models"][0]["capabilities"]["images"], false);
    assert_eq!(models["models"][1]["capabilities"]["images"], true);
    assert_eq!(models["models"][2]["capabilities"]["images"], true);
    (app, mock, log)
}

/// 按协议构造文本、图文或仅图片消息，含图场景包含两张图片。
fn payload(protocol: &str, model: &str, kind: &str, stream: bool) -> Value {
    let mut parts = Vec::new();
    if kind != "images" {
        parts.push(
            json!({"type":if protocol == "responses" {"input_text"} else {"text"},"text":"hi"}),
        );
    }
    if kind != "text" {
        for _ in 0..2 {
            parts.push(match protocol {
                "responses" => json!({"type":"input_image","image_url":IMAGE}),
                "messages" => json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":IMAGE.split_once(',').unwrap().1}}),
                _ => json!({"type":"image_url","image_url":{"url":IMAGE}}),
            });
        }
    }
    let message = json!({"role":"user","content":parts});
    match protocol {
        "responses" => {
            json!({"model":model,"input":[message],"max_output_tokens":32,"stream":stream})
        }
        _ => json!({"model":model,"messages":[message],"max_tokens":32,"stream":stream}),
    }
}

/// 三种协议均保留多图，Auto 按输入筛选且能选择支持图片的后端原生 Auto。
#[tokio::test]
async fn all_protocols_route_images_and_text_with_backend_auto() {
    let (app, mock, _) = fixture().await;
    for stream in [false, true] {
        for (protocol, path) in [
            ("chat", "/v1/chat/completions"),
            ("responses", "/v1/responses"),
            ("messages", "/v1/messages"),
        ] {
            for kind in ["text", "mixed", "images"] {
                let (status, body) =
                    send(&app, "POST", path, payload(protocol, "auto", kind, stream)).await;
                assert_eq!(status, StatusCode::OK, "{protocol}/{kind}/{stream}: {body}");
                assert!(body.contains("OK"));
                let bodies = mock.bodies.lock().unwrap();
                let upstream = bodies.last().unwrap();
                assert_eq!(image_count(upstream), if kind == "text" { 0 } else { 2 });
                assert_eq!(
                    upstream["model"],
                    if kind == "text" { "text" } else { "auto" }
                );
            }
            let (status, body) = send(
                &app,
                "POST",
                path,
                payload(protocol, "local/auto", "images", stream),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let bodies = mock.bodies.lock().unwrap();
            assert_eq!(bodies.last().unwrap()["model"], "auto");
            assert_eq!(image_count(bodies.last().unwrap()), 2);
        }
    }
    assert_eq!(mock.bodies.lock().unwrap().len(), 24);
}

/// 用完整工具调用历史模拟游戏截图；假图片只验证传输与预算，不调用真实视觉模型。
fn tool_image_history(protocol: &str, rounds: usize, stream: bool) -> Value {
    let image = format!("data:image/png;base64,{}", "A".repeat(32 * 1024));
    let mut history = vec![json!({"role":"user","content":"complete the game"})];
    for index in 0..rounds {
        let id = format!("screenshot_{index}");
        match protocol {
            "responses" => {
                history.push(json!({
                    "type":"function_call","call_id":id,"name":"screenshot","arguments":"{}",
                }));
                history.push(json!({
                    "type":"function_call_output","call_id":id,
                    "output":[
                        {"type":"input_text","text":format!("screen {index}")},
                        {"type":"input_image","image_url":image},
                    ],
                }));
            }
            "messages" => {
                history.push(json!({"role":"assistant","content":[{
                    "type":"tool_use","id":id,"name":"screenshot","input":{},
                }]}));
                history.push(json!({"role":"user","content":[{
                    "type":"tool_result","tool_use_id":id,"content":[
                        {"type":"text","text":format!("screen {index}")},
                        {"type":"image","source":{"type":"base64","media_type":"image/png",
                            "data":image.split_once(',').unwrap().1}},
                    ],
                }]}));
            }
            _ => {
                history.push(json!({"role":"assistant","content":null,"tool_calls":[{
                    "type":"function","id":id,"function":{"name":"screenshot","arguments":"{}"},
                }]}));
                history.push(json!({"role":"tool","tool_call_id":id,"content":[
                    {"type":"text","text":format!("screen {index}")},
                    {"type":"image_url","image_url":{"url":image}},
                ]}));
            }
        }
    }
    history.push(json!({"role":"user","content":"continue"}));
    match protocol {
        "responses" => json!({
            "model":"auto","input":history,"stream":stream,"max_output_tokens":32,
            "tools":[{"type":"function","name":"screenshot","parameters":{"type":"object"}}],
        }),
        "messages" => json!({
            "model":"auto","messages":history,"stream":stream,"max_tokens":32,
            "tools":[{"name":"screenshot","input_schema":{"type":"object"}}],
        }),
        _ => json!({
            "model":"auto","messages":history,"stream":stream,"max_tokens":32,
            "tools":[{"type":"function","function":{
                "name":"screenshot","parameters":{"type":"object"},
            }}],
        }),
    }
}

/// 工具截图的 base64 不能被当作文本 Token，低于体积上限的多轮历史应正常准入。
#[tokio::test]
async fn responses_tool_result_images_keep_multimodal_budget() {
    let (app, mock, log) = fixture().await;
    for stream in [false, true] {
        for rounds in [8, 16] {
            let payload = tool_image_history("responses", rounds, stream);
            let bytes = payload.to_string().len();
            assert!(bytes < 1024 * 1024);
            let (status, body) = send(&app, "POST", "/clients/trae/v1/responses", payload).await;
            assert_eq!(status, StatusCode::OK, "{rounds} screenshots: {body}");
            let bodies = mock.bodies.lock().unwrap();
            let upstream = bodies.last().unwrap();
            assert_eq!(upstream["model"], "auto");
            assert_eq!(image_count(upstream), rounds);
            let record = &log.list(1)[0];
            assert_eq!(record.request_body_bytes, Some(bytes));
            assert_eq!(record.request_body_complete, Some(true));
            assert_eq!(record.image_count, Some(rounds));
            assert_eq!(record.message_count, Some(rounds * 2 + 2));
            assert_eq!(record.image_url_bytes, Some(rounds * (32 * 1024 + 22)));
            assert!(record.input_chars < 1024);
        }
    }
}

/// Messages 的截图工具结果必须完整保留图片，不能在归一时只留下文本。
#[tokio::test]
async fn messages_tool_result_images_reach_vision_model() {
    let (app, mock, log) = fixture().await;
    let (status, body) = send(
        &app,
        "POST",
        "/clients/trae/v1/messages",
        tool_image_history("messages", 8, true),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let bodies = mock.bodies.lock().unwrap();
    let upstream = bodies.last().unwrap();
    assert_eq!(image_count(upstream), 8);
    assert_eq!(upstream["model"], "auto");
    assert_eq!(log.list(1)[0].image_count, Some(8));
}

/// Chat 工具图片保持相同语义，累计图片确实超出窗口时三协议均拒收。
#[tokio::test]
async fn tool_images_share_admission_and_real_context_limits_across_protocols() {
    let (app, mock, log) = fixture().await;
    let (status, body) = send(
        &app,
        "POST",
        "/clients/trae/v1/chat/completions",
        tool_image_history("chat", 16, true),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(image_count(&mock.bodies.lock().unwrap()[0]), 16);
    for (protocol, path) in [
        ("chat", "/clients/trae/v1/chat/completions"),
        ("responses", "/clients/trae/v1/responses"),
        ("messages", "/clients/trae/v1/messages"),
    ] {
        let payload = tool_image_history(protocol, 32, false);
        let bytes = payload.to_string().len();
        assert!(bytes < 2 * 1024 * 1024);
        let (status, body) = send(&app, "POST", path, payload).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{protocol}: {body}");
        assert!(body.contains("context_limit_exceeded"), "{body}");
        let record = &log.list(1)[0];
        assert_eq!(record.image_count, Some(32));
        assert_eq!(record.request_body_bytes, Some(bytes));
        assert!(record.attempts.is_empty());
        assert!(record.routing_rejections.iter().any(|rejection| {
            rejection.reason.contains("context_limit_exceeded")
                && rejection.reason.contains("window=200000")
        }));
        assert!(
            !serde_json::to_string(record)
                .unwrap()
                .contains("data:image")
        );
    }
    assert_eq!(mock.bodies.lock().unwrap().len(), 1);
}

/// 同一 user 消息中的多工具结果各自保留 ID，正文不被并入最后一个结果。
#[tokio::test]
async fn parallel_messages_tool_results_preserve_ids_images_and_text_order() {
    let (app, mock, _) = fixture().await;
    let payload = json!({
        "model":"auto","max_tokens":32,
        "messages":[
            {"role":"assistant","content":[
                {"type":"tool_use","id":"a","name":"screenshot","input":{}},
                {"type":"tool_use","id":"b","name":"screenshot","input":{}}
            ]},
            {"role":"user","content":[
                {"type":"text","text":"before"},
                {"type":"tool_result","tool_use_id":"a","content":"first"},
                {"type":"text","text":"between"},
                {"type":"tool_result","tool_use_id":"b","content":[
                    {"type":"text","text":"second"},
                    {"type":"image","source":{"type":"url","url":IMAGE}}
                ]},
                {"type":"text","text":"after"}
            ]}
        ]
    });
    let (status, body) = send(&app, "POST", "/v1/messages", payload).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let bodies = mock.bodies.lock().unwrap();
    let messages = bodies[0]["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0]["tool_calls"].as_array().unwrap().len(), 2);
    assert_eq!(messages[3]["role"], "user");
    assert_eq!(messages[3]["content"], "beforebetweenafter");
    assert!(messages[3].get("tool_call_id").is_none());
    assert_eq!(messages[1]["role"], "tool");
    assert_eq!(messages[1]["tool_call_id"], "a");
    assert_eq!(messages[1]["content"], "first");
    assert_eq!(messages[2]["role"], "tool");
    assert_eq!(messages[2]["tool_call_id"], "b");
    assert_eq!(messages[2]["content"][0]["text"], "second");
    assert_eq!(messages[2]["content"][1]["image_url"]["url"], IMAGE);
    assert_eq!(bodies[0]["model"], "auto");
}

/// 普通 JSON 工具输出兼容原行为，损坏的图文块在进入上游前明确拒收。
#[tokio::test]
async fn tool_output_json_compatibility_and_invalid_images_are_explicit() {
    let (app, mock, _) = fixture().await;
    for output in [
        json!("ok"),
        json!({"value":1}),
        json!([1, 2]),
        json!([{"type":"row","value":1}]),
    ] {
        let payload = json!({
            "model":"auto","max_output_tokens":32,"input":[
                {"type":"function_call","call_id":"a","name":"screenshot","arguments":"{}"},
                {"type":"function_call_output","call_id":"a","output":output}
            ]
        });
        let (status, body) = send(&app, "POST", "/v1/responses", payload).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let bodies = mock.bodies.lock().unwrap();
        let messages = &bodies.last().unwrap()["messages"];
        let expected = output
            .as_str()
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| output.to_string());
        assert_eq!(messages[1]["content"], expected);
        assert_eq!(messages[1]["tool_call_id"], "a");
    }
    for (path, payload) in [
        (
            "/v1/responses",
            json!({"model":"auto","input":[{
                "type":"function_call_output","call_id":"a",
                "output":[{"type":"input_image"}]
            }]}),
        ),
        (
            "/v1/responses",
            json!({"model":"auto","input":[{
                "type":"function_call_output","call_id":"a",
                "output":[{"type":"input_text","text":"caption"},{"type":"unknown"}]
            }]}),
        ),
        (
            "/v1/messages",
            json!({"model":"auto","max_tokens":32,"messages":[{
                "role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":[
                    {"type":"image","source":{"type":"base64"}}
                ]}]
            }]}),
        ),
    ] {
        let (status, body) = send(&app, "POST", path, payload).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
    assert_eq!(mock.bodies.lock().unwrap().len(), 4);
}

/// Qoder 不能编码工具图片时显式拒收，Auto 选择能保留图片的后端；user 图文仍可编码。
#[tokio::test]
async fn qoder_tool_result_images_are_not_silently_dropped() {
    let (app, mock, log) = fixture_with_qoder(true).await;
    for (protocol, path) in [
        ("chat", "/clients/trae/v1/chat/completions"),
        ("responses", "/clients/trae/v1/responses"),
        ("messages", "/clients/trae/v1/messages"),
    ] {
        let mut explicit = tool_image_history(protocol, 1, false);
        explicit["model"] = json!("qoder/auto");
        let (status, body) = send(&app, "POST", path, explicit).await;
        let encoded_images = mock
            .qoder_bodies
            .lock()
            .unwrap()
            .iter()
            .flat_map(|body| body["messages"].as_array().into_iter().flatten())
            .flat_map(|message| message["contents"].as_array().into_iter().flatten())
            .filter(|part| part["type"] == "image_url")
            .count();
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{body}; Qoder encoded images={encoded_images}"
        );
        assert!(body.contains("image_role_unsupported"), "{body}");
        assert!(log.list(1)[0].attempts.is_empty());
        assert!(mock.qoder_bodies.lock().unwrap().is_empty());
        let (status, body) = send(&app, "POST", path, tool_image_history(protocol, 1, true)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(log.list(1)[0].resolved_model, "local/auto");
        assert_eq!(image_count(mock.bodies.lock().unwrap().last().unwrap()), 1);
    }
    let (status, body) = send(
        &app,
        "POST",
        "/v1/chat/completions",
        payload("chat", "qoder/auto", "images", false),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let native = mock.qoder_bodies.lock().unwrap();
    assert_eq!(native.len(), 1);
    assert_eq!(
        native[0]["messages"][0]["contents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|part| part["type"] == "image_url")
            .count(),
        2
    );
}
