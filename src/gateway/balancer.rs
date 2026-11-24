//! 原子会话预占和两级负载选择；网络请求从不持有此锁。

use super::{GatewaySettings, retry::FailureScope};
use crate::routing::{Candidate, RoutePolicy};
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use tokio::time::Instant as RateInstant;

#[derive(Clone)]
struct Binding {
    target: String,
    upstream_session: String,
    expires: Instant,
    version: u64,
}
#[derive(Default)]
struct Load {
    in_flight: usize,
    cursor: u64,
    submissions: VecDeque<(u64, RateInstant)>,
}

impl Load {
    /// 只保留当前滚动窗口内的生成提交和预占。
    fn prune(&mut self, now: RateInstant) {
        while self
            .submissions
            .front()
            .is_some_and(|(_, time)| *time + Duration::from_secs(1) <= now)
        {
            self.submissions.pop_front();
        }
    }

    /// 降低限额时等待足够多的旧提交过期，不能仅等待最早一条。
    fn rate_deadline(&self, qps: u32) -> Option<RateInstant> {
        if qps == 0 || self.submissions.len() < qps as usize {
            None
        } else {
            Some(self.submissions[self.submissions.len() - qps as usize].1 + Duration::from_secs(1))
        }
    }

    /// 不限频时也保留有限历史，启用或降低限额不会绕过已有提交。
    fn record(&mut self, sequence: u64, now: RateInstant) {
        self.submissions.push_back((sequence, now));
        while self.submissions.len() > super::MAX_SERVICE_QPS as usize {
            self.submissions.pop_front();
        }
    }
}
struct Cooldown {
    until: Instant,
    reason: String,
    generation: u64,
    probing: bool,
    status: Option<u16>,
}

/// 配置热加载后依然存活；所有计数、绑定预占与迁移共用短锁。
#[derive(Default)]
pub struct Balancer {
    data: Mutex<Data>,
    pub changed: tokio::sync::Notify,
}
#[derive(Default)]
struct Data {
    groups: HashMap<String, Binding>,
    active_groups: HashMap<String, usize>,
    loads: HashMap<String, Load>,
    cooldowns: HashMap<String, Cooldown>,
    sequence: u64,
}

/// 调度失败时区分容量等待与无可用目标。
pub enum ReserveError {
    Busy,
    RateLimited(RateInstant),
    Unavailable,
    GroupLimit,
}

/// 一个完整上游流占用容量，取消或 drop 也会释放。
pub struct Lease {
    owner: Arc<Balancer>,
    backend: String,
    account: String,
    pub affinity_hit: bool,
    pub migrated: bool,
    pub version: u64,
    /// 厂商会话由绑定单独拥有，过期或迁移后重新生成。
    pub upstream_session: String,
    /// 容量与限频在原子预占完成时的快照，用于关联上游限流。
    pub admission: AdmissionSnapshot,
    probes: Vec<(String, u64)>,
    group: String,
    previous: Option<Binding>,
    rate: RatePermit,
}

/// 单次生成准入的负载及限制，不包含凭证或消息。
#[derive(Serialize)]
pub struct AdmissionSnapshot {
    pub service_in_flight: usize,
    pub service_capacity: usize,
    pub account_in_flight: usize,
    pub account_capacity: usize,
    pub service_qps: u32,
    pub service_submissions_in_window: usize,
}

/// 未提交的限频预占自动退款，已提交生成保留到滚动窗口过期。
pub struct RatePermit {
    owner: Arc<Balancer>,
    backend: String,
    sequence: u64,
    committed: AtomicBool,
}

impl RatePermit {
    /// 只有尝试预算准入后才确认一次生成提交。
    pub fn commit(&self) {
        self.committed.store(true, Ordering::SeqCst);
    }
}

