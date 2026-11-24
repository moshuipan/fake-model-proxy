//! MonkeyCode 模型凭证；模型密钥与网页登录态分别用于各自的接口。

use crate::core::AppError;
use serde::{Deserialize, Serialize};

/// 官方模型密钥接口返回的账号凭证，附带本次网页登录态。
#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Credential {
    #[serde(default, alias = "apiKey")]
    pub api_key: String,
    #[serde(default, alias = "signingSecret")]
    pub signing_secret: String,
    #[serde(default)]
    pub cookie: String,
    #[serde(default)]
    pub server: String,
    #[serde(default)]
    pub base_url: String,
}

impl Credential {
    /// 严格校验必需的密钥，错误信息不包含输入内容。
    pub fn parse(raw: &str) -> Result<Self, AppError> {
        let mut value: Self = serde_json::from_str(raw)
            .map_err(|_| AppError::bad_request("MonkeyCode 凭证需要合法的 JSON 对象"))?;
        value.api_key = value.api_key.trim().into();
        value.signing_secret = value.signing_secret.trim().into();
        value.cookie = value.cookie.trim().into();
        if value.api_key.is_empty() || value.signing_secret.is_empty() {
            return Err(AppError::bad_request(
                "MonkeyCode 凭证必须包含 api_key 和 signing_secret；请重新登录",
            ));
        }
        for header in [&value.api_key, &value.cookie] {
            reqwest::header::HeaderValue::from_str(header)
                .map_err(|_| AppError::bad_request("MonkeyCode 凭证包含无效的请求头字符"))?;
        }
        Ok(value)
    }

    /// 序列化到受管理的账号存储，管理接口不回显此字段。
    pub fn serialize(&self) -> Result<String, AppError> {
        serde_json::to_string(self).map_err(|_| AppError::Internal {
            message: "MonkeyCode 凭证序列化失败".into(),
        })
    }

    /// 凭证携带来源时必须与配置相同，避免区域切换后误发密钥。
    pub fn validate_origin(&self, server: &str, gateway: &str) -> Result<(), AppError> {
        for (saved, expected) in [(&self.server, server), (&self.base_url, gateway)] {
            if !saved.is_empty() && saved.trim_end_matches('/') != expected {
                return Err(AppError::bad_request(
                    "MonkeyCode 凭证所属服务与当前接入地址不同，请选择对应区域并重新登录",
                ));
            }
        }
        Ok(())
    }
}
