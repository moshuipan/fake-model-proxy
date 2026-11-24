//! 小浣熊官方 HTTP API；业务认证与聊天共用令牌，不模拟桌面客户端。

use super::credential::Credential;
use crate::core::{AppError, UpstreamError};
use crate::store::BackendRecord;
use reqwest::Method;
use serde_json::{Value, json};
use std::time::Duration;

pub const ORIGIN: &str = "https://xiaohuanxiong.com";
pub const MODELS_PATH: &str = "/api/web/llm/v2/model_catalog";
pub const CHAT_PATH: &str = "/api/web/llm/v2/chat/completions";
pub const BALANCE_PATH: &str = "/api/web/points/v1/balance";
pub const USER_PATH: &str = "/api/web/auth/v1/user_info";
pub const SETTINGS_PATH: &str = "/api/web/office/v3/setting_info";
pub const BILLS_PATH: &str = "/api/web/points/v2/bills";
const EXCHANGE_PATH: &str = "/api/web/auth/v1/login_with_authorization_code";
const REFRESH_PATH: &str = "/api/web/auth/v1/refresh";

/// 固定官方源，离线测试只允许显式本机 HTTP 服务。
#[derive(Clone)]
pub struct Client {
    client: reqwest::Client,
    origin: String,
}

impl Client {
    /// 禁用跳转，防止 Bearer 凭证离开已校验域。
    pub fn new(record: &BackendRecord, timeout: Duration) -> Result<Self, AppError> {
        Ok(Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(timeout)
                .build()?,
            origin: profile(record)?,
        })
    }

    /// 业务调用只添加官方使用的 Bearer 头。
    pub fn request(
        &self,
        method: Method,
        path: &str,
        credential: &Credential,
    ) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.origin))
            .bearer_auth(&credential.access_token)
    }

    /// 授权码只换票一次；验证码和授权码申请由官网登录页完成。
    pub async fn exchange(&self, code: &str) -> Result<Credential, AppError> {
        let data = self
            .post_auth(EXCHANGE_PATH, json!({"authorization_code":code}))
            .await?;
        Credential::from_data(&data, None)
    }

    /// 刷新令牌对，调用者负责锁、配置校验与条件持久化。
    pub async fn refresh(&self, credential: &Credential) -> Result<Credential, AppError> {
        let data = self
            .post_auth(
                REFRESH_PATH,
                json!({"refresh_token":credential.refresh_token}),
            )
            .await?;
        Credential::from_data(&data, Some(credential))
    }

    /// 无 Bearer 的换票接口；不回显含令牌的响应或传输 URL。
    async fn post_auth(&self, path: &str, body: Value) -> Result<Value, AppError> {
        let response = self
            .client
            .post(format!("{}{path}", self.origin))
            .timeout(Duration::from_secs(20))
            .json(&body)
            .send()
            .await
            .map_err(|_| transport_error())?;
        decode(response).await
    }
}

/// 生产仅接受官方根地址，回环测试不允许路径、查询、用户信息和远程域。
pub fn profile(record: &BackendRecord) -> Result<String, AppError> {
    let text = record.base_url.trim().trim_end_matches('/');
    let url = reqwest::Url::parse(text).map_err(|_| AppError::bad_request("小浣熊接口地址无效"))?;
    let official = url.scheme() == "https"
        && url.host_str() == Some("xiaohuanxiong.com")
        && url.port().is_none();
    let loopback = url.scheme() == "http"
        && url
            .host_str()
            .and_then(|host| host.parse::<std::net::IpAddr>().ok())
            .is_some_and(|ip| ip.is_loopback())
        && url.port().is_some();
    if !(official || loopback)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(AppError::bad_request(
            "小浣熊接口地址必须为 https://xiaohuanxiong.com",
        ));
    }
    Ok(text.into())
}

/// 校验 HTTP 和业务状态，再取 data；错误不携带上游响应原文。
pub async fn decode(response: reqwest::Response) -> Result<Value, AppError> {
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(http_error(status));
    }
    let value = response
        .json::<Value>()
        .await
        .map_err(|_| protocol_error("小浣熊未返回有效 JSON"))?;
    if value["code"].as_i64() != Some(0) {
        return Err(protocol_error(if value["code"].as_i64() == Some(200035) {
            "小浣熊授权码已使用或过期，请重新开始授权"
        } else {
            "小浣熊接口未返回成功状态"
        }));
    }
    value
        .get("data")
        .filter(|data| !data.is_null())
        .cloned()
        .ok_or_else(|| protocol_error("小浣熊响应缺少业务数据"))
}

/// 保留状态以支持确定的 401 重试、限流和账号冷却。
pub fn http_error(status: u16) -> AppError {
    AppError::Upstream(UpstreamError {
        status: Some(status),
        code: "raccoon_upstream_error".into(),
        message: format!("小浣熊接口返回 HTTP {status}"),
        retryable: status == 429,
        execution_unknown: matches!(status, 408 | 500..=599),
    })
}

/// 协议错误不泄露上游数据，保持结果未知。
pub fn protocol_error(message: &str) -> AppError {
    AppError::Upstream(UpstreamError {
        status: None,
        code: "raccoon_protocol_error".into(),
        message: message.into(),
        retryable: false,
        execution_unknown: true,
    })
}

/// 隐去传输错误中可能包含的地址和查询参数。
pub fn transport_error() -> AppError {
    AppError::Upstream(UpstreamError::transport("小浣熊网络请求失败"))
}
