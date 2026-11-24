//! Auto 端到端路由契约：预算、比较域、账号固定、亲和和不可重放错误。

use async_trait::async_trait;
use fakemodelproxy::accounts::quota::QuotaSnapshot;
use fakemodelproxy::accounts::remote::{QuotaConfidence, QuotaInfo};
use fakemodelproxy::accounts::{Account, AccountPool, AccountState};
use fakemodelproxy::backend::{Backend, CostTier, Registry};
use fakemodelproxy::core::stream::EventStream;
use fakemodelproxy::core::{
    AppError, Capabilities, ChatRequest, ContextTier, ModelInfo, Protocol, TokenLimits,
    UpstreamError,
};
use fakemodelproxy::routing::{RoutePolicy, Router};
use fakemodelproxy::store::BackendRecord;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// 不同新组均匀分散，同组保持原账号，完整读取后释放容量。
#[tokio::test]
async fn spread_distributes_new_groups_and_preserves_existing_binding() {
    let (router, registry, calls) = fixture(None);
    {
        let mut data = router.metadata.data.write().unwrap();
        for record in data.records.values_mut() {
            record.priority = 10;
            record.options = "{}".into();
        }
    }
    for group in ["s0", "s1", "s2", "s3", "s4", "s5", "s0"] {
        let (stream, _) = router
            .dispatch(
                &registry,
                request("auto:spread", Some(group)),
                RoutePolicy::Auto,
                false,
                CancellationToken::new(),
                4,
            )
            .await
            .unwrap();
        assert!(stream.collect::<Vec<_>>().await.iter().all(Result::is_ok));
    }
    let calls = calls.lock().unwrap();
    let backends: Vec<_> = calls
        .iter()
        .map(|call| call.split('/').next().unwrap())
        .collect();
    assert_eq!(backends, ["a", "b", "c", "a", "b", "c", "a"]);
    assert_eq!(calls[0], calls[6]);
    assert_ne!(calls[0], calls[3]);
    assert!(
        router
            .gateway
            .balancer
            .snapshot()
            .loads
            .iter()
            .all(|load| load.in_flight == 0)
    );
}

/// 明确未执行的限流可以换目标，总尝试数仍受调用上限约束。
#[tokio::test]
async fn safe_retries_use_distinct_targets_and_respect_attempt_limit() {
    let (router, registry, calls) = fixture(Some(UpstreamError::status(429, "rate limited")));
    let result = router
        .dispatch(
            &registry,
            request("auto:spread", Some("retry")),
            RoutePolicy::Auto,
            false,
            CancellationToken::new(),
            2,
        )
        .await;
    assert!(result.is_err());
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_ne!(calls[0], calls[1]);
    assert!(
        router
            .gateway
            .balancer
            .snapshot()
            .loads
            .iter()
            .all(|load| load.in_flight == 0)
    );
}

/// 普通 503 未声明幂等语义，不能因未交付响应而重新提交生成。
#[tokio::test]
async fn unknown_http_failure_is_not_replayed() {
    let (router, registry, calls) = fixture(Some(UpstreamError::status(503, "unknown execution")));
    assert!(
        router
            .dispatch(
                &registry,
                request("auto:spread", None),
                RoutePolicy::Auto,
                false,
                CancellationToken::new(),
                4
            )
            .await
            .is_err()
    );
    assert_eq!(calls.lock().unwrap().len(), 1);
}

/// 下游放弃读取释放容量，避免长期占据后端账号。
#[tokio::test]
async fn dropping_generation_stream_releases_both_capacity_leases() {
    let (router, registry, _) = fixture(None);
    let (stream, _) = router
        .dispatch(
            &registry,
            request("auto:spread", Some("drop")),
            RoutePolicy::Auto,
            false,
            CancellationToken::new(),
            4,
        )
        .await
        .unwrap();
    assert_eq!(
        router
            .gateway
            .balancer
            .snapshot()
            .loads
            .iter()
            .map(|load| load.in_flight)
            .sum::<usize>(),
        2
    );
    drop(stream);
    assert!(
        router
            .gateway
            .balancer
            .snapshot()
            .loads
            .iter()
            .all(|load| load.in_flight == 0)
    );
}

/// 可观测的账号后端，以固定目录和余额替代外部网络。
struct Provider {
    id: String,
    pool: AccountPool,
    factor: f64,
    balance: i64,
    called: Arc<Mutex<Vec<String>>>,
    error: Option<UpstreamError>,
}

