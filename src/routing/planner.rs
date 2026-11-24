//! 所有 Auto 共用模型预算与账号准入；排序在比较域内形成全序。

use super::{Candidate, Decision, Rejection, RoutePolicy, Router, estimate_tokens};
use crate::accounts::quota::QuotaSnapshot;
use crate::backend::{Backend, CostTier, Registry};
use crate::core::{ChatRequest, ModelInfo};
use crate::store::BackendRecord;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::RwLock;

/// 后端 options.routing 可覆盖的准入预算，图片按张数预留容量。
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct RouteSettings {
    /// 整个服务每个滚动一秒窗口的生成提交上限，0 表示不限。
    #[schemars(range(max = "crate::gateway::MAX_SERVICE_QPS"))]
    pub qps: u32,
    pub candidate_models: Vec<String>,
    pub model_priority: HashMap<String, u8>,
    /// 用户显式确认的同一计费基准；不同额度单位仍保持独立。
    pub comparison_scope: Option<String>,
    pub default_output_tokens: u32,
    /// 不执行 max_tokens 的上游，由部署显式确认的最坏输出预留。
    pub provider_output_reserve: Option<u32>,
    pub input_limit: Option<u32>,
    pub context_limit: Option<u32>,
    pub provider_input_reserve: u32,
    /// 每张图片的容量估算；null 可显式禁止图片参与 Auto。
    pub image_tokens: Option<u32>,
    pub input_margin_tokens: u32,
    pub input_margin_percent: u32,
}

impl Default for RouteSettings {
    /// 以明确的输出硬限制和输入余量作为默认准入预算。
    fn default() -> Self {
        Self {
            qps: 0,
            candidate_models: Vec::new(),
            model_priority: HashMap::new(),
            comparison_scope: None,
            default_output_tokens: 8192,
            provider_output_reserve: None,
            input_limit: None,
            context_limit: None,
            provider_input_reserve: 1024,
            image_tokens: Some(8192),
            input_margin_tokens: 1024,
            input_margin_percent: 5,
        }
    }
}

/// 一次模型准入的限制及估算来源。
#[derive(Debug, Clone, Serialize)]
pub struct EffectiveLimits {
    pub input_limit: Option<u32>,
    pub context_limit: Option<u32>,
    pub output_limit: Option<u32>,
    pub output_reserve: u32,
    pub output_enforced: bool,
    pub estimated_input: u32,
    pub input_margin: u32,
    pub estimator: &'static str,
    pub limit_source: &'static str,
}

/// 路由持有的无凭证元数据，热加载后同步失效。
#[derive(Default)]
pub struct Metadata {
    pub data: RwLock<Snapshot>,
    pub refresh: tokio::sync::Mutex<()>,
    store: RwLock<Option<crate::store::Store>>,
    frozen: bool,
}

#[derive(Default, Clone)]
pub struct Snapshot {
    pub backends: Option<Vec<std::sync::Arc<dyn Backend>>>,
    pub records: HashMap<String, BackendRecord>,
    pub models: HashMap<String, (u64, Vec<ModelInfo>)>,
    pub quotas: HashMap<String, (u64, QuotaSnapshot)>,
    pub version: u64,
}

impl Metadata {
    /// 请求固定本次准入配置，重试也不混入下一 generation 的候选。
    pub fn fork(&self) -> Self {
        Self {
            data: RwLock::new(self.data.read().unwrap_or_else(|p| p.into_inner()).clone()),
            refresh: Default::default(),
            store: RwLock::new(self.store.read().unwrap_or_else(|p| p.into_inner()).clone()),
            frozen: true,
        }
    }

    /// 明确额度不足后撤销该账号的旧准入快照，后续保留未知而不是错误正余额。
    pub async fn invalidate_quota(&self, candidate: &Candidate) {
        let Some(account) = &candidate.account_id else {
            return;
        };
        let key = format!("{}/{}", candidate.backend.id(), account);
        let checked = self
            .data
            .write()
            .ok()
            .and_then(|mut data| data.quotas.remove(&key).map(|(checked, _)| checked));
        let store = self.store.read().ok().and_then(|store| store.clone());
        if let (Some(store), Some(checked)) = (store, checked)
            && let Err(error) = store
                .invalidate_quota(candidate.backend.id(), account, checked)
                .await
        {
            tracing::warn!(backend = candidate.backend.id(), %error, "invalidate quota cache failed");
        }
    }

