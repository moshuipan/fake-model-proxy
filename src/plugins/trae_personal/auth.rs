//! TRAE 个人版官方网页 loopback 登录；官网回传的登录标识绑定独立会话。

use crate::accounts::Account;
use crate::admin::authorize;
use crate::backend::trae_personal::{
    Options, TraePersonalBackend, WEBSITE,
    credential::{CLIENT_ID, Credential},
    device::ClientDevice,
};
use crate::core::{AppError, ModelInfo};
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
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    net::TcpListener,
    sync::{Mutex, oneshot},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

const LIFETIME: Duration = Duration::from_secs(600);

/// 令牌留在服务器，管理端只轮询无凭证的终态。
#[derive(Default)]
pub struct Sessions {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

#[async_trait::async_trait]
impl crate::plugins::auth::SessionStore for Sessions {
    /// 服务连接身份变化时释放本机授权监听。
    async fn cancel_service(&self, id: &str) {
        self.cancel_backend(id).await;
    }
}

struct Session {
    backend_id: String,
    nonce: String,
    deadline: Instant,
    cancel: CancellationToken,
    grant: Mutex<Option<oneshot::Sender<HashMap<String, String>>>>,
    result: Mutex<Option<Result<Value, String>>>,
}

impl Drop for Session {
    /// 会话释放后结束临时监听。
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
impl Drop for Sessions {
    /// 应用关闭时取消所有授权任务。
    fn drop(&mut self) {
        for session in self.sessions.get_mut().values() {
            session.cancel.cancel();
        }
    }
}
impl Sessions {
    /// 会话过期后释放端口，并限制未清理会话数量。
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
            return Err(AppError::bad_request("TRAE 登录等待过多，请关闭已有窗口"));
        }
        let id = uuid::Uuid::new_v4().to_string();
        sessions.insert(id.clone(), session);
        Ok(id)
    }
    /// 服务 ID 与会话 ID 必须同时匹配。
    async fn get(&self, input: &Input) -> Result<Arc<Session>, AppError> {
        self.sessions
            .lock()
            .await
            .get(&input.session_id)
            .filter(|session| session.backend_id == input.backend_id)
            .cloned()
            .ok_or_else(|| AppError::bad_request("TRAE 个人登录会话不存在"))
    }
    /// 服务配置变更后不接受迟到回调。
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

/// 开始、轮询和取消的管理输入。
#[derive(Deserialize)]
pub struct Input {
    pub backend_id: String,
    #[serde(default)]
    pub session_id: String,
}

/// 官网只接受固定 /authorize 路径，回传 loginTraceID 必须匹配此次随机标识。
async fn callback(
    State(session): State<Arc<Session>>,
    Query(input): Query<HashMap<String, String>>,
) -> (StatusCode, HeaderMap, Html<&'static str>) {
    let mut headers = HeaderMap::new();
    for (key, value) in [
        ("cache-control", "no-store"),
        ("referrer-policy", "no-referrer"),
        (
            "content-security-policy",
            "default-src 'none'; frame-ancestors 'none'",
        ),
    ] {
        headers.insert(key, value.parse().unwrap());
    }
    let status = if input
        .get("loginTraceID")
        .or_else(|| input.get("login_trace_id"))
        != Some(&session.nonce)
        || input.values().map(String::len).sum::<usize>() > 65536
        || !input.contains_key("userJwt") && !input.contains_key("refreshToken")
    {
        StatusCode::BAD_REQUEST
    } else if session.cancel.is_cancelled() || Instant::now() >= session.deadline {
        StatusCode::GONE
    } else {
        match session.grant.lock().await.take() {
            Some(sender) => {
                if sender.send(input).is_ok() {
                    StatusCode::OK
                } else {
                    StatusCode::CONFLICT
                }
            }
            _ => StatusCode::CONFLICT,
        }
    };
    (
        status,
        headers,
        Html(
            "<!doctype html><meta charset=utf-8><title>TRAE 个人授权</title><p>请返回 FakeModelProxy 控制台查看个人账号授权结果。</p>",
        ),
    )
}

/// 只允许已保存并启用的个人版 CN 服务。
async fn backend(state: &AppState, id: &str) -> Result<BackendRecord, AppError> {
    let record = state
        .store
        .get_backend(id)
        .await?
        .filter(|backend| backend.enabled && backend.kind == "trae_personal")
        .ok_or_else(|| AppError::bad_request("请先保存并启用 TRAE 个人版服务"))?;
    crate::backend::trae_personal::profile(&record)?;
    Ok(record)
}

/// 授权与个人 API 复用服务设备；没有注册信息时使用 0，不再每次编造随机设备。
fn login_url(
    port: u16,
    nonce: &str,
    backend_id: &str,
    device: Option<&ClientDevice>,
) -> reqwest::Url {
    let fallback_machine = format!(
        "{:x}",
        Sha256::digest(format!("fmp:trae:machine:{backend_id}"))
    );
    let machine = device
        .map(|device| device.machine_id.as_str())
        .filter(|machine| !machine.is_empty())
        .unwrap_or(&fallback_machine);
    let device_id = device.map_or("0", |device| device.device_id.as_str());
    let mut url = reqwest::Url::parse(&format!("{WEBSITE}/authorization")).unwrap();
    let callback = format!("http://127.0.0.1:{port}/authorize");
    for (key, value) in [
        ("login_version", "1"),
        ("auth_from", "solo"),
        ("login_channel", "native_ide"),
        (
            "plugin_version",
            device.map_or("local", |device| &device.plugin_version),
        ),
        ("auth_type", "local"),
        ("client_id", CLIENT_ID),
        ("redirect", "0"),
        ("login_trace_id", nonce),
        ("auth_callback_url", &callback),
        ("machine_id", machine),
        ("device_id", device_id),
        ("x_device_id", device_id),
        ("x_machine_id", machine),
        ("x_env", ""),
        ("hide_saas_login", "true"),
    ] {
        url.query_pairs_mut().append_pair(key, value);
    }
    if let Some(device) = device {
        for (key, value) in [
            ("x_device_brand", &device.device_brand),
            ("x_device_type", &device.device_type),
            ("x_os_version", &device.os_version),
            ("x_app_version", &device.app_version),
            ("x_app_type", &device.app_type),
        ] {
            url.query_pairs_mut().append_pair(key, value);
        }
    }
    url
}

/// 官方回调支持直接个人 userJwt 或个人 refreshToken 换票。
async fn grant(input: HashMap<String, String>) -> Result<Credential, AppError> {
    if let Some(raw) = input.get("userJwt") {
        let value: Value = serde_json::from_str(raw)
            .map_err(|_| AppError::bad_request("TRAE 个人授权回调格式无效，请重新登录"))?;
        if value["Token"]
            .as_str()
            .is_some_and(|token| !token.is_empty())
        {
            return Credential::parse(raw);
        }
        if let Some(refresh) = value["RefreshToken"]
            .as_str()
            .filter(|token| !token.is_empty())
        {
            return Credential::exchange(
                &reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()?,
                crate::backend::trae_personal::credential::AUTH_ORIGIN,
                refresh,
                "",
            )
            .await;
        }
    }
    let refresh = input
        .get("refreshToken")
        .filter(|token| !token.is_empty())
        .ok_or_else(|| AppError::bad_request("TRAE 个人回调缺少授权票据，请重新登录"))?;
    Credential::exchange(
        &reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?,
        crate::backend::trae_personal::credential::AUTH_ORIGIN,
        refresh,
        "",
    )
    .await
}

/// 启动独立临时端口；每次登录只消费一次回调。
pub async fn start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Input>,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    start_authorized(state, input).await
}