#[async_trait]
impl Backend for Provider {
    /// 测试服务标识。
    fn id(&self) -> &str {
        &self.id
    }
    /// 测试服务名称。
    fn name(&self) -> &str {
        &self.id
    }
    /// 测试使用通用预算路径。
    fn kind(&self) -> &str {
        "fixture"
    }
    /// 声明确定的模型能力与窗口。
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tools: true,
            images: true,
            context_window: Some(200000),
            ..Default::default()
        }
    }
    /// 所有账号使用套餐额度。
    fn cost_tier(&self) -> CostTier {
        CostTier::Subscription
    }
    /// 配置覆盖此缺省优先级。
    fn priority(&self) -> u8 {
        100
    }
    /// 返回一个已声明模型。
    fn static_models(&self) -> Vec<String> {
        vec!["m".into()]
    }
    /// 动态目录带有价格与输出约束。
    async fn list_models(&self) -> Result<Vec<ModelInfo>, AppError> {
        Ok(vec![ModelInfo {
            id: "m".into(),
            display_name: None,
            price_factor: Some(self.factor),
            original_price_factor: None,
            price_valid_until: None,
            capabilities: self.capabilities(),
            token_limits: TokenLimits {
                context_window: Some(200000),
                max_output_tokens: Some(32000),
                ..Default::default()
            },
        }])
    }
    /// 路由与实际调用共享一个账号池。
    fn routing_accounts(&self) -> Option<Vec<AccountState>> {
        Some(self.pool.snapshot())
    }
    /// 账号 small 与 rich 余额明确不同，验证固定 rich 账号而非再次轮询。
    async fn routing_quota(&self, id: &str) -> Result<QuotaInfo, AppError> {
        Ok(QuotaInfo {
            backend_id: self.id.clone(),
            account_id: id.into(),
            used: None,
            total: None,
            remaining: Some(if id == "rich" { self.balance } else { 1 }),
            unit: Some("credits".into()),
            reset_at: None,
            confidence: QuotaConfidence::Verified,
            note: None,
            checked_at: fakemodelproxy::records::now_seconds(),
            routing: None,
        })
    }
    /// 捕获最终账号与输出硬限制，未知执行错误不能触发第二次调用。
    async fn chat(
        &self,
        request: ChatRequest,
        _: CancellationToken,
    ) -> Result<EventStream, AppError> {
        let account = self.pool.select().expect("routed account");
        assert_eq!(request.max_tokens, Some(8192));
        self.called
            .lock()
            .unwrap()
            .push(format!("{}/{}", self.id, account.id));
        if let Some(error) = &self.error {
            return Err(AppError::Upstream(error.clone()));
        }
        Ok(Box::pin(futures_util::stream::iter(vec![Ok(
            fakemodelproxy::core::Event::Finish(fakemodelproxy::core::FinishReason::Stop),
        )])))
    }
}

/// 创建完整请求，支持显式会话。
fn request(model: &str, session: Option<&str>) -> ChatRequest {
    ChatRequest {
        protocol: Protocol::Chat,
        model: model.into(),
        messages: vec![fakemodelproxy::core::Message::text(
            fakemodelproxy::core::Role::User,
            "你好",
        )],
        tools: Vec::new(),
        tool_choice: None,
        max_tokens: None,
        temperature: None,
        top_p: None,
        stop: Vec::new(),
        reasoning_effort: None,
        stream: false,
        session: session.map(str::to_string),
    }
}

/// 同一已确认计费基准下，三个模型的优先级、倍率、余额依次指向不同服务。
fn fixture(error: Option<UpstreamError>) -> (Router, Registry, Arc<Mutex<Vec<String>>>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut backends: Vec<Arc<dyn Backend>> = Vec::new();
    let mut records = Vec::new();
    for (id, priority, factor, balance) in [
        ("a", 10, 0.8, 200),
        ("b", 20, 0.2, 500),
        ("c", 30, 0.5, 800),
    ] {
        let accounts = ["small", "rich"].map(|account| Account {
            id: account.into(),
            backend_id: id.into(),
            name: account.into(),
            credential: "{}".into(),
            enabled: true,
        });
        backends.push(Arc::new(Provider {
            id: id.into(),
            pool: AccountPool::new(accounts.to_vec()),
            factor,
            balance,
            called: calls.clone(),
            error: error.clone(),
        }));
        records.push(BackendRecord {
            id: id.into(),
            priority,
            cost_tier: "subscription".into(),
            options: r#"{"routing":{"comparison_scope":"confirmed-plan"}}"#.into(),
            ..Default::default()
        });
    }
    let router = Router::new();
    router.metadata.configure(&records, &HashMap::new());
    (router, Registry::from_backends(backends), calls)
}

