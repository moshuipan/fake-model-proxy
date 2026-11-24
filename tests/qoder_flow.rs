//! Qoder 运行时回归：队列错误必须及时返回，不能隐藏等待或影响账号其他请求。

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::Response;
use axum::routing::post;
use fakemodelproxy::accounts::Account;
use fakemodelproxy::backend::Registry;
use fakemodelproxy::config::Config;
use fakemodelproxy::state::AppState;
use fakemodelproxy::store::{BackendRecord, Store};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tower::ServiceExt;

/// 模拟上游响应模式，并统计实际接收到的请求数量。
struct Upstream {
    mode: Mutex<&'static str>,
    requests: AtomicUsize,
    users: Mutex<Vec<String>>,
}

/// 构造与真实上游一致的三层资源限制错误，排队字段保留明确布尔值。
fn resource_error(queued: bool) -> Value {
    let details = json!({
        "errorCode":0, "isQueued":queued, "modelKey":"auto", "queueCount":1,
        "queueType":"p4", "retryAfterSeconds":if queued {60} else {0},
        "serviceAvailable":queued, "subCode":"RESOURCE_LIMIT", "waitTime":60,
    });
    json!({"code":"403", "message":json!({
        "code":"10605", "message":details.to_string(), "subCode":"RESOURCE_LIMIT",
    }).to_string()})
}

/// 假上游可返回 HTTP 或 SSE 队列错误，以及后续成功的生成。
async fn upstream(State(state): State<Arc<Upstream>>, headers: HeaderMap) -> Response {
    state.requests.fetch_add(1, Ordering::SeqCst);
    let mode = *state.mode.lock().unwrap();
    let user = headers["cosy-user"].to_str().unwrap();
    state.users.lock().unwrap().push(user.into());
    if matches!(mode, "same-frame-usage" | "tail-usage" | "finish-event") {
        let usage = json!({"prompt_tokens":12,"completion_tokens":7,"total_tokens":19});
        let first = json!({"choices":[{"delta":{"content":"完成"},"finish_reason":"stop"}],
            "usage":if mode=="same-frame-usage" {usage.clone()} else {Value::Null}});
        let mut body = format!("data: [NOT_EXCEED_QUOTA]\r\n\r\ndata: {first}\r\n\r\n");
        if mode != "same-frame-usage" {
            body.push_str(&format!(
                "data: {}\r\n\r\n",
                json!({"choices":[],"usage":usage})
            ));
        }
        body.push_str("data: [NOTIFICATIONS]#metadata\n\n");
        body.push_str(if mode == "finish-event" {
            "event: finish\n\n"
        } else {
            "data: [DONE]\n\n"
        });
        return Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from(body))
            .unwrap();
    }
    if mode == "invalid-inner" {
        return Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from(
                "data: {\"statusCodeValue\":200,\"body\":\"broken\"}\n\n",
            ))
            .unwrap();
    }
    if mode == "http-limit" || (mode == "first-account-limit" && user == "fake-user") {
        return Response::builder()
            .status(429)
            .header("retry-after", "37")
            .body(Body::from("account rate limit"))
            .unwrap();
    }
    let detail = resource_error(mode != "restricted");
    if mode == "http-queue" {
        return Response::builder()
            .status(403)
            .header("content-type", "application/json")
            .body(Body::from(detail.to_string()))
            .unwrap();
    }
    let envelope = match mode {
        "success" | "first-account-limit" => json!({"statusCodeValue":200,"body":json!({
            "choices":[{"delta":{"content":"连接成功"},"finish_reason":"stop"}],
        }).to_string()}),
        "numeric-queue" => json!({"code":403,"message":detail["message"]}),
        _ => json!({"statusCodeValue":403,"body":detail.to_string()}),
    };
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from(format!("data: {envelope}\n\n")))
        .unwrap()
}

