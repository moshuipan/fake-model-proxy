//! AutoGLM 国内短信网页与海外 OAuth，登录完成后条件保存账号。

use crate::accounts::Account;
use crate::admin::authorize;
use crate::backend::autoglm::client::Client;
use crate::backend::autoglm::oauth::{EXPIRES_IN, Flow, Provider};
use crate::core::AppError;
use crate::state::AppState;
use crate::store::BackendRecord;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, header};
use axum::response::{Html, IntoResponse};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// 会话只存在于本进程内；国内网页与海外回调各自拥有独立设备和期限。
#[derive(Default)]
pub struct Sessions {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

#[async_trait::async_trait]
impl crate::plugins::auth::SessionStore for Sessions {
    /// 由宿主撤销该服务的所有地区授权会话。
    async fn cancel_service(&self, id: &str) {
        self.cancel_backend(id).await;
    }
}

/// 已保存结果不含凭证，轮询不会再次兑换或重复写入。
struct Session {
    backend_id: String,
    deadline: Instant,
    cancel: CancellationToken,
    result: Mutex<Option<Result<Value, String>>>,
    domestic: Option<DomesticLogin>,
}

/// 保存开始时的服务快照，不允许中途换域后继续使用已发短信。
struct DomesticLogin {
    backend: BackendRecord,
    client: Client,
    device_id: String,
    sms: Mutex<SmsState>,
}

/// 串行发码和登录，只有确认成功的短信才能解锁对应手机号。
#[derive(Default)]
struct SmsState {
    captcha_required: Option<bool>,
    phone: Option<String>,
    sent_at: Option<Instant>,
}

impl Sessions {
    /// 丢弃过期记录后登记授权，监听器本身由 Flow 的期限释放。
    async fn insert(&self, session: Arc<Session>) -> String {
        let mut sessions = self.sessions.lock().await;
        sessions.retain(|_, session| session.deadline > Instant::now());
        let id = uuid::Uuid::new_v4().to_string();
        sessions.insert(id.clone(), session);
        id
    }

    /// 同时匹配后端与会话 ID，阻止领取其他服务的授权结果。
    async fn get(&self, input: &PollInput) -> Result<Arc<Session>, AppError> {
        self.sessions
            .lock()
            .await
            .get(&input.session_id)
            .filter(|session| session.backend_id == input.backend_id)
            .cloned()
            .ok_or_else(|| AppError::bad_request("授权会话不存在或不属于此服务，请重新开始"))
    }

    /// 随机会话只能访问自身国内登录操作，不能领取管理权限或海外结果。
    async fn domestic(&self, id: &str) -> Result<Arc<Session>, AppError> {
        self.sessions
            .lock()
            .await
            .get(id)
            .filter(|session| session.domestic.is_some())
            .cloned()
            .ok_or_else(|| AppError::NotFound {
                message: "登录会话不存在，请回到控制台重新开始".into(),
            })
    }

