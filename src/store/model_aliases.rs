//! 持久模型名称分配。SQLite 写事务串行化冲突检查与主名称切换。

use super::Store;
use crate::core::{AppError, ModelInfo};
use crate::model_names::{ModelAlias, check_native_collision, default_alias, validate_alias};
use crate::store::BackendRecord;
use sqlx::Row;
use std::collections::HashMap;

impl Store {
    /// 历史映射无级联删除，避免已用名称被分配给其他服务。
    pub(super) async fn migrate_model_aliases(&self) -> Result<(), AppError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS model_aliases (
                alias TEXT PRIMARY KEY, backend_id TEXT NOT NULL,
                model_id TEXT NOT NULL, is_primary INTEGER NOT NULL DEFAULT 1
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_model_primary_alias
             ON model_aliases(backend_id, model_id) WHERE is_primary = 1",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 读取主名称及历史名称，无凭证信息。
    pub async fn list_model_aliases(&self) -> Result<Vec<ModelAlias>, AppError> {
        let rows = sqlx::query(
            "SELECT alias, backend_id, model_id, is_primary FROM model_aliases ORDER BY alias",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| ModelAlias {
                alias: row.get("alias"),
                backend_id: row.get("backend_id"),
                model_id: row.get("model_id"),
                is_primary: row.get::<i64, _>("is_primary") != 0,
            })
            .collect())
    }

    /// 给配置及动态目录中尚无主名称的模型分配名称，冲突时添加序号。
    pub async fn ensure_model_aliases(
        &self,
        records: &[BackendRecord],
        catalogs: &HashMap<String, Vec<ModelInfo>>,
    ) -> Result<(), AppError> {
        let mut tx = self.pool.begin().await?;
        // 在读取前获得 SQLite 写锁，避免两个首次发现请求同时挑选同名。
        sqlx::query("UPDATE model_aliases SET is_primary = is_primary WHERE 0")
            .execute(&mut *tx)
            .await?;
        for record in records {
            let catalog = catalogs
                .get(&record.id)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let mut models: Vec<&str> = record
                .models
                .split(',')
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .collect();
            models.extend(catalog.iter().map(|model| model.id.as_str()));
            models.sort_unstable();
            models.dedup();
            for model_id in models {
                let exists: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM model_aliases WHERE backend_id = ? AND model_id = ? AND is_primary = 1",
                ).bind(&record.id).bind(model_id).fetch_one(&mut *tx).await?;
                if exists > 0 {
                    continue;
                }
                let display = catalog
                    .iter()
                    .find(|model| model.id == model_id)
                    .and_then(|model| model.display_name.as_deref());
                let base = default_alias(record, model_id, display);
                let mut suffix = 1;
                loop {
                    let alias = ModelAlias {
                        alias: if suffix == 1 {
                            base.clone()
                        } else {
                            format!("{base}-{suffix}")
                        },
                        backend_id: record.id.clone(),
                        model_id: model_id.into(),
                        is_primary: true,
                    };
                    let occupied: i64 =
                        sqlx::query_scalar("SELECT count(*) FROM model_aliases WHERE alias = ?")
                            .bind(&alias.alias)
                            .fetch_one(&mut *tx)
                            .await?;
                    if occupied == 0 && check_native_collision(&alias, records).is_ok() {
                        sqlx::query("INSERT INTO model_aliases(alias, backend_id, model_id, is_primary) VALUES (?, ?, ?, 1)")
                            .bind(&alias.alias).bind(&alias.backend_id).bind(&alias.model_id)
                            .execute(&mut *tx).await?;
                        break;
                    }
                    suffix += 1;
                }
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// 原子修改主名称，要求客户端提供其读取到的旧名称，历史名称继续保留。
    pub async fn rename_model_alias(
        &self,
        backend_id: &str,
        model_id: &str,
        alias: &str,
        previous_alias: &str,
    ) -> Result<(), AppError> {
        validate_alias(alias)?;
        let records = self.list_backends().await?;
        check_native_collision(
            &ModelAlias {
                alias: alias.into(),
                backend_id: backend_id.into(),
                model_id: model_id.into(),
                is_primary: true,
            },
            &records,
        )?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE model_aliases SET is_primary = is_primary WHERE 0")
            .execute(&mut *tx)
            .await?;
        let current: Option<String> = sqlx::query_scalar(
            "SELECT alias FROM model_aliases WHERE backend_id = ? AND model_id = ? AND is_primary = 1",
        ).bind(backend_id).bind(model_id).fetch_optional(&mut *tx).await?;
        if current.as_deref() != Some(previous_alias) {
            return Err(AppError::bad_request("调用名称已变更，请刷新后再保存"));
        }
        let owner: Option<(String, String)> =
            sqlx::query_as("SELECT backend_id, model_id FROM model_aliases WHERE alias = ?")
                .bind(alias)
                .fetch_optional(&mut *tx)
                .await?;
        if owner
            .as_ref()
            .is_some_and(|(backend, model)| backend != backend_id || model != model_id)
        {
            return Err(AppError::bad_request("调用名称已被其他模型使用或保留"));
        }
        sqlx::query(
            "UPDATE model_aliases SET is_primary = 0 WHERE backend_id = ? AND model_id = ?",
        )
        .bind(backend_id)
        .bind(model_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO model_aliases(alias, backend_id, model_id, is_primary) VALUES (?, ?, ?, 1)
             ON CONFLICT(alias) DO UPDATE SET is_primary = 1",
        )
        .bind(alias)
        .bind(backend_id)
        .bind(model_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// 导入完整映射；任何冲突整体回滚，绝不把旧名称转交给另一模型。
    pub async fn import_model_aliases(
        &self,
        aliases: &[ModelAlias],
        records: &[BackendRecord],
    ) -> Result<(), AppError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE model_aliases SET is_primary = is_primary WHERE 0")
            .execute(&mut *tx)
            .await?;
        for alias in aliases {
            validate_alias(&alias.alias)?;
            check_native_collision(alias, records)?;
            let owner: Option<(String, String)> =
                sqlx::query_as("SELECT backend_id, model_id FROM model_aliases WHERE alias = ?")
                    .bind(&alias.alias)
                    .fetch_optional(&mut *tx)
                    .await?;
            if owner.as_ref().is_some_and(|(backend, model)| {
                backend != &alias.backend_id || model != &alias.model_id
            }) {
                return Err(AppError::bad_request(format!(
                    "调用名称 `{}` 已绑定其他模型",
                    alias.alias
                )));
            }
            if alias.is_primary {
                sqlx::query(
                    "UPDATE model_aliases SET is_primary = 0 WHERE backend_id = ? AND model_id = ?",
                )
                .bind(&alias.backend_id)
                .bind(&alias.model_id)
                .execute(&mut *tx)
                .await?;
            }
            sqlx::query(
                "INSERT INTO model_aliases(alias, backend_id, model_id, is_primary) VALUES (?, ?, ?, ?)
                 ON CONFLICT(alias) DO UPDATE SET is_primary = max(model_aliases.is_primary, excluded.is_primary)",
            ).bind(&alias.alias).bind(&alias.backend_id).bind(&alias.model_id)
                .bind(i64::from(alias.is_primary)).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