/// 完成同帧、尾随用量与显式 finish 事件都保留用量，混合换行不丢帧。
#[tokio::test]
async fn completion_preserves_same_frame_and_trailing_usage() {
    for mode in ["same-frame-usage", "tail-usage", "finish-event"] {
        let fixture = Fixture::new(mode).await;
        let (status, body) = fixture
            .send(
                "/v1/chat/completions",
                json!({
                    "model":"qoder/auto","messages":[{"role":"user","content":"hi"}]
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["choices"][0]["message"]["content"], "完成");
        assert_eq!(body["usage"]["prompt_tokens"], 12);
        assert_eq!(body["usage"]["completion_tokens"], 7);
        assert_eq!(body["usage"]["total_tokens"], 19);
        assert_eq!(fixture.upstream.requests.load(Ordering::SeqCst), 1);
    }
}

/// 损坏的内层信封不能静默丢弃，执行未知不重放。
#[tokio::test]
async fn malformed_inner_envelope_reports_error_without_replay() {
    let fixture = Fixture::new("invalid-inner").await;
    let (_, body) = fixture
        .send(
            "/v1/chat/completions",
            json!({
                "model":"qoder/auto","messages":[{"role":"user","content":"hi"}]
            }),
        )
        .await;
    assert!(body.contains("invalid_upstream_chunk"), "{body}");
    assert_eq!(fixture.upstream.requests.load(Ordering::SeqCst), 1);
}

/// 隔离的本地代理，所有凭证均为假数据，不接触用户的账号或数据库。
struct Fixture {
    app: Router,
    upstream: Arc<Upstream>,
    state: AppState,
}

impl Fixture {
    /// 启动假上游，通过凭证端点覆盖将真实 Qoder 适配器指向它。
    async fn new(mode: &'static str) -> Self {
        Self::with_accounts(mode, &[("fake-account", "fake-user")]).await
    }

    /// 激活配置前准备独立假账号，确保测试使用同一完整运行快照。
    async fn with_accounts(mode: &'static str, accounts: &[(&str, &str)]) -> Self {
        let upstream_state = Arc::new(Upstream {
            mode: Mutex::new(mode),
            requests: AtomicUsize::new(0),
            users: Mutex::new(Vec::new()),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/chat", listener.local_addr().unwrap());
        let upstream_app = Router::new()
            .route("/chat", post(upstream))
            .with_state(upstream_state.clone());
        tokio::spawn(async move {
            axum::serve(listener, upstream_app).await.unwrap();
        });
        let store = Store::open(std::path::Path::new(":memory:")).await.unwrap();
        store
            .upsert_backend(&BackendRecord {
                id: "qoder".into(),
                name: "Qoder".into(),
                kind: "qoder".into(),
                base_url: "https://api1.qoder.sh".into(),
                models: "auto,efficient".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        for (id, user) in accounts {
            store
                .upsert_account(&Account {
                    id: (*id).into(),
                    name: (*id).into(),
                    backend_id: "qoder".into(),
                    enabled: true,
                    credential: json!({
                        "uid":user,"device_token":"fake-device-token","endpoint":endpoint,
                    })
                    .to_string(),
                })
                .await
                .unwrap();
        }
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
        Self {
            app: fakemodelproxy::build_router(state.clone()),
            upstream: upstream_state,
            state,
        }
    }

    /// 通过完整 HTTP 路由执行请求；两秒内未完成即证明隐藏等待仍存在。
    async fn send(&self, path: &str, payload: Value) -> (StatusCode, String) {
        let (status, _, body) = self.send_with_headers(path, payload).await;
        (status, body)
    }

    /// 保留响应头并收完整正文，检查限流及其恢复提示在协议转换后是否一致。
    async fn send_with_headers(
        &self,
        path: &str,
        payload: Value,
    ) -> (StatusCode, HeaderMap, String) {
        tokio::time::timeout(Duration::from_secs(2), async {
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
            let headers = response.headers().clone();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
        })
        .await
        .expect("Qoder 队列错误应立即返回，不能等待 60 秒后重试")
    }
}

/// 普通 HTTP 429 在三协议及流式入口保留恢复时间，并对整个账号冷却。
#[tokio::test]
async fn http_rate_limit_preserves_account_cooldown_for_all_protocols() {
    for path in [
        "/v1/chat/completions",
        "/v1/messages",
        "/clients/trae/v1/responses",
    ] {
        for stream in [false, true] {
            let fixture = Fixture::new("http-limit").await;
            let payload = json!({
                "model":"qoder/auto","messages":[{"role":"user","content":"hi"}],
                "input":"hi","max_tokens":32,"stream":stream,
            });
            let (status, headers, body) = fixture.send_with_headers(path, payload.clone()).await;
            assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{path}: {body}");
            assert_eq!(headers["retry-after"], "37");
            assert!(body.contains("account rate limit"), "{body}");
            assert!(!body.contains("response.completed") && !body.contains("[DONE]"));
            let mut next = payload;
            next["model"] = json!("qoder/efficient");
            let (status, headers, body) = fixture.send_with_headers(path, next).await;
            assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{path}: {body}");
            assert_eq!(headers["retry-after"], "37");
            assert!(
                body.contains(if path == "/v1/messages" {
                    "eligible gateway targets are cooling down"
                } else {
                    "gateway_target_cooling"
                }),
                "{body}"
            );
            assert_eq!(fixture.upstream.requests.load(Ordering::SeqCst), 1);
        }
    }
}

/// 同模型在限流后用另一个账号恢复；后续请求继续避开冷却中的账号。
#[tokio::test]
async fn http_rate_limit_recovers_through_another_account() {
    let fixture = Fixture::with_accounts(
        "first-account-limit",
        &[
            ("fake-account", "fake-user"),
            ("second-account", "second-user"),
        ],
    )
    .await;
    let payload = json!({"model":"qoder/auto","input":"hi"});
    for _ in 0..2 {
        let (status, body) = fixture
            .send("/clients/trae/v1/responses", payload.clone())
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("连接成功"), "{body}");
    }
    assert_eq!(
        *fixture.upstream.users.lock().unwrap(),
        ["fake-user", "second-user", "second-user"]
    );
    let records = fixture.state.records.list(2);
    assert_eq!(records[0].attempts.len(), 1);
    assert_eq!(records[1].attempts.len(), 2);
    assert_eq!(records[1].attempts[0].upstream_status, Some(429));
}

/// 三协议非流式入口均及时呈现队列错误，建议时间保持为上游返回值。
#[tokio::test]
async fn queue_is_immediate_for_all_protocols() {
    let requests = [
        (
            "/v1/chat/completions",
            json!({"model":"qoder/auto","messages":[{"role":"user","content":"hi"}]}),
        ),
        (
            "/v1/messages",
            json!({"model":"qoder/auto","max_tokens":32,"messages":[{"role":"user","content":"hi"}]}),
        ),
        ("/v1/responses", json!({"model":"qoder/auto","input":"hi"})),
    ];
    for (path, payload) in requests {
        let fixture = Fixture::new("sse-queue").await;
        let (status, body) = fixture.send(path, payload).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{path}: {body}");
        assert!(body.contains("60 秒后重试"), "{path}: {body}");
        // Anthropic 协议使用 rate_limit_error 类型，OpenAI 入口保留错误码。
        assert!(
            body.contains(if path == "/v1/messages" {
                "rate_limit_error"
            } else {
                "qoder_queued"
            }),
            "{path}: {body}"
        );
        assert!(!body.contains("isQueued"), "内部转义错误不应回显");
        assert_eq!(fixture.upstream.requests.load(Ordering::SeqCst), 1);
    }
}

/// 流式入口必须返回明确错误，不能伪造空成功或重复发起上游请求。
#[tokio::test]
async fn queue_stream_returns_error_without_replay() {
    let fixture = Fixture::new("sse-queue").await;
    let (status, body) = fixture
        .send(
            "/v1/chat/completions",
            json!({
                "model":"qoder/auto","stream":true,"messages":[{"role":"user","content":"hi"}],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(body.contains("qoder_queued"));
    assert!(!body.contains("[DONE]"));
    assert_eq!(fixture.upstream.requests.load(Ordering::SeqCst), 1);
}

/// 并发排队与冷却后的再次请求保持 429 和恢复时间，不在本地重放生成。
#[tokio::test]
async fn concurrent_queue_and_cooldown_preserve_retry_after() {
    let fixture = Fixture::new("sse-queue").await;
    let responses = futures_util::future::join_all((0..4).map(|_| {
        fixture.send(
            "/v1/responses",
            json!({"model":"qoder/auto","input":"concurrent","stream":true}),
        )
    }))
    .await;
    for (status, body) in responses {
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
        assert!(!body.contains("gateway_no_eligible_target"), "{body}");
    }
    let submitted = fixture.upstream.requests.load(Ordering::SeqCst);
    assert!((1..=4).contains(&submitted));
    let response = fixture
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model":"qoder/auto","input":"retry"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()["retry-after"], "60");
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(
        String::from_utf8(body.to_vec())
            .unwrap()
            .contains("gateway_target_cooling")
    );
    assert_eq!(fixture.upstream.requests.load(Ordering::SeqCst), submitted);
}

/// 队列是模型容量问题，不能把账号误判为鉴权失效并禁止后续生成。
#[tokio::test]
async fn queued_model_does_not_disable_account() {
    let fixture = Fixture::new("http-queue").await;
    let payload = json!({"model":"qoder/auto","messages":[{"role":"user","content":"hi"}]});
    let (status, body) = fixture.send("/v1/chat/completions", payload.clone()).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(body.contains("60 秒后重试"));
    *fixture.upstream.mode.lock().unwrap() = "success";
    let (status, body) = fixture
        .send(
            "/v1/chat/completions",
            json!({"model":"qoder/efficient","messages":[{"role":"user","content":"hi"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("连接成功"));
    assert_eq!(fixture.upstream.requests.load(Ordering::SeqCst), 2);
}

/// isQueued=false 且没有重试时间时应报告资源权限问题，不能误当排队。
#[tokio::test]
async fn unavailable_model_is_not_treated_as_queue() {
    let fixture = Fixture::new("restricted").await;
    let (status, body) = fixture
        .send(
            "/v1/chat/completions",
            json!({
                "model":"qoder/auto","messages":[{"role":"user","content":"hi"}],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.contains("qoder_resource_limit") && body.contains("套餐权限"));
    assert!(!body.contains("qoder_queued"));
    assert_eq!(fixture.upstream.requests.load(Ordering::SeqCst), 1);
}

/// 数字业务码同样属于错误，不能静默忽略而返回空成功。
#[tokio::test]
async fn numeric_business_queue_code_is_preserved() {
    let fixture = Fixture::new("numeric-queue").await;
    let (status, body) = fixture
        .send(
            "/v1/chat/completions",
            json!({
                "model":"qoder/auto","messages":[{"role":"user","content":"hi"}],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(body.contains("qoder_queued"));
}
