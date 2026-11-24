//! 服务插件命名空间小状态；不替代公共账号、任务或配置表。

use super::Store;
use crate::core::AppError;
use sqlx::Row;

impl Store {
    /// 创建带插件与服务作用域的状态表。
    pub(super) async fn migrate_plugin_state(&self) -> Result<(), AppError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS plugin_state (
            plugin_id TEXT NOT NULL, service_id TEXT NOT NULL, key TEXT NOT NULL,
            schema_version INTEGER NOT NULL, value TEXT NOT NULL,
            PRIMARY KEY(plugin_id, service_id, key))",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 宿主读取一个命名空间键。
    pub(crate) async fn plugin_state(
        &self,
        plugin: &str,
        service: &str,
        key: &str,
    ) -> Result<Option<(u32, serde_json::Value)>, AppError> {
        let row = sqlx::query("SELECT schema_version,value FROM plugin_state WHERE plugin_id=? AND service_id=? AND key=?")
            .bind(plugin).bind(service).bind(key).fetch_optional(&self.pool).await?;
        row.map(|row| {
            Ok((
                row.get::<i64, _>(0) as u32,
                serde_json::from_str(row.get::<&str, _>(1))
                    .map_err(|_| AppError::bad_request("invalid stored plugin state"))?,
            ))
        })
        .transpose()
    }

    /// 宿主写入有限大小的插件小状态。
    pub(crate) async fn save_plugin_state(
        &self,
        plugin: &str,
        service: &str,
        key: &str,
        version: u32,
        value: &serde_json::Value,
    ) -> Result<(), AppError> {
        let encoded = value.to_string();
        if key.is_empty() || key.len() > 128 || encoded.len() > 65_536 {
            return Err(AppError::bad_request(
                "plugin state exceeds key or value limit",
            ));
        }
        sqlx::query(
            "INSERT INTO plugin_state VALUES(?,?,?,?,?) ON CONFLICT(plugin_id,service_id,key)
            DO UPDATE SET schema_version=excluded.schema_version,value=excluded.value",
        )
        .bind(plugin)
        .bind(service)
        .bind(key)
        .bind(version)
        .bind(encoded)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
