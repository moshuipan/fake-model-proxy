//! 请求审计集成回归：真实三协议路由连接本地假上游，不使用厂商凭证。

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::Json;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use fakemodelproxy::backend::Registry;
use fakemodelproxy::config::Config;
use fakemodelproxy::records::{RecordLog, RequestRecord};
use fakemodelproxy::state::AppState;
use fakemodelproxy::store::{BackendRecord, Store};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use tower::ServiceExt;

const PATHS: [&str; 3] = ["/v1/chat/completions", "/v1/messages", "/v1/responses"];
const TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const PARENT: &str = "00f067aa0ba902b7";

/// 本地假上游分别返回正常响应、建连失败、流内错误和提前 EOF。
async fn mock(Json(payload): Json<Value>) -> Response {
    let model = payload["model"].as_str().unwrap_or_default();
    if model == "error" {
        return (StatusCode::SERVICE_UNAVAILABLE, "PRIVATE_UPSTREAM_BODY").into_response();
    }
    if matches!(model, "tool" | "tool-text") {
        let text = if model == "tool-text" {
            "PRIVATE_OUTPUT"
        } else {
            ""
        };
        let calls = json!([{
            "id": "call_echo", "type": "function",
            "function": {"name": "echo", "arguments": "{\"text\":\"PRIVATE_TOOL\"}"}
        }]);
        if payload["stream"] == true {
            let mut call = calls[0].clone();
            call["index"] = json!(0);
            return (
                [("content-type", "text/event-stream")],
                format!(
                    "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                    json!({"choices":[{"delta":{"content":text,"tool_calls":[call]}}]}),
                    json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]})
                ),
            )
                .into_response();
        }
        return Json(json!({
            "choices":[{"message":{"role":"assistant","content":text,"tool_calls":calls},
                "finish_reason":"tool_calls"}]
        }))
        .into_response();
    }
    if payload["stream"] == true {
        let start = if matches!(model, "empty-finish" | "blank-eof") {
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":\"\"}]}\n\ndata: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking\",\"content\":\"\"},\"finish_reason\":\"\"}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"PRIVATE_OUTPUT\"},\"finish_reason\":\" \"}]}\n\n"
        } else {
            "data: {\"choices\":[{\"delta\":{\"content\":\"PRIVATE_OUTPUT\"}}]}\n\n"
        };
        if model == "slow" {
            let stream = futures_util::stream::once(async move {
                Ok::<_, std::io::Error>(Bytes::from_static(start.as_bytes()))
            })
            .chain(futures_util::stream::pending());
            return (
                [("content-type", "text/event-stream")],
                Body::from_stream(stream),
            )
                .into_response();
        }
        let tail = match model {
            "empty-finish" => {
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3,\"total_tokens\":10}}\n\ndata: [DONE]\n\n"
            }
            "blank-eof" => "data: [DONE]\n\n",
            "duplicate" => {
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3,\"total_tokens\":10}}\n\ndata: [DONE]\n\n"
            }
            "conflicting" => {
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\ndata: [DONE]\n\n"
            }
            "late-finish" => {
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: {\"error\":{\"status\":429,\"code\":\"rate_limited\",\"message\":\"PRIVATE_UPSTREAM_BODY\"}}\n\ndata: [DONE]\n\n"
            }
            "late" => {
                "data: {\"error\":{\"status\":429,\"code\":\"rate_limited\",\"message\":\"PRIVATE_UPSTREAM_BODY\"}}\n\ndata: [DONE]\n\n"
            }
            "broken" => "",
            _ => {
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3,\"total_tokens\":10}}\n\ndata: [DONE]\n\n"
            }
        };
        return (
            [("content-type", "text/event-stream")],
            format!("{start}{tail}"),
        )
            .into_response();
    }
    if model == "late" {
        return Json(
            json!({"error":{"status":429,"code":"rate_limited","message":"PRIVATE_UPSTREAM_BODY"}}),
        )
        .into_response();
    }
    let finish = if model == "broken" {
        Value::Null
    } else {
        json!("stop")
    };
    Json(json!({
        "choices":[{"message":{"role":"assistant","content":"PRIVATE_OUTPUT"},"finish_reason":finish}],
        "usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}
    })).into_response()
}

/// 独立临时文件与本地服务，仅测试文件使用临时目录。
struct Fixture {
    app: Router,
    log: Arc<RecordLog>,
    directory: PathBuf,
    upstream: tokio::task::JoinHandle<()>,
}

impl Fixture {
    /// 装配完整代理与文件记录，包含一个失败 auto 候选。
    async fn new(capacity: usize) -> Self {
        Self::with_key(capacity, "").await
    }

