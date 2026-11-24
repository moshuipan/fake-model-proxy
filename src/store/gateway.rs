//! 网关设置为独立单行配置，旧数据库首次读取使用默认值。

use super::Store;
use crate::{core::AppError, gateway::GatewaySettings};

impl Store {
    /// 创建独立设置表，避免配置升级影响账号与凭证。
    pub(super) async fn migrate_gateway(&self) -> Result<(), AppError> {
        sqlx::query("CREATE TABLE IF NOT EXISTS gateway_settings (id INTEGER PRIMARY KEY CHECK(id = 1), settings TEXT NOT NULL)")
            .execute(&self.pool).await?;
        Ok(())
    }

    /// 缺少保存记录时使用兼容默认配置，损坏配置不能静默回退。
    pub async fn gateway_settings(&self) -> Result<GatewaySettings, AppError> {
        let value: Option<String> =
            sqlx::query_scalar("SELECT settings FROM gateway_settings WHERE id = 1")
                .fetch_optional(&self.pool)
                .await?;
        let settings: GatewaySettings = value
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .map_err(|_| AppError::Internal {
                message: "stored gateway settings are invalid".into(),
            })?
            .unwrap_or_default();
        settings.validate()?;
        Ok(settings)
    }

    /// 先校验后原子替换完整配置；账号和后端覆盖与预算一起生效。
    pub async fn save_gateway_settings(&self, settings: &GatewaySettings) -> Result<(), AppError> {
        settings.validate()?;
        let value = serde_json::to_string(settings).map_err(|_| AppError::Internal {
            message: "serialize gateway settings failed".into(),
        })?;
        sqlx::query("INSERT INTO gateway_settings (id, settings) VALUES (1, ?) ON CONFLICT(id) DO UPDATE SET settings = excluded.settings")
            .bind(value).execute(&self.pool).await?;
        Ok(())
    }
}
