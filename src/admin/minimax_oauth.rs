//! MiniMax 独立设备授权；每个窗口隔离，账号保存后只返回无凭证结果。

use super::authorize;
use crate::accounts::Account;
use crate::backend::minimax_code::{
    oauth::{OAuth, PollResult},
    profile,
};
use crate::core::AppError;
use crate::state::AppState;
use crate::store::BackendRecord;
use axum::{Json, extract::State, http::HeaderMap};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};
use tokio::{sync::Mutex, time::Instant};
use tokio_util::sync::CancellationToken;

/// 设备授权秘密仅保留至截止时间，结果不含令牌。
#[derive(Default)]
pub struct Sessions {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

struct Session {
    backend_id: String,
    deadline: Instant,
    cancel: CancellationToken,
    result: Mutex<Option<Result<Value, String>>>,
}

impl Sessions {
    /// 清理过期记录后创建独立随机会话。
    async fn insert(&self, session: Arc<Session>) -> String {
        let mut sessions = self.sessions.lock().await;
        sessions.retain(|_, value| value.deadline > Instant::now());
        let id = uuid::Uuid::new_v4().to_string();
        sessions.insert(id.clone(), session);
        id
    }

    /// 后端和会话必须同时匹配，不能领取其他服务的登录结果。
    async fn get(&self, input: &Input) -> Result<Arc<Session>, AppError> {
        self.sessions
            .lock()
            .await
            .get(&input.session_id)
            .filter(|session| session.backend_id == input.backend_id)
            .cloned()
            .ok_or_else(|| AppError::bad_request("MiniMax 授权会话不存在，请重新开始"))
    }

    /// 配置变更或删除时取消未完成授权；已保存结果不受影响。
    pub async fn cancel_backend(&self, id: &str) {
        let sessions = self
            .sessions
            .lock()
            .await
            .values()
            .filter(|s| s.backend_id == id)
            .cloned()
            .collect::<Vec<_>>();
        for session in sessions {
            cancel_session(&session).await;
        }
    }
}

/// 开始仅需要服务 ID；轮询和取消必须携带会话 ID。
#[derive(Deserialize)]
pub struct Input {
    pub backend_id: String,
    #[serde(default)]
    pub session_id: String,
}

/// 保存并启用服务后向所选区域发起独立 OAuth，前端不接触设备码秘密。
pub async fn start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Input>,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    let backend = state
        .store
        .get_backend(&input.backend_id)
        .await?
        .filter(|b| b.enabled && b.kind == "minimax_code")
        .ok_or_else(|| AppError::bad_request("请先保存并启用 MiniMax Code 服务"))?;
    let (region, base) = profile(&backend)?;
    let mut oauth = OAuth::new(&region)?;
    // 明确配置的回环假上游可用于离线验收，生产域不允许覆盖账号域。
    if base.starts_with("http://") {
        oauth.origin = base;
    }
    let device = oauth.start().await?;
    let session = Arc::new(Session {
        backend_id: backend.id.clone(),
        deadline: Instant::now() + std::time::Duration::from_secs(device.expires_in),
        cancel: CancellationToken::new(),
        result: Mutex::new(None),
    });
    let session_id = state.minimax_oauth.insert(session.clone()).await;
    let response = json!({
        "kind":"device_oauth","session_id":session_id,"login_url":device.login_url,
        "user_code":device.user_code,"expires_in":device.expires_in,"automatic_account_save":true,
    });
    tokio::spawn(async move {
        let mut interval = device.interval;
        let result = async {
            loop {
                tokio::select! {
                    _ = session.cancel.cancelled() => return Err(AppError::bad_request("MiniMax 授权已取消")),
                    _ = tokio::time::sleep_until(session.deadline) => return Err(AppError::bad_request("MiniMax 设备码已过期")),
                    _ = tokio::time::sleep(interval) => {}
                }
                let poll = tokio::select! {
                    _ = session.cancel.cancelled() => return Err(AppError::bad_request("MiniMax 授权已取消")),
                    _ = tokio::time::sleep_until(session.deadline) => return Err(AppError::bad_request("MiniMax 设备码已过期")),
                    result = oauth.poll(&device) => result?,
                };
                match poll {
                    PollResult::Pending => {}
                    PollResult::SlowDown => interval += std::time::Duration::from_secs(5),
                    PollResult::Done(credential) => return Ok(credential),
                }
            }
        }.await;
        // 不在等待刷新锁期间锁住会话终态；用户仍可取消迟到的授权。
        let _credential_guard = if result.is_ok() {
            tokio::select! {
                _ = session.cancel.cancelled() => None,
                _ = tokio::time::sleep_until(session.deadline) => None,
                guard = state.store.lock_credentials() => Some(guard),
            }
        } else {
            None
        };
        let mut terminal = session.result.lock().await;
        if terminal.is_some() {
            return;
        }
        if session.cancel.is_cancelled() || Instant::now() >= session.deadline {
            *terminal = Some(Err("MiniMax 授权已取消或过期".into()));
            return;
        }
        *terminal = Some(match result {
            Ok(credential) => save(&state, &backend, credential)
                .await
                .map_err(|e| e.message()),
            Err(error) => Err(error.message()),
        });
    });
    Ok(Json(response))
}

/// 调用者持有刷新锁；条件保存阻止服务改区域后迟到授权复活账号。
async fn save(
    state: &AppState,
    backend: &BackendRecord,
    credential: crate::backend::minimax_code::oauth::Credential,
) -> Result<Value, AppError> {
    let identity = if credential.subject.is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        credential.subject.clone()
    };
    let id = format!(
        "minimax-{:x}",
        md5::compute(format!("{}:{identity}", backend.id))
    );
    let existing = state.store.get_account(&id).await?;
    let name = existing
        .map(|a| a.name)
        .unwrap_or_else(|| format!("MiniMax · {}", identity.chars().take(8).collect::<String>()));
    let account = Account {
        id: id.clone(),
        backend_id: backend.id.clone(),
        name: name.clone(),
        credential: credential.serialize()?,
        enabled: true,
    };
    if !state.store.save_login_account(&account, backend).await? {
        return Err(AppError::bad_request("服务已停用或区域已变更，请重新授权"));
    }
    state.store.abort_credential_refresh(&id).await?;
    state.reload_backends().await?;
    Ok(json!({"status":"done","account_id":id,"name":name}))
}

/// 查询终态不会再次向 OAuth 换票，不返回任何令牌。
pub async fn poll(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Input>,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    let session = state.minimax_oauth.get(&input).await?;
    let mut result = session.result.lock().await;
    if result.is_none() && Instant::now() >= session.deadline {
        session.cancel.cancel();
        *result = Some(Err("MiniMax 授权已过期，请重新开始".into()));
    }
    match result.as_ref() {
        Some(Ok(value)) => Ok(Json(value.clone())),
        Some(Err(message)) => Err(AppError::bad_request(message)),
        None => Ok(Json(json!({"status":"pending"}))),
    }
}

/// 关闭等待窗口后停止后端轮询，不覆盖已保存终态。
async fn cancel_session(session: &Session) {
    let mut result = session.result.lock().await;
    if result.is_none() {
        session.cancel.cancel();
        *result = Some(Err("MiniMax 授权已取消".into()));
    }
}

/// 管理鉴权和会话归属均验证后取消。
pub async fn cancel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Input>,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    let session = state.minimax_oauth.get(&input).await?;
    cancel_session(&session).await;
    Ok(Json(json!({"status":"cancelled"})))
}
