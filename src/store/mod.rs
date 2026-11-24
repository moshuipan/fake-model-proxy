//! SQLite 持久化。
//!
//! 运行配置以数据库为唯一事实源；热路径读取内存快照，不在网络流期间持有写事务。

use crate::accounts::Account;
use crate::core::AppError;
use crate::core::ModelInfo;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

mod checkin_schedule;
mod configuration;
mod credential_refresh;
mod gateway;
mod model_aliases;
mod modelscope;
mod operations;
mod plugin_state;
mod presets;
pub(crate) mod quota_cache;
pub use quota_cache::QuotaRefreshRecord;

/// 后端配置记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendRecord {
    pub id: String,
    pub name: String,
    /// 后端类型，首版只有 `openai_compat`。
    pub kind: String,
    pub base_url: String,
    pub api_key: String,
    /// 逗号分隔的模型列表；为空表示由上游 `/models` 返回。
    pub models: String,
    /// 是否启用。
    pub enabled: bool,
    /// auto 排序优先级，数值越小越优先。
    #[serde(default = "default_priority")]
    pub priority: u8,
    /// 费用层级：free / subscription / paid / unknown。
    #[serde(default = "default_cost_tier")]
    pub cost_tier: String,
    /// 后端专属选项 JSON（自定义头、端点覆盖等）。
    #[serde(default)]
    pub options: String,
    /// 能力覆盖 JSON。为空时使用后端默认能力。
    #[serde(default)]
    pub capabilities: String,
    /// 每日 token 预算。0 或缺失表示不限制。
    #[serde(default)]
    pub daily_token_budget: u64,
}

/// 默认优先级。
fn default_priority() -> u8 {
    100
}

/// 默认费用层级。未知表示不能进入「只用免费」的候选。
fn default_cost_tier() -> String {
    "unknown".to_string()
}

impl Default for BackendRecord {
    /// 提供可编辑的空记录，便于测试与新增流程。
    fn default() -> Self {
        BackendRecord {
            id: String::new(),
            name: String::new(),
            kind: "openai_compat".to_string(),
            base_url: String::new(),
            api_key: String::new(),
            models: String::new(),
            enabled: true,
            priority: default_priority(),
            cost_tier: default_cost_tier(),
            options: String::new(),
            capabilities: String::new(),
            daily_token_budget: 0,
        }
    }
}

/// 存储句柄。
#[derive(Debug, Clone)]
pub struct Store {
    pool: SqlitePool,
    /// 凭证续期锁跨后端重载共享，防止并发旋转刷新令牌。
    credential_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
    /// 任务检查、领取和落盘共享一把锁，防止手动执行与调度器同时领取。
    task_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
    /// 已换到但暂未成功落盘的令牌，跨热重载保留，不能再花旧 refresh token。
    refresh_results: std::sync::Arc<tokio::sync::Mutex<HashMap<String, (String, String)>>>,
}

