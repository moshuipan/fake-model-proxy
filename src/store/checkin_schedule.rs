//! 签到配置与抽签计划独立保存；配置版本保护执行前的并发修改。

use super::Store;
use crate::core::AppError;
use crate::tasks::scheduler::{CheckinPlan, CheckinSettings, ScheduleInput};
use sqlx::Row;

impl Store {
    /// 增量建表，不改写已有账号和任务记录。
    pub(super) async fn migrate_checkin_schedule(&self) -> Result<(), AppError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS checkin_settings (
                id INTEGER PRIMARY KEY CHECK(id = 1),
                enabled INTEGER NOT NULL, time TEXT NOT NULL,
                random_minutes INTEGER NOT NULL, revision INTEGER NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS checkin_plans (
                backend_id TEXT NOT NULL, account_id TEXT NOT NULL, day TEXT NOT NULL,
                scheduled_at INTEGER NOT NULL, window_end INTEGER NOT NULL,
                revision INTEGER NOT NULL, status TEXT NOT NULL DEFAULT 'pending',
                PRIMARY KEY(backend_id, account_id, day)
            )",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 仅首次创建配置时读取旧环境变量；后续以数据库为准。
    pub async fn checkin_settings(&self) -> Result<CheckinSettings, AppError> {
        let initial = std::env::var("FMP_CHECKIN").ok().as_deref() == Some("1");
        sqlx::query(
            "INSERT OR IGNORE INTO checkin_settings
             (id, enabled, time, random_minutes, revision) VALUES (1, ?, '10:00', 60, 1)",
        )
        .bind(initial)
        .execute(&self.pool)
        .await?;
        let row = sqlx::query(
            "SELECT enabled, time, random_minutes, revision FROM checkin_settings WHERE id = 1",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(CheckinSettings {
            enabled: row.get("enabled"),
            time: row.get("time"),
            random_minutes: row.get("random_minutes"),
            revision: row.get("revision"),
        })
    }

    /// 相同配置不重新抽签；任何真实修改递增版本，执行过的计划保留。
    pub async fn save_checkin_settings(
        &self,
        input: &ScheduleInput,
    ) -> Result<CheckinSettings, AppError> {
        input.validate()?;
        self.checkin_settings().await?;
        sqlx::query(
            "UPDATE checkin_settings SET enabled = ?, time = ?, random_minutes = ?,
             revision = revision + 1 WHERE id = 1
             AND (enabled != ? OR time != ? OR random_minutes != ?)",
        )
        .bind(input.enabled)
        .bind(&input.time)
        .bind(input.random_minutes)
        .bind(input.enabled)
        .bind(&input.time)
        .bind(input.random_minutes)
        .execute(&self.pool)
        .await?;
        self.checkin_settings().await
    }

    /// 在当前配置版本下保存抽签结果；只有未执行且版本过期的计划允许重排。
    pub async fn save_checkin_plan(&self, plan: &CheckinPlan) -> Result<(), AppError> {
        sqlx::query(
            "INSERT INTO checkin_plans
             (backend_id, account_id, day, scheduled_at, window_end, revision, status)
             SELECT ?, ?, ?, ?, ?, ?, ?
             WHERE EXISTS(SELECT 1 FROM checkin_settings WHERE id = 1 AND revision = ?)
             ON CONFLICT(backend_id, account_id, day) DO UPDATE SET
             scheduled_at = excluded.scheduled_at, window_end = excluded.window_end,
             revision = excluded.revision, status = excluded.status
             WHERE checkin_plans.revision != excluded.revision
             AND checkin_plans.status IN ('pending', 'missed')",
        )
        .bind(&plan.backend_id)
        .bind(&plan.account_id)
        .bind(&plan.day)
        .bind(plan.scheduled_at)
        .bind(plan.window_end)
        .bind(plan.revision)
        .bind(&plan.status)
        .bind(plan.revision)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 读取未来两天计划，供调度与页面共享同一份随机结果。
    pub async fn checkin_plans(&self, from_day: &str) -> Result<Vec<CheckinPlan>, AppError> {
        let rows = sqlx::query(
            "SELECT backend_id, account_id, day, scheduled_at, window_end, revision, status
             FROM checkin_plans WHERE day >= ? ORDER BY scheduled_at, backend_id, account_id",
        )
        .bind(from_day)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| CheckinPlan {
                backend_id: row.get("backend_id"),
                account_id: row.get("account_id"),
                day: row.get("day"),
                scheduled_at: row.get("scheduled_at"),
                window_end: row.get("window_end"),
                revision: row.get("revision"),
                status: row.get("status"),
            })
            .collect())
    }

    /// 到期未执行的计划不补签；保留近期记录，清理一个月以前的计划元数据。
    pub async fn expire_checkin_plans(&self, now: i64) -> Result<(), AppError> {
        sqlx::query(
            "UPDATE checkin_plans SET status = 'missed'
             WHERE status = 'pending' AND window_end + 60 < ?",
        )
        .bind(now)
        .execute(&self.pool)
        .await?;
        sqlx::query("DELETE FROM checkin_plans WHERE window_end < ? AND status != 'running'")
            .bind(now - 31 * 86_400)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 执行前原子占用计划，并再次校验开关、版本、时间和账号状态。
    pub async fn claim_checkin_plan(&self, plan: &CheckinPlan, now: i64) -> Result<bool, AppError> {
        let result = sqlx::query(
            "UPDATE checkin_plans SET status = 'running'
             WHERE backend_id = ? AND account_id = ? AND day = ?
             AND status = 'pending' AND revision = ? AND scheduled_at <= ?
             AND window_end + 60 >= ?
             AND EXISTS(SELECT 1 FROM checkin_settings
                        WHERE id = 1 AND enabled = 1 AND revision = ?)
             AND EXISTS(SELECT 1 FROM accounts
                        WHERE id = ? AND backend_id = ? AND enabled = 1)
             AND EXISTS(SELECT 1 FROM backends WHERE id = ? AND enabled = 1)",
        )
        .bind(&plan.backend_id)
        .bind(&plan.account_id)
        .bind(&plan.day)
        .bind(plan.revision)
        .bind(now)
        .bind(now)
        .bind(plan.revision)
        .bind(&plan.account_id)
        .bind(&plan.backend_id)
        .bind(&plan.backend_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// 领取结果落盘失败时保持 running，重启按未知结果恢复。
    pub async fn finish_checkin_plan(
        &self,
        plan: &CheckinPlan,
        status: &str,
    ) -> Result<(), AppError> {
        sqlx::query(
            "UPDATE checkin_plans SET status = ?
             WHERE backend_id = ? AND account_id = ? AND day = ? AND status = 'running'",
        )
        .bind(status)
        .bind(&plan.backend_id)
        .bind(&plan.account_id)
        .bind(&plan.day)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 进程中断后无法证明领取与记录完整完成，自动和手动入口均禁止重放。
    pub async fn recover_checkin_plans(&self) -> Result<(), AppError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO tasks
             (backend_id, account_id, kind, period, status, message, amount, updated_at)
             SELECT backend_id, account_id, 'checkin', day, 'unknown',
                    '自动签到执行时服务中断，请核对上游领取结果', NULL, unixepoch()
             FROM checkin_plans WHERE status = 'running'
             ON CONFLICT(backend_id, account_id, kind, period) DO UPDATE SET
             status = 'unknown', message = excluded.message, updated_at = excluded.updated_at",
        )
        .execute(&mut *transaction)
        .await?;
        sqlx::query("UPDATE checkin_plans SET status = 'unknown' WHERE status = 'running'")
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }
}
