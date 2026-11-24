//! 配置版本、计划、秘密引用和兼容投影在同一 SQLite 事务提交。

use super::Store;
use crate::{
    accounts::Account,
    application::{
        config_service::{ApplyResult, ConfigPlan},
        config_types::{ConfigDocument, error},
    },
    core::AppError,
    store::BackendRecord,
};
use sqlx::Row;
use std::collections::BTreeMap;

impl Store {
    /// 增量建立控制平面表；自动续期同步当前引用，不递增配置 revision。
    pub(super) async fn migrate_configuration(&self) -> Result<(), AppError> {
        for statement in [
            "CREATE TABLE IF NOT EXISTS config_head (id INTEGER PRIMARY KEY CHECK(id=1), revision INTEGER NOT NULL)",
            "INSERT OR IGNORE INTO config_head VALUES (1,0)",
            "CREATE TABLE IF NOT EXISTS config_revisions (revision INTEGER PRIMARY KEY, document TEXT NOT NULL, created_at INTEGER NOT NULL)",
            "CREATE TABLE IF NOT EXISTS config_plans (id TEXT PRIMARY KEY, plan TEXT NOT NULL, document TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS config_applies (key TEXT PRIMARY KEY, digest TEXT NOT NULL, result TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS config_audit (revision INTEGER PRIMARY KEY, actor TEXT NOT NULL, summary TEXT NOT NULL, created_at INTEGER NOT NULL)",
            "CREATE TABLE IF NOT EXISTS config_secrets (ref TEXT PRIMARY KEY, value TEXT NOT NULL, version INTEGER NOT NULL DEFAULT 1)",
            "CREATE TABLE IF NOT EXISTS account_secret_links (account_id TEXT PRIMARY KEY, ref TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS secret_audit (ref TEXT PRIMARY KEY, actor TEXT NOT NULL, created_at INTEGER NOT NULL)",
            "CREATE TRIGGER IF NOT EXISTS rotate_config_secret AFTER UPDATE OF credential ON accounts
             WHEN OLD.credential != NEW.credential BEGIN
               UPDATE config_secrets SET value=NEW.credential, version=version+1
               WHERE ref=(SELECT ref FROM account_secret_links WHERE account_id=NEW.id);
             END",
        ] {
            sqlx::query(statement).execute(&self.pool).await?;
        }
        Ok(())
    }

    /// 读取已提交版本及不含秘密值的规范文档。
    pub async fn config_current(&self) -> Result<(u64, Option<ConfigDocument>), AppError> {
        let row = sqlx::query(
            "SELECT h.revision,r.document FROM config_head h LEFT JOIN config_revisions r ON r.revision=h.revision WHERE h.id=1",
        ).fetch_one(&self.pool).await?;
        let document: Option<String> = row.get("document");
        Ok((
            row.get::<i64, _>("revision") as u64,
            document.map(|text| decode(&text)).transpose()?,
        ))
    }

    /// 回滚只读取配置内容，执行仍由统一 apply 产生新版本。
    pub async fn config_revision(&self, revision: u64) -> Result<ConfigDocument, AppError> {
        let text: Option<String> =
            sqlx::query_scalar("SELECT document FROM config_revisions WHERE revision=?")
                .bind(revision as i64)
                .fetch_optional(&self.pool)
                .await?;
        decode(&text.ok_or_else(|| error("revision_not_found", "配置版本不存在"))?)
    }

