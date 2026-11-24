//! 服务端每十二小时更新账号额度，手动更新共享同一批处理和持久缓存。

use super::{
    Account,
    remote::{QuotaInfo, QuotaReader},
};
use crate::core::AppError;
use crate::state::AppState;
use crate::store::{BackendRecord, Store};
use futures_util::{StreamExt, stream};
use serde::Serialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

pub const INTERVAL_SECONDS: u64 = 12 * 60 * 60;

/// 调度器与手动刷新共享串行锁，避免重叠查询。
#[derive(Default)]
pub struct Control {
    active: AtomicBool,
    refreshing: AtomicBool,
    completed_batches: AtomicU64,
    lock: Mutex<()>,
    pub changed: Notify,
    pub shutdown: CancellationToken,
}

/// 页面读取本地快照，不需要访问厂商。
#[derive(Serialize)]
pub struct View {
    pub quota: Vec<QuotaInfo>,
    pub auto_refresh: Status,
}

/// 自动更新状态，额度查询失败与服务是否运行分别展示。
#[derive(Serialize)]
pub struct Status {
    pub enabled: bool,
    pub running: bool,
    pub in_progress: bool,
    pub interval_seconds: u64,
    #[serde(flatten)]
    pub record: crate::store::QuotaRefreshRecord,
}

/// 作用域结束或任务中断时清除忙碌标记。
struct RefreshGuard(Arc<Control>);
impl Drop for RefreshGuard {
    /// 不论查询如何结束，下一批都能继续执行。
    fn drop(&mut self) {
        self.0.refreshing.store(false, Ordering::Relaxed);
    }
}

/// 获取缓存及持久计划，重开控制台也能显示已有额度。
pub async fn view(state: &AppState) -> Result<View, AppError> {
    Ok(View {
        quota: state.store.cached_quota().await?,
        auto_refresh: Status {
            enabled: true,
            running: state.quota_refresh.active.load(Ordering::Relaxed),
            in_progress: state.quota_refresh.refreshing.load(Ordering::Relaxed),
            interval_seconds: INTERVAL_SECONDS,
            record: state.store.quota_refresh_record().await?,
        },
    })
}

/// 处理启用的账号，限制并发及每个查询时长；任一账号失败不会中止整批。
async fn collect(state: &AppState) -> Result<Vec<(BackendRecord, QuotaInfo)>, AppError> {
    let accounts = state.store.list_accounts().await?;
    let backends = state.store.list_backends().await?;
    let mut readers = std::collections::HashMap::new();
    for backend in backends.iter().filter(|backend| backend.enabled) {
        let runtime = match state.registry.runtime(&backend.id) {
            Some(runtime) => Ok(runtime),
            // 直接调用额度服务的旧入口可以没有运行注册表；每服务仍只装配一次。
            None => crate::plugins::builtin::registry().prepare(
                backend,
                crate::plugins::ScopedHostServices::new(
                    backend,
                    &accounts,
                    state.store.clone(),
                    Duration::from_secs(10),
                    state.quota_refresh.clone(),
                ),
            ),
        };
        match runtime {
            Ok(runtime) => {
                if let Some(reader) = runtime.quota {
                    readers.insert(backend.id.clone(), Ok(reader));
                }
            }
            Err(error) => {
                readers.insert(backend.id.clone(), Err(error));
            }
        }
    }
    let jobs = accounts.into_iter().filter_map(|account| {
        let backend = backends
            .iter()
            .find(|backend| backend.id == account.backend_id && backend.enabled && account.enabled)?
            .clone();
        let reader = readers.get(&backend.id)?.clone();
        Some(async move {
            let read = async { reader?.read(&account).await };
            let info = match tokio::time::timeout(Duration::from_secs(30), read).await {
                Ok(Ok(info)) => info,
                Ok(Err(error)) => QuotaInfo::unknown(&backend.id, &account.id, error.message()),
                Err(_) => QuotaInfo::unknown(&backend.id, &account.id, "额度查询超时"),
            };
            (backend, info)
        })
    });
    Ok(stream::iter(jobs).buffer_unordered(4).collect().await)
}