impl Drop for RatePermit {
    /// 取消或预算拒绝不能消耗服务限频额度。
    fn drop(&mut self) {
        if !self.committed.load(Ordering::SeqCst) {
            if let Ok(mut data) = self.owner.data.lock()
                && let Some(load) = data.loads.get_mut(&self.backend)
            {
                load.submissions
                    .retain(|(sequence, _)| *sequence != self.sequence);
            }
            self.owner.changed.notify_waiters();
        }
    }
}

/// 管理接口中的无凭证运行快照。
#[derive(Serialize)]
pub struct RuntimeSnapshot {
    pub groups: usize,
    pub loads: Vec<LoadSnapshot>,
    pub cooldowns: Vec<CooldownSnapshot>,
}
#[derive(Serialize)]
pub struct LoadSnapshot {
    pub target: String,
    pub in_flight: usize,
    pub active_groups: usize,
}
#[derive(Serialize)]
pub struct CooldownSnapshot {
    pub scope: String,
    pub reason: String,
    pub remaining_ms: u64,
}

/// 账号负载按 backend/account 聚合，不因模型目录条数增加权重。
pub fn account_key(candidate: &Candidate) -> String {
    format!(
        "{}/{}",
        candidate.backend.id(),
        candidate.account_id.as_deref().unwrap_or("@gateway")
    )
}

/// 同一故障范围的所有候选共享避让键。
pub fn scope_key(candidate: &Candidate, scope: FailureScope) -> String {
    match scope {
        FailureScope::Caller => String::new(),
        FailureScope::Account => format!("account:{}", account_key(candidate)),
        FailureScope::AccountModel => format!("account_model:{}", candidate.binding()),
        FailureScope::BackendModel => format!("model:{}", candidate.address()),
        FailureScope::Backend => format!("backend:{}", candidate.backend.id()),
    }
}

impl Balancer {
    /// 候选准入查看局部冷却；过期冷却不阻止恢复。
    pub fn available(&self, candidate: &Candidate) -> bool {
        self.data.lock().is_ok_and(|data| {
            let now = Instant::now();
            [
                FailureScope::Account,
                FailureScope::AccountModel,
                FailureScope::BackendModel,
                FailureScope::Backend,
            ]
            .iter()
            .all(|scope| {
                data.cooldowns
                    .get(&scope_key(candidate, *scope))
                    .is_none_or(|cooldown| cooldown.until <= now && !cooldown.probing)
            })
        })
    }

    /// 后端级冷却在构造模型候选前即可过滤。
    pub fn backend_available(&self, id: &str) -> bool {
        self.data.lock().is_ok_and(|data| {
            data.cooldowns
                .get(&format!("backend:{id}"))
                .is_none_or(|cooldown| cooldown.until <= Instant::now() && !cooldown.probing)
        })
    }

