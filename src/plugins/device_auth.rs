//! 设备登录的公共会话生命周期，协议状态由插件提供，每个应用独立持有。

use crate::{accounts::Account, core::AppError, state::AppState, store::BackendRecord};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// 协议代码只处理单次待授权状态，不访问全局应用状态。
#[async_trait::async_trait]
pub trait DeviceProtocol: Send + Sync + 'static {
    type Pending: Send + Sync;
    const ID: &'static str;
    /// 创建协议参数并返回授权地址、有效期与待授权状态。
    async fn start(backend: &BackendRecord) -> Result<(Value, Self::Pending), AppError>;
    /// 返回成功账号和脱敏身份；尚未确认时返回 None。
    async fn poll(
        backend: &BackendRecord,
        pending: &Self::Pending,
    ) -> Result<Option<(Account, Value)>, AppError>;
}

struct Session<P: DeviceProtocol> {
    backend: BackendRecord,
    pending: P::Pending,
    deadline: Instant,
    cancel: CancellationToken,
    terminal: Mutex<Option<Value>>,
    polling: Mutex<()>,
}

struct Sessions<P: DeviceProtocol> {
    entries: Mutex<HashMap<String, Arc<Session<P>>>>,
}

impl<P: DeviceProtocol> Default for Sessions<P> {
    /// 为每个应用和插件建立独立会话集合。
    fn default() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait::async_trait]
impl<P: DeviceProtocol> super::auth::SessionStore for Sessions<P> {
    /// 配置停用先发取消信号，网络等待不会阻塞撤销。
    async fn cancel_service(&self, id: &str) {
        for session in self
            .entries
            .lock()
            .await
            .values()
            .filter(|s| s.backend.id == id)
        {
            session.cancel.cancel();
        }
    }
}

#[derive(Deserialize)]
pub struct Input {
    pub backend_id: String,
    #[serde(default)]
    pub session_id: String,
}

/// 获取会话同时校验归属；v1 没有 session_id 时只允许唯一未过期会话。
async fn session<P: DeviceProtocol>(
    state: &AppState,
    input: &Input,
) -> Result<Arc<Session<P>>, AppError> {
    let sessions = state.auth.sessions::<Sessions<P>>();
    let entries = sessions.entries.lock().await;
    let found = if input.session_id.is_empty() {
        let mut matches = entries
            .values()
            .filter(|s| s.backend.id == input.backend_id && s.deadline > Instant::now());
        let found = matches.next().cloned();
        if matches.next().is_some() {
            None
        } else {
            found
        }
    } else {
        entries.get(&input.session_id).cloned()
    };
    found
        .filter(|s| s.backend.id == input.backend_id)
        .ok_or_else(|| AppError::bad_request("授权会话不存在或不属于该服务"))
}

/// 创建有界会话，保存开始时的配置身份用于迟到授权围栏。
pub async fn start<P: DeviceProtocol>(
    state: AppState,
    input: Input,
) -> Result<axum::Json<Value>, AppError> {
    let backend = state
        .store
        .get_backend(&input.backend_id)
        .await?
        .filter(|b| b.enabled && b.kind == P::ID)
        .ok_or_else(|| AppError::bad_request("请先保存并启用服务"))?;
    let (mut result, pending) = P::start(&backend).await?;
    let sessions = state.auth.sessions::<Sessions<P>>();
    let mut entries = sessions.entries.lock().await;
    entries.retain(|_, s| s.deadline > Instant::now() && !s.cancel.is_cancelled());
    if entries.len() >= 64 {
        return Err(AppError::bad_request("等待授权的会话过多"));
    }
    for previous in entries.values().filter(|s| s.backend.id == backend.id) {
        previous.cancel.cancel();
    }
    entries.retain(|_, s| !s.cancel.is_cancelled());
    let id = uuid::Uuid::new_v4().to_string();
    entries.insert(
        id.clone(),
        Arc::new(Session::<P> {
            backend,
            pending,
            deadline: Instant::now()
                + Duration::from_secs(result["expires_in"].as_u64().unwrap_or(300)),
            cancel: CancellationToken::new(),
            terminal: Mutex::new(None),
            polling: Mutex::new(()),
        }),
    );
    result["session_id"] = json!(id);
    Ok(axum::Json(result))
}

