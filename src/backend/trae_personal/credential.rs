//! TRAE CN 个人账号凭证；只接受个人网页登录获取的 Cloud-IDE-JWT。

use crate::core::{AppError, UpstreamError};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const CLIENT_ID: &str = "ono9krqynydwx5";
pub const AUTH_ORIGIN: &str = "https://api.trae.cn";

/// 持久凭证不实现 Debug，避免令牌进入日志。
#[derive(Clone, Serialize, Deserialize)]
pub struct Credential {
    #[serde(alias = "Token", alias = "token")]
    pub access_token: String,
    #[serde(default, alias = "RefreshToken", alias = "refreshToken")]
    pub refresh_token: String,
    #[serde(default, alias = "UserID", alias = "userId")]
    pub subject: String,
    #[serde(
        default = "default_client",
        alias = "ClientID",
        alias = "clientId",
        alias = "clientID"
    )]
    pub client_id: String,
    #[serde(default)]
    pub expires_at: u64,
}

/// 个人版桌面授权的公开客户端标识。
fn default_client() -> String {
    CLIENT_ID.into()
}

impl Credential {
    /// 从官方授权结果读取个人 JWT，身份与期限以令牌声明为准。
    pub fn parse(raw: &str) -> Result<Self, AppError> {
        let credential: Self = serde_json::from_str(raw)
            .map_err(|_| AppError::bad_request("TRAE 个人版凭证须包含个人 token"))?;
        credential.validate()
    }

    /// 本地声明仅用于一致性检查，令牌有效性由个人版官方网关验证。
    fn validate(mut self) -> Result<Self, AppError> {
        if self.access_token.len() > 32768
            || self.refresh_token.len() > 32768
            || self.client_id != CLIENT_ID
        {
            return Err(AppError::bad_request("TRAE 个人版凭证或客户端标识无效"));
        }
        let parts = self.access_token.split('.').collect::<Vec<_>>();
        if parts.len() != 3 || parts.iter().any(|part| part.is_empty()) {
            return Err(AppError::bad_request("TRAE 个人版需要 Cloud-IDE-JWT"));
        }
        let claims: Value = URL_SAFE_NO_PAD
            .decode(parts[1])
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .ok_or_else(|| AppError::bad_request("TRAE 个人 JWT 格式无效"))?;
        let subject = claims["data"]["id"]
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control))
            .ok_or_else(|| AppError::bad_request("TRAE 个人 JWT 缺少账号身份"))?;
        if !self.subject.is_empty() && self.subject != subject {
            return Err(AppError::bad_request("TRAE 凭证与账号身份不同，请重新登录"));
        }
        self.subject = subject.into();
        self.expires_at = claims["exp"]
            .as_u64()
            .filter(|expiry| *expiry > 0)
            .ok_or_else(|| AppError::bad_request("TRAE 个人 JWT 缺少有效期限"))?;
        Ok(self)
    }

    /// 统一持久格式不含上游主机，账号不能改变令牌发送目标。
    pub fn serialize(&self) -> Result<String, AppError> {
        serde_json::to_string(self).map_err(|_| AppError::bad_request("TRAE 个人凭证无法保存"))
    }

    /// 提前五分钟续期，避免生成中途失效。
    pub fn expires_soon(&self) -> bool {
        self.expires_at <= crate::records::now_seconds().saturating_add(300)
    }

    /// 官方 ExchangeToken 只提交一次；不确定的结果由 Store 围栏阻止重放。
    pub async fn exchange(
        client: &reqwest::Client,
        origin: &str,
        refresh: &str,
        subject: &str,
    ) -> Result<Self, AppError> {
        if refresh.is_empty() {
            return Err(AppError::bad_request("TRAE 缺少续期票据，请重新网页登录"));
        }
        let response = client
            .post(format!("{origin}/cloudide/api/v3/trae/oauth/ExchangeToken"))
            .json(&json!({"ClientID":CLIENT_ID,"RefreshToken":refresh,"ClientSecret":"-","UserID":subject}))
            .timeout(std::time::Duration::from_secs(25))
            .send()
            .await
            .map_err(|_| super::error(None, "TRAE 续期网络失败，结果未知，请重新登录", true))?;
        if matches!(response.status().as_u16(), 400 | 401 | 403) {
            return Err(rejected());
        }
        if !response.status().is_success() {
            return Err(super::error(
                Some(response.status().as_u16()),
                "TRAE 续期失败，结果未知",
                true,
            ));
        }
        let value: Value = response
            .json()
            .await
            .map_err(|_| super::error(None, "TRAE 续期响应格式无效，结果未知", true))?;
        if !value["ResponseMetadata"]["Error"].is_null() {
            return Err(rejected());
        }
        let result = &value["Result"];
        let mut credential: Self = serde_json::from_value(result.clone())
            .map_err(|_| super::error(None, "TRAE 续期响应缺少令牌，结果未知", true))?;
        if credential.refresh_token.is_empty() {
            credential.refresh_token = refresh.into();
        }
        credential = credential
            .validate()
            .map_err(|_| super::error(None, "TRAE 续期令牌无效，结果未知", true))?;
        if !subject.is_empty() && credential.subject != subject {
            return Err(super::error(
                None,
                "TRAE 续期账号发生变化，请重新登录",
                true,
            ));
        }
        Ok(credential)
    }
}

/// 明确拒绝的刷新没有成功换票，可以解除持久围栏。
fn rejected() -> AppError {
    AppError::Upstream(UpstreamError {
        status: Some(401),
        code: "trae_personal_oauth_rejected".into(),
        message: "TRAE 个人续期票据被拒绝，请重新网页登录；已保存的访问令牌仍保留".into(),
        retryable: false,
        execution_unknown: false,
    })
}
