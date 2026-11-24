//! 网关生命周期集成验证：三协议、分组、容量、预算与可持久配置。

use async_trait::async_trait;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use fakemodelproxy::{
    accounts::{Account, AccountPool, AccountState},
    backend::{Backend, Registry},
    config::Config,
    core::stream::EventStream,
    core::{
        AppError, Capabilities, ChatRequest, Event, FinishReason, ModelInfo, ToolCallDelta,
        UpstreamError,
    },
    gateway::{GatewaySettings, TargetSettings},
    records::RecordLog,
    state::AppState,
    store::Store,
};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU32, Ordering},
    },
};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

#[derive(Clone, Copy)]
enum Mode {
    Success,
    RateLimit,
    ModelRateLimit,
    PrimaryModelRateLimit,
    QuotaExhausted,
    Unknown,
    StartupPending,
    StreamPending,
    Broken,
    Tool,
    PartialTool,
    InternalRetry,
}

struct Call {
    backend: String,
    account: String,
    request: ChatRequest,
}
struct Provider {
    id: &'static str,
    pool: AccountPool,
    mode: Mutex<Mode>,
    calls: Arc<Mutex<Vec<Call>>>,
    qps: AtomicU32,
}

#[async_trait]
impl Backend for Provider {
    /// 注入的后端拥有稳定标识。
    fn id(&self) -> &str {
        self.id
    }
    /// 名称不参与调度。
    fn name(&self) -> &str {
        self.id
    }
    /// 本地事件后端不访问外部账号。
    fn kind(&self) -> &str {
        "fixture"
    }
    /// 套餐目标允许参与默认自动路由。
    fn cost_tier(&self) -> fakemodelproxy::backend::CostTier {
        fakemodelproxy::backend::CostTier::Subscription
    }
    /// 同优先级保证分散策略只按负载与组数选后端。
    fn priority(&self) -> u8 {
        100
    }
    /// 模型和账号共用可热调的服务限额。
    fn qps(&self) -> u32 {
        self.qps.load(Ordering::SeqCst)
    }
    /// 确定窗口与工具能力，避免测试依赖远程目录。
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tools: true,
            parallel_tools: true,
            context_window: Some(200_000),
            ..Default::default()
        }
    }
    /// 两个模型用于验证模型故障的局部隔离。
    fn static_models(&self) -> Vec<String> {
        vec!["m".into(), "n".into()]
    }
    /// 生成热路径不应调用远端目录。
    async fn list_models(&self) -> Result<Vec<ModelInfo>, AppError> {
        panic!("unexpected remote metadata lookup")
    }
    /// 生成热路径不应单独查询额度。
    async fn routing_quota(
        &self,
        _: &str,
    ) -> Result<fakemodelproxy::accounts::remote::QuotaInfo, AppError> {
        panic!("unexpected remote quota lookup")
    }
    /// 调度器和适配器共享可观测的账号池。
    fn routing_accounts(&self) -> Option<Vec<AccountState>> {
        Some(self.pool.snapshot())
    }
    /// 记录实际账号、会话参数与正文，按测试模式返回确定的生命周期。
    async fn chat(
        &self,
        request: ChatRequest,
        _: CancellationToken,
    ) -> Result<EventStream, AppError> {
        let account = self.pool.select().expect("fixed account");
        let record = || {
            self.calls.lock().unwrap().push(Call {
                backend: self.id.into(),
                account: account.id.clone(),
                request: request.clone(),
            })
        };
        record();
        let mode = *self.mode.lock().unwrap();
        match mode {
            Mode::RateLimit => {
                return Err(AppError::Upstream(UpstreamError::status(429, "limited")));
            }
            Mode::ModelRateLimit | Mode::PrimaryModelRateLimit
                if matches!(mode, Mode::ModelRateLimit) || request.model == "m" =>
            {
                return Err(AppError::Upstream(UpstreamError {
                    status: Some(429),
                    code: "model_overload".into(),
                    message: "model capacity reached".into(),
                    retryable: true,
                    execution_unknown: false,
                }));
            }
            Mode::QuotaExhausted => {
                return Err(fakemodelproxy::gateway::retry::gateway_error(
                    402,
                    "insufficient_quota",
                    "quota exhausted",
                    false,
                ));
            }
            Mode::Unknown => {
                return Err(AppError::Upstream(UpstreamError::status(
                    503,
                    "unknown execution",
                )));
            }
            Mode::StartupPending => return Ok(Box::pin(futures_util::stream::pending())),
            Mode::InternalRetry => loop {
                fakemodelproxy::gateway::retry::additional_submission().await?;
                record();
            },
            _ => {}
        }
        let text = Event::Text {
            text: format!("reply {}/{}", self.id, account.id),
        };
        let tool = Event::ToolCall(ToolCallDelta {
            index: 0,
            id: Some(format!("call_{}/{}", self.id, account.id)),
            name: Some("echo".into()),
            arguments: Some(
                if matches!(mode, Mode::PartialTool) {
                    "{\"x\":"
                } else {
                    "{\"x\":1}"
                }
                .into(),
            ),
        });
        let events = match mode {
            Mode::Tool => vec![Ok(tool), Ok(Event::Finish(FinishReason::ToolCalls))],
            Mode::PartialTool => vec![
                Ok(tool),
                Err(AppError::Upstream(UpstreamError::transport("cut off"))),
            ],
            Mode::Broken => vec![Ok(text)],
            _ => vec![Ok(text), Ok(Event::Finish(FinishReason::Stop))],
        };
        if matches!(mode, Mode::StreamPending) {
            return Ok(Box::pin(
                futures_util::stream::iter(events.into_iter().take(1))
                    .chain(futures_util::stream::pending()),
            ));
        }
        Ok(Box::pin(futures_util::stream::iter(events)))
    }
}

struct Fixture {
    app: Router,
    state: AppState,
    providers: Vec<Arc<Provider>>,
    calls: Arc<Mutex<Vec<Call>>>,
}

