//! 小浣熊官方 loopback 授权；每个标签页拥有独立 state 和一次性回调。

use crate::accounts::Account;
use crate::admin::authorize;
use crate::backend::Backend;
use crate::backend::raccoon::{
    RaccoonBackend,
    client::{Client, ORIGIN, USER_PATH, decode, transport_error},
};
use crate::core::AppError;
use crate::state::AppState;
use crate::store::BackendRecord;
use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::Html,
    routing::get,
};
use rand_core::{OsRng, RngCore};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    net::TcpListener,
    sync::{Mutex, oneshot},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

const LIFETIME: Duration = Duration::from_secs(600);

/// 登录结果不含令牌；未完成会话随应用、配置或取消操作释放。
#[derive(Default)]
pub struct Sessions {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

#[async_trait::async_trait]
impl crate::plugins::auth::SessionStore for Sessions {
    /// 由宿主统一撤销本服务的等待授权。
    async fn cancel_service(&self, id: &str) {
        self.cancel_backend(id).await;
    }
}

struct Session {
    backend_id: String,
    state: String,
    deadline: Instant,
    cancel: CancellationToken,
    code: Mutex<Option<oneshot::Sender<String>>>,
    result: Mutex<Option<Result<Value, String>>>,
}

impl Drop for Session {
    /// 最后一个会话引用释放时关闭回调监听。
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Sessions {
    /// 清理过期监听并限制未清理的会话数，防止重复点击占用本机端口。
    async fn insert(&self, session: Arc<Session>) -> Result<String, AppError> {
        let mut sessions = self.sessions.lock().await;
        sessions.retain(|_, session| {
            if session.deadline <= Instant::now() {
                session.cancel.cancel();
                false
            } else {
                true
            }
        });
        if sessions.len() >= 64 {
            return Err(AppError::bad_request(
                "小浣熊授权会话过多，请关闭已有等待窗口",
            ));
        }
        let id = uuid::Uuid::new_v4().to_string();
        sessions.insert(id.clone(), session);
        Ok(id)
    }

    /// 轮询和取消必须同时匹配会话 ID 与服务 ID。
    async fn get(&self, input: &Input) -> Result<Arc<Session>, AppError> {
        self.sessions
            .lock()
            .await
            .get(&input.session_id)
            .filter(|session| session.backend_id == input.backend_id)
            .cloned()
            .ok_or_else(|| AppError::bad_request("小浣熊授权会话不存在，请重新开始"))
    }

    /// 服务删除、停用或地址修改后拒绝迟到回调。
    pub async fn cancel_backend(&self, id: &str) {
        let sessions = self
            .sessions
            .lock()
            .await
            .values()
            .filter(|session| session.backend_id == id)
            .cloned()
            .collect::<Vec<_>>();
        for session in sessions {
            cancel_session(&session).await;
        }
    }
}

impl Drop for Sessions {
    /// 应用关闭时结束仍被异步任务引用的会话。
    fn drop(&mut self) {
        for session in self.sessions.get_mut().values() {
            session.cancel.cancel();
        }
    }
}

/// 控制台开始、轮询和取消的输入。
#[derive(Deserialize)]
pub struct Input {
    pub backend_id: String,
    #[serde(default)]
    pub session_id: String,
}

/// 官网桌面 loopback 模式使用 code，而非旧 IDE 模式的 authorization_code。
#[derive(Deserialize)]
struct Callback {
    state: String,
    code: String,
}

/// 校验 state 后消费一次授权码；静态页面不回显任何查询参数。
async fn callback(
    State(session): State<Arc<Session>>,
    Query(input): Query<Callback>,
) -> (StatusCode, HeaderMap, Html<&'static str>) {
    let mut headers = HeaderMap::new();
    headers.insert("cache-control", "no-store".parse().unwrap());
    headers.insert("referrer-policy", "no-referrer".parse().unwrap());
    headers.insert(
        "content-security-policy",
        "default-src 'none'; frame-ancestors 'none'"
            .parse()
            .unwrap(),
    );
    if input.state != session.state
        || input.code.is_empty()
        || input.code.len() > 4096
        || input.code.chars().any(char::is_control)
    {
        return (
            StatusCode::BAD_REQUEST,
            headers,
            Html(
                "<!doctype html><meta charset=utf-8><title>授权无效</title><p>小浣熊授权回调无效，请从控制台重新开始。</p>",
            ),
        );
    }
    if session.cancel.is_cancelled() || Instant::now() >= session.deadline {
        return (
            StatusCode::GONE,
            headers,
            Html(
                "<!doctype html><meta charset=utf-8><title>授权已结束</title><p>授权已取消或过期，请回到控制台。</p>",
            ),
        );
    }
    let status = match session.code.lock().await.take() {
        Some(sender) => {
            if sender.send(input.code).is_ok() {
                StatusCode::OK
            } else {
                StatusCode::CONFLICT
            }
        }
        _ => StatusCode::CONFLICT,
    };
    (
        status,
        headers,
        Html(
            "<!doctype html><meta charset=utf-8><title>小浣熊授权</title><p>授权码已收到。账号保存结果请回到 FakeModelProxy 控制台查看。</p>",
        ),
    )
}

/// 使用官网支持的独立本机 HTTP 回调，不打开桌面应用或独立浏览器。
pub async fn start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Input>,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    start_authorized(state, input).await
}

/// 统一操作沿用回调 state 与过期规则，不创建第二套授权流程。
pub(crate) async fn start_authorized(
    state: AppState,
    input: Input,
) -> Result<Json<Value>, AppError> {
    let backend = state
        .store
        .get_backend(&input.backend_id)
        .await?
        .filter(|backend| backend.enabled && backend.kind == "raccoon")
        .ok_or_else(|| AppError::bad_request("请先保存并启用小浣熊服务"))?;
    let client = Client::new(&backend, Duration::from_secs(10))?;
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|_| AppError::bad_request("小浣熊本机回调端口无法启动"))?;
    let port = listener.local_addr().map_err(|_| transport_error())?.port();
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let nonce = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let (sender, receiver) = oneshot::channel();
    let session = Arc::new(Session {
        backend_id: backend.id.clone(),
        state: nonce.clone(),
        deadline: Instant::now() + LIFETIME,
        cancel: CancellationToken::new(),
        code: Mutex::new(Some(sender)),
        result: Mutex::new(None),
    });
    let session_id = state
        .auth
        .sessions::<Sessions>()
        .insert(session.clone())
        .await?;
    let mut url = reqwest::Url::parse(&format!("{ORIGIN}/authorize")).unwrap();
    url.query_pairs_mut()
        .append_pair("login_source", "desktop")
        .append_pair("desktop_callback", "loopback")
        .append_pair("appname", "FakeModelProxy")
        .append_pair(
            "redirect",
            &format!("http://127.0.0.1:{port}/auth/callback"),
        )
        .append_pair("state", &nonce)
        .append_pair("lang", "zh");
    let response = json!({
        "kind":"loopback_oauth","session_id":session_id,"login_url":url.as_str(),
        "expires_in":LIFETIME.as_secs(),"automatic_account_save":true,
    });
    let app = Router::new()
        .route("/auth/callback", get(callback))
        .with_state(session.clone());
    let server_cancel = session.cancel.clone();
    let server_deadline = session.deadline;
    tokio::spawn(async move {
        let result = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                tokio::select! {
                    _ = server_cancel.cancelled() => {},
                    _ = tokio::time::sleep_until(server_deadline) => {},
                }
            })
            .await;
        if result.is_err() {
            tracing::warn!("小浣熊回调监听已停止");
        }
    });
    tokio::spawn(async move {
        let grant = tokio::select! {
            _ = session.cancel.cancelled() => Err(AppError::bad_request("小浣熊授权已取消")),
            _ = tokio::time::sleep_until(session.deadline) => Err(AppError::bad_request("小浣熊授权已过期")),
            result = async {
                let code = receiver.await.map_err(|_| AppError::bad_request("小浣熊回调已结束"))?;
                let credential = client.exchange(&code).await?;
                let response = client.request(reqwest::Method::GET, USER_PATH, &credential)
                    .timeout(Duration::from_secs(20)).send().await.map_err(|_| transport_error())?;
                Ok::<_, AppError>((credential, decode(response).await?))
            } => result,
        };
        let config_service = state.config_service();
        let write_guard = if grant.is_ok() {
            tokio::select! {
                _ = session.cancel.cancelled() => None,
                _ = tokio::time::sleep_until(session.deadline) => None,
                guard = config_service.lock_write() => Some(guard),
            }
        } else {
            None
        };
        let mut terminal = session.result.lock().await;
        if terminal.is_some() {
            return;
        }
        if session.cancel.is_cancelled() || Instant::now() >= session.deadline {
            *terminal = Some(Err("小浣熊授权已取消或过期".into()));
            session.cancel.cancel();
            return;
        }
        // 保存是终态提交边界；提交后目录失败单独呈现，不能误报账号授权失败。
        let saved = match grant {
            Ok((credential, user)) => {
                save(
                    &state,
                    write_guard
                        .as_ref()
                        .expect("valid authorization holds write guard"),
                    &backend,
                    credential,
                    &user,
                )
                .await
            }
            Err(error) => Err(error),
        };
        drop(write_guard);
        match saved {
            Ok(account) => {
                // 已保存的账号不能被随后取消回滚；后台同步不再占用凭证锁。
                *terminal = Some(Ok(json!({"status":"saving","name":account.name})));
                drop(terminal);
                let synced = async {
                    let provider =
                        RaccoonBackend::new(&backend, vec![account.clone()], state.store.clone())?;
                    let models = provider.list_models().await?;
                    state
                        .config_service()
                        .sync_models(&backend, &models)
                        .await?;
                    Ok::<_, AppError>(())
                }
                .await;
                let mut result = json!({"status":"done","account_id":account.id,"name":account.name,
                    "models_synced":synced.is_ok()});
                if let Err(error) = synced {
                    result["sync_error"] = json!(error.message());
                }
                if let Err(error) = state.reload_backends().await {
                    result["reload_error"] = json!(error.message());
                }
                let quota = crate::accounts::quota_refresh::refresh_account(
                    &state.store,
                    &state.quota_refresh,
                    &backend,
                    &account,
                )
                .await;
                if let Err(error) = quota {
                    result["quota_error"] = json!(error.message());
                }
                *session.result.lock().await = Some(Ok(result));
            }
            Err(error) => *terminal = Some(Err(error.message())),
        }
        session.cancel.cancel();
    });
    Ok(Json(response))
}