/// 到期或手动请求时更新；等待同一批的并发手动请求复用刚完成的结果。
pub async fn refresh(state: &AppState, force: bool, now: u64) -> Result<View, AppError> {
    let observed = state
        .quota_refresh
        .completed_batches
        .load(Ordering::Relaxed);
    let _lock = state.quota_refresh.lock.lock().await;
    let current = state.store.quota_refresh_record().await?;
    if (!force && current.next_refresh_at.is_some_and(|next| next > now))
        || (force
            && observed
                != state
                    .quota_refresh
                    .completed_batches
                    .load(Ordering::Relaxed))
    {
        return view(state).await;
    }
    state
        .quota_refresh
        .refreshing
        .store(true, Ordering::Relaxed);
    let _busy = RefreshGuard(state.quota_refresh.clone());
    state.store.start_quota_refresh(now).await?;
    let started = Instant::now();
    let entries = collect(state).await?;
    let completed = now.saturating_add(started.elapsed().as_secs());
    state
        .store
        .finish_quota_refresh(
            &entries,
            completed,
            completed.saturating_add(INTERVAL_SECONDS),
        )
        .await?;
    state
        .quota_refresh
        .completed_batches
        .fetch_add(1, Ordering::Relaxed);
    state.quota_refresh.changed.notify_one();
    // 页面结果在忙碌标记清除后读取，避免已完成的手动刷新仍显示进行中。
    drop(_busy);
    view(state).await
}

/// 签到后只查询当前账号；与全量采集串行，凭证变更后的迟到结果不能发布。
pub async fn refresh_account(
    store: &Store,
    control: &Control,
    backend: &BackendRecord,
    account: &Account,
) -> Result<QuotaInfo, AppError> {
    let host = crate::plugins::ScopedHostServices::legacy(
        backend,
        std::slice::from_ref(account),
        store.clone(),
        Duration::from_secs(10),
    );
    let reader = crate::plugins::builtin::registry()
        .prepare(backend, host)?
        .quota
        .ok_or_else(|| AppError::bad_request("service does not support quota"))?;
    refresh_account_with_reader(store, control, backend, account, reader.as_ref()).await
}

/// 任务和操作复用已装配的额度能力，保持并发更新和迟到结果的写回检查。
pub async fn refresh_account_with_reader(
    store: &Store,
    control: &Control,
    backend: &BackendRecord,
    account: &Account,
    reader: &dyn QuotaReader,
) -> Result<QuotaInfo, AppError> {
    let _lock = control.lock.lock().await;
    let current = store.get_account(&account.id).await?;
    let configured = store.get_backend(&backend.id).await?;
    if current.is_none_or(|current| {
        !current.enabled
            || current.backend_id != backend.id
            || current.credential != account.credential
    }) || configured.is_none_or(|current| {
        !current.enabled
            || current.kind != backend.kind
            || current.base_url != backend.base_url
            || crate::plugins::service::catalog_identity(&current)
                != crate::plugins::service::catalog_identity(backend)
    }) {
        return Err(AppError::bad_request("额度查询前账号或服务配置已变更"));
    }
    let read = reader.read(account);
    let info = match tokio::time::timeout(Duration::from_secs(30), read).await {
        Ok(Ok(info)) => info,
        Ok(Err(error)) => QuotaInfo::unknown(&backend.id, &account.id, error.message()),
        Err(_) => QuotaInfo::unknown(&backend.id, &account.id, "额度查询超时"),
    };
    let latest = store
        .get_account(&account.id)
        .await?
        .filter(|latest| {
            latest.enabled
                && latest.backend_id == backend.id
                && reader.same_credential_identity(&account.credential, &latest.credential)
        })
        .ok_or_else(|| AppError::bad_request("额度查询期间账号或服务配置已变更"))?;
    if !store.cache_account_quota(backend, &latest, &info).await? {
        return Err(AppError::bad_request("额度查询期间账号或服务配置已变更"));
    }
    Ok(info)
}

/// 启动单个可取消调度器，初次/到期执行；数据库失败一分钟后重试。
pub fn spawn(state: AppState) -> Option<tokio::task::JoinHandle<()>> {
    if state.quota_refresh.active.swap(true, Ordering::Relaxed) {
        return None;
    }
    Some(tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = state.quota_refresh.shutdown.cancelled() => break,
                result = refresh(&state, false, crate::records::now_seconds()) => {
                    if let Err(error) = result {
                        tracing::warn!(%error, "account quota refresh failed");
                    }
                }
            }
            tokio::select! {
                _ = state.quota_refresh.shutdown.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(60)) => {},
                _ = state.quota_refresh.changed.notified() => {},
            }
        }
        state.quota_refresh.active.store(false, Ordering::Relaxed);
    }))
}
