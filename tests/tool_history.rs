//! 工具历史回归：严格假上游校验调用批次、连续结果和 ID，不使用真实账号。

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::State,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use fakemodelproxy::{
    accounts::Account,
    backend::{Registry, qoder::cosy},
    config::Config,
    records::RecordLog,
    state::AppState,
    store::{BackendRecord, Store},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex, RwLock},
};
use tower::ServiceExt;

#[derive(Default)]
struct Mock {
    bodies: Mutex<Vec<Value>>,
}

/// 从每个 assistant 调用批次检查紧随其后的完整结果集合，并拒绝孤立结果。
fn valid_wire_history(body: &Value) -> bool {
    let Some(messages) = body["messages"].as_array() else {
        return false;
    };
    let mut matched = BTreeSet::new();
    for (index, message) in messages.iter().enumerate() {
        let Some(calls) = message["tool_calls"]
            .as_array()
            .filter(|calls| !calls.is_empty())
        else {
            continue;
        };
        if message["role"] != "assistant" {
            return false;
        }
        let mut ids = BTreeSet::new();
        for call in calls {
            let Some(id) = call["id"].as_str().filter(|id| !id.is_empty()) else {
                return false;
            };
            if !ids.insert(id) {
                return false;
            }
        }
        for offset in 1..=calls.len() {
            let Some(result) = messages.get(index + offset) else {
                return false;
            };
            if result["role"] != "tool"
                || !result["tool_call_id"]
                    .as_str()
                    .is_some_and(|id| ids.remove(id))
            {
                return false;
            }
            matched.insert(index + offset);
        }
    }
    messages
        .iter()
        .enumerate()
        .all(|(index, message)| message["role"] != "tool" || matched.contains(&index))
}

/// 返回与用户错误相同的 400/2013，成功响应支持 JSON 与 SSE。
fn reply(body: &Value, native: bool) -> Response {
    if !valid_wire_history(body) {
        let error = json!({
            "type":"error","error":{
                "type":"bad_request_error",
                "message":"invalid params, tool call result does not follow tool call (2013)",
                "http_code":"400"
            },"request_id":"fake-upstream-request"
        });
        if native {
            let envelope = json!({"statusCodeValue":400,"body":error.to_string()});
            return (
                [("content-type", "text/event-stream")],
                format!("data: {envelope}\n\n"),
            )
                .into_response();
        }
        return (StatusCode::BAD_REQUEST, Json(error)).into_response();
    }
    let delta = json!({"choices":[{"delta":{"content":"OK"},"finish_reason":"stop"}]});
    if native {
        let envelope = json!({"statusCodeValue":200,"body":delta.to_string()});
        return (
            [("content-type", "text/event-stream")],
            format!("data: {envelope}\n\n"),
        )
            .into_response();
    }
    if body["stream"] == true {
        return (
            [("content-type", "text/event-stream")],
            format!("data: {delta}\n\ndata: [DONE]\n\n"),
        )
            .into_response();
    }
    Json(json!({"choices":[{
        "message":{"role":"assistant","content":"OK"},"finish_reason":"stop"
    }]}))
    .into_response()
}

/// 观察标准兼容封包，校验逻辑不调用 FMP 的解析或历史验证实现。
async fn openai(State(mock): State<Arc<Mock>>, Json(body): Json<Value>) -> Response {
    mock.bodies.lock().unwrap().push(body.clone());
    reply(&body, false)
}

/// 解码真实 Qoder 适配器生成的自定义字母表封包后执行同一上游规则。
async fn qoder(State(mock): State<Arc<Mock>>, bytes: Bytes) -> Response {
    let decoded = cosy::decode_body(std::str::from_utf8(&bytes).unwrap()).unwrap();
    let body: Value = serde_json::from_slice(&decoded).unwrap();
    mock.bodies.lock().unwrap().push(body.clone());
    reply(&body, true)
}

struct Fixture {
    app: Router,
    mock: Arc<Mock>,
    log: Arc<RecordLog>,
    upstream: tokio::task::JoinHandle<()>,
}