/// 用户身份来自官方 user_info；服务归属和配置由数据库条件保存验证。
async fn save(
    state: &AppState,
    guard: &crate::application::config_service::ConfigWriteGuard<'_>,
    backend: &BackendRecord,
    credential: crate::backend::raccoon::credential::Credential,
    user: &Value,
) -> Result<Account, AppError> {
    let identity = user["id"]
        .as_str()
        .map(ToOwned::to_owned)
        .or_else(|| user["id"].as_u64().map(|id| id.to_string()))
        .filter(|id| !id.is_empty())
        .ok_or_else(|| AppError::bad_request("小浣熊用户信息缺少账号 ID"))?;
    let id = format!(
        "raccoon-{:x}",
        md5::compute(format!("{}:{identity}", backend.id))
    );
    let existing = state.store.get_account(&id).await?;
    let name = existing.map(|account| account.name).unwrap_or_else(|| {
        user["name"]
            .as_str()
            .filter(|name| !name.trim().is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("小浣熊 · {}", identity.chars().take(8).collect::<String>()))
    });
    let account = Account {
        id,
        backend_id: backend.id.clone(),
        name,
        credential: credential.serialize()?,
        enabled: true,
    };
    if !state
        .config_service()
        .save_login_locked(guard, &account, backend)
        .await?
    {
        return Err(AppError::bad_request("小浣熊服务已删除、停用或配置已变更"));
    }
    Ok(account)
}