    /// 应用运行时复用服务端持久缓存，独立 Router 调用仍支持远端元数据刷新。
    pub fn attach_store(&self, store: crate::store::Store) {
        if let Ok(mut current) = self.store.write() {
            *current = Some(store);
        }
    }

    /// 原子更新候选配置与已持久化目录，并让在途旧快照不能覆盖新配置。
    pub fn configure(&self, records: &[BackendRecord], catalogs: &HashMap<String, Vec<ModelInfo>>) {
        self.configure_snapshot(records, catalogs, None);
    }

    /// 同时发布后端实例、配置和目录，热更新不会混用新旧后端。
    pub fn configure_registry(
        &self,
        records: &[BackendRecord],
        catalogs: &HashMap<String, Vec<ModelInfo>>,
        registry: &Registry,
    ) {
        self.configure_snapshot(records, catalogs, Some(registry.list()));
    }

    /// 在同一个写锁内发布完整的路由配置快照。
    fn configure_snapshot(
        &self,
        records: &[BackendRecord],
        catalogs: &HashMap<String, Vec<ModelInfo>>,
        backends: Option<Vec<std::sync::Arc<dyn Backend>>>,
    ) {
        {
            let mut data = self.data.write().unwrap_or_else(|p| p.into_inner());
            data.backends = backends;
            data.records = records
                .iter()
                .map(|record| (record.id.clone(), record.clone()))
                .collect();
            data.models = catalogs
                .iter()
                .map(|(id, models)| (id.clone(), (0, models.clone())))
                .collect();
            data.quotas.clear();
            data.version = data.version.wrapping_add(1);
        }
    }

    /// 合并并发刷新，后端之间并行；目录五分钟、余额一分钟，过期报价立即刷新。
    pub async fn refresh(&self, registry: &Registry) {
        if self.frozen {
            return;
        }
        let store = self.store.read().ok().and_then(|store| store.clone());
        if let Some(store) = store {
            // 生成热路径不访问厂商额度和模型目录；缺失/失效保留未知。
            let version = self.data.read().map(|data| data.version).unwrap_or(0);
            if let Ok(quotas) = store.cached_quota().await
                && let Ok(mut data) = self.data.write()
                && data.version == version
            {
                data.quotas = quotas
                    .into_iter()
                    .map(|info| {
                        (
                            format!("{}/{}", info.backend_id, info.account_id),
                            (info.checked_at, QuotaSnapshot::from_info(&info)),
                        )
                    })
                    .collect();
            }
            return;
        }
        let _guard = self.refresh.lock().await;
        let now = crate::records::now_seconds();
        let (version, backends) = self
            .data
            .read()
            .map(|data| {
                (
                    data.version,
                    data.backends.clone().unwrap_or_else(|| registry.list()),
                )
            })
            .unwrap_or((0, registry.list()));
        let jobs = backends.into_iter().map(|backend| async move {
            let needs_models = self
                .data
                .read()
                .map(|data| {
                    data.models
                        .get(backend.id())
                        .is_none_or(|(checked, models)| {
                            now.saturating_sub(*checked) >= 300
                                || models.iter().any(|model| {
                                    model.price_valid_until.is_some_and(|expiry| expiry <= now)
                                })
                        })
                })
                .unwrap_or(true);
            if needs_models {
                let result =
                    tokio::time::timeout(std::time::Duration::from_secs(25), backend.list_models())
                        .await;
                if let Ok(Ok(models)) = result
                    && let Ok(mut data) = self.data.write()
                    && data.version == version
                {
                    data.models.insert(backend.id().into(), (now, models));
                }
            }
            let accounts = backend.routing_accounts().unwrap_or_default();
            let jobs = accounts
                .into_iter()
                .filter(|account| account.enabled && !account.cooling)
                .map(|account| {
                    let backend = backend.clone();
                    async move {
                        let key = format!("{}/{}", backend.id(), account.id);
                        let needs_quota = self
                            .data
                            .read()
                            .map(|data| {
                                data.quotas.get(&key).is_none_or(|(checked, snapshot)| {
                                    now.saturating_sub(*checked) >= 60
                                        || snapshot.valid_until.is_some_and(|expiry| expiry <= now)
                                })
                            })
                            .unwrap_or(true);
                        if !needs_quota {
                            return;
                        }
                        let result = tokio::time::timeout(
                            std::time::Duration::from_secs(25),
                            backend.routing_quota(&account.id),
                        )
                        .await;
                        let snapshot = match result {
                            Ok(Ok(info)) => QuotaSnapshot::from_info(&info),
                            _ => QuotaSnapshot::default(),
                        };
                        if let Ok(mut data) = self.data.write()
                            && data.version == version
                        {
                            data.quotas.insert(key, (now, snapshot));
                        }
                    }
                });
            futures_util::future::join_all(jobs).await;
        });
        futures_util::future::join_all(jobs).await;
    }

