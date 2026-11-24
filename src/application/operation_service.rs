//! 可恢复的操作记录；外部动作先落意图、再执行，未知结果不自动重放。

use super::{
    access::{Principal, Scope},
    config_types::error,
};
use crate::{core::AppError, state::AppState};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;

/// 输入只包含动作参数；秘密写入使用独立接口，不进入操作历史。
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OperationRequest {
    pub service_id: String,
    pub action: String,
    #[serde(default = "empty_input")]
    pub input: Value,
    pub idempotency_key: String,
}

/// 默认无参数动作也使用对象。
fn empty_input() -> Value {
    json!({})
}

/// 公共操作视图；内部会话句柄不出现在普通结果和配置快照中。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Operation {
    pub operation_id: String,
    pub actor: String,
    pub service_id: String,
    pub plugin_id: String,
    pub action: String,
    pub status: String,
    pub created_at: u64,
    pub deadline: u64,
    pub result: Option<Value>,
    pub error: Option<Value>,
    pub next_action: Option<Value>,
    #[serde(skip)]
    pub(crate) session: Option<Value>,
}

/// 运行中的任务有短独占锁；存储始终保存最近完整状态。
#[derive(Default)]
pub struct OperationControl {
    sessions: Mutex<HashMap<String, Arc<Mutex<Operation>>>>,
}

/// 管理调用始终携带身份；异步任务捕获已授权的单个动作。
pub struct OperationService<'a> {
    state: &'a AppState,
    principal: &'a Principal,
}

impl<'a> OperationService<'a> {
    /// HTTP/CLI/MCP 共用这一权限和执行边界。
    pub fn new(state: &'a AppState, principal: &'a Principal) -> Self {
        Self { state, principal }
    }

    /// 查询不轮询上游；远程授权轮询由已经授权的运行任务驱动。
    pub async fn get(&self, id: &str) -> Result<Operation, AppError> {
        self.principal.require(Scope::Read)?;
        let operation = self.state.store.operation(id).await?;
        self.check_owner(&operation)?;
        Ok(operation)
    }

    /// 返回当前身份可读的最近操作。
    pub async fn list(&self) -> Result<Vec<Operation>, AppError> {
        self.principal.require(Scope::Read)?;
        self.state
            .store
            .operations(
                (!matches!(self.principal.id.as_str(), "admin" | "local"))
                    .then_some(self.principal.id.as_str()),
            )
            .await
    }

