//! 持久化账号额度快照和十二小时刷新进度；缓存不保存凭证。

use super::{BackendRecord, Store};
use crate::accounts::{Account, remote::QuotaInfo};
use crate::core::AppError;
use serde::Serialize;
use sqlx::Row;

/// 最近一批更新和下一次更新时间，重启后继续沿用。
#[derive(Debug, Clone, Default, Serialize)]
pub struct QuotaRefreshRecord {
    pub generation: u64,
    pub last_started_at: Option<u64>,
    pub last_completed_at: Option<u64>,
    pub next_refresh_at: Option<u64>,
    pub last_error: Option<String>,
}

/// 用服务的额度接口配置识别缓存，切换区域或端点后旧缓存不再展示。
fn config_key(record: &BackendRecord) -> String {
    let source = crate::plugins::service::catalog_identity(record).to_string();
    format!("{:x}", md5::compute(source))
}

/// 仅发布仍启用且配置相同的快照；单账号更新还校验查询时的凭证。
async fn publish(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    backend: &BackendRecord,
    info: &QuotaInfo,
    credential: Option<&str>,
) -> Result<bool, AppError> {
    let snapshot = serde_json::to_string(info).map_err(|_| AppError::Internal {
        message: "serialize quota snapshot failed".into(),
    })?;
    let result = sqlx::query(
        "INSERT INTO account_quota_cache (account_id, backend_id, config_key, snapshot)
         SELECT a.id, b.id, ?, ? FROM accounts a JOIN backends b ON b.id = a.backend_id
         WHERE a.id = ? AND b.id = ? AND a.enabled = 1 AND b.enabled = 1
           AND b.kind = ? AND b.base_url = ?
           AND json_remove(CASE WHEN json_valid(b.options) THEN b.options ELSE '{}' END,'$.routing')
               = json_remove(CASE WHEN json_valid(?) THEN ? ELSE '{}' END,'$.routing')
           AND b.api_key = ?
           AND (? IS NULL OR a.credential = ?)
         ON CONFLICT(account_id) DO UPDATE SET
           backend_id = excluded.backend_id, config_key = excluded.config_key,
           snapshot = excluded.snapshot",
    )
    .bind(config_key(backend))
    .bind(snapshot)
    .bind(&info.account_id)
    .bind(&backend.id)
    .bind(&backend.kind)
    .bind(&backend.base_url)
    .bind(&backend.options)
    .bind(&backend.options)
    .bind(&backend.api_key)
    .bind(credential)
    .bind(credential)
    .execute(&mut **transaction)
    .await?;
    Ok(result.rows_affected() > 0)
}