impl Fixture {
    /// 两个后端只连接本机；Auto 优先 Qoder，以覆盖真实私有封包路径。
    async fn new() -> Self {
        let mock = Arc::new(Mock::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let upstream_app = Router::new()
            .route("/v1/chat/completions", post(openai))
            .route("/qoder", post(qoder))
            .with_state(mock.clone());
        let upstream =
            tokio::spawn(async move { axum::serve(listener, upstream_app).await.unwrap() });
        let store = Store::open(std::path::Path::new(":memory:")).await.unwrap();
        for (id, kind, model, priority) in [
            ("local", "openai_compat", "strict", 10),
            ("qoder", "qoder", "auto", 1),
        ] {
            store
                .upsert_backend(&BackendRecord {
                    id: id.into(),
                    name: id.into(),
                    kind: kind.into(),
                    base_url: format!("{base}/v1"),
                    models: model.into(),
                    priority,
                    cost_tier: "subscription".into(),
                    capabilities: r#"{"tools":true,"context_window":200000}"#.into(),
                    ..Default::default()
                })
                .await
                .unwrap();
        }
        store
            .upsert_account(&Account {
                id: "fake-account".into(),
                name: "Fake".into(),
                backend_id: "qoder".into(),
                enabled: true,
                credential: json!({
                    "uid":"fake-user","device_token":"fake-token","endpoint":format!("{base}/qoder")
                })
                .to_string(),
            })
            .await
            .unwrap();
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
        Self {
            app: fakemodelproxy::build_router(state),
            mock,
            log,
            upstream,
        }
    }

    /// 走 TRAE 专用入口并完整读取响应，使失败和成功都留下终态记录。
    async fn send(&self, protocol: &str, payload: Value) -> (StatusCode, String) {
        let path = match protocol {
            "responses" => "/clients/trae/v1/responses",
            "messages" => "/clients/trae/v1/messages",
            _ => "/clients/trae/v1/chat/completions",
        };
        let response = self
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }
}

impl Drop for Fixture {
    /// 每个用例结束后停止本机假上游，不留下监听器。
    fn drop(&mut self) {
        self.upstream.abort();
    }
}

/// 构造一批并行调用，参数标记用于确认解析没有改写工具内容。
fn calls(ids: &[&str]) -> Value {
    json!({"role":"assistant","content":"prepare","tool_calls":ids.iter().map(|id| json!({
        "id":id,"type":"function","function":{"name":"echo","arguments":"{\"value\":\"PRIVATE_ARGS\"}"}
    })).collect::<Vec<_>>()})
}

/// 构造调用结果，保留 ID，不依赖工具定义是否仍存在。
fn result(id: &str) -> Value {
    json!({"role":"tool","tool_call_id":id,"content":"PRIVATE_RESULT"})
}

/// 将相同的工具历史映射成三种客户端协议，Responses 使用独立 flat items。
fn payload(protocol: &str, model: &str, stream: bool, messages: &[Value]) -> Value {
    match protocol {
        "responses" => {
            let mut input = Vec::new();
            for message in messages {
                if message["role"] == "tool" {
                    input.push(json!({"type":"function_call_output",
                        "call_id":message["tool_call_id"],"output":message["content"]}));
                    continue;
                }
                input.push(
                    json!({"type":"message","role":message["role"],"content":message["content"]}),
                );
                if let Some(calls) = message["tool_calls"].as_array() {
                    for call in calls {
                        input.push(json!({"type":"function_call","call_id":call["id"],
                            "name":call["function"]["name"],"arguments":call["function"]["arguments"]}));
                    }
                }
            }
            json!({"model":model,"stream":stream,"max_output_tokens":32,"input":input})
        }
        "messages" => {
            let mapped = messages.iter().map(|message| {
                if message["role"] == "tool" {
                    return json!({"role":"user","content":[{
                        "type":"tool_result","tool_use_id":message["tool_call_id"],"content":message["content"]
                    }]});
                }
                let mut content = vec![json!({"type":"text","text":message["content"]})];
                if let Some(calls) = message["tool_calls"].as_array() {
                    content.extend(calls.iter().map(|call| json!({
                        "type":"tool_use","id":call["id"],"name":call["function"]["name"],
                        "input":serde_json::from_str::<Value>(call["function"]["arguments"].as_str().unwrap()).unwrap()
                    })));
                }
                json!({"role":message["role"],"content":content})
            }).collect::<Vec<_>>();
            json!({"model":model,"stream":stream,"max_tokens":32,"messages":mapped})
        }
        _ => json!({"model":model,"stream":stream,"max_tokens":32,"messages":messages}),
    }
}

/// 同轮多个 Responses 调用必须合并；结果可逆序，多轮批次不可串联或遗漏。
#[tokio::test]
async fn responses_parallel_calls_reach_strict_qoder_and_openai_in_one_turn() {
    let fixture = Fixture::new().await;
    let messages = vec![
        json!({"role":"user","content":"PRIVATE_PROMPT"}),
        calls(&["a", "b"]),
        result("b"),
        result("a"),
        json!({"role":"assistant","content":"completed"}),
        json!({"role":"user","content":"next"}),
        calls(&["c"]),
        result("c"),
        json!({"role":"user","content":"continue"}),
    ];
    for model in ["qoder/auto", "local/strict", "auto"] {
        for stream in [false, true] {
            let mut request = payload("responses", model, stream, &messages);
            request["input"].as_array_mut().unwrap().insert(4, json!({
                "type":"message","role":"assistant","content":[{"type":"output_text","text":"execute"}]
            }));
            request["input"]
                .as_array_mut()
                .unwrap()
                .insert(3, json!({"type":"reasoning"}));
            let (status, body) = fixture.send("responses", request).await;
            assert_eq!(status, StatusCode::OK, "{model}/{stream}: {body}");
            let bodies = fixture.mock.bodies.lock().unwrap();
            let last = bodies.last().unwrap();
            assert!(valid_wire_history(last));
            let turns = last["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|message| {
                    message["tool_calls"]
                        .as_array()
                        .is_some_and(|calls| !calls.is_empty())
                })
                .collect::<Vec<_>>();
            assert_eq!(turns.len(), 2);
            assert_eq!(turns[0]["tool_calls"].as_array().unwrap().len(), 2);
            assert_eq!(turns[1]["tool_calls"].as_array().unwrap().len(), 1);
            assert_eq!(turns[0]["content"], "prepareexecute");
            assert_eq!(
                turns[0]["tool_calls"][0]["function"]["arguments"],
                "{\"value\":\"PRIVATE_ARGS\"}"
            );
            let record = &fixture.log.list(1)[0];
            assert_eq!(record.attempts.len(), 1);
            assert!(!serde_json::to_string(record).unwrap().contains("PRIVATE_"));
        }
    }
}

