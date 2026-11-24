//! 迁移旧魔搭预设并管理单令牌引用账号，不在账号表重复保存令牌。

use super::{Store, row_to_record};
use crate::backend::modelscope::{SOURCE, official_url, reference};
use crate::core::AppError;
use sqlx::Row;

impl Store {
    /// 在一次事务中迁移及同步，替换令牌删除旧引用，其他来源的账号保持原状。
    pub async fn sync_modelscope_accounts(&self) -> Result<(), AppError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS schema_migrations (name TEXT PRIMARY KEY)")
            .execute(&mut *tx)
            .await?;
        let migrate_capabilities = sqlx::query(
            "INSERT INTO schema_migrations (name) VALUES ('modelscope_model_capabilities_v1')
             ON CONFLICT(name) DO NOTHING",
        )
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        let rows = sqlx::query("SELECT * FROM backends")
            .fetch_all(&mut *tx)
            .await?;
        let mut desired = Vec::new();
        for row in rows {
            let mut record = row_to_record(row);
            if record.kind == "openai_compat" && official_url(&record.base_url) {
                sqlx::query("UPDATE backends SET kind = 'modelscope' WHERE id = ?")
                    .bind(&record.id)
                    .execute(&mut *tx)
                    .await?;
                record.kind = "modelscope".into();
            }
            // 旧控制台预设生成过这段固定覆盖，导致官方视觉能力永远被屏蔽。
            // 首次升级只清除三项皆 false 的旧默认；升级后的新覆盖不再被迁移。
            if migrate_capabilities
                && record.kind == "modelscope"
                && serde_json::from_str::<serde_json::Value>(&record.capabilities).is_ok_and(
                    |value| {
                        value.as_object().is_some_and(|object| {
                            object.len() == 3
                                && ["tools", "parallel_tools", "images"].iter().all(|key| {
                                    object.get(*key).and_then(serde_json::Value::as_bool)
                                        == Some(false)
                                })
                        })
                    },
                )
            {
                sqlx::query("UPDATE backends SET capabilities = '' WHERE id = ?")
                    .bind(&record.id)
                    .execute(&mut *tx)
                    .await?;
                record.capabilities.clear();
            }
            if record.kind == "modelscope" && !record.api_key.trim().is_empty() {
                desired.push(reference(&record));
            }
        }
        let managed = sqlx::query(
            "SELECT id FROM accounts WHERE json_valid(credential)
             AND json_extract(credential, '$.source') = ?",
        )
        .bind(SOURCE)
        .fetch_all(&mut *tx)
        .await?;
        for row in managed {
            let id: String = row.get("id");
            if !desired.iter().any(|account| account.id == id) {
                sqlx::query("DELETE FROM accounts WHERE id = ?")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        for account in desired {
            sqlx::query(
                "INSERT INTO accounts (id, backend_id, name, credential, enabled, created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, unixepoch(), unixepoch())
                 ON CONFLICT(id) DO UPDATE SET name = excluded.name, enabled = excluded.enabled,
                 credential = excluded.credential, updated_at = excluded.updated_at",
            ).bind(&account.id).bind(&account.backend_id).bind(&account.name)
                .bind(&account.credential).bind(account.enabled).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