/// 同一会话只轮询一次；配置锁先于终态锁，等待期间可取消。
pub async fn poll<P: DeviceProtocol>(
    state: AppState,
    input: Input,
) -> Result<axum::Json<Value>, AppError> {
    let session = session::<P>(&state, &input).await?;
    if let Some(result) = session.terminal.lock().await.as_ref() {
        return Ok(axum::Json(result.clone()));
    }
    let _polling = tokio::select! {
        _ = session.cancel.cancelled() => return Ok(axum::Json(json!({"status":"cancelled"}))),
        _ = tokio::time::sleep_until(session.deadline.into()) => return Ok(axum::Json(json!({"status":"expired"}))),
        guard = session.polling.lock() => guard,
    };
    if let Some(result) = session.terminal.lock().await.as_ref() {
        return Ok(axum::Json(result.clone()));
    }
    let result = tokio::select! {
        _ = session.cancel.cancelled() => return Ok(axum::Json(json!({"status":"cancelled"}))),
        _ = tokio::time::sleep_until(session.deadline.into()) => return Ok(axum::Json(json!({"status":"expired"}))),
        result = P::poll(&session.backend, &session.pending) => result?,
    };
    let Some((account, mut result)) = result else {
        return Ok(axum::Json(json!({"status":"pending"})));
    };
    // 网络阶段结束后才申请配置写锁，不用授权轮询阻塞其他配置。
    let service = state.config_service();
    let guard = tokio::select! {
        _ = session.cancel.cancelled() => return Ok(axum::Json(json!({"status":"cancelled"}))),
        _ = tokio::time::sleep_until(session.deadline.into()) => return Ok(axum::Json(json!({"status":"expired"}))),
        guard = service.lock_write() => guard,
    };
    let mut terminal = session.terminal.lock().await;
    if let Some(result) = &*terminal {
        return Ok(axum::Json(result.clone()));
    }
    if session.cancel.is_cancelled() || session.deadline <= Instant::now() {
        return Ok(axum::Json(json!({"status":"cancelled"})));
    }
    if !service
        .save_login_locked(&guard, &account, &session.backend)
        .await?
    {
        return Err(AppError::bad_request("服务配置已变化，请重新登录"));
    }
    result["status"] = json!("done");
    result["account_id"] = json!(account.id);
    *terminal = Some(result.clone());
    Ok(axum::Json(result))
}

/// 先中断网络等待，再读最终保存结果；成功不能被取消覆盖。
pub async fn cancel<P: DeviceProtocol>(
    state: AppState,
    input: Input,
) -> Result<axum::Json<Value>, AppError> {
    let session = session::<P>(&state, &input).await?;
    session.cancel.cancel();
    let mut terminal = session.terminal.lock().await;
    Ok(axum::Json(
        terminal
            .get_or_insert_with(|| json!({"status":"cancelled"}))
            .clone(),
    ))
}

/// 生成兼容 handler 与统一操作入口，协议实现不再持有 AppState。
macro_rules! device_handlers {
    ($protocol:ty) => {
        /// 兼容授权开始入口。
        pub async fn start(
            axum::extract::State(state): axum::extract::State<crate::state::AppState>,
            headers: axum::http::HeaderMap,
            axum::Json(input): axum::Json<super::super::device_auth::Input>,
        ) -> Result<axum::Json<serde_json::Value>, crate::core::AppError> {
            crate::admin::authorize(&state, &headers)?;
            start_authorized(state, input).await
        }
        /// 已授权的开始用例。
        pub(crate) async fn start_authorized(
            state: crate::state::AppState,
            input: super::super::device_auth::Input,
        ) -> Result<axum::Json<serde_json::Value>, crate::core::AppError> {
            super::super::device_auth::start::<$protocol>(state, input).await
        }
        /// 兼容授权状态入口。
        pub async fn poll(
            axum::extract::State(state): axum::extract::State<crate::state::AppState>,
            headers: axum::http::HeaderMap,
            axum::Json(input): axum::Json<super::super::device_auth::Input>,
        ) -> Result<axum::Json<serde_json::Value>, crate::core::AppError> {
            crate::admin::authorize(&state, &headers)?;
            poll_authorized(state, input).await
        }
        /// 已授权的轮询用例。
        pub(crate) async fn poll_authorized(
            state: crate::state::AppState,
            input: super::super::device_auth::Input,
        ) -> Result<axum::Json<serde_json::Value>, crate::core::AppError> {
            super::super::device_auth::poll::<$protocol>(state, input).await
        }
        /// 兼容撤销入口。
        pub async fn cancel(
            axum::extract::State(state): axum::extract::State<crate::state::AppState>,
            headers: axum::http::HeaderMap,
            axum::Json(input): axum::Json<super::super::device_auth::Input>,
        ) -> Result<axum::Json<serde_json::Value>, crate::core::AppError> {
            crate::admin::authorize(&state, &headers)?;
            cancel_authorized(state, input).await
        }
        /// 已授权的撤销用例。
        pub(crate) async fn cancel_authorized(
            state: crate::state::AppState,
            input: super::super::device_auth::Input,
        ) -> Result<axum::Json<serde_json::Value>, crate::core::AppError> {
            super::super::device_auth::cancel::<$protocol>(state, input).await
        }
    };
}
pub(crate) use device_handlers;