    /// 后台更新五分钟目录缓存，生成请求只使用已发布的快照。
    pub async fn refresh_models(&self, registry: &Registry) {
        let _guard = self.refresh.lock().await;
        let now = crate::records::now_seconds();
        let (version, backends, records) = self
            .data
            .read()
            .map(|data| {
                (
                    data.version,
                    data.backends.clone().unwrap_or_else(|| registry.list()),
                    data.records.clone(),
                )
            })
            .unwrap_or_default();
        let store = self.store.read().ok().and_then(|store| store.clone());
        let jobs: Vec<_> = backends.into_iter().map(|backend| {
            let store = store.clone();
            let record = records.get(backend.id()).cloned();
            async move {
                let needed = self.data.read().is_ok_and(|data| data.models.get(backend.id())
                    .is_none_or(|(checked, models)| now.saturating_sub(*checked) >= 300
                        || models.iter().any(|model| model.price_valid_until.is_some_and(|expiry| expiry <= now))));
                if !needed { return; }
                if let Ok(Ok(models)) = tokio::time::timeout(std::time::Duration::from_secs(25), backend.list_models()).await {
                    let current = self.data.read().is_ok_and(|data| data.version == version);
                    if !current { return; }
                    if let (Some(store), Some(record)) = (&store, record.as_ref())
                        && let Err(error) = store.cache_routing_models(record, &models).await
                    {
                        tracing::warn!(backend = backend.id(), %error, "cache routing models failed");
                    }
                    if let Ok(mut data) = self.data.write() && data.version == version {
                        data.models.insert(backend.id().into(), (now, models));
                    }
                }
            }
        }).collect();
        use futures_util::StreamExt;
        futures_util::stream::iter(jobs)
            .buffer_unordered(4)
            .collect::<Vec<_>>()
            .await;
    }
}

/// 应用启动后独立刷新目录，关闭时取消未完成的元数据请求。
pub fn spawn_refresh(state: crate::state::AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = state.quota_refresh.shutdown.cancelled() => break,
                _ = state.router.metadata.refresh_models(&state.registry) => {},
            }
            tokio::select! {
                _ = state.quota_refresh.shutdown.cancelled() => break,
                _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {},
            }
        }
    })
}

/// 取两个已知约束的最小值，单方未知时保留已知约束。
fn minimum(left: Option<u32>, right: Option<u32>) -> Option<u32> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        _ => left.or(right),
    }
}