impl Fixture {
    /// 创建独立内存存储和两服务、四账号，不加载用户的数据库。
    async fn new() -> Self {
        Self::with_accounts(&["one", "two"]).await
    }

    /// 用指定账号数量复现单账号与多账号的模型恢复差异。
    async fn with_accounts(accounts: &[&str]) -> Self {
        let store = Store::open(std::path::Path::new(":memory:")).await.unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let providers: Vec<_> = ["a", "b"]
            .into_iter()
            .map(|id| {
                Arc::new(Provider {
                    id,
                    calls: calls.clone(),
                    mode: Mutex::new(Mode::Success),
                    qps: AtomicU32::new(0),
                    pool: AccountPool::new(
                        accounts
                            .iter()
                            .copied()
                            .map(|account| Account {
                                id: account.into(),
                                backend_id: id.into(),
                                name: account.into(),
                                credential: "{}".into(),
                                enabled: true,
                            })
                            .collect(),
                    ),
                })
            })
            .collect();
        let registry = Arc::new(Registry::from_backends(
            providers
                .iter()
                .cloned()
                .map(|p| p as Arc<dyn Backend>)
                .collect(),
        ));
        let state = AppState::new(
            Arc::new(Config::default()),
            store.clone(),
            registry,
            Arc::new(RecordLog::new(1000)),
            Arc::new(RwLock::new(fakemodelproxy::tasks::TaskRegistry::new())),
            Arc::new(fakemodelproxy::routing::Router::new()),
            Arc::new(fakemodelproxy::usage::UsageLedger::new(store)),
        );
        state
            .router
            .gateway
            .configure(GatewaySettings {
                backoff_base_ms: 0,
                backoff_cap_ms: 0,
                ..Default::default()
            })
            .unwrap();
        Self {
            app: fakemodelproxy::build_router(state.clone()),
            state,
            providers,
            calls,
        }
    }

