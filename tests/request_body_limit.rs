//! 请求体上限回归：完整长历史透传、精确字节边界和无长度头的分块请求。

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use fakemodelproxy::backend::Registry;
use fakemodelproxy::config::{Config, DEFAULT_MAX_REQUEST_BODY_BYTES};
use fakemodelproxy::records::RecordLog;
use fakemodelproxy::state::AppState;
use fakemodelproxy::store::{BackendRecord, Store};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::{Arc, RwLock};
use tokio::sync::Mutex;
use tower::ServiceExt;

const PATHS: [&str; 3] = ["/v1/chat/completions", "/v1/responses", "/v1/messages"];

/// 保存摘要与消息规模，不在断言失败时打印数兆字节的正文。
struct Captured {
    first_bytes: usize,
    first_digest: String,
    messages: usize,
    last_text: String,
    stream: bool,
}

/// 假上游明确放开自身限制，避免把上游框架的默认限制当作代理限制。
async fn upstream(
    State(seen): State<Arc<Mutex<Vec<Captured>>>>,
    Json(body): Json<Value>,
) -> Response {
    let messages = body["messages"].as_array().unwrap();
    let first = messages[0]["content"].as_str().unwrap();
    let stream = body["stream"] == true;
    seen.lock().await.push(Captured {
        first_bytes: first.len(),
        first_digest: format!("{:x}", md5::compute(first)),
        messages: messages.len(),
        last_text: messages.last().unwrap()["content"].as_str().unwrap().into(),
        stream,
    });
    if stream {
        let frame =
            json!({"choices":[{"index":0,"delta":{"content":"accepted"},"finish_reason":"stop"}]});
        (
            [("content-type", "text/event-stream")],
            format!("data: {frame}\n\ndata: [DONE]\n\n"),
        )
            .into_response()
    } else {
        Json(json!({
            "choices":[{"message":{"role":"assistant","content":"accepted"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
        })).into_response()
    }
}

/// 真实代理路由连接本地假上游，全部测试都不使用厂商账号或真实模型。
struct Fixture {
    app: Router,
    seen: Arc<Mutex<Vec<Captured>>>,
    log: Arc<RecordLog>,
    upstream: tokio::task::JoinHandle<()>,
}

impl Fixture {
    /// 使用显式字节上限构造代理，所有协议共享同一配置。
    async fn new(limit: usize) -> Self {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mock = Router::new()
            .route("/v1/chat/completions", post(upstream))
            .layer(DefaultBodyLimit::max(DEFAULT_MAX_REQUEST_BODY_BYTES))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let upstream = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        let store = Store::open(std::path::Path::new(":memory:")).await.unwrap();
        store
            .upsert_backend(&BackendRecord {
                id: "mock".into(),
                name: "Mock".into(),
                base_url: base,
                models: "model".into(),
                ..BackendRecord::default()
            })
            .await
            .unwrap();
        let log = Arc::new(RecordLog::default());
        let state = AppState::new(
            Arc::new(Config {
                max_request_body_bytes: limit,
                ..Config::default()
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
            seen,
            log,
            upstream,
        }
    }

    /// 分别模拟有 Content-Length 的请求和无法预知总长度的流式请求体。
    async fn send(&self, path: &str, bytes: &[u8], chunked: bool) -> (StatusCode, String) {
        let mut builder = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json");
        let body = if chunked {
            let chunks: Vec<_> = bytes
                .chunks(700)
                .map(|chunk| Ok::<_, std::io::Error>(Bytes::copy_from_slice(chunk)))
                .collect();
            Body::from_stream(futures_util::stream::iter(chunks))
        } else {
            builder = builder.header("content-length", bytes.len());
            Body::from(bytes.to_vec())
        };
        let response = self
            .app
            .clone()
            .oneshot(builder.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }
}

impl Drop for Fixture {
    /// 退出测试时停止假上游，不遗留本机监听器。
    fn drop(&mut self) {
        self.upstream.abort();
    }
}

/// 为三个协议构造相同历史，字节大小变化不改变消息语义。
fn payload(path: &str, text: &str, stream: bool) -> Value {
    let messages = json!([
        {"role":"user","content":text},
        {"role":"assistant","content":"ack"},
        {"role":"user","content":"tail-marker"}
    ]);
    if path == "/v1/responses" {
        json!({"model":"mock/model","input":messages,"stream":stream})
    } else {
        json!({"model":"mock/model","messages":messages,"max_tokens":64,"stream":stream})
    }
}

/// 超过旧 2 MiB 上限的多字节历史，在三协议的流式和非流式请求中完整到达上游。
#[tokio::test]
async fn long_history_above_two_mib_reaches_upstream_without_truncation() {
    let fixture = Fixture::new(Config::default().max_request_body_bytes).await;
    let text = format!("history-start{}history-end", "长".repeat(1024 * 1024));
    let digest = format!("{:x}", md5::compute(&text));
    for path in PATHS {
        for stream in [false, true] {
            let body = payload(path, &text, stream).to_string();
            assert!(body.len() > 2 * 1024 * 1024);
            let (status, response) = fixture.send(path, body.as_bytes(), false).await;
            assert_eq!(status, StatusCode::OK, "{path}: {response}");
            assert!(response.contains("accepted"));
            let seen = fixture.seen.lock().await;
            let latest = seen.last().unwrap();
            assert_eq!(latest.first_bytes, text.len());
            assert_eq!(latest.first_digest, digest);
            assert_eq!(latest.messages, 3);
            assert_eq!(latest.last_text, "tail-marker");
            assert_eq!(latest.stream, stream);
        }
    }
    assert_eq!(fixture.seen.lock().await.len(), 6);
}

/// 上限处允许完整请求，多一个字节即拒收；无长度头的请求不能绕过限制。
#[tokio::test]
async fn configured_byte_boundary_returns_protocol_json_and_never_calls_upstream() {
    const LIMIT: usize = 1024;
    let fixture = Fixture::new(LIMIT).await;
    for path in PATHS {
        for chunked in [false, true] {
            let mut body = payload(path, "boundary", false).to_string().into_bytes();
            assert!(body.len() < LIMIT);
            body.resize(LIMIT, b' ');
            assert_eq!(fixture.send(path, &body, chunked).await.0, StatusCode::OK);
            let record = &fixture.log.list(1)[0];
            assert_eq!(record.request_body_bytes, Some(LIMIT));
            assert_eq!(record.request_body_complete, Some(true));
            assert_eq!(record.request_body_limit_bytes, Some(LIMIT));
            assert_eq!(
                record.request_content_length,
                if chunked { None } else { Some(LIMIT as u64) }
            );
            body.push(b' ');
            let (status, response) = fixture.send(path, &body, chunked).await;
            assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
            let error: Value = serde_json::from_str(&response).unwrap();
            assert!(
                error["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("1024 bytes")
            );
            assert!(
                error["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("FMP_MAX_REQUEST_BODY_MB")
            );
            if path == "/v1/messages" {
                assert_eq!(error["type"], "error");
                assert_eq!(error["error"]["type"], "request_too_large");
            } else {
                assert_eq!(error["error"]["code"], "request_body_too_large");
            }
            let record = &fixture.log.list(1)[0];
            assert_eq!(record.request_body_bytes, Some(LIMIT + 1));
            assert_eq!(record.request_body_complete, Some(false));
            assert_eq!(record.image_count, None);
            assert!(record.attempts.is_empty());
            body.resize(3 * LIMIT, b' ');
            assert_eq!(
                fixture.send(path, &body, chunked).await.0,
                StatusCode::PAYLOAD_TOO_LARGE
            );
            let record = &fixture.log.list(1)[0];
            assert_eq!(
                record.request_body_bytes,
                Some(if chunked { 1400 } else { 3 * LIMIT })
            );
            assert_eq!(record.request_body_complete, Some(false));
            assert_eq!(
                record.request_content_length,
                if chunked {
                    None
                } else {
                    Some(3 * LIMIT as u64)
                }
            );
        }
    }
    assert_eq!(fixture.seen.lock().await.len(), 6);
}

/// 对话请求放宽不会同时扩大管理入口，普通 JSON 格式错误也不能误报 413。
#[tokio::test]
async fn management_limits_and_malformed_json_remain_independent() {
    let fixture = Fixture::new(DEFAULT_MAX_REQUEST_BODY_BYTES).await;
    let body = json!({"padding":"x".repeat(3 * 1024 * 1024)}).to_string();
    assert_eq!(
        fixture
            .send("/api/config/import", body.as_bytes(), false)
            .await
            .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    for path in PATHS {
        let (status, response) = fixture.send(path, b"{broken", false).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(serde_json::from_str::<Value>(&response).unwrap()["error"].is_object());
    }
    assert!(fixture.seen.lock().await.is_empty());
}