/// 检查实际默认档位、适配器输入限制、完整文本及输出预留。
pub fn admission(
    backend: &dyn Backend,
    model: &ModelInfo,
    request: &ChatRequest,
    settings: &RouteSettings,
) -> Result<EffectiveLimits, String> {
    backend.validate_request(request)?;
    if request.has_tools() && !model.capabilities.tools {
        return Err("tools_unsupported".into());
    }
    if request.has_image() && !model.capabilities.images {
        return Err("images_unsupported".into());
    }
    let limits = backend.model_limits(model);
    let default_tier = limits
        .context_tiers
        .iter()
        .find(|tier| tier.is_default)
        .map(|tier| tier.tokens);
    let mut context = default_tier.or(limits.context_window);
    context = minimum(context, settings.context_limit);
    let input = minimum(limits.max_input_tokens, settings.input_limit);
    if input.is_none() && context.is_none() {
        return Err("context_limit_unknown".into());
    }
    let requested_output = request.max_tokens.unwrap_or(settings.default_output_tokens);
    if requested_output == 0 {
        return Err("output_limit_invalid".into());
    }
    let output_enforced = backend.output_enforced();
    if output_enforced
        && limits
            .max_output_tokens
            .is_some_and(|limit| requested_output > limit)
    {
        return Err(format!(
            "output_limit_exceeded: requested={requested_output} limit={}",
            limits.max_output_tokens.unwrap()
        ));
    }
    let output = if output_enforced {
        requested_output
    } else {
        match (limits.max_output_tokens, settings.provider_output_reserve) {
            (Some(known), Some(configured)) => Some(known.max(configured)),
            (known, configured) => known.or(configured),
        }
        .filter(|reserve| *reserve > 0)
        .ok_or("provider_output_limit_unknown")?
    };
    let images = request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter(|part| matches!(part, crate::core::ContentPart::ImageUrl { .. }))
        .count() as u32;
    let image_tokens = if images > 0 {
        settings
            .image_tokens
            .filter(|tokens| *tokens > 0)
            .ok_or("image_estimator_unknown")?
    } else {
        0
    };
    let estimated = estimate_tokens(request)
        .saturating_add(settings.provider_input_reserve)
        .saturating_add(images.saturating_mul(image_tokens));
    let margin = settings.input_margin_tokens.max(
        ((u64::from(estimated) * u64::from(settings.input_margin_percent)).div_ceil(100))
            .min(u64::from(u32::MAX)) as u32,
    );
    let needed = u64::from(estimated) + u64::from(margin);
    if input.is_some_and(|limit| needed > u64::from(limit)) {
        return Err(format!(
            "input_limit_exceeded: estimated={estimated} margin={margin} limit={}",
            input.unwrap()
        ));
    }
    if context.is_some_and(|limit| needed + u64::from(output) > u64::from(limit)) {
        return Err(format!(
            "context_limit_exceeded: input={needed} output={output} window={}",
            context.unwrap()
        ));
    }
    Ok(EffectiveLimits {
        input_limit: input,
        context_limit: context,
        output_limit: limits.max_output_tokens,
        output_reserve: output,
        output_enforced,
        estimated_input: estimated,
        input_margin: margin,
        estimator: if images > 0 {
            "conservative_utf8_bytes+per_image_reserve"
        } else {
            "conservative_utf8_bytes"
        },
        limit_source: "model_catalog+adapter+configured_budget",
    })
}

