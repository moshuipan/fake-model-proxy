//! MiniMax Code 独立设备 OAuth；不读取桌面客户端数据。

use crate::core::{AppError, UpstreamError};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::time::Duration;

/// 官方公开 OAuth 客户端参数。
pub const CLIENT_ID: &str = "mcode-public";

/// 独立授权凭证，区域随凭证固定，不能被服务编辑重定向。
#[derive(Clone, Serialize, Deserialize)]
pub struct Credential {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
    pub region: String,
    #[serde(default)]
    pub subject: String,
}

impl Credential {
    /// 解析代理自己的账号记录，拒绝缺失刷新令牌的临时凭证。
    pub fn parse(text: &str) -> Result<Self, AppError> {
        let value: Self = serde_json::from_str(text)
            .map_err(|_| AppError::bad_request("MiniMax 凭证格式无效，请重新授权"))?;
        if value.access_token.is_empty()
            || value.refresh_token.is_empty()
            || !matches!(value.region.as_str(), "cn" | "global")
        {
            return Err(AppError::bad_request("MiniMax 凭证不完整，请重新授权"));
        }
        Ok(value)
    }

    /// 返回仅用于账号库保存的凭证 JSON。
    pub fn serialize(&self) -> Result<String, AppError> {
        serde_json::to_string(self).map_err(|_| AppError::Internal {
            message: "MiniMax 凭证保存失败".into(),
        })
    }

    /// 两分钟提前量覆盖请求耗时和时钟偏差。
    pub fn expires_soon(&self) -> bool {
        self.expires_at <= crate::records::now_seconds() + 120
    }
}

/// 设备授权秘密只存在于会话内存，不返回到控制台。
pub struct DeviceAuthorization {
    pub login_url: String,
    pub user_code: String,
    pub expires_in: u64,
    pub interval: Duration,
    pub(crate) verifier: String,
    pub(crate) code: String,
    pub(crate) polling_key: &'static str,
}

/// 独立 OAuth HTTP 客户端。
#[derive(Clone)]
pub struct OAuth {
    pub(crate) client: reqwest::Client,
    pub(crate) origin: String,
    pub region: String,
}

