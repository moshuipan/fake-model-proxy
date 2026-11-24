//! AutoGLM 客户端凭证边界；兼容桌面端 camelCase 与接口 snake_case。

use crate::core::AppError;
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 国内与国际账号必须向各自账号域发送令牌。
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Region {
    Cn,
    #[serde(alias = "intl")]
    Global,
}

/// 可持久化的登录态；不实现 Debug，避免无意打印令牌。
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Credential {
    #[serde(default, alias = "accessToken")]
    pub access_token: String,
    #[serde(default, alias = "refreshToken")]
    pub refresh_token: String,
    #[serde(default, alias = "deviceId")]
    pub device_id: String,
    #[serde(default, alias = "account", alias = "userId")]
    pub user_id: String,
    #[serde(default, alias = "expiresAt")]
    pub expires_at: Option<u64>,
    #[serde(default)]
    pub region: Option<Region>,
}

impl Credential {
    /// 解析并从 JWT 提取原设备 ID 和到期时间；不生成不匹配的设备身份。
    pub fn parse(raw: &str) -> Result<Self, AppError> {
        let mut credential: Self = serde_json::from_str(raw)
            .map_err(|_| AppError::bad_request("AutoGLM 凭证必须是有效的 JSON 对象"))?;
        credential.access_token = strip_bearer(&credential.access_token).to_string();
        credential.refresh_token = strip_bearer(&credential.refresh_token).to_string();
        let access = claims(&credential.access_token);
        let refresh = claims(&credential.refresh_token);
        if credential.device_id.is_empty() {
            credential.device_id = string_claim(&refresh, "device_id")
                .or_else(|| string_claim(&access, "device_id"))
                .unwrap_or_default();
        }
        if credential.user_id.is_empty() {
            credential.user_id = string_claim(&access, "user_id")
                .or_else(|| string_claim(&refresh, "user_id"))
                .unwrap_or_default();
        }
        if credential.expires_at.is_none() {
            credential.expires_at = access.get("exp").and_then(Value::as_u64);
        }
        if let Some(exp) = credential.expires_at.as_mut()
            && *exp > 100_000_000_000
        {
            *exp /= 1000;
        }
        if credential.region.is_none() {
            credential.region = detected_region(&refresh).or_else(|| detected_region(&access));
        }
        if credential.access_token.is_empty() && credential.refresh_token.is_empty() {
            return Err(AppError::bad_request(
                "AutoGLM 凭证需要 access_token 或 refresh_token",
            ));
        }
        Ok(credential)
    }

    /// 统一鉴权前缀，保证不会产生 Bearer Bearer。
    pub fn authorization(&self) -> String {
        format!("Bearer {}", self.access_token)
    }

    /// JWT 到期前五分钟续期；只提供 access token 时保留正常调用能力。
    pub fn needs_refresh(&self) -> bool {
        self.access_token.is_empty()
            || (!self.refresh_token.is_empty()
                && self
                    .expires_at
                    .is_some_and(|exp| exp <= crate::records::now_seconds() + 300))
    }

    /// 序列化本地凭证，错误中不携带凭证内容。
    pub fn serialize(&self) -> Result<String, AppError> {
        serde_json::to_string(self).map_err(|_| AppError::Internal {
            message: "无法保存 AutoGLM 凭证".into(),
        })
    }
}

/// 去掉常见 Bearer 前缀，兼容桌面端保存的完整 Authorization。
fn strip_bearer(token: &str) -> &str {
    let token = token.trim();
    if token
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("bearer "))
    {
        token[7..].trim()
    } else {
        token
    }
}

/// 只读取 JWT 元数据，不把解码当作签名验证或授权依据。
fn claims(token: &str) -> Value {
    let Some(payload) = strip_bearer(token).split('.').nth(1) else {
        return Value::Null;
    };
    URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .or_else(|_| STANDARD.decode(payload))
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .unwrap_or(Value::Null)
}

/// 读取字符串类型的可选 JWT 字段。
fn string_claim(claims: &Value, key: &str) -> Option<String> {
    claims
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

/// 参考客户端按 source_id 区分短信国内账号与海外 OAuth 账号。
fn detected_region(claims: &Value) -> Option<Region> {
    claims
        .get("source_id")
        .and_then(Value::as_str)
        .map(|source| {
            if source.starts_with("agent") {
                Region::Cn
            } else {
                Region::Global
            }
        })
}