    /// 服务配置变更后取消其未完成授权并释放端口。
    pub async fn cancel_backend(&self, backend_id: &str) {
        let sessions: Vec<_> = self
            .sessions
            .lock()
            .await
            .values()
            .filter(|session| session.backend_id == backend_id)
            .cloned()
            .collect();
        for session in sessions {
            cancel_session(&session).await;
        }
    }
}

/// 国内不选择提供商；海外必须明确选择官方 Z.ai 或 Google。
#[derive(Deserialize)]
pub struct StartInput {
    backend_id: String,
    #[serde(default)]
    provider: Option<Provider>,
    #[serde(default)]
    device_id: String,
    #[serde(default)]
    ali_captcha_verify_param: Option<String>,
}

/// 创建海外授权需要沿用的设备 UUID。
#[derive(Deserialize)]
pub struct CaptchaInput {
    backend_id: String,
}

/// 校验服务类型和启用状态，再按明确区域选择官方入口。
async fn login_backend(state: &AppState, id: &str) -> Result<(BackendRecord, Client), AppError> {
    let backend = state
        .store
        .get_backend(id)
        .await?
        .filter(|backend| backend.kind == "autoglm" && backend.enabled)
        .ok_or_else(|| AppError::bad_request("请先保存并启用 AutoGLM 服务"))?;
    let client = Client::new(&backend)?;
    Ok((backend, client))
}

/// 海外 OAuth 读取前置验证配置，国内配置由独立短信网页读取。
pub async fn captcha_config(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CaptchaInput>,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    let (_, client) = login_backend(&state, &input.backend_id).await?;
    if !client.supports_oauth() {
        return Err(AppError::bad_request("国内站请在短信登录页面完成验证"));
    }
    let device_id = uuid::Uuid::new_v4().to_string();
    let config = client.oauth_captcha_config(&device_id).await?;
    if !config["enabled"].is_boolean() {
        return Err(AppError::bad_request("AutoGLM 安全验证配置格式已变化"));
    }
    Ok(Json(json!({"device_id":device_id,"config":config})))
}

/// 前端必须持有独立会话 ID，不能只按后端领取海外授权结果。
#[derive(Deserialize)]
pub struct PollInput {
    backend_id: String,
    session_id: String,
}

/// 国内创建同源短信网页会话；海外获取官方授权 URL 并接收一次回调。
pub async fn start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<StartInput>,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    start_authorized(state, input).await
}

/// 统一操作复用国内验证码受控视图和海外 OAuth。
pub(crate) async fn start_authorized(
    state: AppState,
    input: StartInput,
) -> Result<Json<Value>, AppError> {
    let (backend, client) = login_backend(&state, &input.backend_id).await?;
    if !client.supports_oauth() || input.provider.is_none() && client.uses_domestic_login() {
        let session = Arc::new(Session {
            backend_id: backend.id.clone(),
            deadline: Instant::now() + Duration::from_secs(EXPIRES_IN),
            cancel: CancellationToken::new(),
            result: Mutex::new(None),
            domestic: Some(DomesticLogin {
                backend,
                client: client.for_browser_sms(),
                device_id: uuid::Uuid::new_v4().to_string(),
                sms: Mutex::new(SmsState::default()),
            }),
        });
        let session_id = state
            .auth
            .sessions::<Sessions>()
            .insert(session.clone())
            .await;
        tokio::spawn(async move {
            tokio::time::sleep_until(session.deadline).await;
            expire_session(&session).await;
        });
        return Ok(Json(json!({
            "kind":"sms_browser",
            "login_url":format!("/auth/autoglm/{session_id}"),
            "session_id":session_id,
            "expires_in":EXPIRES_IN,
            "automatic_account_save":true
        })));
    }
    let provider = input
        .provider
        .ok_or_else(|| AppError::bad_request("国际站请选择 Z.ai 或 Google 授权"))?;
    let device_id = if input.device_id.is_empty() {
        uuid::Uuid::new_v4()
    } else {
        uuid::Uuid::parse_str(&input.device_id)
            .map_err(|_| AppError::bad_request("授权设备 ID 无效，请重新开始"))?
    }
    .to_string();
    if input
        .ali_captcha_verify_param
        .as_ref()
        .is_some_and(|proof| proof.trim().is_empty() || proof.len() > 65536)
    {
        return Err(AppError::bad_request("安全验证回执无效，请重新验证"));
    }
    let flow = Flow::begin(
        client,
        provider,
        device_id,
        input.ali_captcha_verify_param.as_deref(),
    )
    .await?;
    let login_url = flow.login_url.clone();
    let session = Arc::new(Session {
        backend_id: backend.id.clone(),
        deadline: flow.deadline,
        cancel: flow.cancel.clone(),
        result: Mutex::new(None),
        domestic: None,
    });
    let session_id = state
        .auth
        .sessions::<Sessions>()
        .insert(session.clone())
        .await;
    tokio::spawn(async move {
        match flow.wait().await {
            Ok(credential) => {
                let _ = complete_login(&state, &session, &backend, credential, None).await;
            }
            Err(error) => {
                let mut result = session.result.lock().await;
                if result.is_none() {
                    *result = Some(Err(error.message()));
                }
            }
        }
    });
    Ok(Json(json!({
        "kind":"oauth","login_url":login_url,"session_id":session_id,
        "expires_in":EXPIRES_IN,"automatic_account_save":true
    })))
}

