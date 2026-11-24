//! 多协议与管理面端到端测试。

use axum::Router;
use axum::body::Body;
use axum::extract::Json;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use axum::routing::post;
use fakemodelproxy::backend::Registry;
use fakemodelproxy::config::Config;
use fakemodelproxy::records::RecordLog;
use fakemodelproxy::state::AppState;
use fakemodelproxy::store::{BackendRecord, Store};
use fakemodelproxy::tasks::TaskRegistry;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};
use tower::ServiceExt;

/// 启动假上游。
async fn spawn_mock() -> String {
    let app = Router::new()
        .route("/v1/chat/completions", post(mock_chat))
        .route(
            "/v1/models",
            axum::routing::get(|| async {
                Json(json!({"data": [{"id": "mock-model", "context_window":200000, "capabilities":{"tools":true}}]}))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{address}/v1")
}

/// 假上游：流式返回工具调用。
async fn mock_chat(Json(body): Json<Value>) -> Response {
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if streaming {
        let frames = [
            json!({"choices":[{"index":0,"delta":{"content":"thinking"},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"echo","arguments":"{\"text\":\"hi\"}"}}]},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        ];
        let mut payload = String::new();
        for frame in frames {
            payload.push_str(&format!("data: {frame}\n\n"));
        }
        payload.push_str("data: [DONE]\n\n");
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from(payload))
            .expect("sse");
    }
    let payload = json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "done"
            },
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
    });
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))
        .expect("json")
}

/// 构造测试应用。
async fn build_app(base_url: &str) -> Router {
    let store = Store::open(std::path::Path::new(":memory:"))
        .await
        .expect("store");
    store
        .upsert_backend(&BackendRecord {
            id: "mock".into(),
            name: "Mock".into(),
            base_url: base_url.to_string(),
            api_key: "sk-test".into(),
            models: "mock-model".into(),
            cost_tier: "free".into(),
            priority: 10,
            ..BackendRecord::default()
        })
        .await
        .expect("backend");
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
    let usage = Arc::new(fakemodelproxy::usage::UsageLedger::new(store.clone()));
    let state = AppState::new(
        config,
        store,
        registry,
        Arc::new(RecordLog::default()),
        Arc::new(RwLock::new(TaskRegistry::new())),
        Arc::new(fakemodelproxy::routing::Router {
            usage: Some(usage.clone()),
            ..Default::default()
        }),
        usage,
    );
    let app = fakemodelproxy::build_router(state);
    // 生成热路径只读目录缓存；先执行与控制台相同的显式目录同步。
    let (status, body) = send(app.clone(), "POST", "/api/backends/mock/models/sync", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    app
}

/// 发送请求并收集响应。
async fn send(app: Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, String) {
    let builder = Request::builder().method(method).uri(uri);
    let request = match body {
        Some(value) => builder
            .header("content-type", "application/json")
            .body(Body::from(value.to_string()))
            .expect("request"),
        None => builder.body(Body::empty()).expect("request"),
    };
    let response = app.oneshot(request).await.expect("call");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// 固定上游分片通过真实 HTTP 返回，覆盖协议转换而非直接调用编码器。
async fn responses_frames(frames: Vec<Value>) -> Vec<Value> {
    let payload = frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n";
    let mock = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let payload = payload.clone();
            async move {
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from(payload))
                    .expect("sse")
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        axum::serve(listener, mock).await.expect("serve");
    });
    let app = build_app(&format!("http://{address}/v1")).await;
    let (status, body) = send(
        app,
        "POST",
        "/v1/responses",
        Some(json!({
            "model":"mock/mock-model","input":"hi","stream":true,
            "tools":[{"type":"function","name":"echo","parameters":{"type":"object"}}]
        })),
    )
    .await;
    server.abort();
    assert_eq!(status, StatusCode::OK, "{body}");
    parse_responses_events(&body)
}