/// 统一操作沿用回环回调和随机标识检查。
pub(crate) async fn start_authorized(
    state: AppState,
    input: Input,
) -> Result<Json<Value>, AppError> {
    let backend = backend(&state, &input.backend_id).await?;
    let device = Options::parse(&backend)?.client_device;
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|_| AppError::bad_request("TRAE 个人授权回调端口无法启动"))?;
    let port = listener
        .local_addr()
        .map_err(|_| AppError::bad_request("无法读取本机回调端口"))?
        .port();
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let nonce = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let (sender, receiver) = oneshot::channel();
    let session = Arc::new(Session {
        backend_id: backend.id.clone(),
        nonce: nonce.clone(),
        deadline: Instant::now() + LIFETIME,
        cancel: CancellationToken::new(),
        grant: Mutex::new(Some(sender)),
        result: Mutex::new(None),
    });
    let id = state
        .auth
        .sessions::<Sessions>()
        .insert(session.clone())
        .await?;
    let result = json!({"kind":"loopback_oauth","session_id":id,"login_url":login_url(port,&nonce,&backend.id,device.as_ref()).as_str(),
        "expires_in":LIFETIME.as_secs(),"automatic_account_save":true});
    let app = Router::new()
        .route("/authorize", get(callback))
        .with_state(session.clone());
    let cancel = session.cancel.clone();
    let deadline = session.deadline;
    tokio::spawn(async move {
        let _ = axum::serve(listener,app).with_graceful_shutdown(async move {
            tokio::select! { _ = cancel.cancelled() => {}, _ = tokio::time::sleep_until(deadline) => {} }
        }).await;
    });
    tokio::spawn(async move {
        let outcome = tokio::select! {
            _ = session.cancel.cancelled() => Err(AppError::bad_request("TRAE 个人授权已取消")),
            _ = tokio::time::sleep_until(session.deadline) => Err(AppError::bad_request("TRAE 个人授权已过期")),
            result = async {
                let credential = grant(receiver.await.map_err(|_| AppError::bad_request("TRAE 回调已结束"))?).await?;
                let provider = TraePersonalBackend::new(&backend,Vec::new(),state.store.clone())?;
                let models = provider.verify(&credential).await?;
                Ok::<_,AppError>((credential,models))
            } => result,
        };
        let guard = state.config_service().lock_write().await;
        let mut terminal = session.result.lock().await;
        if terminal.is_some() {
            return;
        }
        if session.cancel.is_cancelled() || Instant::now() >= session.deadline {
            *terminal = Some(Err("TRAE 个人授权已取消或过期".into()));
        } else {
            match outcome {
                Ok((credential, models)) => match save(&state, &guard, &backend, &credential).await
                {
                    Ok(account) => {
                        *terminal = Some(Ok(json!({"status":"saving","name":account.name})));
                        drop(terminal);
                        drop(guard);
                        *session.result.lock().await =
                            Some(Ok(finish(&state, &backend, &account, &models).await));
                    }
                    Err(error) => *terminal = Some(Err(error.message())),
                },
                Err(error) => *terminal = Some(Err(error.message())),
            }
        }
        session.cancel.cancel();
    });
    Ok(Json(result))
}