/// 所有旧名称统一选择最高优先级层，层内固定实际账号和输出限制。
#[tokio::test]
async fn legacy_names_use_one_policy_and_pin_the_selected_account() {
    for (model, expected) in [
        ("auto:priority", "a"),
        ("auto:multiplier", "a"),
        ("auto:quota", "a"),
        ("auto:spread", "a"),
        ("auto:fast", "a"),
        ("auto:cost", "a"),
        ("auto", "a"),
    ] {
        let (router, registry, calls) = fixture(None);
        let (policy, free) = RoutePolicy::parse(model).unwrap();
        let (_, address) = router
            .dispatch(
                &registry,
                request(model, None),
                policy,
                free,
                CancellationToken::new(),
                2,
            )
            .await
            .unwrap();
        assert_eq!(address, format!("{expected}/m"));
        assert_eq!(*calls.lock().unwrap(), vec![format!("{expected}/rich")]);
    }
}

/// 未配置换算或共同基准时，两个分属不同计费域的数字不能直接比较。
#[tokio::test]
async fn separate_billing_domains_use_priority_before_native_scores() {
    let (router, registry, _) = fixture(None);
    if let Ok(mut data) = router.metadata.data.write() {
        for record in data.records.values_mut() {
            record.options = "{}".into();
        }
    }
    router.metadata.refresh(&registry).await;
    assert_eq!(
        router
            .plan(&registry, &request("auto", None), RoutePolicy::Auto, false)
            .candidates[0]
            .address(),
        "a/m"
    );
}

/// 高余额不能绕过窗口和输出能力；图片默认有预算，显式禁用时不得准入。
#[tokio::test]
async fn every_strategy_applies_context_and_output_admission() {
    let (router, registry, _) = fixture(None);
    router.metadata.refresh(&registry).await;
    let mut input = request("auto", None);
    input.messages[0].content = vec![fakemodelproxy::core::ContentPart::text("x".repeat(195000))];
    let decision = router.plan(&registry, &input, RoutePolicy::Auto, false);
    assert!(decision.candidates.is_empty());
    assert!(
        decision
            .rejected
            .iter()
            .all(|item| item.reason.contains("context_limit_exceeded"))
    );
    input = request("auto", None);
    input.max_tokens = Some(32001);
    assert!(
        router
            .plan(&registry, &input, RoutePolicy::Auto, false)
            .rejected[0]
            .reason
            .contains("output_limit_exceeded")
    );
    input.max_tokens = None;
    input.messages[0]
        .content
        .push(fakemodelproxy::core::ContentPart::ImageUrl {
            url: "data:image/png;base64,xxx".into(),
            detail: None,
        });
    assert!(
        !router
            .plan(&registry, &input, RoutePolicy::Auto, false)
            .candidates
            .is_empty()
    );
    {
        let mut data = router.metadata.data.write().unwrap();
        for record in data.records.values_mut() {
            record.options = r#"{"routing":{"image_tokens":null}}"#.into();
        }
    }
    assert!(
        router
            .plan(&registry, &input, RoutePolicy::Auto, false)
            .rejected[0]
            .reason
            .contains("image_estimator_unknown")
    );
}