/// 配置锁先于会话终态锁；等待可取消，取得执行权后再次确认期限与终态。
async fn complete_login(
    state: &AppState,
    session: &Session,
    backend: &BackendRecord,
    credential: crate::backend::autoglm::credential::Credential,
    default_name: Option<String>,
) -> Result<Value, AppError> {
    let service = state.config_service();
    let guard = tokio::select! {
        biased;
        _ = session.cancel.cancelled() => return session_result(session).await,
        _ = tokio::time::sleep_until(session.deadline) => return session_result(session).await,
        guard = service.lock_write() => guard,
    };
    let mut result = session.result.lock().await;
    if let Some(result) = result.as_ref() {
        return result.clone().map_err(AppError::bad_request);
    }
    let saved = if session.cancel.is_cancelled() || session.deadline <= Instant::now() {
        session.cancel.cancel();
        Err(AppError::bad_request("授权已取消或超时，请重新开始"))
    } else {
        save_locked(state, &guard, backend, credential, default_name).await
    };
    *result = Some(saved.as_ref().cloned().map_err(AppError::message));
    saved
}

/// 使用已取得的配置写锁条件保存登录，保留名称且不再次申请配置锁。
async fn save_locked(
    state: &AppState,
    guard: &crate::application::config_service::ConfigWriteGuard<'_>,
    backend: &BackendRecord,
    credential: crate::backend::autoglm::credential::Credential,
    default_name: Option<String>,
) -> Result<Value, AppError> {
    let account_id = format!(
        "autoglm-{:x}",
        md5::compute(format!("{}:{}", backend.id, credential.user_id))
    );
    let name = state
        .store
        .get_account(&account_id)
        .await?
        .map(|account| account.name)
        .unwrap_or_else(|| {
            default_name.unwrap_or_else(|| format!("AutoGLM · {}", credential.user_id))
        });
    let account = Account {
        id: account_id.clone(),
        backend_id: backend.id.clone(),
        name: name.clone(),
        credential: credential.serialize()?,
        enabled: true,
    };
    if !state
        .config_service()
        .save_login_locked(guard, &account, backend)
        .await?
    {
        return Err(AppError::bad_request(
            "服务已删除、停用或登录配置已变更，请重新开始授权",
        ));
    }
    Ok(json!({"status":"done","account_id":account_id,"name":name}))
}

/// 查询国内登录或海外回调结果，不回显访问或刷新令牌。
pub async fn poll(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<PollInput>,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    poll_authorized(state, input).await
}

/// 查询受服务归属约束的授权会话。
pub(crate) async fn poll_authorized(
    state: AppState,
    input: PollInput,
) -> Result<Json<Value>, AppError> {
    let session = state.auth.sessions::<Sessions>().get(&input).await?;
    session_result(&session).await.map(Json)
}

/// 超时只结束未完成会话，不覆盖已经保存的结果。
async fn expire_session(session: &Session) {
    if session.deadline <= Instant::now() {
        let mut result = session.result.lock().await;
        if result.is_none() {
            session.cancel.cancel();
            *result = Some(Err("授权已超时，请回到控制台重新开始".into()));
        }
    }
}

/// 管理轮询与短信网页共用无凭证终态。
async fn session_result(session: &Session) -> Result<Value, AppError> {
    expire_session(session).await;
    let result = session.result.lock().await;
    match result.as_ref() {
        Some(Ok(value)) => Ok(value.clone()),
        Some(Err(message)) => Err(AppError::bad_request(message)),
        None => Ok(json!({"status":"pending"})),
    }
}

/// 取消与保存共用终态锁，迟到的上游结果不能继续保存。
async fn cancel_session(session: &Session) {
    let mut result = session.result.lock().await;
    if result.is_none() {
        session.cancel.cancel();
        *result = Some(Err("授权已取消".into()));
    }
}

