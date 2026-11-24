//! 官方浏览器授权流程：验证身份后创建模型密钥，取消或配置变更时拒绝迟到保存。

use super::{browser::Browser, cookie_header_for};
use crate::{
    accounts::Account,
    backend::monkeycode::{credential::Credential, profile},
    core::AppError,
    state::AppState,
    store::BackendRecord,
};
use axum::{Json, extract::State, http::HeaderMap};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{sync::Mutex, time::Instant};
use tokio_util::sync::CancellationToken;

const LIFETIME: Duration = Duration::from_secs(600);
const KEY_PATH: &str = "/api/v1/users/ohmyagent/api-keys";

/// 会话只保存无秘密的终态；Cookie、密钥和调试地址只存在于后台工作任务。
#[derive(Default)]
pub struct Sessions {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

#[async_trait::async_trait]
impl crate::plugins::auth::SessionStore for Sessions {
    /// 停用服务时结束临时浏览器会话。
    async fn cancel_service(&self, id: &str) {
        self.cancel_backend(id).await;
    }
}

struct Session {
    backend_id: String,
    deadline: Instant,
    cancel: CancellationToken,
    result: Mutex<Option<Result<Value, String>>>,
}

/// 开始、轮询和取消均携带服务 ID，后两者还需随机会话 ID。
#[derive(Deserialize)]
pub struct Input {
    backend_id: String,
    #[serde(default)]
    session_id: String,
}

impl Sessions {
    /// 限制同时打开的登录窗口，过期会话的后台任务会关闭其独占浏览器。
    async fn insert(&self, backend_id: &str) -> Result<(String, Arc<Session>), AppError> {
        let mut sessions = self.sessions.lock().await;
        sessions.retain(|_, session| session.deadline > Instant::now());
        if sessions
            .values()
            .filter(|session| !session.cancel.is_cancelled())
            .count()
            >= 8
        {
            return Err(AppError::bad_request(
                "登录窗口过多，请关闭未完成的登录后重试",
            ));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let session = Arc::new(Session {
            backend_id: backend_id.into(),
            deadline: Instant::now() + LIFETIME,
            cancel: CancellationToken::new(),
            result: Mutex::new(None),
        });
        sessions.insert(id.clone(), session.clone());
        Ok((id, session))
    }

    /// 服务和会话必须同时匹配，不能领取其他服务的登录结果。
    async fn get(&self, input: &Input) -> Result<Arc<Session>, AppError> {
        self.sessions
            .lock()
            .await
            .get(&input.session_id)
            .filter(|session| session.backend_id == input.backend_id)
            .cloned()
            .ok_or_else(|| AppError::bad_request("MonkeyCode 登录会话不存在，请重新开始"))
    }

    /// 停用、换区域或删除服务时取消未完成登录。
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

/// 在代理本机创建官方登录窗口；返回值不包含 Cookie、模型密钥或调试端口。
pub async fn start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Input>,
) -> Result<Json<Value>, AppError> {
    crate::admin::authorize(&state, &headers)?;
    start_authorized(state, input).await
}

/// 统一操作启动隔离的本机登录窗口。
pub(crate) async fn start_authorized(
    state: AppState,
    input: Input,
) -> Result<Json<Value>, AppError> {
    if !state.config.is_loopback() {
        return Err(AppError::bad_request(
            "MonkeyCode 登录需要代理监听本地地址，并在代理电脑安装 Chrome",
        ));
    }
    let backend = state
        .store
        .get_backend(&input.backend_id)
        .await?
        .filter(|backend| backend.enabled && backend.kind == "monkeycode")
        .ok_or_else(|| AppError::bad_request("请先保存并启用 MonkeyCode 服务"))?;
    let (server, _) = profile(&backend)?;
    let (session_id, session) = state
        .auth
        .sessions::<Sessions>()
        .insert(&backend.id)
        .await?;
    let browser = match Browser::launch(&state.config.data_dir, &format!("{server}/login")).await {
        Ok(browser) => browser,
        Err(error) => {
            session.cancel.cancel();
            *session.result.lock().await = Some(Err(error.message()));
            return Err(error);
        }
    };
    tokio::spawn(async move {
        run(state, backend, session, browser).await;
    });
    Ok(Json(json!({
        "kind":"local_browser","session_id":session_id,"expires_in":LIFETIME.as_secs(),
        "automatic_account_save":true,"browser_opened":true,
    })))
}

/// 查询终态只读取内存结果，重复轮询不会重复创建模型密钥。
pub async fn poll(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Input>,
) -> Result<Json<Value>, AppError> {
    crate::admin::authorize(&state, &headers)?;
    poll_authorized(state, input).await
}

/// 查询受服务归属约束的授权会话。
pub(crate) async fn poll_authorized(
    state: AppState,
    input: Input,
) -> Result<Json<Value>, AppError> {
    let session = state.auth.sessions::<Sessions>().get(&input).await?;
    let mut terminal = session.result.lock().await;
    if terminal.is_none() && Instant::now() >= session.deadline {
        session.cancel.cancel();
        *terminal = Some(Err("MonkeyCode 登录已过期，请重新开始".into()));
    }
    match terminal.as_ref() {
        Some(Ok(value)) => Ok(Json(value.clone())),
        Some(Err(message)) => Err(AppError::bad_request(message)),
        None => Ok(Json(json!({"status":"pending"}))),
    }
}

/// 管理鉴权和会话归属验证后，立即取消等待。
pub async fn cancel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Input>,
) -> Result<Json<Value>, AppError> {
    crate::admin::authorize(&state, &headers)?;
    cancel_authorized(state, input).await
}

/// 已授权调用者取消当前会话，保留成功终态。
pub(crate) async fn cancel_authorized(
    state: AppState,
    input: Input,
) -> Result<Json<Value>, AppError> {
    let session = state.auth.sessions::<Sessions>().get(&input).await?;
    cancel_session(&session).await;
    Ok(Json(json!({"status":"cancelled"})))
}

/// 取消只修改未完成终态，已保存账号不会因关闭等待框被撤销。
async fn cancel_session(session: &Session) {
    let mut terminal = session.result.lock().await;
    if terminal.is_none() {
        session.cancel.cancel();
        *terminal = Some(Err("MonkeyCode 登录已取消".into()));
    }
}

/// 登录窗口提供 Cookie；生产实现使用临时 Chrome，离线流程可注入独立窗口。
#[async_trait::async_trait]
trait LoginWindow: Send {
    async fn cookies(&mut self, server: &str) -> Result<Value, AppError>;
    async fn close(self);
}

#[async_trait::async_trait]
impl LoginWindow for Browser {
    /// 从本会话窗口读取限定域的 HttpOnly Cookie。
    async fn cookies(&mut self, server: &str) -> Result<Value, AppError> {
        Browser::cookies(self, server).await
    }
    /// 登录结束时释放独占窗口及临时目录。
    async fn close(self) {
        Browser::close(self).await;
    }
}

/// 无重定向、有限超时的官方账号客户端，错误信息不引用远端正文。
fn client() -> Result<reqwest::Client, AppError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(20))
        .user_agent("FakeModelProxy/0.1")
        .build()
        .map_err(|_| AppError::bad_request("无法初始化 MonkeyCode 登录"))
}