impl Store {
    /// 打开数据库并执行迁移。数据目录不存在时自动创建。
    pub async fn open(path: &Path) -> Result<Self, AppError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| AppError::Internal {
                message: format!("create data directory failed: {error}"),
            })?;
        }
        // 文件路径直接传给驱动，避免目录名中的百分号被 URL 解码。
        // 内存数据库保留 SQLx 生成的独立共享缓存，供连接池中的连接共用。
        let options = if path == Path::new(":memory:") {
            SqliteConnectOptions::from_str("sqlite::memory:").map_err(|error| {
                AppError::Internal {
                    message: format!("invalid database path: {error}"),
                }
            })?
        } else {
            SqliteConnectOptions::new().filename(path)
        };
        let options = options
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        let store = Store {
            pool,
            credential_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            task_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            refresh_results: Default::default(),
        };
        store.migrate().await?;
        store.migrate_credential_refresh().await?;
        store.migrate_plugin_state().await?;
        store.migrate_configuration().await?;
        store.migrate_operations().await?;
        store.migrate_presets().await?;
        Ok(store)
    }

    /// 串行化短时凭证续期；锁不覆盖聊天流或普通账号读取。
    pub async fn lock_credentials(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.credential_lock.lock().await
    }

    /// 个人服务的任务串行执行，锁跨注册表重载共享，不覆盖聊天请求。
    pub async fn lock_tasks(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.task_lock.lock().await
    }

    /// 仅在原凭证仍匹配时更新，避免覆盖人工编辑或复活已删除的账号。
    pub async fn replace_credential(
        &self,
        id: &str,
        expected: &str,
        credential: &str,
    ) -> Result<bool, AppError> {
        let result = sqlx::query(
            "UPDATE accounts SET credential = ?, updated_at = unixepoch() WHERE id = ? AND credential = ?",
        )
        .bind(credential)
        .bind(id)
        .bind(expected)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// 续期提交同时验证凭证、账号归属及服务配置，关闭读取校验到写回之间的竞态。
    pub async fn replace_backend_credential(
        &self,
        account: &Account,
        backend: &BackendRecord,
        credential: &str,
    ) -> Result<bool, AppError> {
        if account.backend_id != backend.id {
            return Err(AppError::bad_request("续期账号与服务不匹配"));
        }
        let result = sqlx::query(
            "UPDATE accounts SET credential = ?, updated_at = unixepoch()
             WHERE id = ? AND credential = ? AND enabled = 1 AND backend_id = ?
             AND EXISTS (SELECT 1 FROM backends b WHERE b.id = accounts.backend_id
                  AND b.enabled = 1 AND b.kind = ? AND b.base_url = ?
                  AND json_remove(CASE WHEN json_valid(b.options) THEN b.options ELSE '{}' END,'$.routing')
                      = json_remove(CASE WHEN json_valid(?) THEN ? ELSE '{}' END,'$.routing'))",
        )
        .bind(credential)
        .bind(&account.id)
        .bind(&account.credential)
        .bind(&backend.id)
        .bind(&backend.kind)
        .bind(&backend.base_url)
        .bind(&backend.options)
        .bind(&backend.options)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// 建表与增量迁移。
    async fn migrate(&self) -> Result<(), AppError> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS backends (
                id          TEXT PRIMARY KEY,
                name        TEXT NOT NULL,
                kind        TEXT NOT NULL,
                base_url    TEXT NOT NULL,
                api_key     TEXT NOT NULL,
                models      TEXT NOT NULL DEFAULT '',
                enabled     INTEGER NOT NULL DEFAULT 1,
                priority    INTEGER NOT NULL DEFAULT 100,
                cost_tier   TEXT NOT NULL DEFAULT 'unknown',
                options     TEXT NOT NULL DEFAULT '',
                capabilities TEXT NOT NULL DEFAULT '',
                daily_token_budget INTEGER NOT NULL DEFAULT 0,
                created_at  INTEGER NOT NULL,
                updated_at  INTEGER NOT NULL
            )
            "#,
        )
        .execute(&self.pool)
        .await?;
        // 旧库补齐新增列。SQL 保持字面量，符合 sqlx 的安全约束。
        let rows = sqlx::query("PRAGMA table_info(backends)")
            .fetch_all(&self.pool)
            .await?;
        let mut has_priority = false;
        let mut has_cost_tier = false;
        let mut has_options = false;
        let mut has_capabilities = false;
        let mut has_budget = false;
        for row in &rows {
            if let Ok(name) = row.try_get::<String, _>("name") {
                match name.as_str() {
                    "priority" => has_priority = true,
                    "cost_tier" => has_cost_tier = true,
                    "options" => has_options = true,
                    "capabilities" => has_capabilities = true,
                    "daily_token_budget" => has_budget = true,
                    _ => {}
                }
            }
        }
        if !has_priority {
            sqlx::query("ALTER TABLE backends ADD COLUMN priority INTEGER NOT NULL DEFAULT 100")
                .execute(&self.pool)
                .await?;
        }
        if !has_cost_tier {
            sqlx::query(
                "ALTER TABLE backends ADD COLUMN cost_tier TEXT NOT NULL DEFAULT 'unknown'",
            )
            .execute(&self.pool)
            .await?;
        }
        if !has_options {
            sqlx::query("ALTER TABLE backends ADD COLUMN options TEXT NOT NULL DEFAULT ''")
                .execute(&self.pool)
                .await?;
        }
        if !has_capabilities {
            sqlx::query("ALTER TABLE backends ADD COLUMN capabilities TEXT NOT NULL DEFAULT ''")
                .execute(&self.pool)
                .await?;
        }
        if !has_budget {
            sqlx::query(
                "ALTER TABLE backends ADD COLUMN daily_token_budget INTEGER NOT NULL DEFAULT 0",
            )
            .execute(&self.pool)
            .await?;
        }
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS accounts (
                id          TEXT PRIMARY KEY,
                backend_id  TEXT NOT NULL,
                name        TEXT NOT NULL,
                credential  TEXT NOT NULL,
                enabled     INTEGER NOT NULL DEFAULT 1,
                created_at  INTEGER NOT NULL,
                updated_at  INTEGER NOT NULL
            )
            "#,
        )
        .execute(&self.pool)
        .await?;
        // 目录是可重建元数据，独立于模型白名单及账号凭证保存。
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS backend_model_catalog (
                backend_id TEXT PRIMARY KEY,
                catalog TEXT NOT NULL,
                updated_at INTEGER NOT NULL
            )
            "#,
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_accounts_backend ON accounts(backend_id)")
            .execute(&self.pool)
            .await?;
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS usage_daily (
                backend_id  TEXT NOT NULL,
                day         TEXT NOT NULL,
                tokens      INTEGER NOT NULL DEFAULT 0,
                requests    INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (backend_id, day)
            )
            "#,
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS tasks (
                backend_id  TEXT NOT NULL,
                account_id  TEXT NOT NULL,
                kind        TEXT NOT NULL,
                period      TEXT NOT NULL,
                status      TEXT NOT NULL,
                message     TEXT NOT NULL DEFAULT '',
                amount      INTEGER,
                updated_at  INTEGER NOT NULL,
                PRIMARY KEY (backend_id, account_id, kind, period)
            )
            "#,
        )
        .execute(&self.pool)
        .await?;
        self.migrate_checkin_schedule().await?;
        self.migrate_model_aliases().await?;
        self.migrate_quota_cache().await?;
        self.migrate_gateway().await?;
        Ok(())
    }

    /// 读取某天的全部用量。
    pub async fn load_usage(&self, day: &str) -> Result<Vec<(String, u64)>, AppError> {
        let rows = sqlx::query("SELECT backend_id, tokens FROM usage_daily WHERE day = ?")
            .bind(day)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                (
                    row.get::<String, _>("backend_id"),
                    row.get::<i64, _>("tokens").max(0) as u64,
                )
            })
            .collect())
    }

    /// 累加某天的用量。
    pub async fn add_usage(
        &self,
        backend_id: &str,
        day: &str,
        tokens: u64,
    ) -> Result<(), AppError> {
        sqlx::query(
            r#"
            INSERT INTO usage_daily (backend_id, day, tokens, requests)
            VALUES (?, ?, ?, 1)
            ON CONFLICT(backend_id, day) DO UPDATE SET
                tokens = tokens + excluded.tokens,
                requests = requests + 1
            "#,
        )
        .bind(backend_id)
        .bind(day)
        .bind(tokens as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 读取某周期内的任务记录。
    pub async fn get_task(
        &self,
        backend_id: &str,
        account_id: &str,
        kind: &str,
        period: &str,
    ) -> Result<Option<crate::tasks::TaskRecord>, AppError> {
        let row = sqlx::query(
            "SELECT backend_id, account_id, kind, period, status, message, amount FROM tasks
             WHERE backend_id = ? AND account_id = ? AND kind = ? AND period = ?",
        )
        .bind(backend_id)
        .bind(account_id)
        .bind(kind)
        .bind(period)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| crate::tasks::TaskRecord {
            backend_id: row.get("backend_id"),
            account_id: row.get("account_id"),
            kind: row.get("kind"),
            period: row.get("period"),
            status: row.get("status"),
            message: row.get("message"),
            amount: row.get("amount"),
        }))
    }

    /// 写入或更新任务记录。
    pub async fn upsert_task(&self, record: &crate::tasks::TaskRecord) -> Result<(), AppError> {
        sqlx::query(
            r#"
            INSERT INTO tasks (backend_id, account_id, kind, period, status, message, amount, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(backend_id, account_id, kind, period) DO UPDATE SET
                status = excluded.status,
                message = excluded.message,
                amount = excluded.amount,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(&record.backend_id)
        .bind(&record.account_id)
        .bind(&record.kind)
        .bind(&record.period)
        .bind(&record.status)
        .bind(&record.message)
        .bind(record.amount)
        .bind(chrono_now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 列出最近的任务记录。
    pub async fn list_tasks(&self, limit: i64) -> Result<Vec<crate::tasks::TaskRecord>, AppError> {
        let rows = sqlx::query(
            "SELECT backend_id, account_id, kind, period, status, message, amount FROM tasks
             ORDER BY updated_at DESC LIMIT ?",
        )
        .bind(limit.clamp(1, 500))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| crate::tasks::TaskRecord {
                backend_id: row.get("backend_id"),
                account_id: row.get("account_id"),
                kind: row.get("kind"),
                period: row.get("period"),
                status: row.get("status"),
                message: row.get("message"),
                amount: row.get("amount"),
            })
            .collect())
    }

    /// 列出全部账号，按创建时间排序。
    pub async fn list_accounts(&self) -> Result<Vec<Account>, AppError> {
        let rows = sqlx::query(
            "SELECT id, backend_id, name, credential, enabled FROM accounts ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_account).collect())
    }

    /// 列出某个后端的账号。
    pub async fn list_accounts_for(&self, backend_id: &str) -> Result<Vec<Account>, AppError> {
        let rows = sqlx::query(
            "SELECT id, backend_id, name, credential, enabled FROM accounts WHERE backend_id = ? ORDER BY created_at",
        )
        .bind(backend_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_account).collect())
    }

    /// 读取单个账号。
    pub async fn get_account(&self, id: &str) -> Result<Option<Account>, AppError> {
        let row = sqlx::query(
            "SELECT id, backend_id, name, credential, enabled FROM accounts WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_account))
    }

    /// 新增或更新账号。
    pub async fn upsert_account(&self, account: &Account) -> Result<(), AppError> {
        let now = chrono_now();
        let mut transaction = self.pool.begin().await?;
        // 人工改凭证后不能展示旧账号额度；自动令牌续期仍沿用同一钱包快照。
        sqlx::query(
            "DELETE FROM account_quota_cache WHERE account_id = ? AND EXISTS (
                SELECT 1 FROM accounts WHERE id = ? AND credential != ?
            )",
        )
        .bind(&account.id)
        .bind(&account.id)
        .bind(&account.credential)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO accounts (id, backend_id, name, credential, enabled, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                backend_id = excluded.backend_id,
                name = excluded.name,
                credential = excluded.credential,
                enabled = excluded.enabled,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(&account.id)
        .bind(&account.backend_id)
        .bind(&account.name)
        .bind(&account.credential)
        .bind(if account.enabled { 1_i64 } else { 0_i64 })
        .bind(now)
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    /// 授权网络请求期间服务可能被删除或改区域，按当前配置原子判断后再保存。
    pub async fn save_login_account(
        &self,
        account: &Account,
        backend: &BackendRecord,
    ) -> Result<bool, AppError> {
        if account.backend_id != backend.id {
            return Err(AppError::bad_request("登录账号与服务不匹配"));
        }
        let now = chrono_now();
        let result = sqlx::query(
            r#"
            INSERT INTO accounts (id, backend_id, name, credential, enabled, created_at, updated_at)
            SELECT ?, id, ?, ?, 1, ?, ? FROM backends
            WHERE id = ? AND kind = ? AND enabled = 1 AND base_url = ? AND options = ?
            ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                credential = excluded.credential,
                enabled = excluded.enabled,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(&account.id)
        .bind(&account.name)
        .bind(&account.credential)
        .bind(now)
        .bind(now)
        .bind(&account.backend_id)
        .bind(&backend.kind)
        .bind(&backend.base_url)
        .bind(&backend.options)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 删除账号，返回是否确实删除。
    pub async fn delete_account(&self, id: &str) -> Result<bool, AppError> {
        let result = sqlx::query("DELETE FROM accounts WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 列出全部后端配置，按创建时间排序。
    pub async fn list_backends(&self) -> Result<Vec<BackendRecord>, AppError> {
        let rows = sqlx::query(
            "SELECT id, name, kind, base_url, api_key, models, enabled, priority, cost_tier, options, capabilities, daily_token_budget FROM backends ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_record).collect())
    }

    /// 读取单个后端配置。
    pub async fn get_backend(&self, id: &str) -> Result<Option<BackendRecord>, AppError> {
        let row = sqlx::query(
            "SELECT id, name, kind, base_url, api_key, models, enabled, priority, cost_tier, options, capabilities, daily_token_budget FROM backends WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_record))
    }

    /// 新增或更新后端配置。
    pub async fn upsert_backend(&self, record: &BackendRecord) -> Result<(), AppError> {
        let now = chrono_now();
        sqlx::query(
            r#"
            INSERT INTO backends (
                id, name, kind, base_url, api_key, models, enabled, priority, cost_tier, options, capabilities, daily_token_budget, created_at, updated_at
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                kind = excluded.kind,
                base_url = excluded.base_url,
                api_key = excluded.api_key,
                models = excluded.models,
                enabled = excluded.enabled,
                priority = excluded.priority,
                cost_tier = excluded.cost_tier,
                options = excluded.options,
                capabilities = excluded.capabilities,
                daily_token_budget = excluded.daily_token_budget,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(&record.id)
        .bind(&record.name)
        .bind(&record.kind)
        .bind(&record.base_url)
        .bind(&record.api_key)
        .bind(&record.models)
        .bind(if record.enabled { 1_i64 } else { 0_i64 })
        .bind(record.priority as i64)
        .bind(&record.cost_tier)
        .bind(&record.options)
        .bind(&record.capabilities)
        .bind(record.daily_token_budget as i64)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 删除后端配置，返回是否确实删除了记录。
    pub async fn delete_backend(&self, id: &str) -> Result<bool, AppError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query("DELETE FROM backend_model_catalog WHERE backend_id = ?")
            .bind(id)
            .execute(&mut *transaction)
            .await?;
        let result = sqlx::query("DELETE FROM backends WHERE id = ?")
            .bind(id)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(result.rows_affected() > 0)
    }

    /// 原子同步模型 ID 白名单与完整目录，仅更新目录相关字段。
    pub async fn sync_model_catalog(
        &self,
        backend_id: &str,
        models: &[ModelInfo],
    ) -> Result<(), AppError> {
        self.sync_catalog(backend_id, models, None).await
    }

    /// 登录产生的目录仅在服务仍启用且配置未变时保存，避免迟到目录跨区域写入。
    pub async fn sync_login_model_catalog(
        &self,
        backend: &BackendRecord,
        models: &[ModelInfo],
    ) -> Result<(), AppError> {
        self.sync_catalog(&backend.id, models, Some(backend)).await
    }

    /// 在同一事务中更新白名单及目录，可为登录流程附加配置匹配条件。
    async fn sync_catalog(
        &self,
        backend_id: &str,
        models: &[ModelInfo],
        expected: Option<&BackendRecord>,
    ) -> Result<(), AppError> {
        let catalog = serde_json::to_string(models).map_err(|_| AppError::Internal {
            message: "serialize model catalog failed".into(),
        })?;
        let ids = models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let now = chrono_now();
        let mut transaction = self.pool.begin().await?;
        let updated = sqlx::query(
            "UPDATE backends SET models = ?, updated_at = ? WHERE id = ? AND (? = 0 OR (enabled = 1 AND kind = ? AND base_url = ? AND options = ?))",
        )
            .bind(ids)
            .bind(now)
            .bind(backend_id)
            .bind(i64::from(expected.is_some()))
            .bind(expected.map(|backend| backend.kind.as_str()).unwrap_or(""))
            .bind(expected.map(|backend| backend.base_url.as_str()).unwrap_or(""))
            .bind(expected.map(|backend| backend.options.as_str()).unwrap_or(""))
            .execute(&mut *transaction)
            .await?;
        if updated.rows_affected() == 0 {
            return Err(AppError::NotFound {
                message: "服务已被删除、停用或配置已变更".into(),
            });
        }
        sqlx::query(
            "INSERT INTO backend_model_catalog (backend_id, catalog, updated_at) VALUES (?, ?, ?) ON CONFLICT(backend_id) DO UPDATE SET catalog = excluded.catalog, updated_at = excluded.updated_at",
        )
        .bind(backend_id).bind(catalog).bind(now).execute(&mut *transaction).await?;
        transaction.commit().await?;
        Ok(())
    }

    /// 读取本地目录缓存，页面刷新不需要再次访问上游。
    pub async fn list_model_catalogs(&self) -> Result<HashMap<String, Vec<ModelInfo>>, AppError> {
        let rows = sqlx::query("SELECT backend_id, catalog FROM backend_model_catalog")
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| {
                let catalog: String = row.get("catalog");
                let models = serde_json::from_str(&catalog).map_err(|_| AppError::Internal {
                    message: "stored model catalog is invalid".into(),
                })?;
                Ok((row.get("backend_id"), models))
            })
            .collect()
    }

    /// 后台目录缓存不修改模型白名单，迟到结果只写入仍匹配的服务配置。
    pub async fn cache_routing_models(
        &self,
        record: &BackendRecord,
        models: &[ModelInfo],
    ) -> Result<(), AppError> {
        let catalog = serde_json::to_string(models).map_err(|_| AppError::Internal {
            message: "serialize routing catalog failed".into(),
        })?;
        sqlx::query("INSERT INTO backend_model_catalog (backend_id, catalog, updated_at)
            SELECT id, ?, unixepoch() FROM backends WHERE id = ? AND enabled = 1
            AND kind = ? AND base_url = ? AND options = ? AND models = ? AND capabilities = ? AND api_key = ?
            ON CONFLICT(backend_id) DO UPDATE SET catalog = excluded.catalog, updated_at = excluded.updated_at")
            .bind(catalog).bind(&record.id).bind(&record.kind).bind(&record.base_url)
            .bind(&record.options).bind(&record.models).bind(&record.capabilities).bind(&record.api_key)
            .execute(&self.pool).await?;
        Ok(())
    }
}

/// 行转记录，统一处理新增列的默认值。
fn row_to_record(row: sqlx::sqlite::SqliteRow) -> BackendRecord {
    BackendRecord {
        id: row.get("id"),
        name: row.get("name"),
        kind: row.get("kind"),
        base_url: row.get("base_url"),
        api_key: row.get("api_key"),
        models: row.get("models"),
        enabled: row.get::<i64, _>("enabled") != 0,
        priority: row.get::<i64, _>("priority").clamp(0, 255) as u8,
        cost_tier: row.get("cost_tier"),
        options: row.get("options"),
        capabilities: row.get("capabilities"),
        daily_token_budget: row.get::<i64, _>("daily_token_budget").max(0) as u64,
    }
}

/// 行转账号记录。
fn row_to_account(row: sqlx::sqlite::SqliteRow) -> Account {
    Account {
        id: row.get("id"),
        backend_id: row.get("backend_id"),
        name: row.get("name"),
        credential: row.get("credential"),
        enabled: row.get::<i64, _>("enabled") != 0,
    }
}

/// 返回当前 Unix 时间戳（秒）。用 i64 存储，避免 SQLite 缺少原生时间类型。
fn chrono_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 使用内存数据库验证基本 CRUD 与启用状态往返。
    #[tokio::test]
    async fn backend_crud_round_trip() {
        let store = Store::open(Path::new(":memory:"))
            .await
            .expect("open store");
        let record = BackendRecord {
            id: "b1".into(),
            name: "Demo".into(),
            priority: 10,
            cost_tier: "free".into(),
            ..BackendRecord::default()
        };
        store.upsert_backend(&record).await.expect("insert");
        let listed = store.list_backends().await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "Demo");
        assert_eq!(listed[0].priority, 10);
        assert_eq!(listed[0].cost_tier, "free");
        assert!(listed[0].enabled);

        let mut updated = record.clone();
        updated.enabled = false;
        updated.name = "Renamed".into();
        store.upsert_backend(&updated).await.expect("update");
        let fetched = store.get_backend("b1").await.expect("get").expect("exists");
        assert_eq!(fetched.name, "Renamed");
        assert!(!fetched.enabled);

        assert!(store.delete_backend("b1").await.expect("delete"));
        assert!(store.get_backend("b1").await.expect("get").is_none());
    }
}