/// 关闭等待窗口时结束短信会话或释放本机回调服务。
pub async fn cancel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<PollInput>,
) -> Result<Json<Value>, AppError> {
    authorize(&state, &headers)?;
    cancel_authorized(state, input).await
}

/// 已授权调用者取消当前会话，保留成功终态。
pub(crate) async fn cancel_authorized(
    state: AppState,
    input: PollInput,
) -> Result<Json<Value>, AppError> {
    let session = state.auth.sessions::<Sessions>().get(&input).await?;
    cancel_session(&session).await;
    Ok(Json(json!({"status":"cancelled"})))
}

/// 独立登录页不加载管理密钥，不缓存会话，也不能被其他页面嵌入。
pub async fn browser_page(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    state.auth.sessions::<Sessions>().domestic(&id).await?;
    Ok((
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; connect-src 'self'; frame-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
            ),
        ],
        Html(include_str!("../../../web/autoglm-login.html")),
    ))
}

/// 浏览器操作必须来自当前代理同源页面，阻止外站借助本机代理发送短信。
fn browser_origin(headers: &HeaderMap) -> Result<(), AppError> {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| reqwest::Url::parse(value).ok());
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok());
    let valid = origin.as_ref().is_some_and(|origin| {
        matches!(origin.scheme(), "http" | "https")
            && origin.username().is_empty()
            && origin.password().is_none()
            && origin.path() == "/"
            && origin.query().is_none()
            && origin.fragment().is_none()
            && host.is_some_and(|host| {
                reqwest::Url::parse(&format!("{}://{host}", origin.scheme()))
                    .is_ok_and(|url| url.origin() == origin.origin())
            })
    }) && headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
        == Some("same-origin");
    if !valid {
        return Err(AppError::bad_request("请从当前代理的登录页面操作"));
    }
    Ok(())
}

/// 每次网络请求前后检查会话和服务快照，变更或取消后不再接受结果。
async fn active_domestic(state: &AppState, session: &Session) -> Result<(), AppError> {
    if session_result(session).await?["status"] != "pending" {
        return Err(AppError::bad_request("账号已保存，请回到控制台"));
    }
    let domestic = session.domestic.as_ref().expect("domestic session");
    let current = state.store.get_backend(&session.backend_id).await?;
    if !current.is_some_and(|current| {
        current.enabled
            && current.kind == domestic.backend.kind
            && current.base_url == domestic.backend.base_url
            && current.options == domestic.backend.options
    }) {
        cancel_session(session).await;
        return Err(AppError::bad_request(
            "服务已删除、停用或登录配置已变更，请重新开始",
        ));
    }
    Ok(())
}

/// 短信网络操作受会话取消和截止时间约束，迟到的请求不解锁登录。
async fn domestic_request<T>(
    state: &AppState,
    session: &Session,
    request: impl Future<Output = Result<T, AppError>>,
) -> Result<T, AppError> {
    active_domestic(state, session).await?;
    let result = tokio::select! {
        biased;
        _ = session.cancel.cancelled() => Err(AppError::bad_request("授权已取消")),
        _ = tokio::time::sleep_until(session.deadline) => Err(AppError::bad_request("授权已超时，请重新开始")),
        result = request => result,
    };
    active_domestic(state, session).await?;
    result
}

/// 只返回公开配置；加载页面不会触发发码。
pub async fn browser_captcha_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    browser_origin(&headers)?;
    let session = state.auth.sessions::<Sessions>().domestic(&id).await?;
    let domestic = session.domestic.as_ref().expect("domestic session");
    let mut sms = domestic.sms.lock().await;
    sms.captcha_required = None;
    let config = domestic_request(
        &state,
        &session,
        domestic.client.sms_captcha_config(&domestic.device_id),
    )
    .await?;
    sms.captcha_required = config["enabled"].as_bool();
    Ok(Json(json!({"config":config})))
}

/// 发码和登录只接受手机号文本，验证码转换为官方客户端要求的数值。
#[derive(Deserialize)]
pub struct SmsInput {
    phone: String,
    #[serde(default)]
    code: String,
    #[serde(default)]
    ali_captcha_verify_param: Option<String>,
}