    /// 可选开启鉴权，用同一装配验证鉴权拒收也会持久记录。
    async fn with_key(capacity: usize, api_key: &str) -> Self {
        let directory = std::env::temp_dir().join(format!("fmp-audit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let log = Arc::new(RecordLog::open(&directory.join("requests.jsonl"), capacity).unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let upstream = tokio::spawn(async move {
            let app = Router::new()
                .route("/v1/chat/completions", post(mock))
                .route(
                    "/fail/chat/completions",
                    post(|| async { (StatusCode::TOO_MANY_REQUESTS, "limited") }),
                );
            axum::serve(listener, app).await.unwrap();
        });
        let store = Store::open(std::path::Path::new(":memory:")).await.unwrap();
        for (id, suffix, models, priority) in [
            ("bad", "/fail", "ok", 1),
            (
                "mock",
                "/v1",
                "a-ok,ok,error,late,broken,slow,tool,tool-text,duplicate,conflicting,late-finish,empty-finish,blank-eof",
                2,
            ),
        ] {
            store
                .upsert_backend(&BackendRecord {
                    id: id.into(),
                    name: id.into(),
                    base_url: format!("{base}{suffix}"),
                    models: models.into(),
                    priority,
                    cost_tier: "free".into(),
                    capabilities: r#"{"context_window":200000}"#.into(),
                    ..Default::default()
                })
                .await
                .unwrap();
        }
        let state = AppState::new(
            Arc::new(Config {
                data_dir: directory.clone(),
                max_request_body_bytes: 1024,
                record_capacity: capacity,
                api_key: api_key.into(),
                ..Default::default()
            }),
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
            log,
            directory,
            upstream,
        }
    }

    /// 构造协议请求，共享固定私密标记用于检查正文不落盘。
    fn request(path: &str, model: &str, stream: bool, headers: &[(&str, &str)]) -> Request<Body> {
        let payload = if path == "/v1/responses" {
            json!({"model":model,"input":"PRIVATE_PROMPT","stream":stream,"user":"PRIVATE_USER"})
        } else {
            json!({"model":model,"messages":[{"role":"user","content":"PRIVATE_PROMPT"}],"max_tokens":64,"stream":stream,"user":"PRIVATE_USER","metadata":{"user_id":"PRIVATE_USER"}})
        };
        let mut request = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .header("authorization", "Bearer PRIVATE_KEY");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        request.body(Body::from(payload.to_string())).unwrap()
    }

    /// 完整读取响应，触发请求终态落盘。
    async fn send(
        &self,
        path: &str,
        model: &str,
        stream: bool,
        headers: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap, String) {
        let response = self
            .app
            .clone()
            .oneshot(Self::request(path, model, stream, headers))
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, headers, String::from_utf8(body.to_vec()).unwrap())
    }

    /// 发送完整历史，用于客户端会话关联；正文仍只由本地假上游消费。
    async fn send_payload(&self, path: &str, payload: Value) -> StatusCode {
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
        response.into_body().collect().await.unwrap();
        status
    }

    /// 管理查询不生成推理记录。
    async fn records(&self, query: &str) -> Value {
        let response = self
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/records{query}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
    }
}