/// 所有 Auto 策略均按整段历史中的图片过滤模型，且多图容量仍受窗口约束。
#[tokio::test]
async fn auto_filters_text_models_and_reserves_each_image() {
    let (router, registry, _) = fixture(None);
    router.metadata.refresh(&registry).await;
    {
        let mut data = router.metadata.data.write().unwrap();
        data.models.get_mut("a").unwrap().1[0].capabilities.images = false;
    }
    let mut image_request = request("auto", None);
    image_request.messages[0]
        .content
        .push(fakemodelproxy::core::ContentPart::ImageUrl {
            url: "data:image/png;base64,xxx".into(),
            detail: None,
        });
    // 最新用户消息没有图片，历史图片仍要求视觉能力。
    image_request
        .messages
        .push(fakemodelproxy::core::Message::text(
            fakemodelproxy::core::Role::User,
            "继续分析",
        ));
    for alias in [
        "auto",
        "auto:priority",
        "auto:quota",
        "auto:cost",
        "auto:free",
    ] {
        let (policy, free) = RoutePolicy::parse(alias).unwrap();
        let decision = router.plan(&registry, &image_request, policy, free);
        assert!(!decision.candidates.is_empty());
        assert!(
            decision
                .candidates
                .iter()
                .all(|candidate| candidate.backend.id() != "a")
        );
        assert!(
            decision
                .rejected
                .iter()
                .any(|rejection| rejection.model == "a/m"
                    && rejection.reason == "images_unsupported")
        );
        assert!(
            router
                .plan(&registry, &request("auto", None), policy, false)
                .candidates
                .iter()
                .any(|candidate| candidate.backend.id() == "a")
        );
    }
    let mut many_images = image_request.clone();
    many_images.messages[0].content = vec![
        fakemodelproxy::core::ContentPart::ImageUrl {
            url: "https://example.test/image.png".into(),
            detail: None,
        };
        24
    ];
    let decision = router.plan(&registry, &many_images, RoutePolicy::Auto, false);
    assert!(decision.candidates.is_empty());
    assert!(
        decision
            .rejected
            .iter()
            .any(|rejection| rejection.reason.contains("context_limit_exceeded"))
    );
}

/// 不接受输出硬限制的后端，不能把客户端小上限当作窗口预留。
#[tokio::test]
async fn autoglm_reserves_provider_output_without_claiming_a_request_limit() {
    use fakemodelproxy::routing::planner::{RouteSettings, admission};
    let store = fakemodelproxy::store::Store::open(std::path::Path::new(":memory:"))
        .await
        .unwrap();
    let backend = fakemodelproxy::backend::autoglm::AutoGlmBackend::new(
        &BackendRecord::default(),
        Vec::new(),
        store,
    )
    .unwrap();
    let (_, registry, _) = fixture(None);
    let mut model = registry.list()[0].list_models().await.unwrap().remove(0);
    model.token_limits.context_window = Some(20000);
    model.token_limits.max_output_tokens = None;
    let mut input = request("auto", None);
    input.max_tokens = Some(128);
    assert_eq!(
        admission(&backend, &model, &input, &RouteSettings::default()).unwrap_err(),
        "provider_output_limit_unknown"
    );
    let settings = RouteSettings {
        provider_output_reserve: Some(8192),
        ..Default::default()
    };
    let limits = admission(&backend, &model, &input, &settings).unwrap();
    assert_eq!(limits.output_reserve, 8192);
    assert!(!limits.output_enforced);
    model.token_limits.max_output_tokens = Some(32000);
    assert!(
        admission(&backend, &model, &input, &settings)
            .unwrap_err()
            .contains("context_limit_exceeded")
    );
    model.token_limits.context_window = Some(200000);
    let limits = admission(&backend, &model, &input, &settings).unwrap();
    assert_eq!(limits.output_reserve, 32000);
}

/// 目录最大档位不能覆盖实际默认档位，过期报价也不能继续参与排序。
#[tokio::test]
async fn default_context_tier_and_price_expiration_are_enforced() {
    let (router, registry, _) = fixture(None);
    router.metadata.refresh(&registry).await;
    let mut data = router.metadata.data.write().unwrap();
    let model = &mut data.models.get_mut("a").unwrap().1[0];
    model.token_limits.context_window = Some(1000000);
    model.token_limits.context_tiers = vec![
        ContextTier {
            tokens: 10000,
            is_default: true,
        },
        ContextTier {
            tokens: 1000000,
            is_default: false,
        },
    ];
    let b = &mut data.models.get_mut("b").unwrap().1[0];
    b.price_valid_until = Some(1);
    drop(data);
    let decision = router.plan(&registry, &request("auto", None), RoutePolicy::Auto, false);
    assert!(
        decision
            .rejected
            .iter()
            .any(|item| item.model == "a/m" && item.reason.contains("context_limit_exceeded"))
    );
    assert_eq!(decision.candidates[0].address(), "b/m");
    assert!(decision.candidates[0].price_factor.is_none());
}

