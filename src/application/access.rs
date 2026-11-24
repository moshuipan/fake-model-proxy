//! 控制平面身份与权限；应用用例自行检查权限，HTTP/CLI/MCP 不复制规则。

use crate::{config::Config, core::AppError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// 最小独立权限；操作许可不隐含配置或秘密写入。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Read,
    ConfigWrite,
    Operation,
    SecretWrite,
}

/// 启动时配置的具名访问密钥；密钥本身从不参与序列化输出。
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessKey {
    pub id: String,
    pub key: String,
    pub scopes: BTreeSet<Scope>,
}

impl std::fmt::Debug for AccessKey {
    /// 调试配置也不能泄漏管理密钥。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessKey")
            .field("id", &self.id)
            .field("scopes", &self.scopes)
            .finish_non_exhaustive()
    }
}

/// 只有鉴权入口能创建外部身份；用例保留调用者用于审计。
#[derive(Clone, Debug, Serialize)]
pub struct Principal {
    pub id: String,
    pub scopes: BTreeSet<Scope>,
}

impl Principal {
    /// 在操作执行之前拒绝缺失范围的调用，不靠工具说明约束 AI。
    pub fn require(&self, scope: Scope) -> Result<(), AppError> {
        if self.scopes.contains(&scope) {
            Ok(())
        } else {
            Err(super::config_types::error(
                "forbidden",
                format!("缺少权限 {scope:?}"),
            ))
        }
    }

    /// 兼容管理密钥代表全权限；无密钥仅在回环监听上允许本地管理。
    pub fn authenticate(config: &Config, provided: Option<&str>) -> Result<Self, AppError> {
        let all = BTreeSet::from([
            Scope::Read,
            Scope::ConfigWrite,
            Scope::Operation,
            Scope::SecretWrite,
        ]);
        if let Some(key) = provided.filter(|value| !value.is_empty()) {
            if !config.admin_key.is_empty() && equal(key, &config.admin_key) {
                return Ok(Self {
                    id: "admin".into(),
                    scopes: all,
                });
            }
            if let Some(access) = config.access_keys.iter().find(|a| equal(key, &a.key)) {
                return Ok(Self {
                    id: access.id.clone(),
                    scopes: access.scopes.clone(),
                });
            }
        }
        if config.admin_key.is_empty() && config.access_keys.is_empty() && config.is_loopback() {
            return Ok(Self {
                id: "local".into(),
                scopes: all,
            });
        }
        Err(super::config_types::error(
            "invalid_admin_key",
            "管理身份验证失败",
        ))
    }
}

/// 对固定摘要做常量工作量比较，不按第一个差异字符提前返回。
fn equal(left: &str, right: &str) -> bool {
    use sha2::{Digest, Sha256};
    let a = Sha256::digest(left.as_bytes());
    let b = Sha256::digest(right.as_bytes());
    a.iter().zip(b).fold(0_u8, |diff, (a, b)| diff | (a ^ b)) == 0
}

/// 校验启动密钥清单，拒绝匿名、重复身份及与主管理密钥重复的配置。
pub fn validate_keys(keys: &[AccessKey], admin: &str) -> Result<(), AppError> {
    let mut ids = BTreeSet::new();
    let mut values = BTreeSet::new();
    for access in keys {
        if access.id.trim().is_empty()
            || matches!(access.id.as_str(), "admin" | "local")
            || access.key.len() < 16
            || access.scopes.is_empty()
            || !ids.insert(&access.id)
            || !values.insert(&access.key)
            || equal(&access.key, admin)
        {
            return Err(super::config_types::error(
                "access_config_invalid",
                "FMP_ACCESS_KEYS 需要唯一身份、至少 16 字符的独立密钥和非空权限",
            ));
        }
    }
    Ok(())
}