/// 创建模型和账号候选；绑定只能命中本轮仍满足全部准入条件的目标。
pub fn plan(
    router: &Router,
    registry: &Registry,
    request: &ChatRequest,
    policy: RoutePolicy,
    free_only: bool,
) -> Decision {
    let mut decision = Decision::default();
    let free_only = free_only || !router.gateway.settings().auto_allow_paid_fallback;
    let Ok(data) = router.metadata.data.read() else {
        return decision;
    };
    decision.config_version = data.version;
    let now = crate::records::now_seconds();
    for backend in data.backends.clone().unwrap_or_else(|| registry.list()) {
        let record = data.records.get(backend.id());
        let options: serde_json::Value = record
            .and_then(|record| serde_json::from_str(&record.options).ok())
            .unwrap_or_default();
        let mut settings: RouteSettings = match options
            .get("routing")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
        {
            Ok(settings) => settings.unwrap_or_default(),
            Err(_) => {
                reject(
                    &mut decision,
                    format!("{}/*", backend.id()),
                    "routing_config_invalid".into(),
                );
                continue;
            }
        };
        let overrides = match record
            .map(|record| record.capabilities.trim())
            .filter(|value| !value.is_empty())
            .map(serde_json::from_str::<crate::backend::openai_compat::CapabilityOverride>)
            .transpose()
        {
            Ok(overrides) => overrides,
            Err(_) => {
                reject(
                    &mut decision,
                    format!("{}/*", backend.id()),
                    "capability_config_invalid".into(),
                );
                continue;
            }
        };
        if let Some(overrides) = &overrides {
            let mut configured = crate::core::Capabilities::default();
            overrides.apply(&mut configured);
            settings.context_limit = minimum(settings.context_limit, configured.context_window);
        }
        let priority = record.map_or(backend.priority(), |record| record.priority);
        let tier = record.map_or(backend.cost_tier(), |record| {
            CostTier::parse(&record.cost_tier)
        });
        let budget = record.map_or(backend.daily_token_budget(), |record| {
            record.daily_token_budget
        });
        let models = data
            .models
            .get(backend.id())
            .map(|(_, models)| models.clone())
            .unwrap_or_else(|| {
                backend
                    .static_models()
                    .into_iter()
                    .map(|id| ModelInfo {
                        id,
                        display_name: None,
                        price_factor: None,
                        original_price_factor: None,
                        price_valid_until: None,
                        capabilities: backend.capabilities(),
                        token_limits: Default::default(),
                    })
                    .collect()
            });
        if models.is_empty() {
            reject(
                &mut decision,
                format!("{}/*", backend.id()),
                "model_catalog_unknown".into(),
            );
        }
        for mut model in models {
            if let Some(overrides) = &overrides {
                overrides.apply(&mut model.capabilities);
            }
            let address = format!("{}/{}", backend.id(), model.id);
            let declared = backend.static_models();
            if !declared.is_empty() && !declared.contains(&model.id) {
                continue;
            }
            if !settings.candidate_models.is_empty()
                && !settings.candidate_models.contains(&model.id)
                && !settings.candidate_models.contains(&address)
                && !settings
                    .candidate_models
                    .contains(&registry.public_model_id(&address))
            {
                continue;
            }
            let reason = if !router.health.is_available(backend.id()) {
                Some("backend cooling".into())
            } else if !super::health::cost_allowed(tier, free_only) {
                Some("cost_not_allowed".into())
            } else if router
                .usage
                .as_ref()
                .is_some_and(|usage| usage.over_budget(backend.id(), Some(budget)))
            {
                Some("daily_token_budget_exhausted".into())
            } else {
                None
            };
            if let Some(reason) = reason {
                reject(&mut decision, address, reason);
                continue;
            }
            let limits = match admission(backend.as_ref(), &model, request, &settings) {
                Ok(limits) => limits,
                Err(reason) => {
                    reject(&mut decision, address, reason);
                    continue;
                }
            };
            let accounts = match backend.routing_accounts() {
                Some(accounts) => accounts
                    .into_iter()
                    .filter(|account| account.enabled && !account.cooling)
                    .map(|account| Some(account.id))
                    .collect::<Vec<_>>(),
                None => vec![None],
            };
            if accounts.is_empty() {
                reject(&mut decision, address.clone(), "account_unavailable".into());
            }
            for (account_order, account) in accounts.into_iter().enumerate() {
                let quota = account
                    .as_ref()
                    .and_then(|id| data.quotas.get(&format!("{}/{}", backend.id(), id)))
                    .filter(|(checked, quota)| {
                        now.saturating_sub(*checked)
                            < if router
                                .metadata
                                .store
                                .read()
                                .is_ok_and(|store| store.is_some())
                            {
                                crate::accounts::quota_refresh::INTERVAL_SECONDS
                            } else {
                                60
                            }
                            && quota.valid_until.is_none_or(|expiry| expiry > now)
                    })
                    .map(|(_, quota)| quota.clone())
                    .unwrap_or_default();
                if quota.amount().is_some_and(|amount| amount <= 0) && !quota.unlimited {
                    reject(
                        &mut decision,
                        format!("{address}@{}", account.as_deref().unwrap_or("gateway")),
                        "quota_exhausted".into(),
                    );
                    continue;
                }
                let factor = model
                    .price_factor
                    .filter(|factor| factor.is_finite() && *factor >= 0.0)
                    .filter(|_| model.price_valid_until.is_none_or(|expiry| expiry > now));
                let candidate = Candidate {
                    backend: backend.clone(),
                    model: model.id.clone(),
                    account_id: account,
                    account_order,
                    cost_tier: tier,
                    priority: settings
                        .model_priority
                        .get(&model.id)
                        .copied()
                        .unwrap_or(priority),
                    latency_ms: router.health.latency_ms(backend.id()),
                    daily_token_budget: budget,
                    price_factor: factor,
                    comparison_scope: format!(
                        "{}:{}",
                        settings.comparison_scope.as_deref().unwrap_or(backend.id()),
                        match quota.unit.as_str() {
                            "" if backend.kind() == "workbuddy" => "credits",
                            "" if backend.kind() == "minimax_code" => "0.01 积分",
                            unit => unit,
                        }
                    ),
                    quota,
                    limits: limits.clone(),
                };
                if router.gateway.balancer.available(&candidate) {
                    decision.candidates.push(candidate);
                } else {
                    reject(&mut decision, candidate.binding(), "target cooling".into());
                    decision.blocked.push(candidate);
                }
            }
        }
    }
    sort(&mut decision.candidates, policy);
    let key = affinity_key(request, policy, free_only, data.version);
    if let Some(key) = key
        && let Some(bound) = router.affinity.get(&key)
        && let Some(index) = decision
            .candidates
            .iter()
            .position(|candidate| candidate.binding() == bound)
    {
        decision.affinity_hit = true;
        let candidate = decision.candidates.remove(index);
        decision.candidates.insert(0, candidate);
    }
    decision.comparison_scope = decision
        .candidates
        .first()
        .map(|candidate| candidate.comparison_scope.clone());
    decision
}

