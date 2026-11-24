//! 移除旧内部 TRAE 后端的持久配置；个人版使用独立后端类型。

use super::Store;
use crate::core::AppError;

impl Store {
    /// 原子清理退役后端及账号，不影响其他服务或历史用量；重复启动保持幂等。
    pub(super) async fn remove_retired_trae(&self) -> Result<(), AppError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "DELETE FROM credential_refresh WHERE account_id IN (
                SELECT id FROM accounts WHERE backend_id IN (
                    SELECT id FROM backends WHERE kind = 'trae'
                )
            )",
        )
        .execute(&mut *transaction)
        .await?;
        // 额度缓存由现有账号和后端删除触发器清理。
        for statement in [
            "DELETE FROM model_aliases WHERE backend_id IN (SELECT id FROM backends WHERE kind = 'trae')",
            "DELETE FROM backend_model_catalog WHERE backend_id IN (SELECT id FROM backends WHERE kind = 'trae')",
            "DELETE FROM tasks WHERE backend_id IN (SELECT id FROM backends WHERE kind = 'trae')",
            "DELETE FROM accounts WHERE backend_id IN (SELECT id FROM backends WHERE kind = 'trae')",
        ] {
            sqlx::query(statement).execute(&mut *transaction).await?;
        }
        sqlx::query("DELETE FROM backends WHERE kind = 'trae'")
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::Account;
    use crate::store::BackendRecord;
    use serde_json::json;

    /// 旧内部账号和目录清理后，个人版独立类型、其他服务及历史用量仍然保留。
    #[tokio::test]
    async fn retirement_removes_internal_records_without_touching_other_backends() {
        let store = Store::open(std::path::Path::new(":memory:")).await.unwrap();
        for (id, kind) in [
            ("old", "trae"),
            ("personal", "trae_personal"),
            ("other", "openai_compat"),
        ] {
            store
                .upsert_backend(&BackendRecord {
                    id: id.into(),
                    name: id.into(),
                    kind: kind.into(),
                    models: "model".into(),
                    ..Default::default()
                })
                .await
                .unwrap();
            store
                .upsert_account(&Account {
                    id: format!("{id}-account"),
                    backend_id: id.into(),
                    name: id.into(),
                    credential: "{}".into(),
                    enabled: true,
                })
                .await
                .unwrap();
            let model = serde_json::from_value(json!({"id":"model"})).unwrap();
            store.sync_model_catalog(id, &[model]).await.unwrap();
        }
        let records = store.list_backends().await.unwrap();
        let catalogs = store.list_model_catalogs().await.unwrap();
        store
            .ensure_model_aliases(&records, &catalogs)
            .await
            .unwrap();
        store.add_usage("old", "2026-11-21", 10).await.unwrap();
        store.remove_retired_trae().await.unwrap();
        store.remove_retired_trae().await.unwrap();
        assert!(store.get_backend("old").await.unwrap().is_none());
        assert!(store.get_account("old-account").await.unwrap().is_none());
        assert!(
            !store
                .list_model_catalogs()
                .await
                .unwrap()
                .contains_key("old")
        );
        assert!(
            !store
                .list_model_aliases()
                .await
                .unwrap()
                .iter()
                .any(|a| a.backend_id == "old")
        );
        for id in ["personal", "other"] {
            assert!(store.get_backend(id).await.unwrap().is_some());
            assert!(
                store
                    .get_account(&format!("{id}-account"))
                    .await
                    .unwrap()
                    .is_some()
            );
        }
        assert_eq!(
            store.load_usage("2026-11-21").await.unwrap(),
            vec![("old".into(), 10)]
        );
    }
}