/// 已知耗尽账号排除；未知余额保留，不能被补成零。
#[tokio::test]
async fn exhausted_and_unknown_quota_remain_distinct() {
    let (router, registry, _) = fixture(None);
    router.metadata.refresh(&registry).await;
    let now = fakemodelproxy::records::now_seconds();
    let mut data = router.metadata.data.write().unwrap();
    for id in ["small", "rich"] {
        data.quotas.insert(
            format!("c/{id}"),
            (
                now,
                QuotaSnapshot {
                    remaining: Some("0".into()),
                    complete: true,
                    unit: "credits".into(),
                    ..Default::default()
                },
            ),
        );
    }
    data.quotas
        .insert("b/rich".into(), (now, QuotaSnapshot::default()));
    drop(data);
    let decision = router.plan(&registry, &request("auto", None), RoutePolicy::Auto, false);
    assert!(
        decision
            .candidates
            .iter()
            .all(|candidate| candidate.backend.id() != "c")
    );
    assert!(
        decision
            .candidates
            .iter()
            .any(|candidate| candidate.backend.id() == "b"
                && candidate.account_id.as_deref() == Some("rich"))
    );
}

/// 同一会话切换兼容名称仍保留原目标，别名不生成独立分组。
#[tokio::test]
async fn legacy_aliases_share_the_same_session_binding() {
    let (router, registry, calls) = fixture(None);
    for (model, expected) in [("auto", "a/m"), ("auto:quota", "a/m")] {
        let (policy, free) = RoutePolicy::parse(model).unwrap();
        let (_, address) = router
            .dispatch(
                &registry,
                request(model, Some("s")),
                policy,
                free,
                CancellationToken::new(),
                2,
            )
            .await
            .unwrap();
        assert_eq!(address, expected);
    }
    assert_eq!(calls.lock().unwrap().len(), 2);
    assert_eq!(router.gateway.balancer.snapshot().groups, 1);
}

/// 上游执行结果未知时不能尝试其他候选，防止重复消费与重复生成。
#[tokio::test]
async fn unknown_execution_is_never_replayed() {
    let (router, registry, calls) = fixture(Some(UpstreamError {
        status: None,
        code: "unknown".into(),
        message: "unknown execution".into(),
        retryable: true,
        execution_unknown: true,
    }));
    assert!(
        router
            .dispatch(
                &registry,
                request("auto", None),
                RoutePolicy::Auto,
                false,
                CancellationToken::new(),
                2
            )
            .await
            .is_err()
    );
    assert_eq!(calls.lock().unwrap().len(), 1);
}

/// 注册表在发布新路由快照前变化，Auto 仍使用同一版本的实例与预算。
#[tokio::test]
async fn registry_changes_do_not_mix_versions_inside_auto_snapshot() {
    let (router, registry, _) = fixture(None);
    let records = router
        .metadata
        .data
        .read()
        .unwrap()
        .records
        .values()
        .cloned()
        .collect::<Vec<_>>();
    router
        .metadata
        .configure_registry(&records, &HashMap::new(), &registry);
    let empty = Registry::new();
    router.metadata.refresh(&empty).await;
    let decision = router.plan(&empty, &request("auto", None), RoutePolicy::Auto, false);
    assert_eq!(decision.candidates[0].address(), "a/m");
    assert_eq!(decision.config_version, 2);
}

/// 新目录只有输入上限时，兼容窗口字段不能再推断成总窗口。
#[test]
fn input_only_catalog_keeps_total_context_unknown() {
    let backend = fakemodelproxy::backend::workbuddy::WorkBuddyBackend::new(
        "wb".into(),
        "WB".into(),
        fakemodelproxy::backend::workbuddy::profile_from_options("{}").unwrap(),
        Vec::new(),
        Vec::new(),
    )
    .unwrap();
    let model = ModelInfo {
        id: "m".into(),
        display_name: None,
        price_factor: None,
        original_price_factor: None,
        price_valid_until: None,
        capabilities: Capabilities {
            context_window: Some(20000),
            ..Default::default()
        },
        token_limits: TokenLimits {
            max_input_tokens: Some(20000),
            max_output_tokens: Some(32000),
            ..Default::default()
        },
    };
    let mut input = request("auto", None);
    input.messages[0].content = vec![fakemodelproxy::core::ContentPart::text("x".repeat(10000))];
    let limits =
        fakemodelproxy::routing::planner::admission(&backend, &model, &input, &Default::default())
            .unwrap();
    assert_eq!(limits.context_limit, None);
    assert_eq!(limits.input_limit, Some(20000));
}