    /// 保持三协议各自的输入形状，只对完整响应执行终态断言。
    async fn send(
        &self,
        path: &str,
        payload: Value,
        session: Option<&str>,
    ) -> (StatusCode, String) {
        let mut request = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json");
        if let Some(session) = session {
            request = request.header("x-fmp-session", session);
        }
        let response = self
            .app
            .clone()
            .oneshot(request.body(Body::from(payload.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    /// 测试范围内修改完整配置，在途请求仍持有原快照。
    fn configure(&self, change: impl FnOnce(&mut GatewaySettings)) {
        let mut settings = self.state.router.gateway.settings();
        change(&mut settings);
        self.state.router.gateway.configure(settings).unwrap();
    }
}

/// 三入口的指定模型均共享服务额度，换模型或账号不会扩大 QPS。
#[tokio::test]
async fn service_qps_is_shared_by_models_and_accounts_for_all_protocols() {
    for path in ["/v1/chat/completions", "/v1/messages", "/v1/responses"] {
        let fixture = Fixture::new().await;
        fixture.providers[0].qps.store(1, Ordering::SeqCst);
        fixture.configure(|s| s.queue_timeout_ms = 10);
        let payload = |model| json!({"model":model,"messages":[{"role":"user","content":"hi"}],"input":"hi","max_tokens":32});
        assert_eq!(
            fixture.send(path, payload("a/m"), Some("first")).await.0,
            StatusCode::OK
        );
        let (status, body) = fixture.send(path, payload("a/n"), Some("second")).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{path}: {body}");
        assert!(body.contains("QPS"), "{path}: {body}");
        assert_eq!(fixture.calls.lock().unwrap().len(), 1);
        assert_eq!(fixture.state.router.gateway.snapshot()["waiting"], 0);
    }
}

/// 新 Auto 会话避开限频服务，原绑定等待限额恢复，不能因此拆散会话。
#[tokio::test]
async fn auto_skips_rate_limited_services_but_keeps_existing_bindings() {
    let fixture = Fixture::new().await;
    fixture.providers[0].qps.store(1, Ordering::SeqCst);
    fixture.configure(|s| s.queue_timeout_ms = 10);
    let payload =
        json!({"model":"auto","messages":[{"role":"user","content":"hi"}],"max_tokens":32});
    let path = "/v1/chat/completions";
    assert_eq!(
        fixture
            .send(path, payload.clone(), Some("original"))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        fixture.send(path, payload.clone(), Some("new")).await.0,
        StatusCode::OK
    );
    let (status, body) = fixture.send(path, payload, Some("original")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    let calls = fixture.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].backend, "a");
    assert_eq!(calls[1].backend, "b");
}

/// 无流释放通知时由窗口计时器准入下一请求，等待不消耗生成次数。
#[tokio::test]
async fn qps_window_timer_resumes_waiting_without_notifications() {
    let fixture = Fixture::new().await;
    fixture.providers[0].qps.store(1, Ordering::SeqCst);
    fixture.configure(|s| {
        s.queue_timeout_ms = 1500;
        s.max_attempts = 1;
    });
    let payload = json!({"model":"a/m","messages":[{"role":"user","content":"hi"}]});
    let path = "/v1/chat/completions";
    assert_eq!(
        fixture.send(path, payload.clone(), None).await.0,
        StatusCode::OK
    );
    let started = tokio::time::Instant::now();
    let (status, body) = fixture.send(path, payload, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(started.elapsed() >= std::time::Duration::from_millis(900));
    assert_eq!(fixture.calls.lock().unwrap().len(), 2);
}

/// 内部生成重试不能绕过 QPS，本地拒绝不把账号写入上游冷却。
#[tokio::test]
async fn internal_retries_share_qps_without_cooling_the_account() {
    let fixture = Fixture::new().await;
    fixture.providers[0].qps.store(1, Ordering::SeqCst);
    *fixture.providers[0].mode.lock().unwrap() = Mode::InternalRetry;
    fixture.configure(|s| s.queue_timeout_ms = 10);
    let (status, body) = fixture
        .send(
            "/v1/chat/completions",
            json!({"model":"a/m","messages":[{"role":"user","content":"hi"}]}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert!(body.contains("gateway_qps_timeout"));
    assert_eq!(fixture.calls.lock().unwrap().len(), 1);
    assert!(
        fixture
            .state
            .router
            .gateway
            .balancer
            .snapshot()
            .cooldowns
            .is_empty()
    );
    assert_eq!(
        fixture.state.records.list(1)[0].attempts[0]
            .routing
            .as_ref()
            .unwrap()["generation_budget_used"],
        1
    );
}

/// 服务限频等待仍受总 deadline 约束，超时不提交新的上游生成。
#[tokio::test]
async fn qps_wait_respects_total_deadline() {
    let fixture = Fixture::new().await;
    fixture.providers[0].qps.store(1, Ordering::SeqCst);
    fixture.configure(|s| {
        s.total_timeout_ms = 20;
        s.queue_timeout_ms = 1500;
    });
    let payload = json!({"model":"a/m","messages":[{"role":"user","content":"hi"}]});
    let path = "/v1/chat/completions";
    assert_eq!(
        fixture.send(path, payload.clone(), None).await.0,
        StatusCode::OK
    );
    let (status, body) = fixture.send(path, payload, None).await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
    assert!(body.contains("gateway_deadline"));
    assert_eq!(fixture.calls.lock().unwrap().len(), 1);
}

/// 取消限频等待释放队列名额，满队列请求不能提交生成或消耗额度。
#[tokio::test]
async fn cancelling_qps_wait_releases_the_bounded_queue() {
    let fixture = Fixture::new().await;
    fixture.providers[0].qps.store(1, Ordering::SeqCst);
    fixture.configure(|s| {
        s.queue_timeout_ms = 1500;
        s.queue_capacity = 1;
    });
    let payload = json!({"model":"a/m","messages":[{"role":"user","content":"hi"}]});
    assert_eq!(
        fixture
            .send("/v1/chat/completions", payload.clone(), None)
            .await
            .0,
        StatusCode::OK
    );
    let state = fixture.state.clone();
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let request = fakemodelproxy::ingress::chat::parse_chat_request(&payload, None).unwrap();
    let task = tokio::spawn(async move {
        state
            .router
            .gateway
            .execute(
                &state.router,
                &state.registry,
                request,
                fakemodelproxy::gateway::DispatchOptions {
                    explicit: Some((state.registry.get("a").unwrap(), "m".into())),
                    policy: fakemodelproxy::routing::RoutePolicy::Spread,
                    free_only: false,
                    max_attempts: 1,
                },
                token,
            )
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    assert_eq!(fixture.state.router.gateway.snapshot()["waiting"], 1);
    let (status, body) = fixture.send("/v1/chat/completions", payload, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(body.contains("gateway_queue_full"));
    cancel.cancel();
    let error = task.await.unwrap().err().unwrap();
    assert_eq!(error.code(), "request_cancelled");
    assert_eq!(fixture.state.router.gateway.snapshot()["waiting"], 0);
    assert_eq!(fixture.calls.lock().unwrap().len(), 1);
}

/// 原子预占在并发账号与模型选择下仍只允许服务总限额次数。
#[tokio::test]
async fn concurrent_reservations_cannot_multiply_service_qps() {
    use fakemodelproxy::routing::RoutePolicy;
    let fixture = Fixture::new().await;
    fixture.providers[0].qps.store(4, Ordering::SeqCst);
    let request = fakemodelproxy::ingress::chat::parse_chat_request(
        &json!({"model":"auto","messages":[{"role":"user","content":"hi"}]}),
        None,
    )
    .unwrap();
    let candidates: Vec<_> = fixture
        .state
        .router
        .plan(&fixture.state.registry, &request, RoutePolicy::Auto, false)
        .candidates
        .into_iter()
        .filter(|c| c.backend.id() == "a")
        .collect();
    assert_eq!(candidates.len(), 4);
    let mut tasks = Vec::new();
    for index in 0..32 {
        let balancer = fixture.state.router.gateway.balancer.clone();
        let candidates = candidates.clone();
        let settings = fixture.state.router.gateway.settings();
        tasks.push(tokio::spawn(async move {
            match balancer.reserve(
                &candidates,
                &format!("concurrent-{index}"),
                RoutePolicy::Auto,
                &settings,
                &Default::default(),
            ) {
                Ok((_, lease)) => {
                    lease.submitted();
                    true
                }
                Err(fakemodelproxy::gateway::balancer::ReserveError::RateLimited(_)) => false,
                Err(_) => panic!("unexpected reservation failure"),
            }
        }));
    }
    let mut submitted = 0;
    for task in tasks {
        submitted += usize::from(task.await.unwrap());
    }
    assert_eq!(submitted, 4);
}

/// 在途流的 Trace 保留准入时的并发与配置，后续请求不能改写旧快照。
#[tokio::test]
async fn audit_preserves_admission_load_and_limits_while_streams_remain_active() {
    let fixture = Fixture::with_accounts(&["one"]).await;
    fixture.providers[0].qps.store(5, Ordering::SeqCst);
    *fixture.providers[0].mode.lock().unwrap() = Mode::StreamPending;
    fixture.configure(|s| {
        s.targets.insert(
            "a".into(),
            TargetSettings {
                capacity: 7,
                weight: 1,
            },
        );
        s.targets.insert(
            "a/one".into(),
            TargetSettings {
                capacity: 3,
                weight: 1,
            },
        );
    });
    let mut responses = Vec::new();
    for _ in 0..3 {
        let response = fixture
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/clients/trae/v1/responses")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "model":"a/m","input":"parallel","stream":true,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        responses.push(response);
    }
    let records = fixture.state.records.list(3);
    let mut loads = records
        .iter()
        .map(|record| {
            let admission = &record.attempts[0].routing.as_ref().unwrap()["admission"];
            assert_eq!(admission["service_capacity"], 7);
            assert_eq!(admission["account_capacity"], 3);
            assert_eq!(admission["service_qps"], 5);
            assert_eq!(
                admission["service_in_flight"],
                admission["account_in_flight"]
            );
            assert_eq!(
                admission["service_in_flight"],
                admission["service_submissions_in_window"]
            );
            admission["service_in_flight"].as_u64().unwrap()
        })
        .collect::<Vec<_>>();
    loads.sort();
    assert_eq!(loads, [1, 2, 3]);
    drop(responses);
    for record in records {
        let retained = fixture
            .state
            .records
            .list(3)
            .into_iter()
            .find(|r| r.id == record.id)
            .unwrap();
        assert_eq!(retained.attempts[0].routing, record.attempts[0].routing);
    }
}

/// 退款、启用限额和降低限额均使用同一历史，热更新不能重置滚动窗口。
#[tokio::test(start_paused = true)]
async fn qps_history_survives_policy_changes_and_refunds_unsubmitted_permits() {
    use fakemodelproxy::gateway::balancer::{Balancer, ReserveError};
    use std::time::Duration;
    let balancer = Arc::new(Balancer::default());
    let start = tokio::time::Instant::now();
    balancer
        .reserve_submission("service", 0)
        .unwrap_or_else(|_| panic!("initial"))
        .commit();
    tokio::time::advance(Duration::from_millis(200)).await;
    balancer
        .reserve_submission("service", 0)
        .unwrap_or_else(|_| panic!("second"))
        .commit();
    tokio::time::advance(Duration::from_millis(200)).await;
    balancer
        .reserve_submission("service", 0)
        .unwrap_or_else(|_| panic!("third"))
        .commit();
    let ReserveError::RateLimited(until) = balancer.reserve_submission("service", 2).err().unwrap()
    else {
        panic!("lowered limit must retain history");
    };
    assert_eq!(until, start + Duration::from_millis(1200));
    let permit = balancer
        .reserve_submission("service", 4)
        .unwrap_or_else(|_| panic!("raised"));
    drop(permit);
    assert!(balancer.reserve_submission("service", 4).is_ok());
    tokio::time::advance(Duration::from_millis(800)).await;
    let permit = balancer
        .reserve_submission("service", 2)
        .unwrap_or_else(|_| panic!("window expired"));
    permit.commit();
    assert!(matches!(
        balancer.reserve_submission("service", 2),
        Err(ReserveError::RateLimited(_))
    ));
}

/// 三种协议、流式和非流式均安全换目标，首次失败不能污染最终成功。
#[tokio::test]
async fn safe_startup_failure_recovers_for_all_protocols() {
    for path in ["/v1/chat/completions", "/v1/messages", "/v1/responses"] {
        for stream in [false, true] {
            let fixture = Fixture::new().await;
            *fixture.providers[0].mode.lock().unwrap() = Mode::RateLimit;
            let payload = json!({"model":"auto:spread", "input":"hi", "max_tokens":32, "messages":[{"role":"user","content":"hi"}], "stream":stream});
            let (status, body) = fixture.send(path, payload, None).await;
            assert_eq!(status, StatusCode::OK, "{path}: {body}");
            let calls = fixture.calls.lock().unwrap();
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[1].backend, "b");
            assert_eq!(fixture.state.records.list(1)[0].outcome, "ok");
            assert!(
                fixture
                    .state
                    .router
                    .gateway
                    .balancer
                    .snapshot()
                    .loads
                    .iter()
                    .all(|load| load.in_flight == 0)
            );
        }
    }
}

/// 已发送但结果未知的生成只能执行一次；首帧之前返回失败 HTTP 状态。
#[tokio::test]
async fn unknown_execution_and_startup_timeout_never_replay() {
    for mode in [Mode::Unknown, Mode::StartupPending] {
        let fixture = Fixture::new().await;
        *fixture.providers[0].mode.lock().unwrap() = mode;
        fixture.configure(|s| s.startup_timeout_ms = 10);
        let (status, _) = fixture
            .send(
                "/v1/responses",
                json!({"model":"auto:spread","input":"hi","stream":true}),
                None,
            )
            .await;
        assert_eq!(
            status,
            if matches!(mode, Mode::Unknown) {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::GATEWAY_TIMEOUT
            }
        );
        assert_eq!(fixture.calls.lock().unwrap().len(), 1);
        assert_eq!(fixture.state.records.list(1)[0].outcome, "error");
    }
}

/// 文本或部分工具参数已输出后，流中错误不能触发第二个生成。
#[tokio::test]
async fn partial_streams_are_errors_without_splicing_another_generation() {
    for mode in [Mode::Broken, Mode::PartialTool, Mode::StreamPending] {
        let fixture = Fixture::new().await;
        *fixture.providers[0].mode.lock().unwrap() = mode;
        fixture.configure(|s| s.idle_timeout_ms = 10);
        let (status, body) = fixture
            .send(
                "/v1/responses",
                json!({"model":"auto:spread","input":"hi","stream":true}),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("response.failed"), "{body}");
        assert_eq!(fixture.calls.lock().unwrap().len(), 1);
        assert_eq!(fixture.state.records.list(1)[0].outcome, "error");
    }
}

/// 适配器内部再次提交和外层目标切换共享一个上限。
#[tokio::test]
async fn adapter_resubmissions_share_the_generation_budget() {
    let fixture = Fixture::new().await;
    *fixture.providers[0].mode.lock().unwrap() = Mode::InternalRetry;
    fixture.configure(|s| s.max_attempts = 3);
    let (status, _) = fixture
        .send(
            "/v1/chat/completions",
            json!({"model":"auto:spread","messages":[{"role":"user","content":"hi"}]}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(fixture.calls.lock().unwrap().len(), 3);
    assert_eq!(
        fixture.state.records.list(1)[0].attempts[0]
            .routing
            .as_ref()
            .unwrap()["generation_budget_used"],
        3
    );
}

/// 同一明确组的并发调用原子绑定到同一账号；热更新不拆组。
#[tokio::test]
async fn concurrent_group_reservation_and_hot_settings_preserve_the_target() {
    let fixture = Fixture::new().await;
    let payload = json!({"model":"a/m","messages":[{"role":"user","content":"hi"}]});
    futures_util::future::join_all(
        (0..4).map(|_| fixture.send("/v1/chat/completions", payload.clone(), Some("group"))),
    )
    .await;
    fixture.configure(|s| s.max_attempts = 3);
    fixture
        .send("/v1/chat/completions", payload, Some("group"))
        .await;
    let calls = fixture.calls.lock().unwrap();
    assert_eq!(calls.len(), 5);
    assert!(
        calls
            .iter()
            .all(|call| call.backend == "a" && call.account == calls[0].account)
    );
}

/// 容量等待到时返回过载，客户端丢弃流也会释放原租约。
#[tokio::test]
async fn busy_group_wait_is_bounded_and_cancel_releases_capacity() {
    let fixture = Fixture::new().await;
    *fixture.providers[0].mode.lock().unwrap() = Mode::StreamPending;
    fixture.configure(|s| {
        s.queue_timeout_ms = 10;
        s.group_ttl_ms = 1;
        s.targets = HashMap::from([(
            "a/one".into(),
            TargetSettings {
                capacity: 1,
                weight: 1,
            },
        )]);
    });
    let payload = json!({"model":"a/m","messages":[{"role":"user","content":"hi"}],"stream":true});
    let response = fixture
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .header("x-fmp-session", "busy")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    let (status, body) = fixture
        .send("/v1/chat/completions", payload, Some("busy"))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("gateway_capacity_timeout"));
    drop(response);
    tokio::task::yield_now().await;
    assert!(
        fixture
            .state
            .router
            .gateway
            .balancer
            .snapshot()
            .loads
            .iter()
            .all(|load| load.in_flight == 0)
    );
}

/// 相同首轮独立分组，完整历史命中原目标；TRAE 包装变化不改写正文。
#[tokio::test]
async fn trae_normalized_continuation_pins_only_the_matching_completed_group() {
    let fixture = Fixture::new().await;
    let path = "/clients/trae/v1/responses";
    let first = "<system-reminder>time A</system-reminder><user_input>task</user_input>";
    fixture
        .send(
            path,
            json!({"model":"auto:spread","input":[{"role":"user","content":first}]}),
            None,
        )
        .await;
    let original = fixture.state.records.list(1)[0].session_id.clone();
    fixture
        .send(
            path,
            json!({"model":"auto:spread","input":[{"role":"user","content":first}]}),
            None,
        )
        .await;
    assert_ne!(fixture.state.records.list(1)[0].session_id, original);
    fixture.send(path, json!({"model":"auto:spread","input":[
        {"role":"user","content":"<system-reminder>time B</system-reminder><user_input>task</user_input>"},
        {"role":"assistant","content":"reply "},{"role":"assistant","content":"a/one"},
        {"role":"user","content":"next"}
    ]}), None).await;
    let record = &fixture.state.records.list(1)[0];
    assert_eq!(record.session_id, original);
    assert_eq!(
        record.session.as_ref().unwrap().source,
        fakemodelproxy::client_plugins::SessionSource::HistoryContinuation
    );
    let calls = fixture.calls.lock().unwrap();
    assert_eq!(calls[0].account, calls[2].account);
    assert_eq!(calls[2].backend, "a");
    assert!(
        calls[2].request.messages[0]
            .text_content()
            .contains("time B")
    );
    assert!(calls[2].request.session.is_none());
}

/// 协议结束即停止读取后，交错新会话不能抢走原 TRAE 会话的 Auto 绑定。
#[tokio::test]
async fn trae_terminal_delivery_preserves_auto_binding_across_interleaved_groups() {
    for (path, terminal) in [
        ("/clients/trae/v1/chat/completions", "data: [DONE]"),
        ("/clients/trae/v1/messages", "event: message_stop"),
        ("/clients/trae/v1/responses", "event: response.completed"),
    ] {
        let fixture = Fixture::new().await;
        let first = "<system-reminder>time A</system-reminder><user_input>task</user_input>";
        let payload = json!({"model":"auto:spread","stream":true,"max_tokens":32,
            "input":[{"role":"user","content":first}],
            "messages":[{"role":"user","content":first}]});
        let response = fixture
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
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body().into_data_stream();
        let mut completed = false;
        while let Some(chunk) = body.next().await {
            if String::from_utf8_lossy(&chunk.unwrap()).contains(terminal) {
                completed = true;
                break;
            }
        }
        assert!(completed, "{path}");
        let original = fixture.state.records.list(1)[0].clone();
        assert_eq!(original.outcome, "ok", "{path}");
        drop(body);
        assert_eq!(fixture.state.records.list(1)[0].outcome, "ok");

        let mut independent = payload;
        independent["stream"] = json!(false);
        assert_eq!(
            fixture.send(path, independent, None).await.0,
            StatusCode::OK
        );
        assert_ne!(
            fixture.state.records.list(1)[0].session_id,
            original.session_id
        );

        let history = json!([
            {"role":"user","content":"<system-reminder>time B</system-reminder><user_input>task</user_input>"},
            {"role":"assistant","content":"reply "},
            {"role":"assistant","content":"a/one"},
            {"role":"user","content":"next"}
        ]);
        assert_eq!(
            fixture
                .send(
                    path,
                    json!({"model":"auto:spread","max_tokens":32,
                        "input":history,"messages":history}),
                    None,
                )
                .await
                .0,
            StatusCode::OK
        );
        let next = fixture.state.records.list(1)[0].clone();
        assert_eq!(next.session_id, original.session_id, "{path}");
        assert_eq!(next.resolved_model, original.resolved_model);
        assert_eq!(
            next.session.as_ref().unwrap().source,
            fakemodelproxy::client_plugins::SessionSource::HistoryContinuation
        );
        let routing = next.attempts[0].routing.as_ref().unwrap();
        assert_eq!(routing["affinity_hit"], true);
        assert_eq!(
            routing["binding_version"],
            original.attempts[0].routing.as_ref().unwrap()["binding_version"]
        );
        let calls = fixture.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].backend, "a");
        assert_eq!(calls[0].account, "one");
        assert_ne!(
            (&calls[1].backend, &calls[1].account),
            (&calls[0].backend, &calls[0].account)
        );
        assert_eq!(calls[2].backend, calls[0].backend);
        assert_eq!(calls[2].account, calls[0].account);
        assert_eq!(calls[2].request.model, calls[0].request.model);
        assert!(calls[2].request.session.is_none());
        assert!(
            calls[2].request.messages[0]
                .text_content()
                .contains("time B")
        );
    }
}

/// 上游及后台发送已完成时，仅读首帧仍不能发布未交付的历史证据。
#[tokio::test]
async fn trae_queued_completion_does_not_publish_history_before_delivery() {
    for path in [
        "/clients/trae/v1/chat/completions",
        "/clients/trae/v1/messages",
        "/clients/trae/v1/responses",
    ] {
        let fixture = Fixture::new().await;
        let response = fixture
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"model":"auto:spread","stream":true,"max_tokens":32,
                            "input":"task","messages":[{"role":"user","content":"task"}]})
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while fixture
                .state
                .router
                .gateway
                .balancer
                .snapshot()
                .loads
                .iter()
                .any(|load| load.in_flight != 0)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("fixture upstream must complete before downstream reads");
        let original = fixture.state.records.list(1)[0].clone();
        assert_eq!(original.outcome, "running");
        let mut body = response.into_body().into_data_stream();
        body.next().await.unwrap().unwrap();
        drop(body);
        assert_eq!(fixture.state.records.list(1)[0].outcome, "cancelled");
        let history = json!([
            {"role":"user","content":"task"},
            {"role":"assistant","content":"reply a/one"},
            {"role":"user","content":"next"}
        ]);
        assert_eq!(
            fixture
                .send(
                    path,
                    json!({"model":"auto:spread","max_tokens":32,
                        "input":history,"messages":history}),
                    None,
                )
                .await
                .0,
            StatusCode::OK
        );
        let next = fixture.state.records.list(1)[0].clone();
        assert_ne!(next.session_id, original.session_id);
        assert_eq!(
            next.session.as_ref().unwrap().source,
            fakemodelproxy::client_plugins::SessionSource::HistoryNew
        );
    }
}

/// 裁剪后的完整工具调用及结果，只有唯一已完成组可以作为恢复锚点。
#[tokio::test]
async fn unique_tool_anchor_recovers_a_truncated_history() {
    let fixture = Fixture::new().await;
    *fixture.providers[0].mode.lock().unwrap() = Mode::Tool;
    let path = "/v1/responses";
    fixture
        .send(path, json!({"model":"auto:spread","input":"start"}), None)
        .await;
    let original = fixture.state.records.list(1)[0].session_id.clone();
    *fixture.providers[0].mode.lock().unwrap() = Mode::Success;
    fixture.send(path, json!({"model":"auto:spread","input":[
        {"type":"function_call","call_id":"call_a/one","name":"echo","arguments":"{ \"x\": 1 }"},
        {"type":"function_call_output","call_id":"call_a/one","output":"done"}
    ]}), None).await;
    let record = &fixture.state.records.list(1)[0];
    assert_eq!(record.session_id, original);
    assert_eq!(
        record.session.as_ref().unwrap().source,
        fakemodelproxy::client_plugins::SessionSource::HistoryToolAnchor
    );
}

/// 内部重提可耗用完整单请求次数预算，不再受 Gateway 共享信用限制。
#[tokio::test]
async fn request_budget_allows_more_than_ten_internal_submissions() {
    for path in ["/v1/chat/completions", "/v1/messages", "/v1/responses"] {
        for stream in [false, true] {
            let fixture = Fixture::new().await;
            fixture.configure(|s| s.max_attempts = 16);
            for provider in &fixture.providers {
                *provider.mode.lock().unwrap() = Mode::InternalRetry;
            }
            let (status, body) = fixture
                .send(
                    path,
                    json!({"model":"auto","input":"hi","max_tokens":32,"messages":[{"role":"user","content":"hi"}],"stream":stream}),
                    None,
                )
                .await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}: {body}");
            assert!(body.contains("attempt budget exhausted"), "{path}: {body}");
            assert_eq!(fixture.calls.lock().unwrap().len(), 16);
        }
    }
}

/// 模型限流后本次请求避开同一账号，其他账号仍可恢复，冷却范围保持模型级。
#[tokio::test]
async fn model_rate_limit_avoids_repeated_submissions_on_the_same_account() {
    for mode in [Mode::PrimaryModelRateLimit, Mode::ModelRateLimit] {
        let fixture = Fixture::new().await;
        fixture.configure(|s| s.max_attempts = 8);
        for provider in &fixture.providers {
            *provider.mode.lock().unwrap() = mode;
        }
        let (status, body) = fixture
            .send(
                "/v1/responses",
                json!({"model":"auto","input":"hi"}),
                Some("limited-group"),
            )
            .await;
        let recovers = matches!(mode, Mode::PrimaryModelRateLimit);
        assert_eq!(
            status,
            if recovers {
                StatusCode::OK
            } else {
                StatusCode::TOO_MANY_REQUESTS
            },
            "{body}"
        );
        let calls = fixture.calls.lock().unwrap();
        assert_eq!(calls.len(), if recovers { 3 } else { 4 });
        let accounts: std::collections::HashSet<_> = calls
            .iter()
            .map(|call| (&call.backend, &call.account))
            .collect();
        assert_eq!(accounts.len(), calls.len());
        if recovers {
            assert_eq!(calls.last().unwrap().request.model, "n");
            assert_eq!(calls.last().unwrap().account, "two");
        }
        drop(calls);
        let snapshot = fixture.state.router.gateway.balancer.snapshot();
        assert!(
            snapshot
                .cooldowns
                .iter()
                .all(|cooldown| cooldown.scope.starts_with("model:"))
        );
        assert!(snapshot.loads.iter().all(|load| load.in_flight == 0));
    }
}

/// 每服务仅一个账号时，排队不能在同请求中轮询该账号的其他模型。
#[tokio::test]
async fn single_account_queue_does_not_rotate_through_the_model_catalog() {
    let fixture = Fixture::with_accounts(&["one"]).await;
    fixture.configure(|s| s.max_attempts = 8);
    for provider in &fixture.providers {
        *provider.mode.lock().unwrap() = Mode::ModelRateLimit;
    }
    let (status, body) = fixture
        .send(
            "/v1/responses",
            json!({"model":"auto","input":"hi"}),
            Some("single-account"),
        )
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(fixture.calls.lock().unwrap().len(), 2);
    for provider in &fixture.providers {
        *provider.mode.lock().unwrap() = Mode::Success;
    }
    let (status, body) = fixture
        .send(
            "/v1/responses",
            json!({"model":"a/n","input":"next"}),
            Some("single-account"),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(fixture.calls.lock().unwrap().len(), 3);
}

/// 并发请求在目标冷却后继续保持 429，不对同一限流账号重复提交。
#[tokio::test]
async fn concurrent_requests_preserve_rate_limits_during_cooldown() {
    let fixture = Fixture::new().await;
    *fixture.providers[0].mode.lock().unwrap() = Mode::RateLimit;
    let (status, _) = fixture
        .send(
            "/v1/responses",
            json!({"model":"auto","input":"warmup"}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fixture.calls.lock().unwrap().len(), 2);
    *fixture.providers[1].mode.lock().unwrap() = Mode::RateLimit;
    let responses = futures_util::future::join_all((0..12).map(|_| {
        fixture.send(
            "/v1/responses",
            json!({"model":"b/m","input":"concurrent"}),
            None,
        )
    }))
    .await;
    for (status, body) in responses {
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
        assert!(!body.contains("gateway_no_eligible_target"), "{body}");
    }
    assert_eq!(fixture.calls.lock().unwrap().len(), 4);
}

/// 配置保存进入 SQLite，旧 v1 包缺字段不会覆盖网关策略。
#[tokio::test]
async fn gateway_api_persists_settings_and_keeps_legacy_bundle_compatibility() {
    let fixture = Fixture::new().await;
    let settings = GatewaySettings {
        auto_allow_paid_fallback: true,
        max_attempts: 2,
        connect_timeout_ms: 1234,
        ..Default::default()
    };
    let response = fixture
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/gateway")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&settings).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.into_body().collect().await.unwrap();
    let (status, _) = fixture
        .send(
            "/api/config/import",
            json!({"version":1,"backends":[],"accounts":[]}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let saved = fixture.state.store.gateway_settings().await.unwrap();
    assert!(saved.auto_allow_paid_fallback);
    assert_eq!(saved.connect_timeout_ms, 1234);
    let restarted = fakemodelproxy::routing::Router::new();
    restarted.gateway.configure(saved).unwrap();
    assert_eq!(restarted.gateway.settings().max_attempts, 2);
    assert_eq!(restarted.gateway.balancer.snapshot().groups, 0);
}

/// 前一次生成失败后的迁移，不能把明确会话句柄送到新账号。
#[tokio::test]
async fn cross_request_migration_clears_the_old_vendor_session() {
    let fixture = Fixture::new().await;
    *fixture.providers[0].mode.lock().unwrap() = Mode::Unknown;
    let payload = json!({"model":"auto:spread","input":"hi"});
    assert_eq!(
        fixture
            .send("/v1/responses", payload.clone(), Some("vendor-session"))
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        fixture
            .send("/v1/responses", payload, Some("vendor-session"))
            .await
            .0,
        StatusCode::OK
    );
    let calls = fixture.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_ne!(calls[0].request.session.as_deref(), Some("vendor-session"));
    assert!(uuid::Uuid::parse_str(calls[0].request.session.as_deref().unwrap()).is_ok());
    assert_eq!(calls[1].backend, "b");
    assert_ne!(calls[0].request.session, calls[1].request.session);
}

/// 同绑定的厂商句柄稳定，TTL 到期后重新建立句柄而不复用旧账号身份。
#[tokio::test]
async fn vendor_handle_is_stable_until_binding_expiration() {
    let fixture = Fixture::new().await;
    fixture.configure(|settings| settings.group_ttl_ms = 100);
    let payload = json!({"model":"a/m","input":"hi"});
    for _ in 0..2 {
        assert_eq!(
            fixture
                .send("/v1/responses", payload.clone(), Some("stable"))
                .await
                .0,
            StatusCode::OK
        );
    }
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    assert_eq!(
        fixture
            .send("/v1/responses", payload, Some("stable"))
            .await
            .0,
        StatusCode::OK
    );
    let calls = fixture.calls.lock().unwrap();
    assert_eq!(calls[0].request.session, calls[1].request.session);
    assert_ne!(calls[0].request.session, calls[2].request.session);
}

/// 冷却过期只允许一个探测，迟到的成功不能清除较新一代故障。
#[tokio::test]
async fn half_open_probe_is_exclusive_and_versioned() {
    use fakemodelproxy::{gateway::retry::FailureScope, routing::RoutePolicy};
    let fixture = Fixture::new().await;
    let request = fakemodelproxy::ingress::chat::parse_chat_request(
        &json!({"model":"auto:spread","messages":[{"role":"user","content":"hi"}]}),
        None,
    )
    .unwrap();
    let candidate = fixture
        .state
        .router
        .plan(&fixture.state.registry, &request, RoutePolicy::Auto, false)
        .candidates
        .remove(0);
    let balancer = &fixture.state.router.gateway.balancer;
    balancer.failure(
        &candidate,
        FailureScope::Account,
        std::time::Duration::ZERO,
        "probe",
    );
    let settings = fixture.state.router.gateway.settings();
    let (_, lease) = balancer
        .reserve(
            std::slice::from_ref(&candidate),
            "probe-1",
            RoutePolicy::Auto,
            &settings,
            &Default::default(),
        )
        .ok()
        .unwrap();
    assert!(
        balancer
            .reserve(
                std::slice::from_ref(&candidate),
                "probe-2",
                RoutePolicy::Auto,
                &settings,
                &Default::default()
            )
            .is_err()
    );
    balancer.failure(
        &candidate,
        FailureScope::Account,
        std::time::Duration::from_secs(60),
        "new-failure",
    );
    lease.completed();
    drop(lease);
    assert!(!balancer.available(&candidate));
}

/// 管理读写必须经过管理鉴权，非法配置不进入持久库。
#[tokio::test]
async fn gateway_api_checks_auth_and_rejects_invalid_limits() {
    let fixture = Fixture::new().await;
    let mut state = fixture.state.clone();
    state.config = Arc::new(Config {
        admin_key: "test-admin".into(),
        ..Default::default()
    });
    let app = fakemodelproxy::build_router(state);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/gateway")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/gateway")
                .header("x-admin-key", "test-admin")
                .header("content-type", "application/json")
                .body(Body::from(json!({"max_attempts":0}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        fixture
            .state
            .store
            .gateway_settings()
            .await
            .unwrap()
            .max_attempts,
        4
    );
}

/// 旧分散开关不是付费许可，旧配置导入后必须仍拒绝付费自动后备。
#[tokio::test]
async fn legacy_gateway_settings_do_not_authorize_paid_fallback() {
    let fixture = Fixture::new().await;
    let (status, _) = fixture
        .send(
            "/api/config/import",
            json!({"version":1,"gateway":{"default_auto_spread":true,"max_attempts":2}}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let settings = fixture.state.store.gateway_settings().await.unwrap();
    assert!(!settings.auto_allow_paid_fallback);
    assert_eq!(settings.max_attempts, 2);
    assert!(
        serde_json::to_value(settings)
            .unwrap()
            .get("default_auto_spread")
            .is_none()
    );
}

/// HTTP Retry-After 秒数和日期均透传；冷却中的再次请求在本地拒收。
#[tokio::test]
async fn retry_after_survives_protocol_errors_and_local_cooldown() {
    let format = time::format_description::parse_borrowed::<2>(
        "[weekday repr:short], [day] [month repr:short] [year] [hour]:[minute]:[second] GMT",
    )
    .unwrap();
    let date = (time::OffsetDateTime::now_utc() + time::Duration::seconds(120))
        .format(&format)
        .unwrap();
    for hint in ["60".to_string(), date] {
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = count.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let upstream = tokio::spawn(async move {
            let app = Router::new().route(
                "/chat/completions",
                axum::routing::post(move || {
                    let hint = hint.clone();
                    let observed = observed.clone();
                    async move {
                        observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        (
                            StatusCode::TOO_MANY_REQUESTS,
                            [("retry-after", hint)],
                            "limited",
                        )
                    }
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });
        let fixture = Fixture::new().await;
        fixture
            .state
            .store
            .upsert_backend(&fakemodelproxy::store::BackendRecord {
                id: "remote".into(),
                name: "remote".into(),
                base_url: base,
                models: "m".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        fixture.state.reload_backends().await.unwrap();
        for _ in 0..2 {
            let response = fixture
                .app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/responses")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            json!({"model":"remote/m","input":"hi"}).to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
            let seconds = response.headers()["retry-after"]
                .to_str()
                .unwrap()
                .parse::<u64>()
                .unwrap();
            assert!((59..=120).contains(&seconds), "{seconds}");
            response.into_body().collect().await.unwrap();
        }
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
        upstream.abort();
    }
}

/// 生成复用持久额度；明确耗尽只失效旧快照，不删除更新后的余额。
#[tokio::test]
async fn quota_rejection_invalidates_only_the_previous_account_snapshot() {
    use fakemodelproxy::accounts::remote::{QuotaConfidence, QuotaInfo};
    let fixture = Fixture::new().await;
    let backend = fakemodelproxy::store::BackendRecord {
        id: "a".into(),
        name: "a".into(),
        base_url: "http://127.0.0.1:1".into(),
        models: "m".into(),
        ..Default::default()
    };
    fixture.state.store.upsert_backend(&backend).await.unwrap();
    fixture
        .state
        .store
        .upsert_account(&Account {
            id: "one".into(),
            backend_id: "a".into(),
            name: "one".into(),
            credential: "{}".into(),
            enabled: true,
        })
        .await
        .unwrap();
    let checked = fakemodelproxy::records::now_seconds();
    let info = QuotaInfo {
        backend_id: "a".into(),
        account_id: "one".into(),
        remaining: Some(100),
        used: None,
        total: None,
        unit: Some("credits".into()),
        reset_at: None,
        confidence: QuotaConfidence::Verified,
        checked_at: checked,
        routing: None,
        note: None,
    };
    fixture
        .state
        .store
        .finish_quota_refresh(&[(backend.clone(), info.clone())], checked, checked + 43200)
        .await
        .unwrap();
    *fixture.providers[0].mode.lock().unwrap() = Mode::QuotaExhausted;
    assert_eq!(
        fixture
            .send(
                "/v1/responses",
                json!({"model":"auto:spread","input":"hi"}),
                None
            )
            .await
            .0,
        StatusCode::OK
    );
    assert!(fixture.state.store.cached_quota().await.unwrap().is_empty());
    let refreshed = QuotaInfo {
        checked_at: checked + 1,
        ..info
    };
    fixture
        .state
        .store
        .finish_quota_refresh(&[(backend, refreshed)], checked + 1, checked + 43200)
        .await
        .unwrap();
    fixture
        .state
        .store
        .invalidate_quota("a", "one", checked)
        .await
        .unwrap();
    assert_eq!(fixture.state.store.cached_quota().await.unwrap().len(), 1);
}
