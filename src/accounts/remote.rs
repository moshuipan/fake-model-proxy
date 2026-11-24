//! 凭证刷新与额度读取契约。
//!
//! 这两项都依赖厂商私有接口。未配置端点时返回「不支持」，绝不猜测数字：
//! 额度未知必须显式为 Unknown，不能被渲染成 0。

use crate::accounts::Account;
use crate::core::AppError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// 额度信息的可信度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaConfidence {
    /// 上游明确返回。
    Verified,
    /// 由本地用量推算。
    Estimated,
    /// 无法确定。
    Unknown,
}

/// 一次额度读取结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaInfo {
    /// 精确余额与资源桶元数据；旧字段继续用于界面兼容。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<super::quota::QuotaSnapshot>,
    pub backend_id: String,
    pub account_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<i64>,
    /// 上游直接提供的余额；不等同于套餐总量。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining: Option<i64>,
    /// 单位，例如 credits / tokens / requests。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<String>,
    pub confidence: QuotaConfidence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// 读取时间（Unix 秒）。
    pub checked_at: u64,
}

impl QuotaInfo {
    /// 构造未知结果，用于未配置或不支持的情况。
    pub fn unknown(backend_id: &str, account_id: &str, note: impl Into<String>) -> Self {
        QuotaInfo {
            routing: None,
            backend_id: backend_id.to_string(),
            account_id: account_id.to_string(),
            used: None,
            total: None,
            remaining: None,
            unit: None,
            reset_at: None,
            confidence: QuotaConfidence::Unknown,
            note: Some(note.into()),
            checked_at: crate::records::now_seconds(),
        }
    }
}

/// 凭证刷新能力。实现方负责把新凭证写回账号记录。
#[async_trait]
pub trait CredentialRefresher: Send + Sync {
    /// 所属后端标识。
    fn backend_id(&self) -> &str;

    /// 刷新一个账号的凭证，返回更新后的记录。
    async fn refresh(&self, account: &Account) -> Result<Account, AppError>;
}

/// 额度读取能力。
#[async_trait]
pub trait QuotaReader: Send + Sync {
    /// 所属后端标识。
    fn backend_id(&self) -> &str;

    /// 默认只接受原凭证；支持续期的读取器可核对其稳定账号身份。
    fn same_credential_identity(&self, before: &str, after: &str) -> bool {
        before == after
    }

    /// 读取一个账号的额度。不支持时返回 Unknown 而不是错误。
    async fn read(&self, account: &Account) -> Result<QuotaInfo, AppError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 未知额度必须保持 None 并标记 Unknown。
    #[test]
    fn unknown_quota_keeps_fields_empty() {
        let info = QuotaInfo::unknown("qoder", "acc", "endpoint not configured");
        assert_eq!(info.confidence, QuotaConfidence::Unknown);
        assert!(info.used.is_none());
        assert!(info.total.is_none());
        assert!(info.note.is_some());
    }
}