/// 正常 Chat 历史作为对照，结果逆序和后续回合复用 ID 都不能被误拒收。
#[tokio::test]
async fn chat_parallel_batches_and_reused_ids_remain_valid() {
    let fixture = Fixture::new().await;
    let messages = vec![
        calls(&["a", "b"]),
        result("b"),
        result("a"),
        json!({"role":"user","content":"next"}),
        calls(&["a"]),
        result("a"),
    ];
    for model in ["qoder/auto", "local/strict", "auto"] {
        for stream in [false, true] {
            let (status, body) = fixture
                .send("chat", payload("chat", model, stream, &messages))
                .await;
            assert_eq!(status, StatusCode::OK, "{model}/{stream}: {body}");
            assert!(valid_wire_history(
                fixture.mock.bodies.lock().unwrap().last().unwrap()
            ));
        }
    }
}

/// 缺失、重复、错配及被正文打断的历史在三协议本地拒收，不消耗上游尝试。
#[tokio::test]
async fn malformed_tool_history_is_rejected_before_any_upstream_request() {
    let fixture = Fixture::new().await;
    let cases = [
        vec![result("a")],
        vec![calls(&["a"]), result("b")],
        vec![calls(&["a"]), result("a"), result("a")],
        vec![calls(&["a", "a"]), result("a")],
        vec![calls(&["a", "b"]), result("a")],
        vec![
            calls(&["a"]),
            json!({"role":"user","content":"PRIVATE_PROMPT"}),
            result("a"),
        ],
        vec![calls(&[""]), result("")],
    ];
    for protocol in ["chat", "responses", "messages"] {
        for model in ["qoder/auto", "auto"] {
            for stream in [false, true] {
                for messages in &cases {
                    let (status, body) = fixture
                        .send(protocol, payload(protocol, model, stream, messages))
                        .await;
                    assert_eq!(
                        status,
                        StatusCode::BAD_REQUEST,
                        "{protocol}/{model}: {body}"
                    );
                    let error: Value = serde_json::from_str(&body).unwrap();
                    assert!(
                        error["error"]["message"]
                            .as_str()
                            .unwrap()
                            .contains("invalid tool history")
                    );
                    assert!(!body.contains("PRIVATE_"));
                    if protocol != "messages" {
                        assert_eq!(error["error"]["code"], "invalid_tool_history");
                    }
                    let record = &fixture.log.list(1)[0];
                    assert!(record.attempts.is_empty());
                    assert!(
                        record
                            .error
                            .as_deref()
                            .unwrap()
                            .starts_with("invalid_tool_history")
                    );
                    assert_eq!(record.upstream_status, None);
                    assert!(!serde_json::to_string(record).unwrap().contains("PRIVATE_"));
                }
            }
        }
    }
    assert!(fixture.mock.bodies.lock().unwrap().is_empty());
}

