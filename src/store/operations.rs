//! 持久操作意图与终态；崩溃后不重放曾经发出的远程动作。

use super::Store;
use crate::{
    application::{config_types::error, operation_service::Operation},
    core::AppError,
};
use sqlx::Row;

impl Store {
    /// 幂等查询在服务现状校验前执行，已删除的服务也能取回原操作结果。
    pub async fn operation_by_key(
        &self,
        actor: &str,
        key: &str,
        digest: &str,
    ) -> Result<Option<Operation>, AppError> {
        let row = sqlx::query("SELECT digest,document FROM operations WHERE actor=? AND key=?")
            .bind(actor)
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        if row.get::<String, _>("digest") != digest {
            return Err(error("idempotency_conflict", "同一幂等键不能用于不同操作"));
        }
        Ok(Some(decode(&row.get::<String, _>("document"))?))
    }
    /// 启动时把非终态标为未知；人工交互会话在重启后不可恢复。
    pub(super) async fn migrate_operations(&self) -> Result<(), AppError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS operations (
            id TEXT PRIMARY KEY, actor TEXT NOT NULL, key TEXT NOT NULL,
            digest TEXT NOT NULL, status TEXT NOT NULL, document TEXT NOT NULL,
            UNIQUE(actor,key))",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("UPDATE operations SET status='unknown',
            document=json_set(document,'$.status','unknown','$.next_action',json('null'),
                '$.error',json('{\"code\":\"execution_unknown\",\"message\":\"进程已重启，原执行结果无法确认；不会自动重放\",\"retryable\":false}'))
            WHERE status IN ('pending','running','waiting_user')")
            .execute(&self.pool).await?;
        Ok(())
    }

    /// 幂等键和调用者共同约束输入摘要，并发请求只有一个获得执行权。
    pub async fn create_operation(
        &self,
        operation: &Operation,
        key: &str,
        digest: &str,
    ) -> Result<(Operation, bool), AppError> {
        let inserted = sqlx::query(
            "INSERT OR IGNORE INTO operations(id,actor,key,digest,status,document) VALUES(?,?,?,?,?,?)",
        ).bind(&operation.operation_id).bind(&operation.actor).bind(key).bind(digest)
            .bind(&operation.status).bind(encode(operation)?).execute(&self.pool).await?;
        let row = sqlx::query("SELECT digest,document FROM operations WHERE actor=? AND key=?")
            .bind(&operation.actor)
            .bind(key)
            .fetch_one(&self.pool)
            .await?;
        if row.get::<String, _>("digest") != digest {
            return Err(error("idempotency_conflict", "同一幂等键不能用于不同操作"));
        }
        Ok((
            decode(&row.get::<String, _>("document"))?,
            inserted.rows_affected() == 1,
        ))
    }

    /// 状态和文档同次更新，调用者读取不会遇到半个操作快照。
    pub async fn save_operation(&self, operation: &Operation) -> Result<(), AppError> {
        sqlx::query("UPDATE operations SET status=?,document=? WHERE id=?")
            .bind(&operation.status)
            .bind(encode(operation)?)
            .bind(&operation.operation_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 查询单个操作；身份检查由用例执行。
    pub async fn operation(&self, id: &str) -> Result<Operation, AppError> {
        let text: Option<String> = sqlx::query_scalar("SELECT document FROM operations WHERE id=?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        decode(&text.ok_or_else(|| AppError::NotFound {
            message: "操作不存在".into(),
        })?)
    }

    /// 操作历史仅返回最近记录，默认调用者只能看到自己的操作。
    pub async fn operations(&self, actor: Option<&str>) -> Result<Vec<Operation>, AppError> {
        let texts: Vec<String> = sqlx::query_scalar(
            "SELECT document FROM operations WHERE (? IS NULL OR actor=?) ORDER BY rowid DESC LIMIT 100",
        ).bind(actor).bind(actor).fetch_all(&self.pool).await?;
        texts.iter().map(|s| decode(s)).collect()
    }
}

/// 存储序列化异常不能包含操作数据。
fn encode(value: &Operation) -> Result<String, AppError> {
    serde_json::to_string(value).map_err(|_| error("operation_invalid", "操作记录无法序列化"))
}

/// 操作内容损坏时显式失败，禁止默认为待执行。
fn decode(text: &str) -> Result<Operation, AppError> {
    serde_json::from_str(text).map_err(|_| error("operation_invalid", "操作记录无法读取"))
}
