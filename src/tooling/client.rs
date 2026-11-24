//! CLI 和 MCP 共用的无重试管理客户端；凭证不进入日志或错误输出。

use reqwest::{Client, Method, Url};
use serde_json::{Value, json};
use std::time::Duration;

/// 固定退出码与结构化错误，同时服务 CLI 和 MCP。
#[derive(Debug)]
pub struct ClientError {
    pub code: String,
    pub message: String,
    pub exit_code: u8,
    pub status: Option<u16>,
}

impl ClientError {
    /// 本地错误使用固定消息，不拼接原始输入或秘密。
    pub fn new(code: &str, message: &str, exit_code: u8) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            exit_code,
            status: None,
        }
    }
    /// 输出稳定机器字段，不附加请求正文和地址。
    pub fn value(&self) -> Value {
        json!({"error":{"code":self.code,"message":self.message,"http_status":self.status}})
    }
}

pub struct ApiClient {
    client: Client,
    base: Url,
    key: Option<String>,
}

impl ApiClient {
    /// 禁止 URL 内嵌凭证和重定向，避免认证信息发送到其他服务。
    pub fn new(base: &str, key: Option<String>) -> Result<Self, ClientError> {
        let base = Url::parse(base)
            .map_err(|_| ClientError::new("usage", "服务器地址必须是 HTTP(S) URL", 2))?;
        if !matches!(base.scheme(), "http" | "https")
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(ClientError::new(
                "usage",
                "服务器地址不能包含凭证、查询或片段",
                2,
            ));
        }
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| ClientError::new("client_init", "HTTP 客户端初始化失败", 1))?;
        Ok(Self { client, base, key })
    }

    /// 每个路径段单独编码；请求不重试，动作和 apply 由服务端幂等协调。
    pub async fn request(
        &self,
        method: Method,
        segments: &[&str],
        body: Option<Value>,
    ) -> Result<Value, ClientError> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|_| ClientError::new("usage", "服务器路径无效", 2))?
            .pop_if_empty()
            .extend(["api", "v2"])
            .extend(segments.iter().copied());
        let mut request = self.client.request(method, url);
        if let Some(key) = &self.key {
            request = request.header("x-admin-key", key);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|_| {
            ClientError::new(
                "transport_error",
                "管理服务不可达或请求超时；查询状态后再决定是否重试",
                8,
            )
        })?;
        let status = response.status();
        let value: Value = response.json().await.map_err(|_| {
            ClientError::new(
                "invalid_response",
                "管理服务未返回 JSON；检查服务器地址和版本",
                8,
            )
        })?;
        if !status.is_success() {
            let code = value["error"]["code"].as_str().unwrap_or("http_error");
            // 消息来源为管理 API 的脱敏错误；不将 reqwest 的原始错误输出。
            let message = value["error"]["message"].as_str().unwrap_or("管理请求失败");
            let mut error = ClientError::new(
                code,
                message,
                match status.as_u16() {
                    401 | 403 => 6,
                    409 => 4,
                    400 | 404 | 422 => 3,
                    _ => 8,
                },
            );
            error.status = Some(status.as_u16());
            return Err(error);
        }
        Ok(value)
    }
}

/// 需要用户交接或已失败的业务结果与成功 HTTP 响应分别编码。
pub fn outcome_code(value: &Value) -> u8 {
    if value["valid"] == false {
        return 3;
    }
    match value["status"].as_str() {
        Some("waiting_user") => 5,
        Some("failed" | "unknown" | "cancelled" | "expired") => 7,
        _ => 0,
    }
}