/// 同一假上游分别提供 SSE 和 JSON，验证三个真实入口的输出语义。
async fn scripted_protocol(
    protocol: &str,
    streaming: bool,
    frames: Vec<Value>,
    completion: Value,
) -> (StatusCode, String) {
    let payload = frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n";
    let mock = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(body): Json<Value>| {
            let streaming = body["stream"] == true;
            let (content_type, payload) = if streaming {
                ("text/event-stream", payload.clone())
            } else {
                ("application/json", completion.to_string())
            };
            async move {
                Response::builder()
                    .header("content-type", content_type)
                    .body(Body::from(payload))
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let app = build_app(&format!("http://{address}/v1")).await;
    let mut body = json!({"model":"mock/mock-model","stream":streaming,"max_tokens":128});
    match protocol {
        "responses" => {
            body["input"] = json!("hi");
            body["tools"] =
                json!([{"type":"function","name":"echo","parameters":{"type":"object"}}]);
        }
        "messages" => {
            body["messages"] = json!([{"role":"user","content":"hi"}]);
            body["tools"] = json!([{"name":"echo","input_schema":{"type":"object"}}]);
        }
        _ => {
            body["messages"] = json!([{"role":"user","content":"hi"}]);
            body["tools"] = json!([{"type":"function","function":{"name":"echo","parameters":{"type":"object"}}}]);
        }
    }
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        send(app, "POST", &format!("/v1/{protocol}"), Some(body)),
    )
    .await;
    server.abort();
    result.expect("scripted completion finishes")
}

/// 按 Messages 客户端重建顺序块，关闭前参数必须完整且每次工具身份唯一。
fn assert_messages_blocks(events: &[Value]) -> Vec<Value> {
    assert_eq!(events.first().unwrap()["type"], "message_start");
    assert_eq!(events.last().unwrap()["type"], "message_stop");
    let mut blocks = Vec::new();
    let mut open = None;
    let mut buffer = String::new();
    let mut ids = BTreeSet::new();
    for event in events {
        match event["type"].as_str().unwrap() {
            "content_block_start" => {
                assert!(open.is_none(), "previous block was not closed");
                let block = event["content_block"].clone();
                let index = event["index"].as_u64().unwrap() as usize;
                assert_eq!(index, blocks.len());
                if block["type"] == "tool_use" {
                    let id = block["id"].as_str().unwrap();
                    assert!(!id.is_empty());
                    assert!(!block["name"].as_str().unwrap().is_empty());
                    assert!(ids.insert(id.to_string()));
                }
                blocks.push(block);
                open = Some(index);
                buffer.clear();
            }
            "content_block_delta" => {
                assert_eq!(open, Some(event["index"].as_u64().unwrap() as usize));
                let delta = &event["delta"];
                let fragment = match delta["type"].as_str().unwrap() {
                    "input_json_delta" => &delta["partial_json"],
                    "thinking_delta" => &delta["thinking"],
                    _ => &delta["text"],
                };
                buffer.push_str(fragment.as_str().unwrap());
            }
            "content_block_stop" => {
                let index = open.take().expect("block must be open");
                assert_eq!(index, event["index"].as_u64().unwrap() as usize);
                let block = &mut blocks[index];
                match block["type"].as_str().unwrap() {
                    "tool_use" => {
                        block["input"] = serde_json::from_str(&buffer)
                            .expect("tool JSON must be complete before closing")
                    }
                    "thinking" => block["thinking"] = json!(buffer),
                    _ => block["text"] = json!(buffer),
                }
            }
            "error" => panic!("unexpected protocol failure: {event}"),
            _ => {}
        }
    }
    assert!(open.is_none());
    blocks
}

/// 稀疏索引、交错参数及工具后的正文不丢失，不创建多余或空身份的块。
#[tokio::test]
async fn messages_interleaved_tools_preserve_block_order_and_complete_arguments() {
    let frames = vec![
        delta(json!({"content":"prefix"})),
        tool_delta(7, Some("call_7"), Some("echo"), "{\"text\":"),
        tool_delta(2, Some("call_2"), Some("echo"), "{\"text\":"),
        delta(json!({"content":"tail"})),
        tool_delta(7, None, None, "\"first\"}"),
        delta(json!({"reasoning_content":"thought"})),
        tool_delta(2, None, None, "\"second\"}"),
        finish("tool_calls"),
    ];
    let (status, body) = scripted_protocol("messages", true, frames, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let blocks = assert_messages_blocks(&parse_responses_events(&body));
    assert_eq!(
        blocks,
        vec![
            json!({"type":"text","text":"prefix"}),
            json!({"type":"tool_use","id":"call_7","name":"echo","input":{"text":"first"}}),
            json!({"type":"tool_use","id":"call_2","name":"echo","input":{"text":"second"}}),
            json!({"type":"text","text":"tail"}),
            json!({"type":"thinking","thinking":"thought"}),
        ]
    );
}

/// 参数或身份晚到及无参工具均产生一个可执行的完整调用。
#[tokio::test]
async fn messages_wait_for_tool_identity_and_normalize_empty_arguments() {
    for arguments in ["{\"text\":\"hi\"}", "", " \n "] {
        let frames = vec![
            tool_delta(0, None, None, arguments),
            tool_delta(0, Some("call_1"), None, ""),
            tool_delta(0, None, Some("echo"), ""),
            finish("tool_calls"),
        ];
        let (status, body) = scripted_protocol("messages", true, frames, json!({})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let blocks = assert_messages_blocks(&parse_responses_events(&body));
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["id"], "call_1");
        assert_eq!(blocks[0]["name"], "echo");
        assert_eq!(
            blocks[0]["input"],
            if arguments.trim().is_empty() {
                json!({})
            } else {
                json!({"text":"hi"})
            }
        );
    }
}

/// 三协议及流式/非流式均保留合法工具输出后的截断、过滤终态。
#[tokio::test]
async fn all_protocols_preserve_non_success_finish_with_valid_tools() {
    for reason in ["length", "content_filter"] {
        for streaming in [false, true] {
            for protocol in ["chat/completions", "messages", "responses"] {
                let frames = vec![
                    tool_delta(0, Some("call_1"), Some("echo"), "{}"),
                    finish(reason),
                ];
                let completion = json!({"choices":[{
                    "message":{"role":"assistant","content":"","tool_calls":[{
                        "id":"call_1","type":"function","function":{"name":"echo","arguments":"{}"}
                    }]},"finish_reason":reason
                }],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}});
                let (status, body) =
                    scripted_protocol(protocol, streaming, frames, completion).await;
                assert_eq!(status, StatusCode::OK, "{protocol}: {body}");
                match protocol {
                    "chat/completions" => {
                        let response: Value = if streaming {
                            assert!(body.contains("data: [DONE]"));
                            body.lines()
                                .filter_map(|line| line.strip_prefix("data: "))
                                .filter(|line| *line != "[DONE]")
                                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                                .find(|value| value["choices"][0]["finish_reason"].is_string())
                                .unwrap()
                        } else {
                            serde_json::from_str(&body).unwrap()
                        };
                        assert_eq!(response["choices"][0]["finish_reason"], reason);
                    }
                    "messages" => {
                        let response: Value = if streaming {
                            let events = parse_responses_events(&body);
                            assert_eq!(assert_messages_blocks(&events).len(), 1);
                            events
                                .into_iter()
                                .find(|value| value["type"] == "message_delta")
                                .unwrap()["delta"]
                                .clone()
                        } else {
                            serde_json::from_str(&body).unwrap()
                        };
                        assert_eq!(
                            response["stop_reason"],
                            if reason == "length" {
                                "max_tokens"
                            } else {
                                "refusal"
                            }
                        );
                    }
                    _ => {
                        let response: Value = if streaming {
                            parse_responses_events(&body).last().unwrap()["response"].clone()
                        } else {
                            serde_json::from_str(&body).unwrap()
                        };
                        assert_eq!(response["status"], "incomplete");
                        assert_eq!(
                            response["incomplete_details"]["reason"],
                            if reason == "length" {
                                "max_output_tokens"
                            } else {
                                reason
                            }
                        );
                    }
                }
            }
        }
    }
}

/// 按客户端收到的 SSE 正文解析事件，校验事件名与 JSON 类型一致。
fn parse_responses_events(body: &str) -> Vec<Value> {
    body.split("\n\n")
        .filter_map(|frame| {
            let event = frame
                .lines()
                .find_map(|line| line.strip_prefix("event: "))?;
            let data = frame.lines().find_map(|line| line.strip_prefix("data: "))?;
            let value: Value = serde_json::from_str(data).expect("SSE JSON");
            assert_eq!(value["type"], event);
            Some(value)
        })
        .collect()
}

/// 四个 TRAE 会话同时输出正文和工具参数，每条响应必须独立保持完整终态。
#[tokio::test]
async fn concurrent_trae_responses_keep_text_tools_and_lifecycle_isolated() {
    let barrier = Arc::new(tokio::sync::Barrier::new(4));
    let mock = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(body): Json<Value>| {
            let barrier = barrier.clone();
            async move {
                let label = body["messages"][0]["content"].as_str().unwrap();
                let frames = [
                    delta(json!({"content":format!("text {label}")})),
                    tool_delta(
                        0,
                        Some(&format!("call_{label}")),
                        Some("echo"),
                        "{\"session\":",
                    ),
                    tool_delta(0, None, None, &format!("\"{label}\"}}")),
                    finish("tool_calls"),
                ];
                let payloads = frames
                    .iter()
                    .map(|frame| format!("data: {frame}\n\n"))
                    .chain(std::iter::once("data: [DONE]\n\n".into()))
                    .collect::<Vec<_>>();
                barrier.wait().await;
                let stream =
                    futures_util::stream::unfold(payloads.into_iter(), |mut frames| async move {
                        tokio::task::yield_now().await;
                        frames
                            .next()
                            .map(|frame| (Ok::<_, std::convert::Infallible>(frame), frames))
                    });
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from_stream(stream))
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let app = build_app(&format!("http://{address}/v1")).await;
    let responses = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        futures_util::future::join_all((0..4).map(|index| {
            send(
                app.clone(),
                "POST",
                "/clients/trae/v1/responses",
                Some(json!({
                    "model":"mock/mock-model","input":format!("session-{index}"),"stream":true,
                    "tools":[{"type":"function","name":"echo","parameters":{"type":"object"}}],
                })),
            )
        })),
    )
    .await;
    server.abort();
    let mut ids = BTreeSet::new();
    for (index, (status, body)) in responses
        .expect("并发会话必须全部完成")
        .into_iter()
        .enumerate()
    {
        assert_eq!(status, StatusCode::OK, "{body}");
        let events = parse_responses_events(&body);
        let response = assert_response_lifecycle(&events, "response.completed");
        let label = format!("session-{index}");
        assert_eq!(
            response["output"][0]["content"][0]["text"],
            format!("text {label}")
        );
        assert_eq!(response["output"][1]["call_id"], format!("call_{label}"));
        let arguments: Value =
            serde_json::from_str(response["output"][1]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(arguments, json!({"session":label}));
        assert!(ids.insert(response["id"].as_str().unwrap().to_string()));
        for item in response["output"].as_array().unwrap() {
            assert!(ids.insert(item["id"].as_str().unwrap().to_string()));
        }
    }
}

/// 构造上游正文或工具分片。
fn delta(value: Value) -> Value {
    json!({"choices":[{"index":0,"delta":value,"finish_reason":null}]})
}

/// 构造上游结束帧和固定用量。
fn finish(reason: &str) -> Value {
    json!({
        "choices":[{"index":0,"delta":{},"finish_reason":reason}],
        "usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}
    })
}

/// 构造工具分片，允许元数据晚到与稀疏的上游索引。
fn tool_delta(index: u32, id: Option<&str>, name: Option<&str>, arguments: &str) -> Value {
    delta(json!({"tool_calls":[{
        "index":index,"id":id,"function":{"name":name,"arguments":arguments}
    }]}))
}

/// 按客户端规则重建输出，验证身份、索引、结束快照和最终响应一致。
fn assert_response_lifecycle<'a>(events: &'a [Value], terminal: &str) -> &'a Value {
    assert_eq!(events[0]["type"], "response.created");
    assert_eq!(events[1]["type"], "response.in_progress");
    let initial = &events[0]["response"];
    let mut items = BTreeMap::<usize, Value>::new();
    let mut done = BTreeSet::new();
    let mut text_done = BTreeSet::new();
    let mut part_done = BTreeSet::new();
    let mut arguments_done = BTreeSet::new();
    for (sequence, event) in events.iter().enumerate() {
        assert_eq!(event["sequence_number"], sequence, "{event}");
        if let Some(response) = event.get("response") {
            for field in [
                "id",
                "created_at",
                "model",
                "object",
                "tools",
                "tool_choice",
                "parallel_tool_calls",
            ] {
                assert_eq!(response[field], initial[field], "{field}");
                assert!(!response[field].is_null(), "{field}");
            }
        }
        let Some(index) = event["output_index"].as_u64().map(|index| index as usize) else {
            continue;
        };
        match event["type"].as_str().expect("type") {
            "response.output_item.added" => {
                assert_eq!(index, items.len(), "added order");
                assert!(items.insert(index, event["item"].clone()).is_none());
                if event["item"]["type"] == "function_call" {
                    assert!(
                        !event["item"]["call_id"]
                            .as_str()
                            .expect("call_id")
                            .is_empty()
                    );
                    assert!(!event["item"]["name"].as_str().expect("name").is_empty());
                }
            }
            "response.content_part.added" => {
                let item = items.get_mut(&index).expect("added item");
                assert_eq!(event["item_id"], item["id"]);
                assert_eq!(event["content_index"], 0);
                item["content"]
                    .as_array_mut()
                    .expect("content")
                    .push(event["part"].clone());
            }
            "response.output_text.delta" | "response.function_call_arguments.delta" => {
                assert!(!done.contains(&index), "delta after done");
                let item = items.get_mut(&index).expect("added item");
                assert_eq!(event["item_id"], item["id"]);
                let text = if item["type"] == "message" {
                    assert_eq!(event["content_index"], 0);
                    assert_eq!(event["logprobs"], json!([]));
                    &mut item["content"][0]["text"]
                } else {
                    &mut item["arguments"]
                };
                if let Value::String(text) = text {
                    text.push_str(event["delta"].as_str().expect("delta"));
                } else {
                    panic!("expected string");
                }
            }
            "response.output_text.done" => {
                assert!(text_done.insert(index));
                assert_eq!(event["text"], items[&index]["content"][0]["text"]);
                assert_eq!(event["item_id"], items[&index]["id"]);
            }
            "response.content_part.done" => {
                assert!(part_done.insert(index));
                assert_eq!(event["part"], items[&index]["content"][0]);
            }
            "response.function_call_arguments.done" => {
                assert!(arguments_done.insert(index));
                assert_eq!(event["arguments"], items[&index]["arguments"]);
                assert_eq!(event["name"], items[&index]["name"]);
                assert_eq!(event["item_id"], items[&index]["id"]);
            }
            "response.output_item.done" => {
                assert!(done.insert(index), "duplicate done");
                let item = items.get_mut(&index).expect("added item");
                if item["type"] == "message" {
                    assert!(text_done.contains(&index) && part_done.contains(&index));
                } else {
                    assert!(arguments_done.contains(&index));
                }
                item["status"] = event["item"]["status"].clone();
                assert_eq!(
                    *item, event["item"],
                    "done must retain identity and all content"
                );
            }
            other => panic!("unexpected indexed event: {other}"),
        }
    }
    assert_eq!(events.last().expect("terminal")["type"], terminal);
    let response = &events.last().expect("terminal")["response"];
    assert_eq!(
        response["output"],
        json!(items.into_values().collect::<Vec<_>>())
    );
    assert_eq!(
        done.len(),
        response["output"].as_array().expect("output").len()
    );
    assert_eq!(response["usage"]["total_tokens"], 5);
    response
}