/// 仅接受国内十一位手机号，避免无效请求进入短信通道。
fn validate_phone(phone: &str) -> Result<(), AppError> {
    if phone.len() != 11 || !phone.starts_with('1') || !phone.bytes().all(|c| c.is_ascii_digit()) {
        return Err(AppError::bad_request("请输入 11 位国内手机号"));
    }
    Ok(())
}

/// 发码串行且间隔至少六十秒；验证回执由上游确认，成功后绑定手机号。
pub async fn browser_send_code(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(input): Json<SmsInput>,
) -> Result<Json<Value>, AppError> {
    browser_origin(&headers)?;
    validate_phone(&input.phone)?;
    let session = state.auth.sessions::<Sessions>().domestic(&id).await?;
    let domestic = session.domestic.as_ref().expect("domestic session");
    let mut sms = domestic.sms.lock().await;
    active_domestic(&state, &session).await?;
    if sms
        .sent_at
        .is_some_and(|sent| sent.elapsed() < Duration::from_secs(60))
    {
        return Err(AppError::bad_request("验证码已发送，请等待 60 秒后重试"));
    }
    let required = sms
        .captcha_required
        .ok_or_else(|| AppError::bad_request("请先加载短信安全验证配置"))?;
    let proof = input.ali_captcha_verify_param.as_deref();
    if proof.is_some_and(|proof| proof.trim().is_empty() || proof.len() > 65536)
        || required && proof.is_none()
    {
        return Err(AppError::bad_request("请先完成短信安全验证"));
    }
    // 开始新发码时解除上次绑定，上游失败不能沿用旧手机号提交。
    sms.phone = None;
    domestic_request(
        &state,
        &session,
        domestic
            .client
            .send_browser_code(&domestic.device_id, &input.phone, proof),
    )
    .await?;
    sms.phone = Some(input.phone);
    sms.sent_at = Some(Instant::now());
    Ok(Json(json!({"status":"sent","retry_after":60})))
}

/// 成功登录只兑换和保存一次；取消与最终写入共用终态锁。
pub async fn browser_login(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(input): Json<SmsInput>,
) -> Result<Json<Value>, AppError> {
    browser_origin(&headers)?;
    validate_phone(&input.phone)?;
    if !(4..=8).contains(&input.code.len()) || !input.code.bytes().all(|c| c.is_ascii_digit()) {
        return Err(AppError::bad_request("请输入 4 至 8 位短信验证码"));
    }
    let session = state.auth.sessions::<Sessions>().domestic(&id).await?;
    let domestic = session.domestic.as_ref().expect("domestic session");
    let sms = domestic.sms.lock().await;
    let current = session_result(&session).await?;
    if current["status"] == "done" {
        return Ok(Json(current));
    }
    if sms.phone.as_deref() != Some(&input.phone) {
        return Err(AppError::bad_request("请先为当前手机号发送验证码"));
    }
    let credential = domestic_request(
        &state,
        &session,
        domestic.client.browser_login(
            &domestic.device_id,
            &input.phone,
            input.code.parse().expect("validated numeric code"),
        ),
    )
    .await?;
    let name = format!("AutoGLM · {}****{}", &input.phone[..3], &input.phone[7..]);
    complete_login(&state, &session, &domestic.backend, credential, Some(name))
        .await
        .map(Json)
}

/// 网页轮询只读取自身终态，待登录时同时检查配置是否仍然有效。
pub async fn browser_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    browser_origin(&headers)?;
    let session = state.auth.sessions::<Sessions>().domestic(&id).await?;
    let result = session_result(&session).await?;
    if result["status"] == "pending" {
        active_domestic(&state, &session).await?;
    }
    Ok(Json(result))
}

/// 短信网页取消自身会话，不能影响其他窗口或已保存账号。
pub async fn browser_cancel(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    browser_origin(&headers)?;
    let session = state.auth.sessions::<Sessions>().domestic(&id).await?;
    cancel_session(&session).await;
    let result = session.result.lock().await;
    Ok(Json(match result.as_ref() {
        Some(Ok(value)) => value.clone(),
        _ => json!({"status":"cancelled"}),
    }))
}
