//! 配置导入导出。
//!
//! 导出默认脱敏：不含 API Key 与账号凭证。只有显式 `include_secrets=true`
//! 才导出密钥，用于本机迁移。导入按 id 合并，空密钥保留原值。

use crate::accounts::Account;
use crate::core::AppError;
use crate::store::BackendRecord;
use serde::{Deserialize, Serialize};

/// 配置文件版本。
pub const BUNDLE_VERSION: u32 = 1;

/// 一次导入导出的完整配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigBundle {
    pub version: u32,
    #[serde(default)]
    pub backends: Vec<BackendRecord>,
    #[serde(default)]
    pub accounts: Vec<Account>,
    #[serde(default)]
    pub model_aliases: Vec<crate::model_names::ModelAlias>,
    /// 旧版配置包缺少此字段时保留部署当前设置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<crate::gateway::GatewaySettings>,
}

impl ConfigBundle {
    /// 从当前配置构造。`include_secrets` 为 false 时清空全部密钥字段。
    pub fn build(
        mut backends: Vec<BackendRecord>,
        mut accounts: Vec<Account>,
        include_secrets: bool,
    ) -> Self {
        if !include_secrets {
            for backend in &mut backends {
                backend.api_key.clear();
                backend.options = redact_options(&backend.options);
            }
            for account in &mut accounts {
                account.credential.clear();
            }
        }
        ConfigBundle {
            version: BUNDLE_VERSION,
            backends,
            accounts,
            model_aliases: Vec::new(),
            gateway: None,
        }
    }

    /// 校验版本与基本字段。
    pub fn validate(&self) -> Result<(), AppError> {
        if let Some(settings) = &self.gateway {
            settings.validate()?;
        }
        if self.version != BUNDLE_VERSION {
            return Err(AppError::bad_request(format!(
                "unsupported config version {}; expected {BUNDLE_VERSION}",
                self.version
            )));
        }
        for backend in &self.backends {
            if backend.kind == "trae_personal" {
                crate::backend::trae_personal::profile(backend)?;
            }
            if backend.id.trim().is_empty() {
                return Err(AppError::bad_request("backend id must not be empty"));
            }
            if backend.base_url.trim().is_empty() && backend.kind == "openai_compat" {
                return Err(AppError::bad_request(format!(
                    "backend `{}` requires base_url",
                    backend.id
                )));
            }
        }
        for account in &self.accounts {
            if account.id.trim().is_empty() || account.backend_id.trim().is_empty() {
                return Err(AppError::bad_request(
                    "account id and backend_id must not be empty",
                ));
            }
        }
        let mut names = std::collections::HashSet::new();
        let mut primary = std::collections::HashSet::new();
        for alias in &self.model_aliases {
            crate::model_names::validate_alias(&alias.alias)?;
            if alias.backend_id.trim().is_empty()
                || alias.model_id.trim().is_empty()
                || !names.insert(&alias.alias)
                || (alias.is_primary && !primary.insert((&alias.backend_id, &alias.model_id)))
            {
                return Err(AppError::bad_request(
                    "配置中的调用名称或主名称重复，或模型地址为空",
                ));
            }
        }
        Ok(())
    }
}

/// 兼容导出与旧表单不回显请求头秘密；空字符串在 v1 保存时保持已有引用。
pub fn redact_options(options: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(options) else {
        return String::new();
    };
    if let Some(headers) = value
        .get_mut("headers")
        .and_then(serde_json::Value::as_object_mut)
    {
        for value in headers.values_mut() {
            *value = serde_json::Value::String(String::new());
        }
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造测试后端。
    fn backend() -> BackendRecord {
        BackendRecord {
            id: "b".into(),
            name: "B".into(),
            api_key: "sk-secret".into(),
            models: "m".into(),
            base_url: "https://example.com/v1".into(),
            ..BackendRecord::default()
        }
    }

    /// 构造测试账号。
    fn account() -> Account {
        Account {
            id: "a".into(),
            backend_id: "b".into(),
            name: "A".into(),
            credential: "{\"api_key\":\"secret\"}".into(),
            enabled: true,
        }
    }

    /// 默认导出必须清空密钥。
    #[test]
    fn export_without_secrets_strips_keys() {
        let bundle = ConfigBundle::build(vec![backend()], vec![account()], false);
        assert!(bundle.backends[0].api_key.is_empty());
        assert!(bundle.accounts[0].credential.is_empty());
    }

    /// 显式包含密钥时保留原值。
    #[test]
    fn export_with_secrets_keeps_keys() {
        let bundle = ConfigBundle::build(vec![backend()], vec![account()], true);
        assert_eq!(bundle.backends[0].api_key, "sk-secret");
        assert!(!bundle.accounts[0].credential.is_empty());
    }

    /// 版本不匹配应拒绝。
    #[test]
    fn rejects_unknown_version() {
        let bundle = ConfigBundle {
            version: 99,
            backends: Vec::new(),
            accounts: Vec::new(),
            model_aliases: Vec::new(),
            gateway: None,
        };
        assert!(bundle.validate().is_err());
    }

    /// 兼容型后端缺地址应拒绝。
    #[test]
    fn rejects_compatible_backend_without_url() {
        let mut record = backend();
        record.base_url.clear();
        let bundle = ConfigBundle {
            version: BUNDLE_VERSION,
            backends: vec![record],
            accounts: Vec::new(),
            model_aliases: Vec::new(),
            gateway: None,
        };
        assert!(bundle.validate().is_err());
    }
}