/// Messages 流式响应必须包含 Anthropic 事件序列。
#[tokio::test]
async fn messages_stream_emits_anthropic_events() {
    let base = spawn_mock().await;
    let app = build_app(&base).await;
    let (status, body) = send(
        app,
        "POST",
        "/v1/messages",
        Some(json!({
            "model": "mock/mock-model",
            "max_tokens": 64,
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "echo", "input_schema": {"type": "object"}}]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(body.contains("message_start"), "body: {body}");
    assert!(body.contains("content_block_delta"), "body: {body}");
    assert!(body.contains("tool_use"), "body: {body}");
    assert!(body.contains("message_stop"), "body: {body}");
}

/// Messages 非流式响应结构必须符合规范。
#[tokio::test]
async fn messages_non_stream_shape() {
    let base = spawn_mock().await;
    let app = build_app(&base).await;
    let (status, body) = send(
        app,
        "POST",
        "/v1/messages",
        Some(json!({
            "model": "mock/mock-model",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "hi"}]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let parsed: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(parsed["type"], "message");
    assert_eq!(parsed["role"], "assistant");
    assert_eq!(parsed["stop_reason"], "end_turn");
}

/// Responses 非流式响应应包含 output 与 usage。
#[tokio::test]
async fn responses_non_stream_shape() {
    let base = spawn_mock().await;
    let app = build_app(&base).await;
    let (status, body) = send(
        app,
        "POST",
        "/v1/responses",
        Some(json!({
            "model": "mock/mock-model",
            "input": "hi"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let parsed: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(parsed["object"], "response");
    assert_eq!(parsed["status"], "completed");
    assert_eq!(parsed["output"][0]["type"], "message");
    assert_eq!(parsed["usage"]["total_tokens"], 5);
}

/// Responses 流式应发出 response.created 与 response.completed。
#[tokio::test]
async fn responses_stream_events() {
    let base = spawn_mock().await;
    let app = build_app(&base).await;
    let (status, body) = send(
        app,
        "POST",
        "/v1/responses",
        Some(json!({
            "model": "mock/mock-model",
            "input": "hi",
            "stream": true
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(body.contains("response.created"), "body: {body}");
    assert!(body.contains("response.completed"), "body: {body}");
}

/// 文本 delta、text.done、part.done、item.done 和最终 output 必须保留同一正文。
#[tokio::test]
async fn responses_stream_retains_text() {
    let events = responses_frames(vec![
        delta(json!({"content":"你好，"})),
        delta(json!({"content":"world"})),
        finish("stop"),
    ])
    .await;
    let response = assert_response_lifecycle(&events, "response.completed");
    assert_eq!(response["output"][0]["content"][0]["text"], "你好，world");
    assert_eq!(response["output"].as_array().expect("output").len(), 1);
}

/// 纯工具调用必须保留完整调用标识、函数名和分片拼接后的 JSON 参数。
#[tokio::test]
async fn responses_stream_retains_fragmented_tool_call() {
    let events = responses_frames(vec![
        tool_delta(0, Some("call_1"), Some("echo"), "{\"text\":"),
        tool_delta(0, None, None, "\"hi\"}"),
        finish("tool_calls"),
    ])
    .await;
    let response = assert_response_lifecycle(&events, "response.completed");
    let call = &response["output"][0];
    assert_eq!(response["output"].as_array().expect("output").len(), 1);
    assert_eq!(call["call_id"], "call_1");
    assert_eq!(call["name"], "echo");
    assert_eq!(call["arguments"], "{\"text\":\"hi\"}");
}

/// 工具调用的相同结束原因可以重复，工具参数与最终完成事件都只交付一次。
#[tokio::test]
async fn responses_stream_deduplicates_repeated_tool_finish() {
    let events = responses_frames(vec![
        tool_delta(0, Some("call_1"), Some("echo"), "{}"),
        finish("tool_calls"),
        finish("tool_calls"),
    ])
    .await;
    let response = assert_response_lifecycle(&events, "response.completed");
    assert_eq!(response["output"][0]["arguments"], "{}");
    assert_eq!(response["usage"]["total_tokens"], 5);
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "response.completed")
            .count(),
        1
    );
}

/// 前置正文改变输出索引后，后续工具分片仍必须归并到原工具 item。
#[tokio::test]
async fn responses_stream_keeps_tool_index_after_text() {
    let events = responses_frames(vec![
        delta(json!({"content":"checking"})),
        delta(json!({"reasoning_content":"call echo"})),
        tool_delta(0, Some("call_1"), Some("echo"), "{\"text\":"),
        tool_delta(0, None, None, "\"hi\"}"),
        delta(json!({"content":"after tool"})),
        finish("tool_calls"),
    ])
    .await;
    let response = assert_response_lifecycle(&events, "response.completed");
    let output = response["output"].as_array().expect("output");
    assert_eq!(output.len(), 3);
    assert_eq!(output[0]["content"][0]["text"], "checking");
    assert_eq!(output[1]["arguments"], "{\"text\":\"hi\"}");
    assert_eq!(output[2]["content"][0]["text"], "after tool");
}

/// 交错、稀疏且逆序的上游工具索引，必须保持首次出现的输出位置和身份。
#[tokio::test]
async fn responses_stream_handles_interleaved_tools() {
    let events = responses_frames(vec![
        tool_delta(7, Some("call_7"), Some("echo"), "{\"text\":"),
        tool_delta(2, Some("call_2"), Some("echo"), "{\"text\":"),
        tool_delta(7, None, None, "\"first\"}"),
        tool_delta(2, None, None, "\"second\"}"),
        finish("tool_calls"),
    ])
    .await;
    let response = assert_response_lifecycle(&events, "response.completed");
    let output = response["output"].as_array().expect("output");
    assert_eq!(output.len(), 2);
    assert_eq!(output[0]["call_id"], "call_7");
    assert_eq!(output[0]["arguments"], "{\"text\":\"first\"}");
    assert_eq!(output[1]["call_id"], "call_2");
    assert_eq!(output[1]["arguments"], "{\"text\":\"second\"}");
}

/// 参数先于身份到达时缓冲输出，不能产生空身份或颠倒 added 顺序。
#[tokio::test]
async fn responses_stream_waits_for_tool_metadata() {
    let events = responses_frames(vec![
        tool_delta(0, None, None, "{\"text\":\"hi\"}"),
        tool_delta(1, Some("call_2"), Some("echo"), "{}"),
        delta(json!({"content":"tail"})),
        tool_delta(0, Some("call_1"), None, ""),
        tool_delta(0, None, Some("echo"), ""),
        finish("tool_calls"),
    ])
    .await;
    let response = assert_response_lifecycle(&events, "response.completed");
    let output = response["output"].as_array().expect("output");
    assert_eq!(output.len(), 3);
    assert_eq!(output[0]["call_id"], "call_1");
    assert_eq!(output[0]["arguments"], "{\"text\":\"hi\"}");
    assert_eq!(output[1]["call_id"], "call_2");
    assert_eq!(output[2]["content"][0]["text"], "tail");
}

/// 无参工具归一为 {}，delta 重建结果也必须与 done 和终态一致。
#[tokio::test]
async fn responses_stream_normalizes_empty_tool_arguments() {
    for arguments in ["", " \n "] {
        let events = responses_frames(vec![
            tool_delta(0, Some("call_1"), Some("echo"), arguments),
            finish("tool_calls"),
        ])
        .await;
        let response = assert_response_lifecycle(&events, "response.completed");
        assert_eq!(response["output"][0]["arguments"], "{}");
    }
}

/// 缺元数据、非法参数或改变身份时发 failed，不能把残缺工具标为 completed。
#[tokio::test]
async fn responses_stream_rejects_invalid_tool_calls() {
    for mut frames in [
        vec![tool_delta(0, None, Some("echo"), "{}")],
        vec![tool_delta(0, Some("call_1"), None, "{}")],
        vec![tool_delta(0, Some("call_1"), Some("echo"), "{\"text\":")],
        vec![
            tool_delta(0, Some("call_1"), Some("echo"), "{}"),
            tool_delta(0, Some("call_changed"), None, ""),
        ],
    ] {
        frames.push(finish("tool_calls"));
        let events = responses_frames(frames).await;
        assert_eq!(events.last().expect("failed")["type"], "response.failed");
        assert_eq!(
            events.last().expect("failed")["response"]["error"]["code"],
            "invalid_tool_call"
        );
        assert!(
            !events
                .iter()
                .any(|event| event["type"] == "response.completed")
        );
        assert!(
            !events
                .iter()
                .any(|event| event["type"] == "response.output_item.done"
                    && event["item"]["type"] == "function_call")
        );
        for (sequence, event) in events.iter().enumerate() {
            assert_eq!(event["sequence_number"], sequence);
        }
    }
}

/// 上游中途报错时保留已收到的正文，并使用完整且身份稳定的 failed 响应。
#[tokio::test]
async fn responses_stream_retains_partial_output_on_upstream_error() {
    let events = responses_frames(vec![
        delta(json!({"content":"partial"})),
        json!({"error":{"code":"test_error","message":"upstream failed"}}),
    ])
    .await;
    let failed = &events.last().expect("failed")["response"];
    assert_eq!(events.last().expect("failed")["type"], "response.failed");
    assert_eq!(failed["id"], events[0]["response"]["id"]);
    assert_eq!(failed["created_at"], events[0]["response"]["created_at"]);
    assert_eq!(failed["model"], events[0]["response"]["model"]);
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["output"][0]["content"][0]["text"], "partial");
    assert!(!failed["error"].is_null());
    assert!(
        !events
            .iter()
            .any(|event| event["type"] == "response.completed")
    );
}

/// token 截断和内容过滤不能被工具调用覆盖，已完整的调用也不能宣称响应完成。
#[tokio::test]
async fn responses_stream_marks_truncation_incomplete() {
    for (finish_reason, reason) in [
        ("length", "max_output_tokens"),
        ("content_filter", "content_filter"),
    ] {
        let events = responses_frames(vec![
            tool_delta(0, Some("call_1"), Some("echo"), "{}"),
            finish(finish_reason),
        ])
        .await;
        let response = assert_response_lifecycle(&events, "response.incomplete");
        assert_eq!(response["status"], "incomplete");
        assert_eq!(response["incomplete_details"]["reason"], reason);
        assert_eq!(response["output"][0]["status"], "incomplete");
        assert_eq!(response["output"][0]["arguments"], "{}");
        assert!(
            !events
                .iter()
                .any(|event| event["type"] == "response.completed")
        );
    }
}

/// 状态化字段必须被明确拒绝。
#[tokio::test]
async fn responses_rejects_previous_response_id() {
    let base = spawn_mock().await;
    let app = build_app(&base).await;
    let (status, body) = send(
        app,
        "POST",
        "/v1/responses",
        Some(json!({
            "model": "mock/mock-model",
            "input": "hi",
            "previous_response_id": "resp_1"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert!(body.contains("previous_response_id"), "body: {body}");
}

/// auto:free 预演应列出候选并说明费用筛选。
#[tokio::test]
async fn route_plan_lists_candidates() {
    let base = spawn_mock().await;
    let app = build_app(&base).await;
    let (status, body) = send(
        app,
        "GET",
        "/api/route/plan?model=auto:free&tools=true",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let parsed: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(parsed["candidates"].as_array().expect("array").len(), 1);
    assert_eq!(parsed["candidates"][0]["backend"], "mock");
    assert_eq!(parsed["candidates"][0]["cost_tier"], "free");
    assert_eq!(parsed["strategy"], "auto");
    assert_eq!(parsed["candidates"][0]["pool"], "quota");
}

/// 模型发现仅展示一个 Auto，旧策略名称不再让客户端产生多余选项。
#[tokio::test]
async fn model_catalog_exposes_one_unified_auto() {
    let app = build_app(&spawn_mock().await).await;
    let (status, body) = send(app, "GET", "/v1/models", None).await;
    assert_eq!(status, StatusCode::OK);
    let payload: Value = serde_json::from_str(&body).unwrap();
    let automatic: Vec<_> = payload["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|model| {
            model["id"]
                .as_str()
                .is_some_and(|id| id == "auto" || id.starts_with("auto:"))
        })
        .collect();
    assert_eq!(automatic.len(), 1);
    assert_eq!(automatic[0]["id"], "auto");
    assert_eq!(automatic[0]["routing_strategy"], "auto");
}

/// auto 请求应能完成一次生成并记录实际模型。
#[tokio::test]
async fn auto_request_completes() {
    let base = spawn_mock().await;
    let app = build_app(&base).await;
    let (status, body) = send(
        app,
        "POST",
        "/v1/chat/completions",
        Some(json!({
            "model": "auto",
            "messages": [{"role": "user", "content": "hi"}]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let parsed: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(parsed["model"], "mock-mock-model");
}

/// 账号 CRUD 与凭证不回显。
#[tokio::test]
async fn account_crud_hides_credential() {
    let base = spawn_mock().await;
    let app = build_app(&base).await;
    let (status, _) = send(
        app.clone(),
        "POST",
        "/api/accounts",
        Some(json!({
            "backend_id": "mock",
            "name": "primary",
            "credential": "{\"api_key\":\"secret\"}"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = send(app, "GET", "/api/accounts", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"has_credential\":true"), "body: {body}");
    assert!(!body.contains("secret"), "credential must not leak: {body}");
}

/// 记录接口应返回刚才的调用。
#[tokio::test]
async fn records_endpoint_returns_requests() {
    let base = spawn_mock().await;
    let app = build_app(&base).await;
    let (status, _) = send(
        app.clone(),
        "POST",
        "/v1/chat/completions",
        Some(json!({
            "model": "mock/mock-model",
            "messages": [{"role": "user", "content": "hi"}]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = send(app, "GET", "/api/records?limit=10", None).await;
    assert_eq!(status, StatusCode::OK);
    let parsed: Value = serde_json::from_str(&body).expect("json");
    assert!(!parsed["records"].as_array().expect("array").is_empty());
    assert_eq!(parsed["records"][0]["resolved_model"], "mock/mock-model");
}

/// 未声明签到能力的后端不应出现在任务执行器里。
#[tokio::test]
async fn checkin_requires_registered_backend() {
    let base = spawn_mock().await;
    let app = build_app(&base).await;
    let (status, body) = send(app, "POST", "/api/tasks/mock/checkin", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
}

/// 预设接口应返回内置列表且不含密钥字段。
#[tokio::test]
async fn presets_endpoint_lists_providers() {
    let base = spawn_mock().await;
    let app = build_app(&base).await;
    let (status, body) = send(app, "GET", "/api/presets", None).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let parsed: Value = serde_json::from_str(&body).expect("json");
    let presets = parsed["presets"].as_array().expect("array");
    assert!(presets.iter().any(|item| item["id"] == "bailian"));
    assert!(presets.iter().all(|item| item.get("api_key").is_none()));
}

/// 导出默认必须清空密钥。
#[tokio::test]
async fn config_export_strips_secrets() {
    let base = spawn_mock().await;
    let app = build_app(&base).await;
    let (status, body) = send(app, "GET", "/api/config/export", None).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let parsed: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(parsed["version"], 1);
    assert_eq!(parsed["backends"][0]["api_key"], "");
    assert!(!body.contains("sk-test"), "secret must not leak: {body}");
}

/// 导入脱敏配置时保留已有密钥。
#[tokio::test]
async fn config_import_keeps_existing_secret() {
    let base = spawn_mock().await;
    let app = build_app(&base).await;
    let bundle = json!({
        "version": 1,
        "backends": [{
            "id": "mock",
            "name": "Mock renamed",
            "kind": "openai_compat",
            "base_url": base,
            "api_key": "",
            "models": "mock-model",
            "enabled": true,
            "priority": 10,
            "cost_tier": "free",
            "options": "",
            "capabilities": ""
        }],
        "accounts": []
    });
    let (status, body) = send(app.clone(), "POST", "/api/config/import", Some(bundle)).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let (status, body) = send(app, "GET", "/api/backends", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Mock renamed"), "body: {body}");
    assert!(
        body.contains("\"has_api_key\":true"),
        "secret must be preserved: {body}"
    );
}

/// 健康接口应返回快照结构。
#[tokio::test]
async fn health_endpoint_reports_snapshots() {
    let base = spawn_mock().await;
    let app = build_app(&base).await;
    let (status, body) = send(app, "GET", "/api/health/backends", None).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let parsed: Value = serde_json::from_str(&body).expect("json");
    assert!(parsed["backends"].is_array());
    assert_eq!(parsed["sessions"], 0);
}

/// 没有 Qoder 账号时额度列表为空，而不是伪造数字。
#[tokio::test]
async fn quota_endpoint_is_empty_without_qoder() {
    let base = spawn_mock().await;
    let app = build_app(&base).await;
    let (status, body) = send(app, "GET", "/api/quota", None).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let parsed: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(parsed["quota"].as_array().expect("array").len(), 0);
}

/// auto:fast 应能完成请求并记录延迟样本。
#[tokio::test]
async fn auto_fast_completes_and_records_latency() {
    let base = spawn_mock().await;
    let app = build_app(&base).await;
    let (status, body) = send(
        app.clone(),
        "POST",
        "/v1/chat/completions",
        Some(json!({
            "model": "auto:fast",
            "messages": [{"role": "user", "content": "hi"}]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let (status, body) = send(app, "GET", "/api/health/backends", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("latency_ms"), "body: {body}");
}

/// 能力覆盖应影响路由筛选：声明不支持工具后，tools 请求不再命中。
#[tokio::test]
async fn capability_override_filters_tools() {
    let base = spawn_mock().await;
    let app = build_app(&base).await;
    let bundle = json!({
        "version": 1,
        "backends": [{
            "id": "mock",
            "name": "Mock",
            "kind": "openai_compat",
            "base_url": base,
            "api_key": "sk-test",
            "models": "mock-model",
            "enabled": true,
            "priority": 10,
            "cost_tier": "free",
            "options": "",
            "capabilities": "{\"tools\":false}"
        }],
        "accounts": []
    });
    let (status, _) = send(app.clone(), "POST", "/api/config/import", Some(bundle)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = send(app, "GET", "/api/route/plan?model=auto&tools=true", None).await;
    assert_eq!(status, StatusCode::OK);
    let parsed: Value = serde_json::from_str(&body).expect("json");
    assert!(parsed["candidates"].as_array().expect("array").is_empty());
    assert!(body.contains("tools_unsupported"), "body: {body}");
}

/// 显式模型请求同样累计日用量。
#[tokio::test]
async fn explicit_model_records_usage() {
    let base = spawn_mock().await;
    let app = build_app(&base).await;
    let (status, body) = send(
        app.clone(),
        "POST",
        "/v1/chat/completions",
        Some(json!({"model": "mock/mock-model", "messages": [{"role": "user", "content": "hi"}]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    let (status, body) = send(app, "GET", "/api/usage", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"backend_id\":\"mock\""), "body: {body}");
    assert!(body.contains("\"used_tokens\":5"), "body: {body}");
}

/// 日预算耗尽后 auto 不再选该后端。
#[tokio::test]
async fn daily_budget_blocks_auto() {
    let base = spawn_mock().await;
    let app = build_app(&base).await;
    // 预算设为 1 token，首次请求的 usage（12）必然超支。
    let bundle = json!({
        "version": 1,
        "backends": [{
            "id": "mock",
            "name": "Mock",
            "kind": "openai_compat",
            "base_url": base,
            "api_key": "sk-test",
            "models": "mock-model",
            "enabled": true,
            "priority": 10,
            "cost_tier": "free",
            "options": "",
            "capabilities": "",
            "daily_token_budget": 1
        }],
        "accounts": []
    });
    let (status, body) = send(app.clone(), "POST", "/api/config/import", Some(bundle)).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    // 第一次请求成功并累计用量。
    let (status, _) = send(
        app.clone(),
        "POST",
        "/v1/chat/completions",
        Some(json!({"model": "auto", "messages": [{"role": "user", "content": "hi"}]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // 第二次 auto 因预算耗尽被拒绝。
    let (status, body) = send(
        app.clone(),
        "POST",
        "/v1/chat/completions",
        Some(json!({"model": "auto", "messages": [{"role": "user", "content": "hi"}]})),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body: {body}");
    assert!(body.contains("gateway_no_eligible_target"), "body: {body}");

    // 用量接口应显示超预算。
    let (status, body) = send(app, "GET", "/api/usage", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"over_budget\":true"), "body: {body}");
}