impl OAuth {
    /// 使用固定区域官方账号域，禁用跳转，避免令牌跨域发送。
    pub fn new(region: &str) -> Result<Self, AppError> {
        Ok(Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(20))
                .build()?,
            origin: if region == "global" {
                "https://account.minimax.io"
            } else {
                "https://account.minimax.cn"
            }
            .into(),
            region: region.into(),
        })
    }

    /// 发起表单请求；错误文本不回显响应原文或 OAuth 秘密。
    async fn post(&self, path: &str, fields: &[(&str, &str)]) -> Result<(u16, Value), AppError> {
        let response = self
            .client
            .post(format!("{}{path}", self.origin))
            .header("Accept", "application/json")
            .form(fields)
            .send()
            .await
            .map_err(|_| {
                AppError::Upstream(UpstreamError::transport("MiniMax OAuth 网络请求失败"))
            })?;
        let status = response.status().as_u16();
        let value = response
            .json::<Value>()
            .await
            .map_err(|_| AppError::bad_request("MiniMax OAuth 未返回有效 JSON"))?;
        Ok((status, value))
    }

    /// 生成 PKCE 并兼容官方标准与 user_code 两种设备授权响应。
    pub async fn start(&self) -> Result<DeviceAuthorization, AppError> {
        let mut random = [0u8; 32];
        OsRng.fill_bytes(&mut random);
        let verifier = URL_SAFE_NO_PAD.encode(random);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let (status, value) = self
            .post(
                "/oauth2/device/code",
                &[
                    ("client_id", CLIENT_ID),
                    ("scope", "agent.default"),
                    ("audience", "agent-backend"),
                    ("code_challenge", &challenge),
                    ("code_challenge_method", "S256"),
                ],
            )
            .await?;
        if !(200..300).contains(&status) || value.get("error").is_some() {
            return Err(oauth_error(status, &value));
        }
        parse_device(&value, verifier, &self.region)
    }

    /// 单次取票；等待和退避由有截止时间的服务端会话控制。
    pub async fn poll(&self, device: &DeviceAuthorization) -> Result<PollResult, AppError> {
        let (status, value) = self
            .post(
                "/oauth2/token",
                &[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("client_id", CLIENT_ID),
                    (device.polling_key, &device.code),
                    ("code_verifier", &device.verifier),
                ],
            )
            .await?;
        let code = value["error"]
            .as_str()
            .or(value["status"].as_str())
            .unwrap_or("");
        if status == 400 || (200..300).contains(&status) {
            match code {
                "authorization_pending" | "pending" => return Ok(PollResult::Pending),
                "slow_down" => return Ok(PollResult::SlowDown),
                _ => {}
            }
        }
        if !(200..300).contains(&status)
            || value.get("error").is_some()
            || matches!(
                code,
                "denied" | "access_denied" | "expired" | "expired_token"
            )
        {
            return Err(oauth_error(status, &value));
        }
        Ok(PollResult::Done(parse_grant(&value, &self.region, None)?))
    }

    /// 轮换一次刷新令牌；调用者必须先保存持久围栏并提交新凭证。
    pub async fn refresh(&self, credential: &Credential) -> Result<Credential, AppError> {
        let (status, value) = self
            .post(
                "/oauth2/token",
                &[
                    ("grant_type", "refresh_token"),
                    ("client_id", CLIENT_ID),
                    ("refresh_token", &credential.refresh_token),
                    ("scope", "agent.default"),
                    ("audience", "agent-backend"),
                ],
            )
            .await?;
        if !(200..300).contains(&status) || value.get("error").is_some() {
            return Err(oauth_error(status, &value));
        }
        let mut next = parse_grant(&value, &self.region, Some(&credential.refresh_token))?;
        if next.subject.is_empty() {
            next.subject = credential.subject.clone();
        }
        Ok(next)
    }
}

/// 会话循环可以区分等待、退避和成功。
pub enum PollResult {
    Pending,
    SlowDown,
    Done(Credential),
}

/// 仅接受所选区域的官方 HTTPS 授权页。
pub fn valid_login_url(text: &str, region: &str) -> bool {
    reqwest::Url::parse(text).is_ok_and(|url| {
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.port().is_none()
            && url.fragment().is_none()
            && url.host_str()
                == Some(if region == "global" {
                    "account.minimax.io"
                } else {
                    "account.minimax.cn"
                })
    })
}

/// 将授权服务器响应规范化，时间和轮询单位遵循官方 OAuth Core。
fn parse_device(
    value: &Value,
    verifier: String,
    region: &str,
) -> Result<DeviceAuthorization, AppError> {
    let user_code = value["user_code"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::bad_request("MiniMax 授权响应缺少短码"))?;
    let standard = value["device_code"].as_str().filter(|s| !s.is_empty());
    let account_mode = standard.is_none() && value["expired_in"].as_u64().is_some();
    let code = standard
        .or(account_mode.then_some(user_code))
        .ok_or_else(|| AppError::bad_request("MiniMax 授权响应缺少设备码"))?;
    let login_url = value["verification_uri_complete"]
        .as_str()
        .or(value["verification_uri"].as_str())
        .or(value["verification_url"].as_str())
        .filter(|url| valid_login_url(url, region))
        .ok_or_else(|| AppError::bad_request("MiniMax 授权地址不是所选区域的官方站点"))?;
    let now_ms = crate::records::now_seconds() * 1000;
    let expires_in = value["expires_in"]
        .as_u64()
        .or_else(|| {
            value["expired_in"].as_u64().map(|v| {
                if v >= 1_000_000_000_000 {
                    v.saturating_sub(now_ms).div_ceil(1000)
                } else {
                    v
                }
            })
        })
        .filter(|v| *v > 0 && *v <= 3600)
        .ok_or_else(|| AppError::bad_request("MiniMax 设备授权有效期无效"))?;
    let raw = value["interval"]
        .as_f64()
        .unwrap_or(if account_mode { 5000.0 } else { 5.0 });
    let seconds = if account_mode { raw / 1000.0 } else { raw };
    if !seconds.is_finite() || seconds <= 0.0 || seconds > 300.0 {
        return Err(AppError::bad_request("MiniMax 授权轮询间隔无效"));
    }
    Ok(DeviceAuthorization {
        login_url: login_url.into(),
        user_code: user_code.into(),
        expires_in,
        interval: Duration::from_secs_f64(seconds.max(1.0)),
        verifier,
        code: code.into(),
        polling_key: if account_mode {
            "user_code"
        } else {
            "device_code"
        },
    })
}