    /// 校验描述、账号归属和权限后持久化意图；重复提交只返回已有状态。
    pub async fn start(&self, request: OperationRequest) -> Result<Operation, AppError> {
        self.principal.require(Scope::Operation)?;
        if matches!(request.action.as_str(), "auth.start") {
            self.principal.require(Scope::ConfigWrite)?;
            self.principal.require(Scope::SecretWrite)?;
        }
        if request.action == "models.sync" {
            self.principal.require(Scope::ConfigWrite)?;
        }
        if request.idempotency_key.trim().is_empty() || request.idempotency_key.len() > 200 {
            return Err(error("idempotency_key_invalid", "需要 1–200 字符的幂等键"));
        }
        let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(
            &json!({"service_id":request.service_id,"action":request.action,"input":request.input})
        ).expect("operation input is JSON")));
        if let Some(operation) = self
            .state
            .store
            .operation_by_key(&self.principal.id, &request.idempotency_key, &digest)
            .await?
        {
            return Ok(operation);
        }
        let backend = self
            .state
            .store
            .get_backend(&request.service_id)
            .await?
            .filter(|b| b.enabled)
            .ok_or_else(|| error("service_unavailable", "服务不存在或已停用"))?;
        let plugin = crate::plugins::builtin::registry().get(&backend.kind)?;
        let descriptor = plugin.descriptor();
        let action = descriptor
            .operations
            .iter()
            .find(|op| op.id == request.action)
            .ok_or_else(|| error("operation_unsupported", "插件未实现该动作"))?;
        let validator = jsonschema::validator_for(&action.input_schema)
            .map_err(|_| error("plugin_incompatible", "动作 schema 无效"))?;
        if !validator.is_valid(&request.input) {
            return Err(error("schema_invalid", "动作输入不符合插件 schema"));
        }
        if let Some(id) = request.input.get("account_id").and_then(Value::as_str) {
            self.state
                .store
                .get_account(id)
                .await?
                .filter(|a| a.backend_id == backend.id && a.enabled)
                .ok_or_else(|| error("account_unavailable", "账号不存在、停用或不属于此服务"))?;
        }
        let operation = Operation {
            operation_id: uuid::Uuid::new_v4().to_string(),
            actor: self.principal.id.clone(),
            service_id: backend.id.clone(),
            plugin_id: backend.kind.clone(),
            action: request.action.clone(),
            status: "pending".into(),
            created_at: crate::records::now_seconds(),
            deadline: crate::records::now_seconds() + 900,
            result: None,
            error: None,
            next_action: None,
            session: None,
        };
        let (operation, created) = self
            .state
            .store
            .create_operation(&operation, &request.idempotency_key, &digest)
            .await?;
        if created {
            let running = Arc::new(Mutex::new(operation.clone()));
            self.state
                .operations
                .sessions
                .lock()
                .await
                .insert(operation.operation_id.clone(), running.clone());
            let state = self.state.clone();
            tokio::spawn(async move {
                run(&state, running, backend, request).await;
            });
        }
        Ok(operation)
    }

    /// 只有待用户确认的授权可取消；取消生成或未知上游动作不能伪装成未执行。
    pub async fn cancel(&self, id: &str) -> Result<Operation, AppError> {
        self.principal.require(Scope::Operation)?;
        let current = self.state.store.operation(id).await?;
        self.check_owner(&current)?;
        if current.action != "auth.start" {
            return Err(error("operation_not_cancellable", "该操作无法安全取消"));
        }
        let active = self.state.operations.sessions.lock().await.get(id).cloned();
        let Some(active) = active else {
            return Ok(current);
        };
        let mut operation = active.lock().await;
        if operation.status == "waiting_user" {
            let control = crate::plugins::builtin::registry()
                .get(&operation.plugin_id)?
                .auth_control()
                .ok_or_else(|| error("operation_unsupported", "无授权执行器"))?;
            if let Some(session) = operation.session.clone() {
                // 插件终态锁决定取消与授权成功的先后；取消后再读取真实终态。
                (control.cancel)(self.state.clone(), session.clone()).await?;
                let result = (control.poll)(self.state.clone(), session).await;
                match result {
                    Ok(value) if value["status"] == "done" => {
                        operation.status = "succeeded".into();
                        operation.result = Some(value);
                    }
                    _ => operation.status = "cancelled".into(),
                }
            }
            operation.next_action = None;
            self.state.store.save_operation(&operation).await?;
        }
        Ok(operation.clone())
    }

    /// 具名调用者不能通过猜测 UUID 读取或取消其他人的授权。
    fn check_owner(&self, operation: &Operation) -> Result<(), AppError> {
        if operation.actor == self.principal.id
            || matches!(self.principal.id.as_str(), "admin" | "local")
        {
            Ok(())
        } else {
            Err(error("forbidden", "操作属于其他身份"))
        }
    }
}

/// 后台任务无自动重试；每个授权 poll 是同一会话协议的状态查询。
async fn run(
    state: &AppState,
    active: Arc<Mutex<Operation>>,
    backend: crate::store::BackendRecord,
    request: OperationRequest,
) {
    let result = async {
        {
            let mut operation = active.lock().await;
            operation.status = "running".into();
            state.store.save_operation(&operation).await?;
        }
        if request.action == "auth.start" {
            run_auth(state, &active, &backend, request.input).await
        } else {
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(120),
                execute(state, &backend, &request.action, &request.input),
            )
            .await
            .map_err(|_| error("execution_unknown", "操作超时，不能确认上游结果"))??;
            let mut operation = active.lock().await;
            operation.status = result
                .get("status")
                .and_then(Value::as_str)
                .filter(|s| matches!(*s, "unknown" | "failed"))
                .unwrap_or("succeeded")
                .into();
            operation.result = Some(result);
            state.store.save_operation(&operation).await
        }
    }
    .await;
    let mut operation = active.lock().await;
    if let Err(cause) = result
        && !matches!(
            operation.status.as_str(),
            "cancelled" | "succeeded" | "expired"
        )
    {
        operation.status = if cause.code() == "execution_unknown"
            || matches!(&cause, AppError::Upstream(e) if e.execution_unknown)
        {
            "unknown"
        } else {
            "failed"
        }
        .into();
        operation.error = Some(json!({"code":cause.code(),
                "message":"操作未完成，请查看服务状态后决定下一步","retryable":false}));
        operation.next_action = None;
        if let Err(error) = state.store.save_operation(&operation).await {
            tracing::error!(code=%error.code(), "operation terminal persistence failed");
        }
    }
    let id = operation.operation_id.clone();
    drop(operation);
    state.operations.sessions.lock().await.remove(&id);
}

