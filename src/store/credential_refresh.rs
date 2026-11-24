//! 一次性刷新令牌的持久围栏和提交恢复；调用者持有 credential_lock。

use super::Store;
use crate::core::AppError;
use sqlx::Row;

impl Store {
    /// 建立独立刷新日志，崩溃后不自动重发结果未知的刷新。
    pub(super) async fn migrate_credential_refresh(&self) -> Result<(), AppError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS credential_refresh (
            account_id TEXT PRIMARY KEY, expected TEXT NOT NULL, next TEXT
        )",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 恢复已成功换票的提交，或丢弃已被重新登录替代的旧围栏。
    pub async fn recover_credential_refresh(
        &self,
        id: &str,
        current: &str,
    ) -> Result<(), AppError> {
        let memory = self.refresh_results.lock().await.get(id).cloned();
        let row = sqlx::query("SELECT expected, next FROM credential_refresh WHERE account_id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        let saved = row.map(|row| {
            (
                row.get::<String, _>("expected"),
                row.get::<Option<String>, _>("next"),
            )
        });
        if let Some((expected, next)) = memory
            && expected == current
        {
            return self.finish_credential_refresh(id, &expected, &next).await;
        }
        if let Some((expected, next)) = saved {
            if expected != current {
                self.abort_credential_refresh(id).await?;
            } else if let Some(next) = next {
                self.finish_credential_refresh(id, &expected, &next).await?;
            } else {
                return Err(AppError::bad_request(
                    "上次令牌刷新结果未知，为避免重复使用一次性令牌，请重新网页登录",
                ));
            }
        }
        Ok(())
    }

    /// 网络请求前写入围栏；重复调用不能再次使用同一旧凭证。
    pub async fn begin_credential_refresh(&self, id: &str, expected: &str) -> Result<(), AppError> {
        let result = sqlx::query(
            "INSERT OR IGNORE INTO credential_refresh(account_id,expected)
             SELECT id,credential FROM accounts WHERE id=? AND credential=? AND enabled=1",
        )
        .bind(id)
        .bind(expected)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() != 1 {
            return Err(AppError::bad_request("账号已有未完成的刷新，请重新授权"));
        }
        Ok(())
    }

    /// 服务端确定未换票时解除围栏；不能用于超时或解析失败。
    pub async fn abort_credential_refresh(&self, id: &str) -> Result<(), AppError> {
        sqlx::query("DELETE FROM credential_refresh WHERE account_id=?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        self.refresh_results.lock().await.remove(id);
        Ok(())
    }

    /// 先保留新令牌，再持久暂存，最后原子替换账号并删除围栏。
    pub async fn finish_credential_refresh(
        &self,
        id: &str,
        expected: &str,
        next: &str,
    ) -> Result<(), AppError> {
        self.refresh_results
            .lock()
            .await
            .insert(id.into(), (expected.into(), next.into()));
        sqlx::query("UPDATE credential_refresh SET next=? WHERE account_id=? AND expected=?")
            .bind(next)
            .bind(id)
            .bind(expected)
            .execute(&self.pool)
            .await?;
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE accounts SET credential=?, updated_at=unixepoch() WHERE id=? AND credential=?",
        )
        .bind(next)
        .bind(id)
        .bind(expected)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM credential_refresh WHERE account_id=? AND expected=?")
            .bind(id)
            .bind(expected)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.refresh_results.lock().await.remove(id);
        Ok(())
    }
}
