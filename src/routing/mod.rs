//! auto 路由。
//!
//! 规则型选择：能力、费用与健康做硬筛选，再按策略排序；会话亲和负责
//! 让同一会话稳定落在同一后端。不做 LLM 任务分类，也不在已向客户端
//! 输出内容后切换后端。

pub mod affinity;
pub mod health;
pub mod planner;

pub use affinity::SessionAffinity;
pub use health::{HealthSnapshot, HealthTracker};

use crate::backend::{Backend, Registry};
use crate::core::ChatRequest;
use crate::core::error::AppError;
use crate::core::stream::EventStream;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// 自动额度池与显式模型共用网关，显式模型只进行账号负载分配。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutePolicy {
    /// 费用和额度分层、层内优先级、同级经济性与负载分散。
    Auto,
    /// 显式模型只按账号负载分散，不改写调用方选择。
    Spread,
}

impl RoutePolicy {
    /// 旧名称作为统一 Auto 的兼容别名；auto:free 保留禁止付费的限制。
    pub fn parse(model: &str) -> Option<(RoutePolicy, bool)> {
        match model {
            "auto" | "auto:priority" | "auto:multiplier" | "auto:quota" | "auto:spread"
            | "auto:cost" | "auto:fast" => Some((RoutePolicy::Auto, false)),
            "auto:free" => Some((RoutePolicy::Auto, true)),
            _ => None,
        }
    }

    /// 返回策略名，用于日志与界面。
    pub fn as_str(self) -> &'static str {
        match self {
            RoutePolicy::Auto => "auto",
            RoutePolicy::Spread => "explicit",
        }
    }
}

/// 一个可调用候选。
#[derive(Clone)]
pub struct Candidate {
    pub account_id: Option<String>,
    pub account_order: usize,
    pub price_factor: Option<f64>,
    pub comparison_scope: String,
    pub quota: crate::accounts::quota::QuotaSnapshot,
    pub limits: planner::EffectiveLimits,
    pub backend: Arc<dyn Backend>,
    /// 上游模型名（不含 backend 前缀）。
    pub model: String,
    pub cost_tier: crate::backend::CostTier,
    pub priority: u8,
    /// 后端延迟估计（毫秒），未知为 None。
    pub latency_ms: Option<u64>,
    /// 每日 token 预算，0 表示不限制。
    pub daily_token_budget: u64,
}

impl Candidate {
    /// 免费与已有套餐额度共用主池，付费和费用未知分别作为显式开启的后备。
    pub fn pool(&self) -> &'static str {
        match self.cost_tier {
            crate::backend::CostTier::Free | crate::backend::CostTier::Subscription => "quota",
            crate::backend::CostTier::Paid => "paid",
            crate::backend::CostTier::Unknown => "unknown_cost",
        }
    }

    /// 缓存已过准入有效期检查，未知余额只进入候补层，不等于零。
    pub fn quota_known(&self) -> bool {
        self.quota.complete
            && (self.quota.unlimited || self.quota.amount().is_some_and(|value| value > 0))
    }

    /// 先分池及额度可信度，再使用显式配置的优先级，数值越小越优先。
    pub fn auto_rank(&self) -> (u8, u8, u8) {
        let pool = match self.pool() {
            "quota" => 0,
            "paid" => 1,
            _ => 2,
        };
        (pool, u8::from(!self.quota_known()), self.priority)
    }

    /// 完整模型地址。
    pub fn address(&self) -> String {
        format!("{}/{}", self.backend.id(), self.model)
    }

    /// 绑定精确模型和账号，余额排序结果不能被轮询覆盖。
    pub fn binding(&self) -> String {
        format!(
            "{}@{}",
            self.address(),
            self.account_id.as_deref().unwrap_or("")
        )
    }
}

impl std::fmt::Debug for Candidate {
    /// 只展示定位信息，避免要求 Backend 实现 Debug。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Candidate")
            .field("backend", &self.backend.id())
            .field("model", &self.model)
            .field("cost_tier", &self.cost_tier)
            .field("priority", &self.priority)
            .field("latency_ms", &self.latency_ms)
            .field("daily_token_budget", &self.daily_token_budget)
            .finish()
    }
}

/// 被排除的候选及原因。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Rejection {
    pub model: String,
    pub reason: String,
}

/// 一次路由决策。
#[derive(Debug, Clone, Default)]
pub struct Decision {
    pub config_version: u64,
    pub comparison_scope: Option<String>,
    pub affinity_hit: bool,
    pub candidates: Vec<Candidate>,
    /// 保留已准入但处于局部冷却的目标，用于解释网关恢复时间。
    pub blocked: Vec<Candidate>,
    pub rejected: Vec<Rejection>,
}