/// 排除记录保留机器可辨认原因及数值，不暴露凭证。
fn reject(decision: &mut Decision, model: String, reason: String) {
    decision.rejected.push(Rejection { model, reason });
}

/// 绑定键包含策略与费用许可；无关配置热加载不改变会话身份。
pub fn affinity_key(
    request: &ChatRequest,
    policy: RoutePolicy,
    free_only: bool,
    _version: u64,
) -> Option<String> {
    request
        .session
        .as_ref()
        .map(|session| format!("{session}:{}:{free_only}", policy.as_str()))
}

/// 预演展示稳定分层顺序；实际分配在容量准入后继续执行同级分散。
pub fn sort(candidates: &mut [Candidate], _policy: RoutePolicy) {
    candidates.sort_by(|left, right| {
        left.auto_rank()
            .cmp(&right.auto_rank())
            .then(left.comparison_scope.cmp(&right.comparison_scope))
            .then_with(|| {
                price_key(left)
                    .partial_cmp(&price_key(right))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then(right.quota.unlimited.cmp(&left.quota.unlimited))
            .then(right.quota.amount().cmp(&left.quota.amount()))
            .then(
                super::health::latency_key(left.latency_ms)
                    .cmp(&super::health::latency_key(right.latency_ms)),
            )
            .then(left.backend.id().cmp(right.backend.id()))
            .then(left.model.cmp(&right.model))
            .then(left.account_order.cmp(&right.account_order))
    });
}

/// 只在同计费范围内比较有效倍率；未知值作为同域后备，不能当作免费。
pub fn price_key(candidate: &Candidate) -> (bool, f64) {
    (
        candidate.price_factor.is_none(),
        candidate.price_factor.unwrap_or(0.0),
    )
}

/// 容量可用的新组候选先取最优层级，再在各个可比较计费域内选择经济档位。
pub fn auto_cohort(candidates: &[Candidate], ready: &[usize]) -> Vec<usize> {
    let Some(rank) = ready.iter().map(|i| candidates[*i].auto_rank()).min() else {
        return Vec::new();
    };
    let mut prices: HashMap<&str, (bool, f64)> = HashMap::new();
    for i in ready
        .iter()
        .copied()
        .filter(|i| candidates[*i].auto_rank() == rank)
    {
        let candidate = &candidates[i];
        let price = price_key(candidate);
        prices
            .entry(&candidate.comparison_scope)
            .and_modify(|best| {
                if price < *best {
                    *best = price;
                }
            })
            .or_insert(price);
    }
    ready
        .iter()
        .copied()
        .filter(|i| {
            let candidate = &candidates[*i];
            candidate.auto_rank() == rank
                && prices.get(candidate.comparison_scope.as_str()) == Some(&price_key(candidate))
        })
        .collect()
}
