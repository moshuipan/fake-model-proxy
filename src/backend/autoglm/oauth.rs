//! 官方 Google / Z.ai 网页授权：本机端口、state 校验及一次性回调。

use super::client::{Client, protocol_error};
use super::credential::{Credential, Region};
use crate::core::AppError;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, oneshot};
use tokio::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

pub const EXPIRES_IN: u64 = 600;
const PORTS: [u16; 4] = [18432, 19654, 19723, 53699];

/// 授权站点和回调路径来自官方客户端，不接受自定义域名。
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Zai,
    Google,
}

impl Provider {
    /// 返回官方接口中的提供商名称。
    pub fn name(self) -> &'static str {
        match self {
            Self::Zai => "zai",
            Self::Google => "google",
        }
    }

    /// 返回精确匹配的官方登录域名。
    fn host(self) -> &'static str {
        match self {
            Self::Zai => "chat.z.ai",
            Self::Google => "accounts.google.com",
        }
    }
}

/// 已占用回调端口的一次授权；不实现 Debug，不输出授权码与 state。
pub struct Flow {
    pub login_url: String,
    pub deadline: Instant,
    pub cancel: CancellationToken,
    client: Client,
    provider: Provider,
    credential: Credential,
    navigate_uri: String,
    expected_state: String,
    listener: TcpListener,
}

impl Flow {
    /// 先占用官方白名单端口，再请求带 state 的可信授权 URL。
    pub async fn begin(
        client: Client,
        provider: Provider,
        device_id: String,
        proof: Option<&str>,
    ) -> Result<Self, AppError> {
        let mut listener = None;
        for port in PORTS {
            if let Ok(bound) = TcpListener::bind(("127.0.0.1", port)).await {
                listener = Some(bound);
                break;
            }
        }
        let listener = listener.ok_or_else(|| {
            AppError::bad_request(
                "AutoGLM 回调端口均被占用，请结束其他授权后重试（18432、19654、19723、53699）",
            )
        })?;
        let port = listener
            .local_addr()
            .map_err(|_| protocol_error("无法读取授权端口"))?
            .port();
        let navigate_uri = format!("http://localhost:{port}/auth/callback-{}", provider.name());
        let credential = Credential {
            device_id,
            region: Some(Region::Global),
            ..Default::default()
        };
        let login_url = client
            .oauth_url(provider, &credential, &navigate_uri, proof)
            .await?;
        let expected_state = validate_url(&login_url, provider)?;
        Ok(Self {
            login_url,
            deadline: Instant::now() + Duration::from_secs(EXPIRES_IN),
            cancel: CancellationToken::new(),
            client,
            provider,
            credential,
            navigate_uri,
            expected_state,
            listener,
        })
    }

    /// 接收一次有效回调并兑换令牌；取消和期限覆盖接收与网络请求。
    pub async fn wait(self) -> Result<Credential, AppError> {
        let (sender, receiver) = oneshot::channel();
        let path = format!("/auth/callback-{}", self.provider.name());
        let expected_state = self.expected_state;
        let callback_state = CallbackState {
            expected_state: expected_state.clone(),
            authority: reqwest::Url::parse(&self.navigate_uri)
                .map_err(|_| protocol_error("授权回调地址无效"))?
                .authority()
                .to_string(),
            sender: Arc::new(Mutex::new(Some(sender))),
        };
        let app = axum::Router::new()
            .route(&path, get(callback))
            .with_state(callback_state);
        let server = axum::serve(self.listener, app);
        // Serve 和兑换在同一作用域：select 结束即丢弃监听器，没有后台端口泄漏。
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(AppError::bad_request("授权已取消")),
            _ = tokio::time::sleep_until(self.deadline) => Err(AppError::bad_request("授权已超时，请重新开始")),
            result = async {
                let code = receiver.await.map_err(|_| protocol_error("授权回调服务已关闭"))??;
                self.client.exchange_oauth(self.provider, &self.credential, &self.navigate_uri,
                    &code, &expected_state).await
            } => result,
            _ = server => Err(protocol_error("AutoGLM 本机回调服务已关闭")),
        }
    }
}