impl Decision {
    /// 返回便于排查的单行摘要。
    pub fn summary(&self) -> String {
        let picked = self
            .candidates
            .iter()
            .map(Candidate::address)
            .collect::<Vec<_>>()
            .join(", ");
        let rejected = self
            .rejected
            .iter()
            .map(|item| format!("{} ({})", item.model, item.reason))
            .collect::<Vec<_>>()
            .join(", ");
        format!("candidates=[{picked}] rejected=[{rejected}]")
    }
}

/// 路由上下文：健康与亲和跨请求保留。
#[derive(Default)]
pub struct Router {
    pub metadata: Arc<planner::Metadata>,
    /// 后端级健康与延迟。
    pub health: Arc<HealthTracker>,
    /// 会话到模型地址的绑定。
    pub affinity: Arc<SessionAffinity>,
    /// 用量账本。None 表示不检查预算（测试或未启用）。
    pub usage: Option<Arc<crate::usage::UsageLedger>>,
    /// 配置热加载不能替换执行中的容量、绑定与重试预算。
    pub gateway: Arc<crate::gateway::Gateway>,
}

impl Router {
    /// 捕获配置与动态元数据；健康、用量、绑定和租约仍跨 generation 共享。
    pub fn fork(&self) -> Self {
        Self {
            metadata: Arc::new(self.metadata.fork()),
            health: self.health.clone(),
            affinity: self.affinity.clone(),
            usage: self.usage.clone(),
            gateway: self.gateway.clone(),
        }
    }

    /// 创建空路由上下文。
    pub fn new() -> Self {
        Self::default()
    }

    /// 生成候选计划。
    pub fn plan(
        &self,
        registry: &Registry,
        request: &ChatRequest,
        policy: RoutePolicy,
        free_only: bool,
    ) -> Decision {
        planner::plan(self, registry, request, policy, free_only)
    }

    /// 兼容已有 Auto 调用方，执行生命周期由统一网关管理。
    pub async fn dispatch(
        &self,
        registry: &Registry,
        request: ChatRequest,
        policy: RoutePolicy,
        free_only: bool,
        cancel: CancellationToken,
        max_attempts: usize,
    ) -> Result<(EventStream, String), AppError> {
        self.gateway
            .execute(
                self,
                registry,
                request,
                crate::gateway::DispatchOptions {
                    policy,
                    free_only,
                    explicit: None,
                    max_attempts,
                },
                cancel,
            )
            .await
    }
}

