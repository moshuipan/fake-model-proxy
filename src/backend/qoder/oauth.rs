//! Qoder 设备码登录。
//!
//! 流程与官方客户端一致：生成 PKCE 参数与 nonce → 打开授权页 → 轮询
//! `deviceToken/poll`。404 表示用户尚未授权，200 返回 device token 与
//! refresh token；随后用 device token 拉取用户信息，组装账号凭证。

use super::QoderRegion;
use super::cosy;
use crate::core::error::AppError;
use serde_json::Value;
use std::time::{Duration, Instant};

/// 官方客户端 client_id。
const OAUTH_CLIENT_ID: &str = "e883ade2-e6e3-4d6d-adf7-f92ceff5fdcb";

/// 待授权会话有效期。超时后必须重新开始。
const LOGIN_TTL: Duration = Duration::from_secs(600);

/// 单次授权的协议状态，由所属应用会话注册表持有。
pub struct PendingLogin {
    region: QoderRegion,
    nonce: String,
    verifier: String,
    created_at: Instant,
}

/// 登录启动结果。
pub struct LoginStart {
    pub login_url: String,
    pub expires_in: u64,
}

/// 登录成功后的身份与令牌。
pub struct LoginResult {
    pub uid: String,
    pub name: String,
    pub user_type: String,
    pub organization_id: String,
    pub organization_name: String,
    pub email: String,
    pub device_token: String,
    pub refresh_token: String,
}

/// 登录流程使用的 HTTP 客户端。
fn client() -> Result<reqwest::Client, AppError> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|error| AppError::Internal {
            message: format!("build http client failed: {error}"),
        })
}

/// 启动登录：生成 PKCE 与 nonce，返回浏览器授权地址。
pub fn start(region: QoderRegion) -> (LoginStart, PendingLogin) {
    let pkce = cosy::pkce();
    let nonce = cosy::random_hex(32);
    let login_url = format!(
        "{}?nonce={}&challenge={}&challenge_method=S256&client_id={}",
        region.device_login_base(),
        nonce,
        pkce.challenge,
        OAUTH_CLIENT_ID
    );
    let pending = PendingLogin {
        region,
        nonce,
        verifier: pkce.verifier,
        created_at: Instant::now(),
    };
    (
        LoginStart {
            login_url,
            expires_in: LOGIN_TTL.as_secs(),
        },
        pending,
    )
}

/// 轮询授权结果。
///
/// 返回 `Ok(None)` 表示用户尚未在浏览器完成授权；成功时返回身份与令牌。
pub async fn poll(pending: &PendingLogin) -> Result<Option<LoginResult>, AppError> {
    if pending.created_at.elapsed() > LOGIN_TTL {
        return Err(AppError::bad_request(
            "qoder login expired; start a new one",
        ));
    }
    let (nonce, verifier, region) = (&pending.nonce, &pending.verifier, pending.region);

    let url = format!(
        "{}?nonce={}&verifier={}&challenge_method=S256",
        region.poll_endpoint(),
        nonce,
        verifier
    );
    let response = client()?.get(&url).send().await?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(AppError::Upstream(crate::core::UpstreamError::status(
            status.as_u16(),
            body,
        )));
    }
    let parsed: Value = serde_json::from_str(&body).map_err(|error| {
        AppError::Upstream(crate::core::UpstreamError {
            status: None,
            code: "invalid_device_token".to_string(),
            message: format!("invalid device token JSON: {error}"),
            retryable: false,
            execution_unknown: false,
        })
    })?;
    let device_token = parsed
        .get("token")
        .or_else(|| parsed.get("device_token"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            AppError::Upstream(crate::core::UpstreamError {
                status: None,
                code: "missing_device_token".to_string(),
                message: "device token response did not contain token".to_string(),
                retryable: false,
                execution_unknown: false,
            })
        })?
        .to_string();
    let refresh_token = parsed
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let userinfo = fetch_userinfo(region, &device_token).await?;
    Ok(Some(LoginResult {
        uid: first_string(&userinfo, &["id", "userId", "uid"]),
        name: first_string(&userinfo, &["name", "nickname"]),
        user_type: first_string(&userinfo, &["userType", "user_type"]),
        organization_id: first_string(&userinfo, &["organization_id", "organizationId"]),
        organization_name: first_string(&userinfo, &["organization_name", "organizationName"]),
        email: first_string(&userinfo, &["email"]),
        device_token,
        refresh_token,
    }))
}

/// 用 device token 拉取用户信息。
async fn fetch_userinfo(region: QoderRegion, device_token: &str) -> Result<Value, AppError> {
    let response = client()?
        .get(region.userinfo_endpoint())
        .header("Authorization", format!("Bearer {device_token}"))
        .header("accept", "application/json")
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(AppError::Upstream(crate::core::UpstreamError::status(
            status.as_u16(),
            body,
        )));
    }
    serde_json::from_str(&body).map_err(|error| {
        AppError::Upstream(crate::core::UpstreamError {
            status: None,
            code: "invalid_userinfo".to_string(),
            message: format!("invalid userinfo JSON: {error}"),
            retryable: false,
            execution_unknown: false,
        })
    })
}

/// 按顺序取第一个非空字符串字段。
fn first_string(value: &Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(found) = value.get(*key).and_then(Value::as_str)
            && !found.trim().is_empty()
        {
            return found.trim().to_string();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 登录地址必须携带 PKCE 与 client_id。
    #[test]
    fn login_url_contains_pkce_params() {
        let (start, _) = start(QoderRegion::Global);
        assert!(
            start
                .login_url
                .starts_with("https://qoder.com/device/selectAccounts?")
        );
        assert!(start.login_url.contains("challenge_method=S256"));
        assert!(start.login_url.contains(OAUTH_CLIENT_ID));
        assert_eq!(start.expires_in, 600);
    }

    /// 已过期状态在访问网络之前拒绝。
    #[tokio::test]
    async fn expired_pending_fails() {
        let (_, mut pending) = start(QoderRegion::Global);
        pending.created_at = Instant::now() - LOGIN_TTL - Duration::from_secs(1);
        assert!(poll(&pending).await.is_err());
    }

    /// 同时发起的授权具有独立随机状态。
    #[test]
    fn independent_login_states() {
        let (_, a) = start(QoderRegion::Global);
        let (_, b) = start(QoderRegion::Global);
        assert_ne!(a.nonce, b.nonce);
        assert_ne!(a.verifier, b.verifier);
    }
}