/// 同一服务内用个人 subject 去重，保存前再次核实配置未改变。
async fn save(
    state: &AppState,
    guard: &crate::application::config_service::ConfigWriteGuard<'_>,
    backend: &BackendRecord,
    credential: &Credential,
) -> Result<Account, AppError> {
    let id = format!(
        "trae-personal-{:x}",
        md5::compute(format!("{}:{}", backend.id, credential.subject))
    );
    let name = state
        .store
        .get_account(&id)
        .await?
        .map(|account| account.name)
        .unwrap_or_else(|| {
            format!(
                "TRAE 个人 · {}",
                credential.subject.chars().take(8).collect::<String>()
            )
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
        return Err(AppError::bad_request(
            "TRAE 个人服务已删除、停用或配置已变更",
        ));
    }
    Ok(account)
}

/// 账号已提交后，目录错误单独呈现，不把保存成功误报成授权失败。
async fn finish(
    state: &AppState,
    backend: &BackendRecord,
    account: &Account,
    models: &[ModelInfo],
) -> Value {
    let synced = state.config_service().sync_models(backend, models).await;
    let mut result = json!({"status":"done","account_id":account.id,"name":account.name,"models_synced":synced.is_ok()});
    if let Err(error) = synced {
        result["sync_error"] = json!(error.message());
    }
    if let Err(error) = state.reload_backends().await {
        result["reload_error"] = json!(error.message());
    }
    result
}

/// 轮询只读取会话，不重复换票。
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
        *result = Some(Err("TRAE 个人授权已过期".into()));
    }
    match result.as_ref() {
        Some(Ok(value)) => Ok(Json(value.clone())),
        Some(Err(message)) => Err(AppError::bad_request(message)),
        None => Ok(Json(json!({"status":"pending"}))),
    }
}

/// 取消不回滚已保存账号。
async fn cancel_session(session: &Session) {
    let mut result = session.result.lock().await;
    if result.is_none() {
        session.cancel.cancel();
        *result = Some(Err("TRAE 个人授权已取消".into()));
    }
}

/// 管理鉴权与服务归属验证后关闭临时监听。
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
