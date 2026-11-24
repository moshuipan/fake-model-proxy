//! WorkBuddy 登录流。
//!
//! 与 Qoder 的 PKCE 设备码不同，WorkBuddy 由服务端签发 state：
//! `POST /v2/plugin/auth/state` 拿授权地址，用户在浏览器完成后，
//! `GET /v2/plugin/auth/token` 返回访问令牌，再用令牌读取账号信息。

use super::{WorkBuddyProfile, common_headers};
use crate::core::error::AppError;
use serde_json::Value;
use std::time::{Duration, Instant};

/// 待授权会话有效期。
const LOGIN_TTL: Duration = Duration::from_secs(300);

/// 单次授权的协议状态，由所属应用会话注册表持有。
pub struct Pending {
    profile: WorkBuddyProfile,
    state: String,
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
    pub nickname: String,
    pub account_type: String,
    pub enterprise_id: String,
    pub department: String,
    pub domain: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
    pub refresh_expires_at: u64,
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

/// 启动登录，返回浏览器授权地址。
pub async fn start(profile: WorkBuddyProfile) -> Result<(LoginStart, Pending), AppError> {
    let url = format!(
        "{}/v2/plugin/auth/state?platform={}",
        profile.region.base(),
        profile.login_platform
    );
    let mut builder = client()?
        .post(&url)
        .header("content-type", "application/json")
        .body("{}");
    for (name, value) in super::anonymous_headers(&profile) {
        builder = builder.header(name, value);
    }
    let response = builder.send().await?;
    let status = response.status();
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
            code: "invalid_login_state".to_string(),
            message: format!("invalid auth state JSON: {error}"),
            retryable: false,
            execution_unknown: false,
        })
    })?;
    let data = parsed.get("data").unwrap_or(&parsed);
    let state = data
        .get("state")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            AppError::Upstream(crate::core::UpstreamError {
                status: None,
                code: "missing_login_state".to_string(),
                message: "auth state response did not contain state".to_string(),
                retryable: false,
                execution_unknown: false,
            })
        })?
        .to_string();
    let auth_url = data
        .get("authUrl")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            AppError::Upstream(crate::core::UpstreamError {
                status: None,
                code: "missing_auth_url".to_string(),
                message: "auth state response did not contain authUrl".to_string(),
                retryable: false,
                execution_unknown: false,
            })
        })?;
    // 官方客户端会在授权地址上补 version，缺失可能被判定为旧客户端。
    let login_url = with_version(auth_url, &profile.client_version);
    let pending = Pending {
        profile,
        state,
        created_at: Instant::now(),
    };
    Ok((
        LoginStart {
            login_url,
            expires_in: LOGIN_TTL.as_secs(),
        },
        pending,
    ))
}

/// 在授权地址上附加 `version` 参数。
fn with_version(auth_url: &str, version: &str) -> String {
    match reqwest::Url::parse(auth_url) {
        Ok(mut url) => {
            url.query_pairs_mut().append_pair("version", version);
            url.to_string()
        }
        Err(_) => auth_url.to_string(),
    }
}

/// 轮询授权结果。`Ok(None)` 表示用户尚未完成授权。
pub async fn poll(pending: &Pending) -> Result<Option<LoginResult>, AppError> {
    if pending.created_at.elapsed() > LOGIN_TTL {
        return Err(AppError::bad_request(
            "workbuddy login expired; start a new one",
        ));
    }
    let (profile, state) = (&pending.profile, &pending.state);

    let base = profile.region.base();
    let token_url = format!("{base}/v2/plugin/auth/token?state={state}");
    let mut builder = client()?.get(&token_url);
    for (name, value) in super::anonymous_headers(profile) {
        builder = builder.header(name, value);
    }
    let response = builder.send().await?;
    let token_status = response.status();
    let token_body = response.text().await.unwrap_or_default();
    let token_json: Value = serde_json::from_str(&token_body).unwrap_or(Value::Null);
    // 11217 表示仍在等待用户在浏览器确认。
    if business_code(&token_json).as_deref() == Some("11217") {
        return Ok(None);
    }
    let token_data = token_json.get("data").unwrap_or(&token_json);
    let access_token = token_data
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let Some(access_token) = access_token else {
        if token_status.is_success() {
            return Ok(None);
        }
        return Err(AppError::Upstream(crate::core::UpstreamError::status(
            token_status.as_u16(),
            token_body,
        )));
    };
    let access_token = access_token.to_string();
    let refresh_token = token_data
        .get("refreshToken")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let now = crate::records::now_seconds();
    let expires_at = token_data
        .get("expiresIn")
        .and_then(Value::as_u64)
        .map(|seconds| now + seconds)
        .unwrap_or(0);
    let refresh_expires_at = token_data
        .get("refreshExpiresIn")
        .and_then(Value::as_u64)
        .map(|seconds| now + seconds)
        .unwrap_or(0);

    let account_url = format!("{base}/v2/plugin/login/account?state={state}");
    let mut builder = client()?.get(&account_url);
    for (name, value) in common_headers(profile) {
        builder = builder.header(name, value);
    }
    builder = builder.header("authorization", format!("Bearer {access_token}"));
    let response = builder.send().await?;
    let account_status = response.status();
    let account_body = response.text().await.unwrap_or_default();
    let account_json: Value = serde_json::from_str(&account_body).unwrap_or(Value::Null);
    if business_code(&account_json).as_deref() == Some("12151") {
        return Ok(None);
    }
    if !account_status.is_success() {
        return Err(AppError::Upstream(crate::core::UpstreamError::status(
            account_status.as_u16(),
            account_body,
        )));
    }
    let account = account_json.get("data").unwrap_or(&account_json);
    let uid = account
        .get("uid")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            AppError::Upstream(crate::core::UpstreamError {
                status: None,
                code: "missing_uid".to_string(),
                message: "login account response did not contain uid".to_string(),
                retryable: false,
                execution_unknown: false,
            })
        })?
        .to_string();

    Ok(Some(LoginResult {
        uid,
        nickname: account
            .get("nickname")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        account_type: account
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        enterprise_id: account
            .get("enterpriseId")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| {
                account
                    .get("enterpriseId")
                    .and_then(Value::as_i64)
                    .map(|value| value.to_string())
            })
            .unwrap_or_default(),
        department: account
            .get("departmentFullName")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        domain: token_data
            .get("domain")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        access_token,
        refresh_token,
        expires_at,
        refresh_expires_at,
    }))
}

/// 取业务 code 的字符串形式。
fn business_code(value: &Value) -> Option<String> {
    value.get("code").map(|code| match code {
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.clone(),
        _ => String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 授权地址必须补上 version 参数。
    #[test]
    fn appends_version() {
        let url = with_version("https://www.workbuddy.cn/auth?state=abc", "5.11.3");
        assert!(url.contains("version=5.11.3"), "got {url}");
        assert!(url.contains("state=abc"), "got {url}");
    }

    /// 过期协议状态在网络请求前拒绝。
    #[tokio::test]
    async fn expired_pending_fails() {
        let profile = super::super::profile_from_options("").expect("profile");
        let pending = Pending {
            profile,
            state: "s".into(),
            created_at: Instant::now() - LOGIN_TTL - Duration::from_secs(1),
        };
        assert!(poll(&pending).await.is_err());
    }
}