/// 用户对账号后端声明禁用工具时，动态模型目录不能覆盖该配置。
#[tokio::test]
async fn private_backend_capability_overrides_apply_to_each_model() {
    let (router, registry, _) = fixture(None);
    router.metadata.refresh(&registry).await;
    {
        let mut data = router.metadata.data.write().unwrap();
        for record in data.records.values_mut() {
            record.capabilities = r#"{"tools":false}"#.into();
        }
    }
    let mut input = request("auto", None);
    input.tools.push(fakemodelproxy::core::ToolDef {
        name: "probe".into(),
        description: String::new(),
        parameters: serde_json::json!({"type":"object"}),
    });
    assert!(
        router
            .plan(&registry, &input, RoutePolicy::Auto, false)
            .candidates
            .is_empty()
    );
}

/// 费用、额度可信度先分层；高优先级不能越过费用限制或把未知余额当作确认可用。
#[tokio::test]
async fn unified_auto_layers_quota_before_priority_and_gates_paid_fallback() {
    let (router, registry, _) = fixture(None);
    router.metadata.refresh(&registry).await;
    {
        let mut data = router.metadata.data.write().unwrap();
        data.records.get_mut("a").unwrap().priority = 1;
        data.records.get_mut("b").unwrap().priority = 100;
        data.records.get_mut("c").unwrap().cost_tier = "paid".into();
        data.records.get_mut("c").unwrap().priority = 0;
        for account in ["small", "rich"] {
            data.quotas.get_mut(&format!("a/{account}")).unwrap().1 = QuotaSnapshot::default();
        }
    }
    let input = request("auto", None);
    let decision = router.plan(&registry, &input, RoutePolicy::Auto, false);
    assert_eq!(decision.candidates[0].backend.id(), "b");
    assert!(
        decision
            .candidates
            .iter()
            .any(|candidate| candidate.backend.id() == "a" && !candidate.quota_known())
    );
    assert!(
        decision
            .rejected
            .iter()
            .any(|rejection| rejection.model == "c/m" && rejection.reason == "cost_not_allowed")
    );
    let mut settings = router.gateway.settings();
    settings.auto_allow_paid_fallback = true;
    router.gateway.configure(settings).unwrap();
    let decision = router.plan(&registry, &input, RoutePolicy::Auto, false);
    assert_eq!(decision.candidates[0].backend.id(), "b");
    assert_eq!(decision.candidates.last().unwrap().pool(), "paid");
    let free = router.plan(&registry, &input, RoutePolicy::Auto, true);
    assert!(
        free.candidates
            .iter()
            .all(|candidate| candidate.pool() == "quota")
    );
}

/// 容量达到上限时自动使用下一优先级，新组可降级，健康旧组仍等待原目标。
#[tokio::test]
async fn unified_auto_falls_back_by_priority_when_capacity_is_full() {
    use fakemodelproxy::gateway::{TargetSettings, balancer::ReserveError};
    let (router, registry, _) = fixture(None);
    router.metadata.refresh(&registry).await;
    let mut settings = router.gateway.settings();
    settings.targets.insert(
        "a".into(),
        TargetSettings {
            capacity: 1,
            weight: 1,
        },
    );
    let decision = router.plan(&registry, &request("auto", None), RoutePolicy::Auto, false);
    let balancer = &router.gateway.balancer;
    let (first, lease) = balancer
        .reserve(
            &decision.candidates,
            "first",
            RoutePolicy::Auto,
            &settings,
            &Default::default(),
        )
        .ok()
        .unwrap();
    assert_eq!(decision.candidates[first].backend.id(), "a");
    assert!(matches!(
        balancer.reserve(
            &decision.candidates,
            "first",
            RoutePolicy::Auto,
            &settings,
            &Default::default()
        ),
        Err(ReserveError::Busy)
    ));
    let (second, other) = balancer
        .reserve(
            &decision.candidates,
            "second",
            RoutePolicy::Auto,
            &settings,
            &Default::default(),
        )
        .ok()
        .unwrap();
    assert_eq!(decision.candidates[second].backend.id(), "b");
    drop((lease, other));
}