/// 查询终态不再换票，令牌始终留在服务端。
pub async fn poll(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Input>,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    poll_authorized(state, input).await
}

/// 查询受服务归属约束的授权会话。
pub(crate) async fn poll_authorized(
    state: AppState,
    input: Input,
) -> Result<Json<Value>, AppError> {
    let session = state.auth.sessions::<Sessions>().get(&input).await?;
    let mut result = session.result.lock().await;
    if result.is_none() && Instant::now() >= session.deadline {
        session.cancel.cancel();
        *result = Some(Err("小浣熊授权已过期，请重新开始".into()));
    }
    match result.as_ref() {
        Some(Ok(value)) => Ok(Json(value.clone())),
        Some(Err(message)) => Err(AppError::bad_request(message)),
        None => Ok(Json(json!({"status":"pending"}))),
    }
}

/// 取消仅作用于未提交会话，不覆盖已保存账号结果。
async fn cancel_session(session: &Session) {
    let mut result = session.result.lock().await;
    if result.is_none() {
        session.cancel.cancel();
        *result = Some(Err("小浣熊授权已取消".into()));
    }
}

/// 管理鉴权及服务归属验证后关闭回调端口。
pub async fn cancel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Input>,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    cancel_authorized(state, input).await
}

/// 已授权调用者取消当前会话，保留成功终态。
pub(crate) async fn cancel_authorized(
    state: AppState,
    input: Input,
) -> Result<Json<Value>, AppError> {
    cancel_session(&*state.auth.sessions::<Sessions>().get(&input).await?).await;
    Ok(Json(json!({"status":"cancelled"})))
}