/// 校验 HTTPS、精确域名、无用户信息及端口，同时只接受一个非空 state。
fn validate_url(raw: &str, provider: Provider) -> Result<String, AppError> {
    let url = reqwest::Url::parse(raw).map_err(|_| protocol_error("AutoGLM 授权地址无效"))?;
    if url.scheme() != "https"
        || url.host_str() != Some(provider.host())
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || raw
            .split_once("://")
            .and_then(|(_, rest)| rest.split(['/', '?', '#']).next())
            .is_none_or(|authority| !authority.eq_ignore_ascii_case(provider.host()))
    {
        return Err(protocol_error("AutoGLM 授权地址不属于预期官方登录站点"));
    }
    let values: Vec<_> = url
        .query_pairs()
        .filter(|(key, _)| key == "state")
        .collect();
    if values.len() != 1 || values[0].1.trim().is_empty() {
        return Err(protocol_error("AutoGLM 授权地址缺少唯一的 state"));
    }
    Ok(values[0].1.to_string())
}

/// 回调只在进程内传递授权码，页面和管理 API 不返回任何令牌。
#[derive(Clone)]
struct CallbackState {
    expected_state: String,
    authority: String,
    sender: CallbackSender,
}

/// 单次回调传递通道，锁只保护是否已经消费。
type CallbackSender = Arc<Mutex<Option<oneshot::Sender<Result<String, AppError>>>>>;

/// 无效 state 或重复参数不消费会话；有效拒绝授权结束当前流程。
async fn callback(
    State(state): State<CallbackState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let pairs: Vec<_> =
        reqwest::Url::parse(&format!("http://localhost/?{}", query.unwrap_or_default()))
            .map(|url| {
                url.query_pairs()
                    .map(|(key, value)| (key.into_owned(), value.into_owned()))
                    .collect()
            })
            .unwrap_or_default();
    let states: Vec<_> = pairs.iter().filter(|(key, _)| key == "state").collect();
    let codes: Vec<_> = pairs.iter().filter(|(key, _)| key == "code").collect();
    let valid = headers.get("host").and_then(|value| value.to_str().ok()) == Some(&state.authority)
        && states.len() == 1
        && states[0].1 == state.expected_state
        && codes.len() <= 1;
    if !valid {
        return callback_page(StatusCode::BAD_REQUEST, "授权回调无效，请返回控制台重试。");
    }
    let denied = pairs.iter().any(|(key, _)| key == "error")
        || codes.first().is_none_or(|(_, code)| code.trim().is_empty());
    let mut sender = state.sender.lock().await;
    let Some(sender) = sender.take() else {
        return callback_page(StatusCode::CONFLICT, "授权回调已收到，请返回控制台。");
    };
    let result = if denied {
        Err(AppError::bad_request("未完成 AutoGLM 授权，请重新开始"))
    } else {
        Ok(codes[0].1.clone())
    };
    let _ = sender.send(result);
    callback_page(
        StatusCode::OK,
        if denied {
            "授权未完成，请返回控制台。"
        } else {
            "已收到授权，请返回控制台查看账号保存结果。"
        },
    )
}

/// 展示固定的本机回调页，防止浏览器缓存包含授权码的地址。
fn callback_page(status: StatusCode, message: &str) -> Response {
    (
        status,
        [("Cache-Control", "no-store"), ("Referrer-Policy", "no-referrer"), ("X-Content-Type-Options", "nosniff")],
        Html(format!("<!doctype html><html lang=\"zh-CN\"><meta charset=\"utf-8\"><title>AutoGLM 授权</title><body><p>{message}</p></body></html>")),
    ).into_response()
}
