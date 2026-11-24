//! 小浣熊业务令牌；JWT 时间仅用于续期调度，不作为账号身份依据。

use crate::core::AppError;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 官方授权码换取的令牌对，只保存在代理账号库。
#[derive(Clone, Serialize, Deserialize)]
pub struct Credential {
    #[serde(alias = "accessToken")]
    pub access_token: String,
    #[serde(alias = "refreshToken")]
    pub refresh_token: String,
    #[serde(default)]
    pub expires_at: u64,
    #[serde(default)]
    pub obtained_at: u64,
}

impl Credential {
    /// 校验手工导入和数据库凭证，拒绝网页 IAM cookie 或缺失续期令牌的记录。
    pub fn parse(text: &str) -> Result<Self, AppError> {
        let mut credential: Self = serde_json::from_str(text)
            .map_err(|_| AppError::bad_request("小浣熊凭证格式无效，请重新授权"))?;
        credential.access_token = clean_token(&credential.access_token)?;
        credential.refresh_token = clean_token(&credential.refresh_token)?;
        if credential.expires_at == 0 {
            credential.expires_at = jwt_exp(&credential.access_token).unwrap_or(0);
        }
        Ok(credential)
    }

    /// 保存统一字段，不向控制台返回访问令牌。
    pub fn serialize(&self) -> Result<String, AppError> {
        serde_json::to_string(self).map_err(|_| AppError::Internal {
            message: "小浣熊凭证保存失败".into(),
        })
    }

    /// 长效令牌在本次有效期的六分之一后滚动续期，短效令牌提前两分钟。
    pub fn needs_refresh(&self) -> bool {
        let now = crate::records::now_seconds();
        if self.expires_at == 0 {
            return self.obtained_at > 0 && now >= self.obtained_at.saturating_add(5 * 86400);
        }
        let lifetime = self.expires_at.saturating_sub(self.obtained_at);
        if self.obtained_at > 0 && lifetime >= 7 * 86400 {
            now >= self.obtained_at.saturating_add(lifetime / 6)
        } else {
            self.expires_at <= now.saturating_add(120)
        }
    }

    /// 从已验证 code=0 的业务响应读取令牌，续期未返回 refresh_token 时保留旧值。
    pub(super) fn from_data(data: &Value, previous: Option<&Self>) -> Result<Self, AppError> {
        let access_token = clean_token(data["access_token"].as_str().unwrap_or(""))?;
        let refresh_token = clean_token(
            data["refresh_token"]
                .as_str()
                .filter(|token| !token.trim().is_empty())
                .or_else(|| previous.map(|credential| credential.refresh_token.as_str()))
                .unwrap_or(""),
        )?;
        let obtained_at = crate::records::now_seconds();
        let expires_at = data["expires_in"]
            .as_u64()
            .filter(|seconds| *seconds > 0 && *seconds <= 31_536_000)
            .map(|seconds| obtained_at.saturating_add(seconds))
            .or_else(|| jwt_exp(&access_token))
            .unwrap_or(0);
        if expires_at > 0 && expires_at <= obtained_at {
            return Err(AppError::bad_request("小浣熊返回了已过期凭证，请重新授权"));
        }
        Ok(Self {
            access_token,
            refresh_token,
            expires_at,
            obtained_at,
        })
    }
}

/// 去除可选 Bearer 前缀，并拒绝不能放进 HTTP 头的值。
fn clean_token(text: &str) -> Result<String, AppError> {
    let text = text.trim();
    let token = text
        .get(..7)
        .filter(|prefix| prefix.eq_ignore_ascii_case("bearer "))
        .map_or(text, |_| text[7..].trim());
    if token.is_empty() || token.len() > 32768 || !token.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(AppError::bad_request(
            "小浣熊令牌缺失或格式无效，请重新授权",
        ));
    }
    Ok(token.into())
}

/// 只解码 exp 做调度；账号 ID 必须从官方 user_info 读取。
fn jwt_exp(token: &str) -> Option<u64> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice::<Value>(&bytes).ok()?["exp"].as_u64()
}