    /// 只解析宿主库引用；env 引用由应用服务在服务器进程内读取。
    pub async fn config_secret(&self, reference: &str) -> Result<Option<String>, AppError> {
        Ok(
            sqlx::query_scalar("SELECT value FROM config_secrets WHERE ref=?")
                .bind(reference)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// 独立秘密写入只创建引用，不改变运行实例；审计不包含秘密值。
    pub async fn insert_config_secret(
        &self,
        reference: &str,
        value: &str,
        actor: &str,
    ) -> Result<(), AppError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO config_secrets(ref,value) VALUES(?,?)")
            .bind(reference)
            .bind(value)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO secret_audit(ref,actor,created_at) VALUES(?,?,unixepoch())")
            .bind(reference)
            .bind(actor)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// 保存经校验的计划；计划含凭证引用，无 API Key、token 或敏感请求头。
    pub async fn save_config_plan(
        &self,
        plan: &ConfigPlan,
        document: &ConfigDocument,
    ) -> Result<(), AppError> {
        sqlx::query("INSERT INTO config_plans (id,plan,document) VALUES (?,?,?)")
            .bind(&plan.id)
            .bind(encode(plan)?)
            .bind(encode(document)?)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 跨进程重启读取计划，并由应用层检查过期时间和契约指纹。
    pub async fn config_plan(&self, id: &str) -> Result<(ConfigPlan, ConfigDocument), AppError> {
        let row = sqlx::query("SELECT plan,document FROM config_plans WHERE id=?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| error("plan_not_found", "配置计划不存在"))?;
        Ok((
            decode(&row.get::<String, _>("plan"))?,
            decode(&row.get::<String, _>("document"))?,
        ))
    }

    /// 同键同摘要返回原结果，同键不同输入绝不复用。
    pub async fn config_apply_result(
        &self,
        key: &str,
        digest: &str,
    ) -> Result<Option<ApplyResult>, AppError> {
        let row = sqlx::query("SELECT digest,result FROM config_applies WHERE key=?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| {
            if row.get::<String, _>("digest") != digest {
                return Err(error("idempotency_conflict", "幂等键已用于其他请求"));
            }
            decode(&row.get::<String, _>("result"))
        })
        .transpose()
    }

    /// 首次升级只建立基线，现有配置与凭证保持原状。
    pub async fn bootstrap_configuration(
        &self,
        document: &ConfigDocument,
        secrets: &BTreeMap<String, String>,
        records: &[BackendRecord],
    ) -> Result<(), AppError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE config_head SET revision=revision WHERE id=1")
            .execute(&mut *tx)
            .await?;
        let exists: i64 = sqlx::query_scalar("SELECT count(*) FROM config_revisions")
            .fetch_one(&mut *tx)
            .await?;
        if exists == 0 {
            // 与运行实例使用完全相同的规范投影，旧字符串差异不能使续期 CAS 失效。
            for record in records {
                sqlx::query("UPDATE backends SET models=?,options=?,capabilities=? WHERE id=?")
                    .bind(&record.models)
                    .bind(&record.options)
                    .bind(&record.capabilities)
                    .bind(&record.id)
                    .execute(&mut *tx)
                    .await?;
            }
            for (reference, value) in secrets {
                sqlx::query("INSERT INTO config_secrets(ref,value) VALUES (?,?)")
                    .bind(reference)
                    .bind(value)
                    .execute(&mut *tx)
                    .await?;
            }
            for account in &document.accounts {
                sqlx::query("INSERT INTO account_secret_links(account_id,ref) VALUES (?,?)")
                    .bind(&account.id)
                    .bind(&account.credential.secret_ref)
                    .execute(&mut *tx)
                    .await?;
            }
            sqlx::query("INSERT INTO config_revisions VALUES (0,?,unixepoch())")
                .bind(encode(document)?)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// 短事务内 CAS、投影、版本、幂等结果和审计一起提交；任何失败整体回滚。
    #[allow(clippy::too_many_arguments)]
    pub async fn commit_configuration(
        &self,
        expected: u64,
        document: &ConfigDocument,
        records: &[BackendRecord],
        accounts: &[Account],
        secrets: &BTreeMap<String, String>,
        key: &str,
        digest: &str,
        actor: &str,
        result: &ApplyResult,
    ) -> Result<(), AppError> {
        let mut tx = self.pool.begin().await?;
        let updated =
            sqlx::query("UPDATE config_head SET revision=revision+1 WHERE id=1 AND revision=?")
                .bind(expected as i64)
                .execute(&mut *tx)
                .await?;
        if updated.rows_affected() != 1 {
            return Err(error("revision_conflict", "配置已变更，请重新生成计划"));
        }
        for (reference, value) in secrets {
            sqlx::query("INSERT INTO config_secrets(ref,value) VALUES (?,?)")
                .bind(reference)
                .bind(value)
                .execute(&mut *tx)
                .await?;
        }
        let service_ids = records.iter().map(|r| r.id.as_str()).collect::<Vec<_>>();
        let account_ids = accounts.iter().map(|a| a.id.as_str()).collect::<Vec<_>>();
        // 按差异删除，不能清表重插：配额与任务结果有独立生命周期。
        let old: Vec<String> = sqlx::query_scalar("SELECT id FROM backends")
            .fetch_all(&mut *tx)
            .await?;
        for id in old.iter().filter(|id| !service_ids.contains(&id.as_str())) {
            sqlx::query("DELETE FROM backends WHERE id=?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM backend_model_catalog WHERE backend_id=?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        let old: Vec<String> = sqlx::query_scalar("SELECT id FROM accounts")
            .fetch_all(&mut *tx)
            .await?;
        for id in old.iter().filter(|id| !account_ids.contains(&id.as_str())) {
            sqlx::query("DELETE FROM accounts WHERE id=?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM account_secret_links WHERE account_id=?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        for record in records {
            sqlx::query(
                "DELETE FROM backend_model_catalog WHERE backend_id=? AND EXISTS
                (SELECT 1 FROM backends WHERE id=? AND (kind!=? OR base_url!=?
                OR json_remove(CASE WHEN json_valid(options) THEN options ELSE '{}' END,'$.routing')
                   != json_remove(?,'$.routing') OR api_key!=?))",
            )
            .bind(&record.id)
            .bind(&record.id)
            .bind(&record.kind)
            .bind(&record.base_url)
            .bind(&record.options)
            .bind(&record.api_key)
            .execute(&mut *tx)
            .await?;
            sqlx::query("INSERT INTO backends
                (id,name,kind,base_url,api_key,models,enabled,priority,cost_tier,options,capabilities,daily_token_budget,created_at,updated_at)
                VALUES (?,?,?,?,?,?,?,?,?,?,?,?,unixepoch(),unixepoch())
                ON CONFLICT(id) DO UPDATE SET name=excluded.name,kind=excluded.kind,base_url=excluded.base_url,
                api_key=excluded.api_key,models=excluded.models,enabled=excluded.enabled,priority=excluded.priority,
                cost_tier=excluded.cost_tier,options=excluded.options,capabilities=excluded.capabilities,
                daily_token_budget=excluded.daily_token_budget,updated_at=excluded.updated_at")
                .bind(&record.id).bind(&record.name).bind(&record.kind).bind(&record.base_url)
                .bind(&record.api_key).bind(&record.models).bind(record.enabled).bind(record.priority as i64)
                .bind(&record.cost_tier).bind(&record.options).bind(&record.capabilities)
                .bind(record.daily_token_budget as i64).execute(&mut *tx).await?;
        }
        for spec in &document.accounts {
            sqlx::query("INSERT INTO account_secret_links VALUES (?,?) ON CONFLICT(account_id) DO UPDATE SET ref=excluded.ref")
                .bind(&spec.id).bind(&spec.credential.secret_ref).execute(&mut *tx).await?;
        }
        for account in accounts {
            sqlx::query(
                "DELETE FROM account_quota_cache WHERE account_id=? AND EXISTS
                (SELECT 1 FROM accounts WHERE id=? AND (credential!=? OR backend_id!=?))",
            )
            .bind(&account.id)
            .bind(&account.id)
            .bind(&account.credential)
            .bind(&account.backend_id)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO accounts(id,backend_id,name,credential,enabled,created_at,updated_at)
                VALUES (?,?,?,?,?,unixepoch(),unixepoch()) ON CONFLICT(id) DO UPDATE SET
                backend_id=excluded.backend_id,name=excluded.name,credential=excluded.credential,
                enabled=excluded.enabled,updated_at=excluded.updated_at",
            )
            .bind(&account.id)
            .bind(&account.backend_id)
            .bind(&account.name)
            .bind(&account.credential)
            .bind(account.enabled)
            .execute(&mut *tx)
            .await?;
        }
        // 历史别名不删除；先切换主名称标记，避免唯一索引中间冲突。
        sqlx::query("UPDATE model_aliases SET is_primary=0")
            .execute(&mut *tx)
            .await?;
        for alias in &document.model_aliases {
            let owner: Option<(String, String)> =
                sqlx::query_as("SELECT backend_id,model_id FROM model_aliases WHERE alias=?")
                    .bind(&alias.alias)
                    .fetch_optional(&mut *tx)
                    .await?;
            if owner.is_some_and(|(b, m)| b != alias.backend_id || m != alias.model_id) {
                return Err(error("alias_conflict", "调用名称已被其他模型保留"));
            }
            sqlx::query(
                "INSERT INTO model_aliases VALUES (?,?,?,?)
                ON CONFLICT(alias) DO UPDATE SET is_primary=excluded.is_primary",
            )
            .bind(&alias.alias)
            .bind(&alias.backend_id)
            .bind(&alias.model_id)
            .bind(alias.is_primary)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query("INSERT INTO gateway_settings VALUES(1,?) ON CONFLICT(id) DO UPDATE SET settings=excluded.settings")
            .bind(encode(&document.gateway)?).execute(&mut *tx).await?;
        let schedule = &document.checkin;
        sqlx::query(
            "UPDATE checkin_settings SET enabled=?,time=?,random_minutes=?,revision=revision+1
            WHERE id=1 AND (enabled!=? OR time!=? OR random_minutes!=?)",
        )
        .bind(schedule.enabled)
        .bind(&schedule.time)
        .bind(schedule.random_minutes)
        .bind(schedule.enabled)
        .bind(&schedule.time)
        .bind(schedule.random_minutes)
        .execute(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO config_revisions VALUES (?,?,unixepoch())")
            .bind((expected + 1) as i64)
            .bind(encode(document)?)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO config_applies VALUES (?,?,?)")
            .bind(key)
            .bind(digest)
            .bind(encode(result)?)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO config_audit VALUES (?,?,?,unixepoch())")
            .bind((expected + 1) as i64)
            .bind(actor)
            .bind(encode(&result.changes)?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
}

/// 存储 JSON 出错视为数据损坏，不回显原始内容。
fn decode<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, AppError> {
    serde_json::from_str(text)
        .map_err(|_| error("stored_config_invalid", "已保存的配置数据无法解析"))
}

/// 序列化固定类型，不把潜在秘密放进错误消息。
fn encode<T: serde::Serialize>(value: &T) -> Result<String, AppError> {
    serde_json::to_string(value).map_err(|_| error("serialization_failed", "配置序列化失败"))
}