impl Store {
    /// 建立缓存与单行调度进度，不改动账号或后端配置。
    pub(super) async fn migrate_quota_cache(&self) -> Result<(), AppError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS account_quota_cache (
                account_id TEXT PRIMARY KEY,
                backend_id TEXT NOT NULL,
                config_key TEXT NOT NULL,
                snapshot TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS quota_refresh_state (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                generation INTEGER NOT NULL DEFAULT 0,
                last_started_at INTEGER,
                last_completed_at INTEGER,
                next_refresh_at INTEGER,
                last_error TEXT
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("INSERT OR IGNORE INTO quota_refresh_state (id) VALUES (1)")
            .execute(&self.pool)
            .await?;
        sqlx::query(
            "CREATE TRIGGER IF NOT EXISTS invalidate_account_quota
             AFTER UPDATE OF backend_id, enabled ON accounts
             WHEN NEW.backend_id != OLD.backend_id OR NEW.enabled != OLD.enabled
             BEGIN DELETE FROM account_quota_cache WHERE account_id = OLD.id; END",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TRIGGER IF NOT EXISTS delete_account_quota AFTER DELETE ON accounts
             BEGIN DELETE FROM account_quota_cache WHERE account_id = OLD.id; END",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TRIGGER IF NOT EXISTS delete_backend_quota AFTER DELETE ON backends
             BEGIN DELETE FROM account_quota_cache WHERE backend_id = OLD.id; END",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 读取调度进度；时间使用 Unix 秒，失败不伪造最近成功时间。
    pub async fn quota_refresh_record(&self) -> Result<QuotaRefreshRecord, AppError> {
        let row = sqlx::query("SELECT * FROM quota_refresh_state WHERE id = 1")
            .fetch_one(&self.pool)
            .await?;
        Ok(QuotaRefreshRecord {
            generation: row.get::<i64, _>("generation").max(0) as u64,
            last_started_at: row
                .get::<Option<i64>, _>("last_started_at")
                .map(|value| value.max(0) as u64),
            last_completed_at: row
                .get::<Option<i64>, _>("last_completed_at")
                .map(|value| value.max(0) as u64),
            next_refresh_at: row
                .get::<Option<i64>, _>("next_refresh_at")
                .map(|value| value.max(0) as u64),
            last_error: row.get("last_error"),
        })
    }

    /// 开始时仅记录尝试；进程中断后旧的到期时间允许重新执行查询。
    pub async fn start_quota_refresh(&self, now: u64) -> Result<(), AppError> {
        sqlx::query("UPDATE quota_refresh_state SET last_started_at = ? WHERE id = 1")
            .bind(now as i64)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 原子发布一批额度和下一次时间，忽略查询期间删除或停用的配置。
    pub async fn finish_quota_refresh(
        &self,
        entries: &[(BackendRecord, QuotaInfo)],
        now: u64,
        next: u64,
    ) -> Result<(), AppError> {
        let mut transaction = self.pool.begin().await?;
        for (backend, info) in entries {
            publish(&mut transaction, backend, info, None).await?;
        }
        let unknown = entries
            .iter()
            .filter(|(_, info)| {
                info.confidence == crate::accounts::remote::QuotaConfidence::Unknown
            })
            .count();
        sqlx::query(
            "UPDATE quota_refresh_state SET generation = generation + 1,
                last_completed_at = ?, next_refresh_at = ?, last_error = ? WHERE id = 1",
        )
        .bind(now as i64)
        .bind(next as i64)
        .bind((unknown > 0).then(|| format!("{unknown} 个账号暂未读取成功")))
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    /// 签到后原子更新一个账号并递增快照版本，保留十二小时全量计划。
    pub async fn cache_account_quota(
        &self,
        backend: &BackendRecord,
        account: &Account,
        info: &QuotaInfo,
    ) -> Result<bool, AppError> {
        if account.backend_id != backend.id
            || info.backend_id != backend.id
            || info.account_id != account.id
        {
            return Err(AppError::bad_request("额度快照与账号或服务不匹配"));
        }
        let mut transaction = self.pool.begin().await?;
        let updated = publish(&mut transaction, backend, info, Some(&account.credential)).await?;
        if updated {
            sqlx::query("UPDATE quota_refresh_state SET generation = generation + 1 WHERE id = 1")
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await?;
        Ok(updated)
    }

    /// 仅返回当前启用的账号与服务快照，区域或接口已变更的缓存保持缺失。
    pub async fn cached_quota(&self) -> Result<Vec<QuotaInfo>, AppError> {
        let backends = self.list_backends().await?;
        let rows = sqlx::query(
            "SELECT q.snapshot, q.backend_id, q.config_key FROM account_quota_cache q
             JOIN accounts a ON a.id = q.account_id AND a.backend_id = q.backend_id
             JOIN backends b ON b.id = q.backend_id
             WHERE a.enabled = 1 AND b.enabled = 1 ORDER BY q.backend_id, q.account_id",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut results = Vec::new();
        for row in rows {
            let id: String = row.get("backend_id");
            let key: String = row.get("config_key");
            if !backends
                .iter()
                .any(|backend| backend.id == id && config_key(backend) == key)
            {
                continue;
            }
            let snapshot: String = row.get("snapshot");
            results.push(
                serde_json::from_str(&snapshot).map_err(|_| AppError::Internal {
                    message: "stored quota snapshot is invalid".into(),
                })?,
            );
        }
        Ok(results)
    }

    /// 额度拒绝只失效该账号的旧快照，迟到失败不能删除后来刷新出的额度。
    pub async fn invalidate_quota(
        &self,
        backend: &str,
        account: &str,
        checked: u64,
    ) -> Result<(), AppError> {
        sqlx::query(
            "DELETE FROM account_quota_cache WHERE backend_id = ? AND account_id = ?
            AND json_extract(snapshot, '$.checked_at') <= ?",
        )
        .bind(backend)
        .bind(account)
        .bind(checked.min(i64::MAX as u64) as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