    /// 原子选择/复用绑定并预占容量；返回候选索引和流租约。
    pub fn reserve(
        self: &Arc<Self>,
        candidates: &[Candidate],
        group: &str,
        policy: RoutePolicy,
        settings: &GatewaySettings,
        excluded: &HashSet<String>,
    ) -> Result<(usize, Lease), ReserveError> {
        let mut data = self.data.lock().map_err(|_| ReserveError::Unavailable)?;
        let now = Instant::now();
        let rate_now = RateInstant::now();
        let Data {
            groups,
            active_groups,
            ..
        } = &mut *data;
        groups.retain(|key, value| {
            value.expires > now || active_groups.get(key).is_some_and(|count| *count > 0)
        });
        data.cooldowns.retain(|_, value| {
            value.probing
                || value.until > now
                || now.duration_since(value.until) < Duration::from_secs(86_400)
        });
        let mut group_loads: HashMap<String, usize> = HashMap::new();
        for binding in data.groups.values() {
            if let Some((address, account)) = binding.target.rsplit_once('@') {
                let backend = address
                    .split_once('/')
                    .map_or(address, |(backend, _)| backend);
                *group_loads.entry(backend.into()).or_default() += 1;
                *group_loads
                    .entry(account_key_parts(address, account))
                    .or_default() += 1;
            }
        }
        // 清理失效配置的闲置计数，仍在飞的旧实例继续保留其租约。
        data.loads.retain(|key, load| {
            load.prune(rate_now);
            load.in_flight > 0 || !load.submissions.is_empty() || group_loads.contains_key(key)
        });
        let valid = |candidate: &Candidate| {
            [
                FailureScope::Account,
                FailureScope::AccountModel,
                FailureScope::BackendModel,
                FailureScope::Backend,
            ]
            .iter()
            .all(|scope| {
                let key = scope_key(candidate, *scope);
                !excluded.contains(&key)
                    && data
                        .cooldowns
                        .get(&key)
                        .is_none_or(|cooldown| cooldown.until <= now && !cooldown.probing)
            })
        };
        let eligible: Vec<usize> = candidates
            .iter()
            .enumerate()
            .filter(|(_, candidate)| valid(candidate))
            .map(|(i, _)| i)
            .collect();
        if eligible.is_empty() {
            return Err(ReserveError::Unavailable);
        }
        let available = |candidate: &Candidate| {
            let backend = candidate.backend.id();
            let account = account_key(candidate);
            data.loads.get(backend).map_or(0, |load| load.in_flight)
                < settings.target(backend, true).capacity
                && data.loads.get(&account).map_or(0, |load| load.in_flight)
                    < settings.target(&account, false).capacity
        };
        let bound = data.groups.get(group).cloned();
        let bound_index = bound.as_ref().and_then(|binding| {
            eligible
                .iter()
                .copied()
                .find(|i| candidates[*i].binding() == binding.target)
        });
        if bound_index.is_some_and(|i| !available(&candidates[i])) {
            return Err(ReserveError::Busy);
        }
        let rate_deadline = |candidate: &Candidate| {
            data.loads
                .get(candidate.backend.id())
                .and_then(|load| load.rate_deadline(candidate.backend.qps()))
        };
        if let Some(until) = bound_index.and_then(|i| rate_deadline(&candidates[i])) {
            return Err(ReserveError::RateLimited(until));
        }
        let capacity_ready: Vec<usize> = eligible
            .into_iter()
            .filter(|i| available(&candidates[*i]))
            .collect();
        let ready: Vec<usize> = capacity_ready
            .iter()
            .copied()
            .filter(|i| rate_deadline(&candidates[*i]).is_none())
            .collect();
        if ready.is_empty() {
            return Err(capacity_ready
                .iter()
                .filter_map(|i| rate_deadline(&candidates[*i]))
                .min()
                .map_or(ReserveError::Busy, ReserveError::RateLimited));
        }
        let score = |key: &str, backend: bool| {
            let limit = settings.target(key, backend);
            let load = data.loads.get(key);
            let groups = group_loads.get(key).copied().unwrap_or(0);
            (
                load.map_or(0, |load| load.in_flight) as f64 / limit.capacity as f64,
                groups as f64 / limit.weight as f64,
                load.map_or(0, |load| load.cursor),
            )
        };
        let index =
            bound_index.unwrap_or_else(|| {
                let ready = if policy == RoutePolicy::Auto {
                    crate::routing::planner::auto_cohort(candidates, &ready)
                } else {
                    ready
                };
                // 仅当此后端的入选账号处于同一计费域时比较余额，避免不传递的两两比较。
                let mut scopes: HashMap<&str, Option<&str>> = HashMap::new();
                for i in &ready {
                    let candidate = &candidates[*i];
                    scopes
                        .entry(candidate.backend.id())
                        .and_modify(|scope| {
                            if *scope != Some(candidate.comparison_scope.as_str()) {
                                *scope = None;
                            }
                        })
                        .or_insert(Some(&candidate.comparison_scope));
                }
                *ready
                    .iter()
                    .min_by(|a, b| {
                        let left = &candidates[**a];
                        let right = &candidates[**b];
                        let l = score(left.backend.id(), true);
                        let r = score(right.backend.id(), true);
                        l.0.total_cmp(&r.0)
                            .then(l.1.total_cmp(&r.1))
                            .then(l.2.cmp(&r.2))
                            .then_with(|| {
                                if policy == RoutePolicy::Auto {
                                    crate::routing::health::latency_key(left.latency_ms)
                                        .cmp(&crate::routing::health::latency_key(right.latency_ms))
                                } else {
                                    std::cmp::Ordering::Equal
                                }
                            })
                            .then_with(|| left.backend.id().cmp(right.backend.id()))
                            .then_with(|| {
                                let l = score(&account_key(left), false);
                                let r = score(&account_key(right), false);
                                l.0.total_cmp(&r.0)
                                    .then(l.1.total_cmp(&r.1))
                                    .then_with(|| {
                                        if policy == RoutePolicy::Auto
                                            && scopes
                                                .get(left.backend.id())
                                                .is_some_and(Option::is_some)
                                        {
                                            right.quota.unlimited.cmp(&left.quota.unlimited).then(
                                                right.quota.amount().cmp(&left.quota.amount()),
                                            )
                                        } else {
                                            std::cmp::Ordering::Equal
                                        }
                                    })
                                    .then(l.2.cmp(&r.2))
                            })
                            .then_with(|| a.cmp(b))
                    })
                    .expect("nonempty ready")
            });
        if !data.groups.contains_key(group) && data.groups.len() >= settings.group_capacity {
            return Err(ReserveError::GroupLimit);
        }
        let candidate = &candidates[index];
        let target = candidate.binding();
        data.sequence = data.sequence.wrapping_add(1);
        let sequence = data.sequence;
        let version = bound
            .as_ref()
            .filter(|value| value.target == target)
            .map_or(sequence, |value| value.version);
        let upstream_session = bound
            .as_ref()
            .filter(|value| value.target == target)
            .map(|value| value.upstream_session.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        data.groups.insert(
            group.into(),
            Binding {
                target,
                upstream_session: upstream_session.clone(),
                expires: now + Duration::from_millis(settings.group_ttl_ms),
                version,
            },
        );
        let backend = candidate.backend.id().to_string();
        let account = account_key(candidate);
        *data.active_groups.entry(group.into()).or_default() += 1;
        let mut probes = Vec::new();
        for scope in [
            FailureScope::Account,
            FailureScope::AccountModel,
            FailureScope::BackendModel,
            FailureScope::Backend,
        ] {
            let key = scope_key(candidate, scope);
            if let Some(cooldown) = data.cooldowns.get_mut(&key) {
                cooldown.probing = true;
                probes.push((key, cooldown.generation));
            }
        }
        for key in [&backend, &account] {
            let load = data.loads.entry(key.clone()).or_default();
            load.in_flight += 1;
            load.cursor = sequence;
        }
        data.loads
            .get_mut(&backend)
            .expect("backend load")
            .record(sequence, rate_now);
        let admission = AdmissionSnapshot {
            service_in_flight: data.loads[&backend].in_flight,
            service_capacity: settings.target(&backend, true).capacity,
            account_in_flight: data.loads[&account].in_flight,
            account_capacity: settings.target(&account, false).capacity,
            service_qps: candidate.backend.qps(),
            service_submissions_in_window: data.loads[&backend].submissions.len(),
        };
        let rate = RatePermit {
            owner: self.clone(),
            backend: backend.clone(),
            sequence,
            committed: AtomicBool::new(false),
        };
        Ok((
            index,
            Lease {
                owner: self.clone(),
                backend,
                account,
                affinity_hit: bound_index.is_some(),
                migrated: bound
                    .as_ref()
                    .is_some_and(|binding| binding.target != candidate.binding()),
                version,
                probes,
                upstream_session,
                group: group.into(),
                previous: bound,
                rate,
                admission,
            },
        ))
    }

    /// 适配器内部重新生成复用同一个服务窗口，不再占用流容量。
    pub fn reserve_submission(
        self: &Arc<Self>,
        backend: &str,
        qps: u32,
    ) -> Result<RatePermit, ReserveError> {
        let mut data = self.data.lock().map_err(|_| ReserveError::Unavailable)?;
        let now = RateInstant::now();
        let load = data.loads.entry(backend.into()).or_default();
        load.prune(now);
        if let Some(until) = load.rate_deadline(qps) {
            return Err(ReserveError::RateLimited(until));
        }
        data.sequence = data.sequence.wrapping_add(1);
        let sequence = data.sequence;
        data.loads
            .get_mut(backend)
            .expect("backend load")
            .record(sequence, now);
        Ok(RatePermit {
            owner: self.clone(),
            backend: backend.into(),
            sequence,
            committed: AtomicBool::new(false),
        })
    }

    /// 局部冷却保持至恢复时间，配置热加载不会清除。
    pub fn failure(
        &self,
        candidate: &Candidate,
        scope: FailureScope,
        delay: Duration,
        reason: &str,
    ) {
        self.failure_with_status(candidate, scope, delay, reason, None);
    }

    /// 保留限流状态以便后续请求在本地冷却时返回同样的恢复语义。
    pub fn failure_with_status(
        &self,
        candidate: &Candidate,
        scope: FailureScope,
        delay: Duration,
        reason: &str,
        status: Option<u16>,
    ) {
        if scope == FailureScope::Caller {
            return;
        }
        if let Ok(mut data) = self.data.lock() {
            let key = scope_key(candidate, scope);
            let until = Instant::now()
                .checked_add(delay)
                .unwrap_or_else(Instant::now);
            data.sequence = data.sequence.wrapping_add(1);
            let generation = data.sequence;
            let entry = data.cooldowns.entry(key).or_insert(Cooldown {
                until,
                reason: reason.into(),
                generation,
                probing: false,
                status,
            });
            entry.until = entry.until.max(until);
            entry.reason = reason.chars().take(128).collect();
            entry.generation = generation;
            entry.probing = false;
            entry.status = status;
        }
        self.changed.notify_waiters();
    }

    /// 所有准入目标均限流时返回 429 及最早恢复时间，混合故障返回 503。
    pub fn blocked_error(
        &self,
        candidates: &[Candidate],
    ) -> Option<(crate::core::AppError, Duration)> {
        if candidates.is_empty() {
            return None;
        }
        let data = self.data.lock().ok()?;
        let now = Instant::now();
        let mut delays = Vec::new();
        let mut rate_limited = true;
        for candidate in candidates {
            let cooldowns: Vec<_> = [
                FailureScope::Account,
                FailureScope::AccountModel,
                FailureScope::BackendModel,
                FailureScope::Backend,
            ]
            .iter()
            .filter_map(|scope| data.cooldowns.get(&scope_key(candidate, *scope)))
            .filter(|cooldown| cooldown.until > now || cooldown.probing)
            .collect();
            if cooldowns.is_empty() {
                return None;
            }
            rate_limited &= cooldowns
                .iter()
                .all(|cooldown| cooldown.status == Some(429));
            delays.push(
                cooldowns
                    .iter()
                    .map(|cooldown| cooldown.until.saturating_duration_since(now))
                    .max()
                    .unwrap_or_default(),
            );
        }
        let delay = delays
            .into_iter()
            .min()
            .unwrap_or_default()
            .max(Duration::from_secs(1));
        Some((
            super::retry::gateway_error(
                if rate_limited { 429 } else { 503 },
                "gateway_target_cooling",
                "eligible gateway targets are cooling down",
                false,
            ),
            delay,
        ))
    }

    /// 只有当前版本可以撤销启动失败的预占，迟到失败不能覆盖迁移绑定。
    pub fn unbind(&self, group: &str, version: u64) {
        if let Ok(mut data) = self.data.lock()
            && data
                .groups
                .get(group)
                .is_some_and(|binding| binding.version == version)
        {
            data.groups.remove(group);
        }
    }

    /// 运行快照在输出前排序，不暴露会话内容或明确 ID。
    pub fn snapshot(&self) -> RuntimeSnapshot {
        let mut result = RuntimeSnapshot {
            groups: 0,
            loads: vec![],
            cooldowns: vec![],
        };
        if let Ok(data) = self.data.lock() {
            let now = Instant::now();
            let mut groups: HashMap<String, usize> = HashMap::new();
            for (group, binding) in &data.groups {
                if binding.expires <= now
                    && data
                        .active_groups
                        .get(group)
                        .is_none_or(|count| *count == 0)
                {
                    continue;
                }
                result.groups += 1;
                if let Some((address, account)) = binding.target.rsplit_once('@') {
                    let backend = address
                        .split_once('/')
                        .map_or(address, |(backend, _)| backend);
                    *groups.entry(backend.into()).or_default() += 1;
                    *groups
                        .entry(account_key_parts(address, account))
                        .or_default() += 1;
                }
            }
            for (key, load) in &data.loads {
                result.loads.push(LoadSnapshot {
                    target: key.clone(),
                    in_flight: load.in_flight,
                    active_groups: groups.get(key).copied().unwrap_or(0),
                });
            }
            for (key, cooldown) in &data.cooldowns {
                if cooldown.until > now || cooldown.probing {
                    result.cooldowns.push(CooldownSnapshot {
                        scope: key.clone(),
                        reason: cooldown.reason.clone(),
                        remaining_ms: cooldown.until.saturating_duration_since(now).as_millis()
                            as u64,
                    });
                }
            }
        }
        result.loads.sort_by(|a, b| a.target.cmp(&b.target));
        result.cooldowns.sort_by(|a, b| a.scope.cmp(&b.scope));
        result
    }
}

/// 将模型绑定转换为账号负载键。
fn account_key_parts(address: &str, account: &str) -> String {
    format!(
        "{}/{}",
        address
            .split_once('/')
            .map_or(address, |(backend, _)| backend),
        if account.is_empty() {
            "@gateway"
        } else {
            account
        }
    )
}

impl Drop for Lease {
    /// 流耗尽或被取消时释放两级容量，并唤醒等待者。
    fn drop(&mut self) {
        if let Ok(mut data) = self.owner.data.lock() {
            if let Some(count) = data.active_groups.get_mut(&self.group) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    data.active_groups.remove(&self.group);
                }
            }
            for key in [&self.backend, &self.account] {
                if let Some(load) = data.loads.get_mut(key) {
                    load.in_flight = load.in_flight.saturating_sub(1);
                }
            }
            for (key, generation) in &self.probes {
                if let Some(cooldown) = data.cooldowns.get_mut(key)
                    && cooldown.generation == *generation
                {
                    cooldown.probing = false;
                }
            }
        }
        self.owner.changed.notify_waiters();
    }
}

impl Lease {
    /// 确认初始生成的限频预占，完成流后仍保留窗口计数。
    pub fn submitted(&self) {
        self.rate.commit();
    }

    /// 尚未允许生成提交时回滚自己的预占，保留上一个账号的迁移证据。
    pub fn abandoned(&self) {
        if let Ok(mut data) = self.owner.data.lock()
            && data
                .groups
                .get(&self.group)
                .is_some_and(|binding| binding.version == self.version)
        {
            if let Some(previous) = &self.previous {
                data.groups.insert(self.group.clone(), previous.clone());
            } else {
                data.groups.remove(&self.group);
            }
        }
    }

    /// 只有成功的半开探测可清除对应代际的故障，迟到旧流不能清除新故障。
    pub fn completed(&self) {
        if let Ok(mut data) = self.owner.data.lock() {
            for (key, generation) in &self.probes {
                if data
                    .cooldowns
                    .get(key)
                    .is_some_and(|cooldown| cooldown.generation == *generation)
                {
                    data.cooldowns.remove(key);
                }
            }
        }
    }
}
