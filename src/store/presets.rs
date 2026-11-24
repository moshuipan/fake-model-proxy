//! 自定义预设事务存储，预设更新不会触发服务重载。

use super::Store;
use crate::{core::AppError, presets::ProviderPreset};

impl Store {
    /// 建立独立目录数据与导入审计。
    pub(super) async fn migrate_presets(&self) -> Result<(), AppError> {
        sqlx::query("CREATE TABLE IF NOT EXISTS provider_presets (id TEXT PRIMARY KEY, document TEXT NOT NULL, actor TEXT NOT NULL, updated_at INTEGER NOT NULL)")
            .execute(&self.pool).await?;
        Ok(())
    }

    /// 读取已保存公开默认值，不包含运行账号信息。
    pub async fn custom_presets(&self) -> Result<Vec<ProviderPreset>, AppError> {
        let records: Vec<String> =
            sqlx::query_scalar("SELECT document FROM provider_presets ORDER BY id")
                .fetch_all(&self.pool)
                .await?;
        records
            .iter()
            .map(|text| {
                serde_json::from_str(text).map_err(|_| {
                    crate::application::config_types::error("preset_invalid", "已存储预设无法读取")
                })
            })
            .collect()
    }

    /// 全部预设校验成功后一次导入，任何数据库失败都会回滚。
    pub async fn import_presets(
        &self,
        presets: &[ProviderPreset],
        actor: &str,
    ) -> Result<(), AppError> {
        let mut tx = self.pool.begin().await?;
        for preset in presets {
            sqlx::query("INSERT INTO provider_presets VALUES(?,?,?,unixepoch())
                ON CONFLICT(id) DO UPDATE SET document=excluded.document,actor=excluded.actor,updated_at=excluded.updated_at")
                .bind(&preset.id).bind(serde_json::to_string(preset).expect("preset is JSON"))
                .bind(actor).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