/// 标准能力复用同一已装配 runtime，不按厂商名称分支。
async fn execute(
    state: &AppState,
    backend: &crate::store::BackendRecord,
    action: &str,
    input: &Value,
) -> Result<Value, AppError> {
    let runtime = state
        .registry
        .runtime(&backend.id)
        .ok_or_else(|| error("service_unavailable", "服务尚未激活"))?;
    let accounts = state
        .store
        .list_accounts_for(&backend.id)
        .await?
        .into_iter()
        .filter(|a| {
            a.enabled
                && input
                    .get("account_id")
                    .and_then(Value::as_str)
                    .is_none_or(|id| id == a.id)
        })
        .collect::<Vec<_>>();
    if action == "models.sync" {
        let models = runtime.inference.list_models().await?;
        if models.is_empty() {
            return Err(error("catalog_empty", "模型目录为空，请检查授权"));
        }
        state.config_service().sync_models(backend, &models).await?;
        return Ok(json!({"models":models}));
    }
    if accounts.is_empty() {
        return Err(error("account_unavailable", "没有启用的账号"));
    }
    let mut results = Vec::new();
    for account in &accounts {
        match action {
            "quota.refresh" => {
                let reader = runtime
                    .quota
                    .as_ref()
                    .ok_or_else(|| error("operation_unsupported", "无额度能力"))?;
                results.push(
                    serde_json::to_value(
                        crate::accounts::quota_refresh::refresh_account_with_reader(
                            &state.store,
                            &state.quota_refresh,
                            backend,
                            account,
                            reader.as_ref(),
                        )
                        .await?,
                    )
                    .expect("quota is JSON"),
                );
            }
            "credential.refresh" => {
                let refresher = runtime
                    .credential_refresh
                    .as_ref()
                    .ok_or_else(|| error("operation_unsupported", "无续期能力"))?;
                refresher.refresh(account).await?;
                results.push(json!({"account_id":account.id,"refreshed":true}));
            }
            "checkin" => {
                let provider = runtime
                    .tasks
                    .iter()
                    .find(|p| p.kind() == crate::tasks::TaskKind::Checkin)
                    .ok_or_else(|| error("operation_unsupported", "无签到能力"))?;
                let outcome = crate::tasks::run_and_record(
                    &state.store,
                    provider.as_ref(),
                    &account.id,
                    &crate::tasks::scheduler::day_at(crate::tasks::scheduler::now()),
                )
                .await?;
                results.push(json!({"account_id":account.id,"outcome":outcome}));
            }
            _ => return Err(error("operation_unsupported", "无动作执行器")),
        }
    }
    if action == "credential.refresh" {
        state.reload_backends().await?;
    }
    let status = if results.iter().any(|v| v["outcome"]["status"] == "unknown") {
        "unknown"
    } else if results.iter().any(|v| v["outcome"]["status"] == "failed") {
        "failed"
    } else {
        "succeeded"
    };
    Ok(json!({"status":status,"results":results}))
}

/// 授权参数只在内存会话保存；操作状态输出下一步交互信息。
async fn run_auth(
    state: &AppState,
    active: &Arc<Mutex<Operation>>,
    backend: &crate::store::BackendRecord,
    mut input: Value,
) -> Result<(), AppError> {
    let plugin = crate::plugins::builtin::registry().get(&backend.kind)?;
    let control = plugin
        .auth_control()
        .ok_or_else(|| error("operation_unsupported", "无授权执行器"))?;
    input["backend_id"] = json!(backend.id);
    let start = (control.start)(state.clone(), input).await?;
    let session = json!({"backend_id":backend.id,"session_id":start.get("session_id").cloned().unwrap_or(json!(""))});
    {
        let mut operation = active.lock().await;
        operation.status = "waiting_user".into();
        operation.deadline =
            crate::records::now_seconds() + start["expires_in"].as_u64().unwrap_or(600).min(900);
        operation.session = Some(session.clone());
        operation.next_action = Some(json!({
            "type":plugin.descriptor().auth_flows.first().map(|f| &f.interaction),
            "url":start.get("login_url"),"user_code":start.get("user_code"),
            "message":start.get("message")
        }));
        state.store.save_operation(&operation).await?;
    }
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let mut operation = active.lock().await;
        if operation.status != "waiting_user" {
            return Ok(());
        }
        if crate::records::now_seconds() >= operation.deadline {
            (control.cancel)(state.clone(), session.clone()).await?;
            match (control.poll)(state.clone(), session.clone()).await {
                Ok(value) if value["status"] == "done" => {
                    operation.status = "succeeded".into();
                    operation.result = Some(value);
                }
                _ => operation.status = "expired".into(),
            }
            operation.next_action = None;
            return state.store.save_operation(&operation).await;
        }
        drop(operation);
        let result = (control.poll)(state.clone(), session.clone()).await?;
        let mut operation = active.lock().await;
        if operation.status != "waiting_user" {
            return Ok(());
        }
        if result["status"] == "done" {
            operation.status = "succeeded".into();
            operation.result = Some(result);
            operation.next_action = None;
            return state.store.save_operation(&operation).await;
        }
        if matches!(
            result["status"].as_str(),
            Some("cancelled" | "expired" | "failed")
        ) {
            operation.status = result["status"].as_str().unwrap().into();
            operation.next_action = None;
            return state.store.save_operation(&operation).await;
        }
    }
}