/// 同优先级与同计费范围才比较倍率；经济目标全忙时立即使用次优档位。
#[tokio::test]
async fn unified_auto_uses_comparable_prices_and_next_price_on_saturation() {
    use fakemodelproxy::gateway::TargetSettings;
    let (router, registry, _) = fixture(None);
    {
        let mut data = router.metadata.data.write().unwrap();
        for record in data.records.values_mut() {
            record.priority = 10;
        }
    }
    router.metadata.refresh(&registry).await;
    let settings = fakemodelproxy::gateway::GatewaySettings {
        targets: HashMap::from([(
            "b".into(),
            TargetSettings {
                capacity: 1,
                weight: 1,
            },
        )]),
        ..Default::default()
    };
    let decision = router.plan(&registry, &request("auto", None), RoutePolicy::Auto, false);
    let (first, lease) = router
        .gateway
        .balancer
        .reserve(
            &decision.candidates,
            "one",
            RoutePolicy::Auto,
            &settings,
            &Default::default(),
        )
        .ok()
        .unwrap();
    assert_eq!(decision.candidates[first].backend.id(), "b");
    assert_eq!(
        decision.candidates[first].account_id.as_deref(),
        Some("rich")
    );
    let (second, other) = router
        .gateway
        .balancer
        .reserve(
            &decision.candidates,
            "two",
            RoutePolicy::Auto,
            &settings,
            &Default::default(),
        )
        .ok()
        .unwrap();
    assert_eq!(decision.candidates[second].backend.id(), "c");
    drop((lease, other));
}

/// 已知免费额度耗尽时，是否能调用付费池由网关开关控制，不由别名绕过。
#[tokio::test]
async fn exhausted_quota_pool_uses_paid_only_after_explicit_opt_in() {
    let (router, registry, calls) = fixture(None);
    router.metadata.refresh(&registry).await;
    {
        let mut data = router.metadata.data.write().unwrap();
        data.records.get_mut("c").unwrap().cost_tier = "paid".into();
        for backend in ["a", "b"] {
            for account in ["small", "rich"] {
                data.quotas
                    .get_mut(&format!("{backend}/{account}"))
                    .unwrap()
                    .1
                    .remaining = Some("0".into());
            }
        }
    }
    assert!(
        router
            .dispatch(
                &registry,
                request("auto:fast", None),
                RoutePolicy::Auto,
                false,
                CancellationToken::new(),
                1
            )
            .await
            .is_err()
    );
    assert!(calls.lock().unwrap().is_empty());
    let mut settings = router.gateway.settings();
    settings.auto_allow_paid_fallback = true;
    router.gateway.configure(settings).unwrap();
    let (stream, address) = router
        .dispatch(
            &registry,
            request("auto", None),
            RoutePolicy::Auto,
            false,
            CancellationToken::new(),
            1,
        )
        .await
        .unwrap();
    assert_eq!(address, "c/m");
    assert!(stream.collect::<Vec<_>>().await.iter().all(Result::is_ok));
    assert!(
        router
            .dispatch(
                &registry,
                request("auto:free", None),
                RoutePolicy::Auto,
                true,
                CancellationToken::new(),
                1
            )
            .await
            .is_err()
    );
}

/// 一个服务的模型目录数量不能改变跨服务的新组份额。
#[tokio::test]
async fn unified_auto_does_not_weight_backends_by_model_count() {
    let (router, registry, _) = fixture(None);
    router.metadata.refresh(&registry).await;
    let mut candidates = router
        .plan(&registry, &request("auto", None), RoutePolicy::Auto, false)
        .candidates;
    for candidate in &mut candidates {
        candidate.priority = 10;
        candidate.comparison_scope = candidate.backend.id().into();
    }
    let model = candidates
        .iter()
        .find(|candidate| candidate.backend.id() == "a")
        .unwrap()
        .clone();
    for index in 0..20 {
        let mut extra = model.clone();
        extra.model = format!("extra-{index}");
        candidates.push(extra);
    }
    let settings = router.gateway.settings();
    let mut chosen = Vec::new();
    for group in 0..12 {
        let (index, lease) = router
            .gateway
            .balancer
            .reserve(
                &candidates,
                &format!("group-{group}"),
                RoutePolicy::Auto,
                &settings,
                &Default::default(),
            )
            .ok()
            .unwrap();
        chosen.push(candidates[index].backend.id().to_string());
        drop(lease);
    }
    for backend in ["a", "b", "c"] {
        assert_eq!(chosen.iter().filter(|id| id.as_str() == backend).count(), 4);
    }
}