/// 校验 Bearer 授权及 scope，到期时间严格使用服务端秒数。
fn parse_grant(
    value: &Value,
    region: &str,
    previous: Option<&str>,
) -> Result<Credential, AppError> {
    let access = value["access_token"].as_str().filter(|s| !s.is_empty());
    let refresh = value["refresh_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or(previous);
    let expires = value["expires_in"]
        .as_u64()
        .filter(|v| *v > 0 && *v <= 31_536_000);
    let scope = value["scope"]
        .as_str()
        .map(|v| v.split_whitespace().any(|s| s == "agent.default"))
        .or_else(|| {
            value["scope"]
                .as_array()
                .map(|v| v.iter().any(|s| s == "agent.default"))
        });
    if access.is_none()
        || refresh.is_none()
        || expires.is_none()
        || !value["token_type"]
            .as_str()
            .is_some_and(|v| v.eq_ignore_ascii_case("bearer"))
        || scope == Some(false)
    {
        return Err(AppError::bad_request(
            "MiniMax 授权凭证响应不完整，请重新授权",
        ));
    }
    let payload = access
        .unwrap()
        .split('.')
        .nth(1)
        .and_then(|v| URL_SAFE_NO_PAD.decode(v).ok())
        .and_then(|v| serde_json::from_slice::<Value>(&v).ok());
    let subject = value["user_id"]
        .as_str()
        .or(value["account_id"].as_str())
        .or_else(|| payload.as_ref().and_then(|p| p["sub"].as_str()))
        .unwrap_or("")
        .to_string();
    Ok(Credential {
        access_token: access.unwrap().into(),
        refresh_token: refresh.unwrap().into(),
        expires_at: crate::records::now_seconds() + expires.unwrap(),
        region: region.into(),
        subject,
    })
}

/// 只映射已知错误码，不将可能含 token 的响应文本写入日志。
fn oauth_error(status: u16, value: &Value) -> AppError {
    if status >= 500 || (300..400).contains(&status) {
        return AppError::Upstream(UpstreamError {
            status: Some(502),
            code: "minimax_oauth_unknown".into(),
            message: "MiniMax OAuth 响应结果未知，请重新授权".into(),
            retryable: false,
            execution_unknown: true,
        });
    }
    let code = value["error"]
        .as_str()
        .or(value["status"].as_str())
        .unwrap_or("");
    let (name, message) = match code {
        "invalid_grant" if status == 400 => {
            ("minimax_invalid_grant", "MiniMax 授权已失效，请重新登录")
        }
        "expired_token" | "expired" => (
            "minimax_authorization_expired",
            "MiniMax 设备码已过期，请重新开始",
        ),
        "access_denied" | "denied" => ("minimax_access_denied", "MiniMax 授权被拒绝"),
        _ => (
            "minimax_oauth_error",
            "MiniMax OAuth 请求被拒绝，请重试或检查区域网络",
        ),
    };
    AppError::Upstream(UpstreamError {
        status: Some(if status >= 400 { status } else { 502 }),
        code: name.into(),
        message: message.into(),
        retryable: status >= 500,
        execution_unknown: false,
    })
}