/// 并发 Responses 会话使用相同调用 ID 时，只匹配各自请求中的批次。
#[tokio::test]
async fn concurrent_responses_keep_tool_batches_isolated() {
    let fixture = Fixture::new().await;
    let messages = vec![calls(&["a", "b"]), result("a"), result("b")];
    let results = futures_util::future::join_all(
        (0..8).map(|_| fixture.send("responses", payload("responses", "auto", true, &messages))),
    )
    .await;
    assert!(
        results
            .iter()
            .all(|(status, body)| *status == StatusCode::OK && body.contains("OK"))
    );
    let bodies = fixture.mock.bodies.lock().unwrap();
    assert_eq!(bodies.len(), 8);
    assert!(bodies.iter().all(valid_wire_history));
    let ids = fixture
        .log
        .list(8)
        .into_iter()
        .map(|record| record.id)
        .collect::<BTreeSet<_>>();
    assert_eq!(ids.len(), 8);
}

/// Messages 的同条混合内容必须让全部工具结果连续出现，普通正文作为后续 user 内容。
#[tokio::test]
async fn messages_mixed_results_keep_a_contiguous_batch_for_strict_upstreams() {
    let fixture = Fixture::new().await;
    let messages = vec![
        calls(&["a", "b"]),
        result("b"),
        result("a"),
        json!({"role":"user","content":"continue"}),
    ];
    for model in ["qoder/auto", "local/strict", "auto"] {
        for stream in [false, true] {
            let mut request = payload("messages", model, stream, &messages);
            request["messages"] = json!([
                request["messages"][0],
                {"role":"user","content":[
                    {"type":"text","text":"before"},
                    {"type":"tool_result","tool_use_id":"b","content":"PRIVATE_RESULT_B"},
                    {"type":"text","text":"between"},
                    {"type":"tool_result","tool_use_id":"a","content":"PRIVATE_RESULT_A"},
                    {"type":"text","text":"after"}
                ]}
            ]);
            let (status, body) = fixture.send("messages", request).await;
            assert_eq!(status, StatusCode::OK, "{model}/{stream}: {body}");
            let bodies = fixture.mock.bodies.lock().unwrap();
            let last = bodies.last().unwrap();
            assert!(valid_wire_history(last));
            assert_eq!(last["messages"][1]["tool_call_id"], "b");
            assert_eq!(last["messages"][2]["tool_call_id"], "a");
            let following = &last["messages"][3];
            assert_eq!(following["role"], "user");
            assert_eq!(
                following["content"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
                    + following["contents"]
                        .as_array()
                        .map(|parts| parts
                            .iter()
                            .filter_map(|part| part["text"].as_str())
                            .collect::<String>())
                        .unwrap_or_default()
                        .as_str(),
                "beforebetweenafter"
            );
        }
    }
}