impl Drop for Fixture {
    /// 清理本地假上游与隔离的测试目录。
    fn drop(&mut self) {
        self.upstream.abort();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// 三种协议的流式与非流式成功都保存模型、真实用量和终态，不保存私密内容。
#[tokio::test]
async fn all_protocols_persist_complete_metadata_without_bodies_or_keys() {
    let fixture = Fixture::new(1000).await;
    for path in PATHS {
        for stream in [false, true] {
            let (status, headers, _) = fixture.send(path, "mock/ok", stream, &[]).await;
            assert_eq!(status, StatusCode::OK);
            let record = &fixture.log.list(1)[0];
            assert_eq!(record.id, headers["x-request-id"]);
            assert_eq!(record.outcome, "ok");
            assert_eq!(record.requested_model, "mock/ok");
            assert_eq!(record.resolved_model, "mock/ok");
            assert_eq!(record.usage.total_tokens, Some(10));
            assert!(
                record
                    .session_id
                    .as_deref()
                    .is_some_and(|id| id.starts_with("generic:inferred_"))
            );
            assert_eq!(record.attempts.len(), 1);
            assert_eq!(record.attempts[0].parent_span_id, record.span_id);
            assert_eq!(record.attempts[0].outcome, "ok");
            assert!(record.first_event_ms.is_some());
        }
    }
    let file = std::fs::read_to_string(fixture.directory.join("requests.jsonl")).unwrap();
    assert_eq!(file.lines().count(), 6);
    for private in [
        "PRIVATE_PROMPT",
        "PRIVATE_OUTPUT",
        "PRIVATE_USER",
        "PRIVATE_KEY",
    ] {
        assert!(!file.contains(private));
    }
    assert_eq!(
        RecordLog::open(&fixture.directory.join("requests.jsonl"), 1000)
            .unwrap()
            .len(),
        6
    );
}

/// 相同结束标记合并为一次终态，尾帧中的最新用量仍必须到达三种协议和审计。
#[tokio::test]
async fn repeated_finish_is_idempotent_and_preserves_final_usage_in_all_protocols() {
    let fixture = Fixture::new(1000).await;
    for path in PATHS {
        let (status, _, body) = fixture.send(path, "mock/duplicate", true, &[]).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        let record = &fixture.log.list(1)[0];
        assert_eq!(record.outcome, "ok", "{path}: {body}");
        assert_eq!(
            record.finish_reason,
            Some(fakemodelproxy::core::FinishReason::Stop)
        );
        assert_eq!(record.usage.total_tokens, Some(10));
        assert_eq!(record.attempts.len(), 1);
        assert_eq!(record.attempts[0].outcome, "ok");
        assert!(!body.contains("upstream_duplicate_finish"), "{body}");
        let terminal = match path {
            "/v1/responses" => "event: response.completed",
            "/v1/messages" => "event: message_stop",
            _ => "\"finish_reason\":\"stop\"",
        };
        assert_eq!(body.matches(terminal).count(), 1, "{path}: {body}");
    }
}

/// WorkBuddy 生成中使用空结束字段，真实 length 终态与最终用量必须保留。
#[tokio::test]
async fn empty_finish_chunks_preserve_actual_terminal_reason_and_usage() {
    let fixture = Fixture::new(1000).await;
    for path in PATHS {
        let (status, _, body) = fixture.send(path, "mock/empty-finish", true, &[]).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        let record = &fixture.log.list(1)[0];
        assert_eq!(record.outcome, "ok", "{path}: {body}");
        assert_eq!(
            record.finish_reason,
            Some(fakemodelproxy::core::FinishReason::Length)
        );
        assert_eq!(record.usage.total_tokens, Some(10));
        assert_eq!(record.attempts.len(), 1);
        assert!(!body.contains("upstream_duplicate_finish"));
        assert!(body.contains("PRIVATE_OUTPUT"));
        let terminal = match path {
            "/v1/responses" => "event: response.incomplete",
            "/v1/messages" => "event: message_stop",
            _ => "\"finish_reason\":\"length\"",
        };
        assert_eq!(body.matches(terminal).count(), 1, "{path}: {body}");
    }
}

/// 只有空结束字段和 DONE 的截断流不能被误判为正常完成。
#[tokio::test]
async fn blank_finish_without_real_terminal_is_still_incomplete() {
    for path in PATHS {
        let fixture = Fixture::new(1000).await;
        let (_, _, body) = fixture.send(path, "mock/blank-eof", true, &[]).await;
        let record = &fixture.log.list(1)[0];
        assert_eq!(record.outcome, "error", "{path}: {body}");
        assert!(
            record
                .error
                .as_ref()
                .unwrap()
                .contains("upstream_incomplete_response")
        );
        assert!(record.finish_reason.is_none());
    }
}

/// 去重不能吞掉矛盾的结束原因或结束标记后的上游错误。
#[tokio::test]
async fn finish_normalization_keeps_conflicting_reasons_and_late_errors_visible() {
    for path in PATHS {
        for (model, code) in [
            ("conflicting", "upstream_duplicate_finish"),
            ("late-finish", "rate_limited"),
        ] {
            let fixture = Fixture::new(1000).await;
            let (_, _, body) = fixture
                .send(path, &format!("mock/{model}"), true, &[])
                .await;
            let record = &fixture.log.list(1)[0];
            assert_eq!(record.outcome, "error", "{path}/{model}: {body}");
            assert!(record.error.as_ref().unwrap().contains(code));
            assert_eq!(record.attempts[0].outcome, "error");
            assert!(record.finish_reason.is_none());
            assert!(!body.contains("event: response.completed"));
        }
    }
}

/// 建连失败、HTTP 200 内嵌错误与提前 EOF 均为失败；流的 HTTP 状态不可回写。
#[tokio::test]
async fn all_protocols_distinguish_http_errors_stream_errors_and_truncation() {
    for path in PATHS {
        for stream in [false, true] {
            for model in ["error", "late", "broken"] {
                let fixture = Fixture::new(1000).await;
                let (status, _, body) = fixture
                    .send(path, &format!("mock/{model}"), stream, &[])
                    .await;
                let record = &fixture.log.list(1)[0];
                assert_eq!(record.outcome, "error", "{path} {model}: {body}");
                assert_eq!(record.attempts[0].outcome, "error");
                let expected = if model == "error" {
                    503
                } else if stream {
                    200
                } else if model == "late" {
                    429
                } else {
                    502
                };
                assert_eq!(status.as_u16(), expected, "{path} {model}: {body}");
                assert_eq!(record.status, expected);
                if model == "late" {
                    assert_eq!(record.upstream_status, Some(429));
                }
                if model == "broken" {
                    assert!(record.error.as_ref().unwrap().contains("incomplete"));
                }
                assert!(
                    !std::fs::read_to_string(fixture.directory.join("requests.jsonl"))
                        .unwrap()
                        .contains("PRIVATE_UPSTREAM_BODY")
                );
            }
        }
    }
}

/// 同一 trace 的串行与并行请求保留父子关系；会话分组与 user 无关。
#[tokio::test]
async fn trace_parentage_and_session_filters_are_explicit_and_isolated() {
    let fixture = Fixture::new(1000).await;
    let traceparent = format!("00-{TRACE}-{PARENT}-01");
    let (_, response, _) = fixture
        .send(
            PATHS[0],
            "mock/ok",
            false,
            &[("traceparent", &traceparent), ("x-fmp-session", "room-a")],
        )
        .await;
    let first = fixture.log.list(1)[0].clone();
    assert_eq!(first.trace_id, TRACE);
    assert_eq!(first.parent_span_id.as_deref(), Some(PARENT));
    let next = response["traceparent"].to_str().unwrap();
    fixture
        .send(
            PATHS[1],
            "mock/ok",
            false,
            &[("traceparent", next), ("x-fmp-session", "room-a")],
        )
        .await;
    assert_eq!(
        fixture.log.list(1)[0].parent_span_id.as_deref(),
        Some(first.span_id.as_str())
    );
    let headers_a = [
        ("traceparent", traceparent.as_str()),
        ("x-fmp-session", "room-b"),
    ];
    let headers_b = [("traceparent", traceparent.as_str())];
    let (a, b) = tokio::join!(
        fixture.send(PATHS[0], "mock/ok", false, &headers_a),
        fixture.send(PATHS[2], "mock/ok", false, &headers_b),
    );
    assert_ne!(a.1["traceparent"], b.1["traceparent"]);
    fixture
        .send(
            PATHS[0],
            "mock/ok",
            false,
            &[(
                "traceparent",
                "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            )],
        )
        .await;
    let invalid = fixture.log.list(1)[0].clone();
    assert_ne!(invalid.trace_id, TRACE);
    assert!(invalid.parent_span_id.is_none());
    assert_eq!(
        fixture.records(&format!("?trace_id={TRACE}")).await["matched"],
        4
    );
    assert_eq!(fixture.records("?session_id=room-a").await["matched"], 2);
    assert_eq!(fixture.records("?session_id=room-b").await["matched"], 1);
    assert_eq!(
        fixture.records("?session_id=PRIVATE_USER").await["matched"],
        0
    );
}

/// 客户端原生头在三协议中自动生效，和已有 FMP 会话键隔离。
#[tokio::test]
async fn minimax_native_sessions_group_without_client_changes() {
    let fixture = Fixture::new(1000).await;
    for path in PATHS {
        for stream in [false, true] {
            assert_eq!(
                fixture
                    .send(
                        path,
                        "mock/ok",
                        stream,
                        &[("x-mavis-session-id", "native-room")]
                    )
                    .await
                    .0,
                StatusCode::OK
            );
            let record = &fixture.log.list(1)[0];
            assert_eq!(
                record.session_id.as_deref(),
                Some("minimax_code:native-room")
            );
            assert_eq!(
                record.client,
                Some(fakemodelproxy::client_plugins::ClientKind::MinimaxCode)
            );
            let session = record.session.as_ref().unwrap();
            assert_eq!(session.client_session_id.as_deref(), Some("native-room"));
            assert_eq!(
                session.confidence,
                fakemodelproxy::client_plugins::SessionConfidence::Explicit
            );
        }
    }
    fixture
        .send(
            PATHS[0],
            "mock/ok",
            false,
            &[("x-fmp-session", "native-room")],
        )
        .await;
    assert_eq!(
        fixture
            .records("?session_id=minimax_code%3Anative-room&client=minimax_code")
            .await["matched"],
        6
    );
    assert_eq!(
        fixture.records("?session_id=native-room").await["matched"],
        1
    );
    assert_eq!(
        fixture.records("?session_confidence=inferred").await["matched"],
        0
    );
    for headers in [
        vec![("x-mavis-session-id", "a"), ("x-mavis-session-id", "b")],
        vec![("x-mavis-session-id", " ")],
        vec![("x-mavis-agent-id", "agent-is-not-session")],
    ] {
        fixture
            .send(
                "/clients/minimax-code/v1/chat/completions",
                "mock/ok",
                false,
                &headers,
            )
            .await;
        assert_eq!(fixture.log.list(1)[0].session_id, None);
    }
}

/// TRAE 必须包含实际已交付输出，变化的系统提示和模型不影响历史延续。
#[tokio::test]
async fn trae_history_continuation_is_record_only_for_all_protocols() {
    let fixture = Fixture::new(1000).await;
    for path in PATHS {
        let path = format!("/clients/trae{path}");
        let first = json!([
            {"role":"system","content":"dynamic-a"},
            {"role":"user","content":format!("PRIVATE_PROMPT-{path}")}
        ]);
        let second = json!([
            {"role":"system","content":"dynamic-b"},
            first[1].clone(),
            {"role":"assistant","content":"PRIVATE_OUTPUT"},
            {"role":"user","content":"PRIVATE_NEXT"}
        ]);
        let mut payload = json!({"model":"mock/ok","messages":first,"max_tokens":64,"stream":true,
            "user":"PRIVATE_USER","metadata":{"user_id":"PRIVATE_USER"}});
        if path.ends_with("/responses") {
            payload["input"] = payload["messages"].take();
            payload.as_object_mut().unwrap().remove("messages");
        } else if path.ends_with("/messages") {
            payload["system"] =
                payload["messages"].as_array_mut().unwrap().remove(0)["content"].clone();
        }
        assert_eq!(
            fixture.send_payload(&path, payload.clone()).await,
            StatusCode::OK
        );
        let start = fixture.log.list(1)[0].clone();
        let mut payload = json!({"model":"mock/a-ok","messages":second,"max_tokens":64});
        if path.ends_with("/responses") {
            payload["input"] = payload["messages"].take();
            payload.as_object_mut().unwrap().remove("messages");
        } else if path.ends_with("/messages") {
            payload["system"] =
                payload["messages"].as_array_mut().unwrap().remove(0)["content"].clone();
        }
        assert_eq!(fixture.send_payload(&path, payload).await, StatusCode::OK);
        let next = fixture.log.list(1)[0].clone();
        assert_eq!(next.session_id, start.session_id);
        let session = next.session.unwrap();
        assert_eq!(
            session.confidence,
            fakemodelproxy::client_plugins::SessionConfidence::Inferred
        );
        assert_eq!(
            session.source,
            fakemodelproxy::client_plugins::SessionSource::HistoryContinuation
        );
        assert_eq!(session.previous_request_id, Some(start.id));
        assert_eq!(session.routing_session(), None);
        assert_eq!(session.client_session_id, None);
    }
    let file = std::fs::read_to_string(fixture.directory.join("requests.jsonl")).unwrap();
    for private in [
        "PRIVATE_PROMPT",
        "PRIVATE_OUTPUT",
        "PRIVATE_USER",
        "PRIVATE_NEXT",
    ] {
        assert!(!file.contains(private));
    }
    assert_eq!(
        fixture
            .records("?client=trae&session_confidence=inferred")
            .await["matched"],
        6
    );
    let recovered = RecordLog::open(&fixture.directory.join("requests.jsonl"), 1000).unwrap();
    assert!(recovered.list(1)[0].session.is_some());
}

/// 客户端读到协议终止事件即可延续历史；仅读部分输出仍不发布会话证据。
#[tokio::test]
async fn trae_protocol_completion_publishes_history_without_waiting_for_http_eof() {
    for path in PATHS {
        for (model, responses_terminal) in [
            ("mock/ok", "event: response.completed"),
            ("mock/empty-finish", "event: response.incomplete"),
        ] {
            for read_terminal in [true, false] {
                let fixture = Fixture::new(1000).await;
                let path = format!("/clients/trae{path}");
                let response = fixture
                    .app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri(&path)
                            .header("content-type", "application/json")
                            .body(Body::from(
                                json!({"model":model,"input":"PRIVATE_PROMPT",
                                "messages":[{"role":"user","content":"PRIVATE_PROMPT"}],
                                "max_tokens":64,"stream":true})
                                .to_string(),
                            ))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let mut body = response.into_body().into_data_stream();
                let terminal = if path.ends_with("/responses") {
                    responses_terminal
                } else if path.ends_with("/messages") {
                    "event: message_stop"
                } else {
                    "data: [DONE]"
                };
                let mut received_terminal = false;
                while let Some(chunk) = body.next().await {
                    let chunk = chunk.unwrap();
                    received_terminal |= String::from_utf8_lossy(&chunk).contains(terminal);
                    if !read_terminal || received_terminal {
                        break;
                    }
                }
                assert_eq!(received_terminal, read_terminal);
                drop(body);
                let start = fixture.log.list(1)[0].clone();
                assert_eq!(
                    start.outcome,
                    if read_terminal { "ok" } else { "cancelled" },
                    "{path}"
                );
                let messages = json!([
                    {"role":"user","content":"PRIVATE_PROMPT"},
                    {"role":"assistant","content":"PRIVATE_OUTPUT"},
                    {"role":"user","content":"PRIVATE_NEXT"}
                ]);
                assert_eq!(
                    fixture
                        .send_payload(
                            &path,
                            json!({"model":"mock/ok","input":messages,"messages":messages,
                            "max_tokens":64}),
                        )
                        .await,
                    StatusCode::OK
                );
                let next = fixture.log.list(1)[0].clone();
                assert_eq!(next.session_id == start.session_id, read_terminal, "{path}");
                let session = next.session.unwrap();
                assert_eq!(
                    session.source,
                    if read_terminal {
                        fakemodelproxy::client_plugins::SessionSource::HistoryContinuation
                    } else {
                        fakemodelproxy::client_plugins::SessionSource::HistoryNew
                    }
                );
            }
        }
    }
}

/// 相同首轮不合并，重复历史的歧义拆组；相似历史不能代替实际输出。
#[tokio::test]
async fn trae_identical_starts_and_ambiguous_histories_stay_separate() {
    let fixture = Fixture::new(1000).await;
    let path = "/clients/trae/v1/chat/completions";
    let first = json!({"model":"mock/ok","messages":[{"role":"user","content":"same start"}]});
    fixture.send_payload(path, first.clone()).await;
    let a = fixture.log.list(1)[0].clone();
    fixture.send_payload(path, first).await;
    let b = fixture.log.list(1)[0].clone();
    assert_ne!(a.session_id, b.session_id);
    let second = json!({"model":"mock/ok","messages":[
        {"role":"user","content":"same start"},
        {"role":"assistant","content":"PRIVATE_OUTPUT"},
        {"role":"user","content":"continue"}
    ]});
    fixture.send_payload(path, second.clone()).await;
    let ambiguous = fixture.log.list(1)[0].clone();
    assert_ne!(ambiguous.session_id, a.session_id);
    assert_ne!(ambiguous.session_id, b.session_id);
    assert_eq!(
        ambiguous.session.as_ref().unwrap().source,
        fakemodelproxy::client_plugins::SessionSource::HistoryAmbiguous
    );
    let mut third = second.clone();
    let messages = third["messages"].as_array_mut().unwrap();
    messages.push(json!({"role":"assistant","content":"PRIVATE_OUTPUT"}));
    messages.push(json!({"role":"user","content":"continue again"}));
    fixture.send_payload(path, third).await;
    assert_eq!(fixture.log.list(1)[0].session_id, ambiguous.session_id);
    let mut unrelated = second;
    unrelated["messages"][1]["content"] = json!("different output");
    fixture.send_payload(path, unrelated).await;
    assert_eq!(
        fixture.log.list(1)[0].session.as_ref().unwrap().source,
        fakemodelproxy::client_plugins::SessionSource::HistoryNew
    );
}

/// 流中失败和客户端取消没有完整交付证据，后续请求不得被误关联。
#[tokio::test]
async fn trae_failed_and_cancelled_responses_do_not_publish_history() {
    let fixture = Fixture::new(1000).await;
    let path = "/clients/trae/v1/chat/completions";
    fixture.send(path, "mock/broken", true, &[]).await;
    let failed = fixture.log.list(1)[0].clone();
    fixture
        .send_payload(
            path,
            json!({"model":"mock/ok","messages":[
                {"role":"user","content":"PRIVATE_PROMPT"},
                {"role":"assistant","content":"PRIVATE_OUTPUT"},
                {"role":"user","content":"continue"}
            ]}),
        )
        .await;
    assert_ne!(fixture.log.list(1)[0].session_id, failed.session_id);

    let response = fixture
        .app
        .clone()
        .oneshot(Fixture::request(path, "mock/slow", true, &[]))
        .await
        .unwrap();
    let mut stream = response.into_body().into_data_stream();
    stream.next().await.unwrap().unwrap();
    drop(stream);
    let cancelled = fixture.log.list(1)[0].clone();
    assert_eq!(cancelled.outcome, "cancelled");
    fixture
        .send_payload(
            path,
            json!({"model":"mock/ok","messages":[
                {"role":"user","content":"PRIVATE_PROMPT"},
                {"role":"assistant","content":"PRIVATE_OUTPUT"},
                {"role":"user","content":"another continuation"}
            ]}),
        )
        .await;
    assert_ne!(fixture.log.list(1)[0].session_id, cancelled.session_id);
    assert_eq!(
        fixture.log.list(1)[0].session.as_ref().unwrap().source,
        fakemodelproxy::client_plugins::SessionSource::HistoryNew
    );
}

/// 工具输出按归一结构匹配，JSON 参数排版差异不拆组。
#[tokio::test]
async fn trae_tool_calls_preserve_history_continuity() {
    let fixture = Fixture::new(1000).await;
    let path = "/clients/trae/v1/chat/completions";
    for stream in [false, true] {
        let prompt = format!("tool request {stream}");
        assert_eq!(
            fixture
                .send_payload(
                    path,
                    json!({"model":"mock/tool","stream":stream,
            "messages":[{"role":"user","content":prompt}]})
                )
                .await,
            StatusCode::OK
        );
        let first = fixture.log.list(1)[0].clone();
        assert_eq!(fixture.send_payload(path, json!({"model":"mock/ok","messages":[
            {"role":"user","content":prompt},
            {"role":"assistant","tool_calls":[{"id":"call_echo","type":"function",
                "function":{"name":"echo","arguments":"{ \"text\": \"PRIVATE_TOOL\" }"}}]},
            {"role":"tool","tool_call_id":"call_echo","content":"PRIVATE_RESULT"}
        ]})).await, StatusCode::OK);
        assert_eq!(fixture.log.list(1)[0].session_id, first.session_id);
    }
    let file = std::fs::read_to_string(fixture.directory.join("requests.jsonl")).unwrap();
    assert!(!file.contains("PRIVATE_TOOL"));
    assert!(!file.contains("PRIVATE_RESULT"));
}

/// 三协议及流式输出均容忍 TRAE 重写工具 ID，且匹配最近完成的工具请求。
#[tokio::test]
async fn trae_rewritten_tool_ids_match_latest_delivered_reply() {
    for stream in [false, true] {
        let fixture = Fixture::new(1000).await;
        for path in [
            "/clients/trae/v1/chat/completions",
            "/clients/trae/v1/messages",
            "/clients/trae/v1/responses",
        ] {
            let prompt = format!("PRIVATE_REWRITE-{path}-{stream}");
            let input = json!([{"role":"user","content":prompt}]);
            let mut start =
                json!({"model":"mock/tool-text","stream":stream,"max_tokens":64,"messages":input});
            if path.ends_with("/responses") {
                start["input"] = start["messages"].take();
                start.as_object_mut().unwrap().remove("messages");
            }
            assert_eq!(fixture.send_payload(path, start).await, StatusCode::OK);
            let first = fixture.log.list(1)[0].clone();
            let mut next = if path.ends_with("/messages") {
                json!({"messages":[input[0].clone(), {"role":"assistant","content":[
                    {"type":"text","text":"PRIVATE_OUTPUT"},
                    {"type":"tool_use","id":"client_rewritten","name":"echo","input":{"text":"PRIVATE_TOOL"}}
                ]}, {"role":"user","content":[{"type":"tool_result","tool_use_id":"client_rewritten","content":"PRIVATE_RESULT"}]}]})
            } else if path.ends_with("/responses") {
                json!({"input":[input[0].clone(), {"role":"assistant","content":"PRIVATE_OUTPUT"},
                    {"type":"function_call","call_id":"client_rewritten","name":"echo","arguments":"{ \"text\": \"PRIVATE_TOOL\" }"},
                    {"type":"function_call_output","call_id":"client_rewritten","output":"PRIVATE_RESULT"}]})
            } else {
                json!({"messages":[input[0].clone(), {"role":"assistant","content":"PRIVATE_OUTPUT"},
                    {"role":"assistant","tool_calls":[{"id":"client_rewritten","type":"function","function":{"name":"echo","arguments":"{ \"text\": \"PRIVATE_TOOL\" }"}}]},
                    {"role":"tool","tool_call_id":"client_rewritten","content":"PRIVATE_RESULT"}]})
            };
            next["model"] = json!("mock/ok");
            next["max_tokens"] = json!(64);
            assert_eq!(fixture.send_payload(path, next).await, StatusCode::OK);
            let record = fixture.log.list(1)[0].clone();
            assert_eq!(record.session_id, first.session_id, "{path}");
            let session = record.session.unwrap();
            assert_eq!(session.previous_request_id, Some(first.id));
            let evidence = session.evidence.unwrap();
            assert_eq!(evidence.matched_messages, 2);
            assert_eq!(evidence.normalized_tool_ids, 1);
        }
        let file = std::fs::read_to_string(fixture.directory.join("requests.jsonl")).unwrap();
        for private in [
            "PRIVATE_REWRITE",
            "PRIVATE_TOOL",
            "PRIVATE_RESULT",
            "client_rewritten",
        ] {
            assert!(!file.contains(private));
        }
    }
}

/// auto 的失败候选与成功候选分别保存子 span，最终请求仍成功。
#[tokio::test]
async fn auto_fallback_keeps_both_attempt_spans() {
    let fixture = Fixture::new(1000).await;
    assert_eq!(
        fixture.send(PATHS[0], "auto", false, &[]).await.0,
        StatusCode::OK
    );
    let record = &fixture.log.list(1)[0];
    assert_eq!(record.outcome, "ok");
    assert_eq!(record.resolved_model, "mock/a-ok");
    assert_eq!(record.attempts.len(), 2);
    assert_eq!(record.attempts[0].model, "bad/ok");
    assert_eq!(record.attempts[0].upstream_status, Some(429));
    assert_eq!(record.attempts[0].outcome, "error");
    assert_eq!(record.attempts[1].outcome, "ok");
    assert_ne!(record.attempts[0].span_id, record.attempts[1].span_id);
}

/// JSON 拒收与分块 413 也必须留记录，不调用上游。
#[tokio::test]
async fn rejected_requests_are_audited_for_all_protocols() {
    let fixture = Fixture::new(1000).await;
    for path in PATHS {
        for bytes in [b"{broken".to_vec(), vec![b' '; 1025]] {
            let expected = if bytes.len() > 1024 { 413 } else { 400 };
            let chunks = vec![Ok::<_, std::io::Error>(Bytes::from(bytes))];
            let response = fixture
                .app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .body(Body::from_stream(futures_util::stream::iter(chunks)))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), expected);
            assert!(response.headers().contains_key("traceparent"));
            response.into_body().collect().await.unwrap();
            let record = &fixture.log.list(1)[0];
            assert_eq!(record.outcome, "error");
            assert_eq!(record.status, expected);
            assert!(record.attempts.is_empty());
            assert_eq!(
                record.request_body_bytes,
                Some(if expected == 413 { 1025 } else { 7 })
            );
            assert_eq!(record.request_body_complete, Some(expected != 413));
            assert_eq!(record.request_body_limit_bytes, Some(1024));
            assert_eq!(record.request_content_length, None);
        }
    }
}

/// 新计数字段在旧 JSONL 中保持未知，不能把缺失数据解释为零。
#[test]
fn legacy_request_records_keep_unknown_request_sizes() {
    let record: RequestRecord =
        serde_json::from_str(r#"{"id":"legacy","input_chars":10,"status":400,"outcome":"error"}"#)
            .unwrap();
    assert_eq!(record.request_body_bytes, None);
    assert_eq!(record.request_body_complete, None);
    assert_eq!(record.request_body_limit_bytes, None);
    assert_eq!(record.image_count, None);
    assert_eq!(record.image_url_bytes, None);
    assert_eq!(record.message_count, None);
    assert!(record.routing_rejections.is_empty());
}

/// 浏览器 favicon 与未注册路径不是推理请求，不应挤占审计容量。
#[tokio::test]
async fn non_inference_paths_do_not_create_request_records() {
    let fixture = Fixture::new(10).await;
    for path in ["/favicon.ico", "/health", "/api/console/status"] {
        let response = fixture
            .app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        response.into_body().collect().await.unwrap();
    }
    assert!(fixture.log.is_empty());
}

/// 鉴权失败也关联 trace，且持久文件不保存提供的凭证。
#[tokio::test]
async fn authorization_failures_are_persisted_without_credentials() {
    let fixture = Fixture::with_key(10, "CORRECT_PRIVATE_KEY").await;
    for path in PATHS {
        let (status, headers, _) = fixture.send(path, "mock/ok", false, &[]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(headers.contains_key("traceparent"));
        let record = &fixture.log.list(1)[0];
        assert_eq!(record.outcome, "error");
        assert!(record.error.as_ref().unwrap().contains("invalid_api_key"));
        assert!(record.attempts.is_empty());
    }
    let file = std::fs::read_to_string(fixture.directory.join("requests.jsonl")).unwrap();
    assert!(!file.contains("PRIVATE_KEY"));
}

/// 非法 W3C 头不得让全零或重复 ID 污染其他请求的 trace。
#[tokio::test]
async fn malformed_or_duplicate_traceparent_starts_new_traces() {
    let fixture = Fixture::new(10).await;
    for value in [
        format!("00-{TRACE}-0000000000000000-00"),
        format!("00-{}-{PARENT}-01", TRACE.to_uppercase()),
        format!("ff-{TRACE}-{PARENT}-01"),
        format!("00-{TRACE}-{PARENT}-01-extra"),
        format!("00-{TRACE}-{PARENT}-xx"),
    ] {
        fixture
            .send(PATHS[0], "mock/ok", false, &[("traceparent", &value)])
            .await;
        let record = &fixture.log.list(1)[0];
        assert_ne!(record.trace_id, TRACE);
        assert!(record.parent_span_id.is_none());
    }
    let valid = format!("00-{TRACE}-{PARENT}-01");
    fixture
        .send(
            PATHS[0],
            "mock/ok",
            false,
            &[("traceparent", &valid), ("traceparent", &valid)],
        )
        .await;
    assert_ne!(fixture.log.list(1)[0].trace_id, TRACE);
}

/// 流式响应尚未结束时丢弃 body，在三个协议都保存 cancelled。
#[tokio::test]
async fn dropping_active_stream_persists_cancellation() {
    let fixture = Fixture::new(1000).await;
    for path in PATHS {
        let response = fixture
            .app
            .clone()
            .oneshot(Fixture::request(path, "mock/slow", true, &[]))
            .await
            .unwrap();
        assert_eq!(fixture.log.list(1)[0].outcome, "running");
        drop(response);
        assert_eq!(fixture.log.list(1)[0].outcome, "cancelled");
        let restored = RecordLog::open(&fixture.directory.join("requests.jsonl"), 1000).unwrap();
        assert_eq!(restored.list(1)[0].outcome, "cancelled");
    }
}

/// 裁剪、清空与重启恢复都作用于文件；进行中记录恢复为 interrupted。
#[tokio::test]
async fn restart_trims_recovers_interruption_and_reports_bad_lines() {
    let fixture = Fixture::new(3).await;
    for _ in 0..5 {
        fixture.send(PATHS[0], "mock/ok", false, &[]).await;
    }
    assert_eq!(fixture.log.len(), 3);
    fixture.log.push(RequestRecord {
        id: "unfinished".into(),
        outcome: "running".into(),
        ..Default::default()
    });
    let file = fixture.directory.join("requests.jsonl");
    std::fs::OpenOptions::new()
        .append(true)
        .open(&file)
        .unwrap()
        .write_all(b"{damaged\n")
        .unwrap();
    let restored = RecordLog::open(&file, 2).unwrap();
    assert_eq!(restored.len(), 2);
    assert_eq!(restored.list(1)[0].outcome, "interrupted");
    assert_eq!(restored.persistence().recovered_invalid_lines, 1);
    assert_eq!(std::fs::read_to_string(&file).unwrap().lines().count(), 2);
    restored.clear();
    assert!(RecordLog::open(&file, 2).unwrap().is_empty());
}

/// 并发更新后的文件不丢新请求；容量外请求的迟到终态不重新插入。
#[tokio::test]
async fn concurrent_snapshots_remain_valid_and_evicted_requests_stay_evicted() {
    let fixture = Fixture::new(10).await;
    let responses = futures_util::future::join_all(
        (0..10).map(|_| fixture.send(PATHS[0], "mock/ok", false, &[])),
    )
    .await;
    assert!(responses.iter().all(|result| result.0 == StatusCode::OK));
    let restored = RecordLog::open(&fixture.directory.join("requests.jsonl"), 10).unwrap();
    assert_eq!(restored.len(), 10);
    assert!(
        restored
            .list(10)
            .iter()
            .all(|record| record.outcome == "ok")
    );
    let ids: std::collections::HashSet<_> = restored
        .list(10)
        .into_iter()
        .map(|record| record.id)
        .collect();
    assert_eq!(ids.len(), 10);
    let oldest = fixture.log.list(10).last().unwrap().clone();
    fixture.send(PATHS[0], "mock/ok", false, &[]).await;
    fixture.log.update(oldest.clone());
    assert!(
        !fixture
            .log
            .list(10)
            .iter()
            .any(|record| record.id == oldest.id)
    );
}

/// 写失败公开错误且不覆盖已保存快照，后续成功写入恢复完整索引。
#[tokio::test]
async fn disk_failure_is_visible_and_recoverable_without_erasing_saved_data() {
    let fixture = Fixture::new(10).await;
    fixture.send(PATHS[0], "mock/ok", false, &[]).await;
    let path = fixture.directory.join("requests.jsonl");
    let saved = std::fs::read_to_string(&path).unwrap();
    let blocked = path.with_extension("jsonl.tmp");
    std::fs::create_dir(&blocked).unwrap();
    fixture.send(PATHS[0], "mock/ok", false, &[]).await;
    assert!(fixture.records("").await["persistence"]["last_error"].is_string());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), saved);
    assert!(RecordLog::open(&path, 10).is_err());
    std::fs::remove_dir(&blocked).unwrap();
    fixture.send(PATHS[0], "mock/ok", false, &[]).await;
    assert!(fixture.records("").await["persistence"]["last_error"].is_null());
    assert_eq!(RecordLog::open(&path, 10).unwrap().len(), 3);
}