/// 保守按 UTF-8 字节估算完整文本、工具调用与结果，图片单独进入模型估算。
pub fn estimate_tokens(request: &ChatRequest) -> u32 {
    let mut bytes = 0_u64;
    for message in &request.messages {
        bytes = bytes.saturating_add(message.text_content().len() as u64 + 32);
        for call in &message.tool_calls {
            bytes = bytes.saturating_add(
                call.id.len() as u64 + call.name.len() as u64 + call.arguments.len() as u64 + 32,
            );
        }
        bytes = bytes.saturating_add(message.tool_call_id.as_ref().map_or(0, |id| id.len()) as u64);
    }
    bytes = bytes
        .saturating_add(serde_json::to_vec(&request.tools).map_or(0, |json| json.len()) as u64);
    bytes.max(1).min(u64::from(u32::MAX)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::CostTier;
    use crate::core::stream::EventStream;
    use crate::core::{Capabilities, ModelInfo, Protocol, UpstreamError};
    use async_trait::async_trait;

    /// 测试后端。
    struct StubBackend {
        id: String,
        models: Vec<String>,
        capabilities: Capabilities,
        cost_tier: CostTier,
        priority: u8,
        fail: bool,
    }

    #[async_trait]
    impl Backend for StubBackend {
        /// 返回标识。
        fn id(&self) -> &str {
            &self.id
        }

        /// 返回名称。
        fn name(&self) -> &str {
            &self.id
        }

        /// 返回类型。
        fn kind(&self) -> &str {
            "stub"
        }

        /// 返回能力。
        fn capabilities(&self) -> Capabilities {
            self.capabilities.clone()
        }

        /// 返回费用层级。
        fn cost_tier(&self) -> CostTier {
            self.cost_tier
        }

        /// 返回优先级。
        fn priority(&self) -> u8 {
            self.priority
        }

        /// 返回静态模型。
        fn static_models(&self) -> Vec<String> {
            self.models.clone()
        }

        /// 返回模型信息。
        async fn list_models(&self) -> Result<Vec<ModelInfo>, AppError> {
            Ok(self
                .models
                .iter()
                .map(|model| ModelInfo {
                    id: model.clone(),
                    display_name: None,
                    price_factor: None,
                    original_price_factor: None,
                    price_valid_until: None,
                    capabilities: self.capabilities.clone(),
                    token_limits: Default::default(),
                })
                .collect())
        }

        /// 按需失败或返回有终止事件的合法流。
        async fn chat(
            &self,
            _request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<EventStream, AppError> {
            if self.fail {
                return Err(AppError::Upstream(UpstreamError::status(
                    503,
                    "unavailable",
                )));
            }
            Ok(Box::pin(futures_util::stream::iter(vec![Ok(
                crate::core::Event::Finish(crate::core::FinishReason::Stop),
            )])))
        }
    }

    /// 构造测试后端。
    fn backend(id: &str, cost_tier: CostTier, priority: u8, fail: bool) -> Arc<dyn Backend> {
        Arc::new(StubBackend {
            id: id.to_string(),
            models: vec!["m1".to_string()],
            capabilities: Capabilities {
                tools: true,
                images: true,
                context_window: Some(200_000),
                ..Capabilities::default()
            },
            cost_tier,
            priority,
            fail,
        })
    }

    /// 构造最小请求。
    fn request(session: Option<&str>) -> ChatRequest {
        ChatRequest {
            protocol: Protocol::Chat,
            model: "auto".into(),
            messages: vec![crate::core::Message::text(crate::core::Role::User, "hi")],
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop: Vec::new(),
            reasoning_effort: None,
            stream: false,
            session: session.map(ToOwned::to_owned),
        }
    }

    /// 失败的后端进入冷却，不再参与候选。
    #[tokio::test]
    async fn failed_backend_is_cooled_down() {
        let router = Router::new();
        let registry = Registry::from_backends(vec![backend("bad", CostTier::Free, 1, true)]);
        let cancel = CancellationToken::new();
        assert!(
            router
                .dispatch(
                    &registry,
                    request(None),
                    RoutePolicy::Auto,
                    false,
                    cancel,
                    1
                )
                .await
                .is_err()
        );
        let decision = router.plan(&registry, &request(None), RoutePolicy::Auto, false);
        assert!(decision.candidates.is_empty());
        assert!(decision.rejected[0].reason.contains("cooling"));
    }

    /// 成功调用后会话绑定到该地址，下次优先命中。
    #[tokio::test]
    async fn session_binds_after_success() {
        let router = Router::new();
        let registry = Registry::from_backends(vec![
            backend("a", CostTier::Free, 10, false),
            backend("b", CostTier::Free, 20, false),
        ]);
        let cancel = CancellationToken::new();
        let (_, address) = router
            .dispatch(
                &registry,
                request(Some("s1")),
                RoutePolicy::Auto,
                false,
                cancel,
                1,
            )
            .await
            .expect("dispatch");
        assert_eq!(address, "a/m1");
        let key = planner::affinity_key(&request(Some("s1")), RoutePolicy::Auto, true, 0).unwrap();
        assert_eq!(router.affinity.get(&key).as_deref(), Some("a/m1@"));

        // 即使出现更高优先级的后端，亲和仍把已绑定候选排在最前。
        let registry = Registry::from_backends(vec![
            backend("a", CostTier::Free, 10, false),
            backend("z", CostTier::Free, 1, false),
        ]);
        let decision = router.plan(&registry, &request(Some("s1")), RoutePolicy::Auto, false);
        assert_eq!(decision.candidates[0].address(), "a/m1");
    }

    /// 已测延迟不能让低优先级目标越过配置的主层级。
    #[test]
    fn priority_precedes_measured_latency() {
        let router = Router::new();
        router
            .health
            .report_success("slow", std::time::Duration::from_millis(900));
        router
            .health
            .report_success("fast", std::time::Duration::from_millis(80));
        let registry = Registry::from_backends(vec![
            backend("slow", CostTier::Free, 1, false),
            backend("fast", CostTier::Free, 99, false),
            backend("unknown", CostTier::Free, 1, false),
        ]);
        let decision = router.plan(&registry, &request(None), RoutePolicy::Auto, false);
        assert_eq!(decision.candidates[0].backend.id(), "slow");
        assert_eq!(decision.candidates[2].backend.id(), "fast");
    }

    /// 旧变体归一到相同策略，免费别名仍保留不付费限制。
    #[test]
    fn parses_auto_variants() {
        assert_eq!(
            RoutePolicy::parse("auto:fast"),
            Some((RoutePolicy::Auto, false))
        );
        assert_eq!(
            RoutePolicy::parse("auto:free"),
            Some((RoutePolicy::Auto, true))
        );
        assert_eq!(RoutePolicy::parse("gpt-4o"), None);
    }
}