/// 持续核实官方身份；暂时网络失败和未登录都继续等待，不将 Cookie 指纹锁死。
async fn identity<W: LoginWindow>(
    session: &Session,
    window: &mut W,
    client: &reqwest::Client,
    server: &str,
) -> Result<(Value, Value), AppError> {
    loop {
        let checked = async {
            let cookies = window.cookies(server).await?;
            if let Ok(cookie) = cookie_header_for(&cookies, server, "/api/v1/users/status")
                && let Ok(response) = client
                    .get(format!("{server}/api/v1/users/status"))
                    .header(reqwest::header::COOKIE, cookie)
                    .send()
                    .await
                && response.status().is_success()
                && let Ok(body) = response.json::<Value>().await
                && body["code"].as_i64() == Some(0)
                && body["data"]["user"]["id"]
                    .as_str()
                    .is_some_and(|id| !id.trim().is_empty())
            {
                return Ok(Some((body["data"]["user"].clone(), cookies)));
            }
            Ok::<_, AppError>(None)
        };
        let result = tokio::select! {
            _ = session.cancel.cancelled() => return Err(AppError::bad_request("MonkeyCode 登录已取消")),
            _ = tokio::time::sleep_until(session.deadline) => return Err(AppError::bad_request("MonkeyCode 登录已过期")),
            result = checked => result?,
        };
        if let Some(identity) = result {
            return Ok(identity);
        }
        tokio::select! {
            _ = session.cancel.cancelled() => return Err(AppError::bad_request("MonkeyCode 登录已取消")),
            _ = tokio::time::sleep_until(session.deadline) => return Err(AppError::bad_request("MonkeyCode 登录已过期")),
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
    }
}

/// 撤销本次已经创建但未保存的模型密钥；路径由远端 ID 编码，响应不写日志。
async fn revoke(client: &reqwest::Client, server: &str, cookie: &str, key_id: &str) {
    let Ok(mut url) = reqwest::Url::parse(&format!("{server}{KEY_PATH}/")) else {
        return;
    };
    let Ok(mut segments) = url.path_segments_mut() else {
        return;
    };
    segments.pop_if_empty().push(key_id);
    drop(segments);
    let _ = client
        .delete(url)
        .header(reqwest::header::COOKIE, cookie)
        .send()
        .await;
}

/// 一次创建模型密钥；不在取消信号中中断非幂等 POST，以便撤销迟到返回的密钥。
async fn create_key(
    client: &reqwest::Client,
    server: &str,
    cookie: &str,
) -> Result<(Credential, String), AppError> {
    let response = client
        .post(format!("{server}{KEY_PATH}"))
        .header(reqwest::header::COOKIE, cookie)
        .send()
        .await
        .map_err(|_| AppError::bad_request("模型密钥创建未取得确认，请重新登录"))?;
    if !response.status().is_success() {
        return Err(AppError::bad_request(format!(
            "模型密钥创建失败（HTTP {}），请重新登录",
            response.status().as_u16()
        )));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|_| AppError::bad_request("模型密钥接口返回无效 JSON"))?;
    let data = &body["data"];
    let id = data["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_owned);
    let credential = if body["code"].as_i64() == Some(0) && id.is_some() {
        Credential::parse(&data.to_string())
    } else {
        Err(AppError::bad_request("模型密钥接口未确认有效密钥"))
    };
    match credential {
        Ok(credential) => Ok((credential, id.unwrap_or_default())),
        Err(error) => {
            if let Some(id) = id {
                revoke(client, server, cookie, &id).await;
            }
            Err(error)
        }
    }
}

/// 以官方用户 ID 稳定更新账号；重新登录优先复用已有密钥，只更新登录态。
async fn save_identity(
    state: &AppState,
    backend: &BackendRecord,
    session: &Session,
    client: &reqwest::Client,
    user: Value,
    cookies: Value,
) -> Result<Value, AppError> {
    let (server, gateway) = profile(backend)?;
    let fingerprint = Sha256::digest(user["id"].as_str().unwrap_or_default().as_bytes())
        .iter()
        .take(12)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let id = format!("monkeycode-user-{}-{fingerprint}", backend.id);
    let existing = state.store.get_account(&id).await?;
    let name = existing
        .as_ref()
        .map(|account| account.name.clone())
        .unwrap_or_else(|| {
            format!(
                "MonkeyCode · {}",
                user["name"]
                    .as_str()
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or("账号")
                    .chars()
                    .take(80)
                    .collect::<String>()
            )
        });
    let key_cookie = cookie_header_for(&cookies, &server, KEY_PATH)?;
    let model_cookie = cookie_header_for(&cookies, &server, "/api/v1/users/models")?;
    let reused = existing
        .as_ref()
        .and_then(|account| Credential::parse(&account.credential).ok())
        .filter(|credential| credential.validate_origin(&server, &gateway).is_ok());
    let (mut credential, created) = if let Some(credential) = reused {
        (credential, None)
    } else {
        let (credential, id) = create_key(client, &server, &key_cookie).await?;
        (credential, Some(id))
    };
    credential.server = server.clone();
    credential.base_url = gateway;
    credential.cookie = model_cookie;
    let account = Account {
        id: id.clone(),
        backend_id: backend.id.clone(),
        name: name.clone(),
        credential: credential.serialize()?,
        enabled: true,
    };
    let catalog = async {
        use crate::backend::Backend;
        let provider =
            crate::backend::monkeycode::MonkeyCodeBackend::new(backend, vec![account.clone()])?;
        let models = provider.list_models().await?;
        if models.is_empty() {
            return Err(AppError::bad_request(
                "模型目录为空，请检查会员权限后重试同步",
            ));
        }
        Ok(models)
    };
    let catalog = tokio::select! {
        _ = session.cancel.cancelled() => Err(AppError::bad_request("MonkeyCode 登录已取消")),
        _ = tokio::time::sleep_until(session.deadline) => Err(AppError::bad_request("MonkeyCode 登录已过期")),
        catalog = catalog => catalog,
    };
    let outcome = async {
        let config_service = state.config_service();
        let guard = tokio::select! {
            _ = session.cancel.cancelled() => return Err(AppError::bad_request("MonkeyCode 登录已取消")),
            _ = tokio::time::sleep_until(session.deadline) => return Err(AppError::bad_request("MonkeyCode 登录已过期")),
            guard = config_service.lock_write() => guard,
        };
        let mut terminal = session.result.lock().await;
        if terminal.is_some() || session.cancel.is_cancelled() || Instant::now() >= session.deadline {
            return Err(AppError::bad_request("MonkeyCode 登录已取消或过期"));
        }
        if !state.config_service().save_login_locked(&guard, &account, backend).await? {
            return Err(AppError::bad_request("服务已停用或配置已变更，请重新登录"));
        }
        drop(guard);
        let synced = match catalog {
            Ok(models) => state.config_service().sync_models(backend, &models).await.map(|_| models.len()),
            Err(error) => Err(error),
        };
        // 保存已成功，此后的刷新错误不撤销有效密钥；发布终态前先更新后端快照。
        let _ = state.store.abort_credential_refresh(&id).await;
        let reload = state.reload_backends().await;
        let mut value = json!({"status":"done","account_id":id,"name":name});
        match synced {
            Ok(count) => { value["models_synced"] = json!(true); value["model_count"] = json!(count); }
            Err(error) => { value["models_synced"] = json!(false); value["sync_error"] = json!(error.message()); }
        }
        let value = if let Err(error) = reload {
            let mut value = value;
            value["reload_error"] = json!(error.message());
            value
        } else { value };
        *terminal = Some(Ok(value.clone()));
        Ok(value)
    }.await;
    if outcome.is_err()
        && let Some(id) = created
    {
        revoke(client, &server, &key_cookie, &id).await;
    }
    outcome
}

/// 后台独占登录流程；所有路径最终关闭窗口，终态查询不再触发账号接口。
async fn run<W: LoginWindow>(
    state: AppState,
    backend: BackendRecord,
    session: Arc<Session>,
    mut window: W,
) {
    let result = async {
        let (server, _) = profile(&backend)?;
        let client = client()?;
        let (user, cookies) = identity(&session, &mut window, &client, &server).await?;
        save_identity(&state, &backend, &session, &client, user, cookies).await
    }
    .await;
    window.close().await;
    let mut terminal = session.result.lock().await;
    if terminal.is_none() {
        *terminal = Some(result.map_err(|error| error.message()));
    }
    session.cancel.cancel();
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Body,
        extract::Path,
        http::{Request, StatusCode},
        routing::{delete, get, post},
    };
    use http_body_util::BodyExt;
    use std::sync::{
        RwLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use tokio::sync::Notify;
    use tower::ServiceExt;

    #[derive(Default)]
    struct Mock {
        failures: AtomicUsize,
        creates: AtomicUsize,
        revokes: AtomicUsize,
        hold: AtomicBool,
        catalog_fails: AtomicBool,
        started: Notify,
        release: Notify,
        closed: AtomicUsize,
    }

    struct Window(Arc<Mock>);

    #[async_trait::async_trait]
    impl LoginWindow for Window {
        /// 注入官方域 Cookie，后台仍需向假上游验证身份和换取密钥。
        async fn cookies(&mut self, server: &str) -> Result<Value, AppError> {
            Ok(json!([{"name":"session","value":"private-cookie",
                "domain":reqwest::Url::parse(server).unwrap().host_str(),"path":"/","host_only":true}]))
        }
        /// 记录所有成功、取消和失败路径释放独占窗口。
        async fn close(self) {
            self.0.closed.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// 偶发错误不能使最后一套有效 Cookie 永久停留在等待状态。
    async fn status(
        State(mock): State<Arc<Mock>>,
        headers: HeaderMap,
    ) -> (StatusCode, Json<Value>) {
        assert_eq!(headers["cookie"], "session=private-cookie");
        if mock
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                count.checked_sub(1)
            })
            .is_ok()
        {
            return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"code":500})));
        }
        (
            StatusCode::OK,
            Json(json!({"code":0,"data":{"user":{"id":"user-one","name":"离线账号"}}})),
        )
    }

    /// 可阻塞非幂等换票响应，复现取消发生在请求已经发出之后。
    async fn key(State(mock): State<Arc<Mock>>, headers: HeaderMap) -> Json<Value> {
        assert_eq!(headers["cookie"], "session=private-cookie");
        assert!(!headers.contains_key("authorization"));
        mock.started.notify_one();
        if mock.hold.load(Ordering::SeqCst) {
            mock.release.notified().await;
        }
        mock.creates.fetch_add(1, Ordering::SeqCst);
        Json(
            json!({"code":0,"data":{"id":"key-one","api_key":"private-api-key","signing_secret":"private-signing-secret"}}),
        )
    }

    /// 仅撤销未被保存的本次密钥，Cookie 和密钥 ID 都留在后台。
    async fn remove_key(
        State(mock): State<Arc<Mock>>,
        Path(id): Path<String>,
        headers: HeaderMap,
    ) -> Json<Value> {
        assert_eq!(id, "key-one");
        assert_eq!(headers["cookie"], "session=private-cookie");
        mock.revokes.fetch_add(1, Ordering::SeqCst);
        Json(json!({"code":0}))
    }

    /// 构造真正 HTTP 假官网，FMP 的管理路由和 SQLite 存储使用生产实现。
    async fn setup() -> (AppState, Arc<Mock>, tokio::task::JoinHandle<()>) {
        let mock = Arc::new(Mock::default());
        let app = Router::new()
            .route("/api/v1/users/status", get(status))
            .route(KEY_PATH, post(key))
            .route(&format!("{KEY_PATH}/{{id}}"), delete(remove_key))
            .route(
                "/api/v1/users/models",
                get(|State(mock): State<Arc<Mock>>| async move {
                    if mock.catalog_fails.load(Ordering::SeqCst) {
                        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"code":500})));
                    }
                    (
                        StatusCode::OK,
                        Json(json!({"code":0,"data":{"models":[
                {"model":"monkeycode-basic/test","interface_type":"openai_chat"}
            ],"page":{"has_next_page":false}}})),
                    )
                }),
            )
            .route(
                "/api/v1/users/subscription",
                get(|| async { Json(json!({"code":0,"data":{"plan":"basic"}})) }),
            )
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let job = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let store = crate::store::Store::open(std::path::Path::new(":memory:"))
            .await
            .unwrap();
        store
            .upsert_backend(&BackendRecord {
                id: "mc".into(),
                name: "MonkeyCode".into(),
                kind: "monkeycode".into(),
                base_url: format!("{base}/v1"),
                ..Default::default()
            })
            .await
            .unwrap();
        let state = AppState::new(
            Arc::new(crate::config::Config::default()),
            store.clone(),
            Arc::new(crate::backend::Registry::new()),
            Arc::new(crate::records::RecordLog::default()),
            Arc::new(RwLock::new(crate::tasks::TaskRegistry::new())),
            Arc::new(crate::routing::Router::new()),
            Arc::new(crate::usage::UsageLedger::new(store)),
        );
        state.reload_backends().await.unwrap();
        (state, mock, job)
    }

    /// 只替换窗口采集器，后台任务和可查询会话仍使用正式实现。
    async fn launch(state: &AppState, mock: &Arc<Mock>) -> (String, tokio::task::JoinHandle<()>) {
        let backend = state.store.get_backend("mc").await.unwrap().unwrap();
        let (id, session) = state
            .auth
            .sessions::<Sessions>()
            .insert("mc")
            .await
            .unwrap();
        let job = tokio::spawn(run(state.clone(), backend, session, Window(mock.clone())));
        (id, job)
    }

    /// 从公开管理路由收集脱敏 JSON，避免直接读取内存终态代替接口测试。
    async fn send(state: &AppState, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let response = crate::build_router(state.clone())
            .oneshot(
                Request::builder()
                    .method(if body.is_some() { "POST" } else { "GET" })
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        body.map(|body| body.to_string()).unwrap_or_default(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    /// 授权完成、重复轮询和再次登录只维护一个账号和一份模型密钥。
    #[tokio::test]
    async fn login_saves_account_once_and_public_poll_never_exposes_secrets() {
        let (state, mock, upstream) = setup().await;
        mock.failures.store(1, Ordering::SeqCst);
        let (id, job) = launch(&state, &mock).await;
        let input = json!({"backend_id":"mc","session_id":id});
        let (_, pending) = send(&state, "/api/oauth/monkeycode/poll", Some(input.clone())).await;
        assert_eq!(pending["status"], "pending");
        tokio::time::timeout(Duration::from_secs(10), job)
            .await
            .unwrap()
            .unwrap();
        for _ in 0..3 {
            let (status, result) =
                send(&state, "/api/oauth/monkeycode/poll", Some(input.clone())).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(result["status"], "done");
            assert_eq!(result["models_synced"], true);
            assert_eq!(result["model_count"], 1);
            for secret in [
                "private-cookie",
                "private-api-key",
                "private-signing-secret",
                "devtools",
            ] {
                assert!(!result.to_string().contains(secret));
            }
        }
        let (_, accounts) = send(&state, "/api/accounts", None).await;
        assert_eq!(accounts["accounts"].as_array().unwrap().len(), 1);
        assert!(!accounts.to_string().contains("private-"));
        assert!(
            state
                .store
                .get_backend("mc")
                .await
                .unwrap()
                .unwrap()
                .models
                .contains("monkeycode-basic/test")
        );
        let (status, models) = send(&state, "/api/backends/mc/models/sync", Some(json!({}))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(models["models"][0]["id"], "monkeycode-basic/test");
        assert!(
            state
                .store
                .get_backend("mc")
                .await
                .unwrap()
                .unwrap()
                .models
                .contains("monkeycode-basic/test")
        );
        let (_, job) = launch(&state, &mock).await;
        job.await.unwrap();
        assert_eq!(mock.creates.load(Ordering::SeqCst), 1);
        assert_eq!(state.store.list_accounts().await.unwrap().len(), 1);
        assert_eq!(mock.closed.load(Ordering::SeqCst), 2);
        assert_eq!(mock.revokes.load(Ordering::SeqCst), 0);
        upstream.abort();
    }

    /// 目录失败仍保留有效账号与密钥，重试目录不要求重新换票。
    #[tokio::test]
    async fn model_sync_failure_keeps_saved_account_for_retry() {
        let (state, mock, upstream) = setup().await;
        mock.catalog_fails.store(true, Ordering::SeqCst);
        let (id, job) = launch(&state, &mock).await;
        job.await.unwrap();
        let (_, result) = send(
            &state,
            "/api/oauth/monkeycode/poll",
            Some(json!({"backend_id":"mc","session_id":id})),
        )
        .await;
        assert_eq!(result["status"], "done");
        assert_eq!(result["models_synced"], false);
        assert!(result["sync_error"].is_string());
        assert_eq!(state.store.list_accounts().await.unwrap().len(), 1);
        assert_eq!(mock.revokes.load(Ordering::SeqCst), 0);
        mock.catalog_fails.store(false, Ordering::SeqCst);
        state.reload_backends().await.unwrap();
        let (status, _) = send(&state, "/api/backends/mc/models/sync", Some(json!({}))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(mock.creates.load(Ordering::SeqCst), 1);
        upstream.abort();
    }

    /// 取消以及配置变更发生在换票中途，都撤销迟到密钥且不复活账号。
    #[tokio::test]
    async fn cancelled_or_changed_backend_rejects_and_revokes_late_key() {
        for changed in [false, true] {
            let (state, mock, upstream) = setup().await;
            mock.hold.store(true, Ordering::SeqCst);
            let (id, job) = launch(&state, &mock).await;
            mock.started.notified().await;
            if changed {
                // 直接改配置不触发取消，额外验证持久层的条件保存。
                let mut backend = state.store.get_backend("mc").await.unwrap().unwrap();
                backend.options = r#"{"region":"global"}"#.into();
                state.store.upsert_backend(&backend).await.unwrap();
            } else {
                let (status, _) = send(
                    &state,
                    "/api/oauth/monkeycode/cancel",
                    Some(json!({"backend_id":"mc","session_id":id})),
                )
                .await;
                assert_eq!(status, StatusCode::OK);
            }
            mock.release.notify_one();
            tokio::time::timeout(Duration::from_secs(10), job)
                .await
                .unwrap()
                .unwrap();
            assert!(state.store.list_accounts().await.unwrap().is_empty());
            assert_eq!(mock.creates.load(Ordering::SeqCst), 1);
            assert_eq!(mock.revokes.load(Ordering::SeqCst), 1);
            assert_eq!(mock.closed.load(Ordering::SeqCst), 1);
            upstream.abort();
        }
    }

    /// 会话归属和管理鉴权在打开窗口、查询或取消之前验证。
    #[tokio::test]
    async fn login_requires_backend_ownership_and_management_auth() {
        let (mut state, _, upstream) = setup().await;
        let (id, session) = state
            .auth
            .sessions::<Sessions>()
            .insert("mc")
            .await
            .unwrap();
        let (status, _) = send(
            &state,
            "/api/oauth/monkeycode/cancel",
            Some(json!({"backend_id":"other","session_id":id})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(!session.cancel.is_cancelled());
        state.config = Arc::new(crate::config::Config {
            admin_key: "test-admin".into(),
            ..Default::default()
        });
        for action in ["start", "poll", "cancel"] {
            let (status, result) = send(
                &state,
                &format!("/api/oauth/monkeycode/{action}"),
                Some(json!({"backend_id":"mc","session_id":id})),
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
            assert_eq!(result["error"]["code"], "invalid_admin_key");
        }
        assert!(!session.cancel.is_cancelled());
        upstream.abort();
    }

    /// 截止时间不依赖前端轮询，过期会话不创建账号或密钥且释放窗口。
    #[tokio::test]
    async fn expired_login_closes_window_without_creating_key() {
        let (state, mock, upstream) = setup().await;
        let session = Arc::new(Session {
            backend_id: "mc".into(),
            deadline: Instant::now() - Duration::from_secs(1),
            cancel: CancellationToken::new(),
            result: Mutex::new(None),
        });
        let backend = state.store.get_backend("mc").await.unwrap().unwrap();
        run(
            state.clone(),
            backend,
            session.clone(),
            Window(mock.clone()),
        )
        .await;
        assert!(session.result.lock().await.as_ref().unwrap().is_err());
        assert!(state.store.list_accounts().await.unwrap().is_empty());
        assert_eq!(mock.creates.load(Ordering::SeqCst), 0);
        assert_eq!(mock.closed.load(Ordering::SeqCst), 1);
        upstream.abort();
    }
}
