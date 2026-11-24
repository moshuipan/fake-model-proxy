//! AutoGLM 客户端后端：短信账号、续期、原生工具流与每日积分。

mod catalog;
mod checkin;
pub mod client;
pub mod credential;
pub mod oauth;

use crate::accounts::remote::{CredentialRefresher, QuotaConfidence, QuotaInfo, QuotaReader};
use crate::accounts::{Account, AccountPool, FailureKind};
use crate::backend::{Backend, CostTier, openai_compat};
use crate::core::stream::EventStream;
use crate::core::{AppError, Capabilities, ChatRequest, ModelInfo};
use crate::store::{BackendRecord, Store};
use crate::tasks::{TaskKind, TaskOutcome, TaskProvider, TaskStatus};
use async_trait::async_trait;
use client::{Client, decode, http_error, protocol_error};
use credential::Credential;
use reqwest::Method;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

/// 上游校验的系统身份首句，仅用于协议兼容。
const BANNER: &str = "You are AutoClaw. Answer the user directly and concisely.";

/// 一个服务对应账号池；令牌始终从 Store 读取，避免热重载留下旧快照。
pub struct AutoGlmBackend {
    record: BackendRecord,
    models: Vec<String>,
    capabilities: Capabilities,
    accounts: Arc<AccountPool>,
    store: crate::plugins::ScopedHostServices,
    client: Client,
}

impl AutoGlmBackend {
    /// 构造聊天与远程账号能力，费用、排序及日预算采用服务配置。
    pub fn new(
        record: &BackendRecord,
        accounts: Vec<Account>,
        store: Store,
    ) -> Result<Self, AppError> {
        Self::new_with_timeout(record, accounts, store, std::time::Duration::from_secs(10))
    }

    /// 将部署连接时限传递到聊天传输，不改变授权客户端默认值。
    pub fn new_with_timeout(
        record: &BackendRecord,
        accounts: Vec<Account>,
        store: Store,
        timeout: std::time::Duration,
    ) -> Result<Self, AppError> {
        Self::new_with_host(
            record,
            crate::plugins::ScopedHostServices::legacy(record, &accounts, store, timeout),
        )
    }

    /// 用服务作用域宿主组合全部账号能力。
    pub fn new_with_host(
        record: &BackendRecord,
        host: crate::plugins::ScopedHostServices,
    ) -> Result<Self, AppError> {
        let accounts = host.accounts();
        let timeout = host.connect_timeout;
        let mut capabilities = Capabilities {
            tools: true,
            parallel_tools: true,
            images: true,
            reasoning: true,
            context_window: None,
        };
        if !record.capabilities.trim().is_empty() {
            let overrides: openai_compat::CapabilityOverride =
                serde_json::from_str(&record.capabilities)
                    .map_err(|_| AppError::bad_request("AutoGLM 能力覆盖格式不正确"))?;
            overrides.apply(&mut capabilities);
        }
        Ok(Self {
            record: record.clone(),
            models: record
                .models
                .split(',')
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(ToOwned::to_owned)
                .collect(),
            capabilities,
            accounts: Arc::new(AccountPool::new(accounts)),
            store: host,
            client: Client::new_with_timeout(record, timeout)?,
        })
    }

    /// 检查账号仍存在、启用且属于本服务，避免使用过期池快照。
    async fn load_account(&self, id: &str) -> Result<Account, AppError> {
        self.store
            .get_account(id)
            .await?
            .filter(|account| account.enabled && account.backend_id == self.record.id)
            .ok_or_else(|| AppError::bad_request("AutoGLM 账号不存在、已停用或不属于此服务"))
    }

    /// 串行续期并条件写回；401 重试时若其他请求已续期则复用新令牌。
    async fn credential(
        &self,
        id: &str,
        force: bool,
        rejected: Option<&str>,
    ) -> Result<Credential, AppError> {
        let current = Credential::parse(&self.load_account(id).await?.credential)?;
        if !force
            && !current.needs_refresh()
            && rejected.is_none_or(|token| token != current.access_token)
        {
            return Ok(current);
        }
        let _guard = self.store.lock_credentials().await;
        let account = self.load_account(id).await?;
        let credential = Credential::parse(&account.credential)?;
        let stale = rejected.is_some_and(|token| token == credential.access_token);
        if !(force || stale || credential.needs_refresh()) {
            return Ok(credential);
        }
        let updated = self.client.refresh(&credential).await?;
        if self
            .store
            .replace_credential(&account.id, &account.credential, &updated.serialize()?)
            .await?
        {
            // 删除和人工编辑可能发生在网络请求期间；成功写回后再读取当前事实。
            return Credential::parse(&self.load_account(id).await?.credential);
        }
        Credential::parse(&self.load_account(id).await?.credential)
    }

    /// 选择一个未冷却的账号；未登录时给出可操作提示。
    fn select_account(&self) -> Result<Account, AppError> {
        self.accounts.select().ok_or_else(|| {
            AppError::bad_request(
                "AutoGLM 尚无可用账号，请在控制台使用官方授权或导入已有凭证；已有账号可能正在冷却",
            )
        })
    }

    /// 读取账号或目录 JSON，仅对确定的鉴权失败续期后重试一次。
    async fn get_json(
        &self,
        account_id: &str,
        path: &str,
        account_api: bool,
    ) -> Result<Value, AppError> {
        let mut credential = self.credential(account_id, false, None).await?;
        for attempt in 0..2 {
            let response = self
                .client
                .request(Method::GET, path, &credential, account_api)
                .timeout(Duration::from_secs(20))
                .send()
                .await?;
            match decode(response).await {
                Err(error)
                    if error.status().as_u16() == 401
                        && attempt == 0
                        && !credential.refresh_token.is_empty() =>
                {
                    credential = self
                        .credential(account_id, false, Some(&credential.access_token))
                        .await?;
                }
                result => return result,
            }
        }
        Err(protocol_error("AutoGLM 鉴权重试失败"))
    }

    /// 发起原生 OpenAI 工具流；所有入站协议均复用同一事件解码器。
    async fn chat_inner(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, AppError> {
        let account = self.select_account()?;
        let model = request
            .model
            .strip_prefix(&format!("{}/", self.record.id))
            .unwrap_or(&request.model);
        let body = shape_body(&request, model);
        let mut credential = self.credential(&account.id, false, None).await?;
        let mut response = self
            .client
            .request(Method::POST, client::CHAT_PATH, &credential, false)
            .header("X-Request-Model", model)
            .header("X-Agent-Id", "main")
            .timeout(Duration::from_secs(300))
            .json(&body)
            .send()
            .await?;
        if response.status().as_u16() == 401 && !credential.refresh_token.is_empty() {
            crate::gateway::retry::additional_submission().await?;
            credential = self
                .credential(&account.id, false, Some(&credential.access_token))
                .await?;
            response = self
                .client
                .request(Method::POST, client::CHAT_PATH, &credential, false)
                .header("X-Request-Model", model)
                .header("X-Agent-Id", "main")
                .timeout(Duration::from_secs(300))
                .json(&body)
                .send()
                .await?;
        }
        if !response.status().is_success() {
            crate::gateway::retry::capture_retry_after(&response);
            let status = response.status().as_u16();
            // 普通 403 不能证明凭证失效，不把模型/客户端拒绝扩大为整个账号的鉴权冷却。
            if status != 403 {
                self.accounts.report_failure(
                    &account.id,
                    if status == 402 {
                        FailureKind::QuotaExhausted
                    } else {
                        FailureKind::classify(Some(status), "")
                    },
                );
            }
            return Err(http_error(status));
        }
        if !response
            .headers()
            .get("content-type")
            .and_then(|header| header.to_str().ok())
            .is_some_and(|content_type| content_type.contains("text/event-stream"))
        {
            decode(response).await?;
            return Err(protocol_error("AutoGLM 未返回预期的 SSE 流"));
        }
        self.accounts.report_success(&account.id);
        let (sender, receiver) = mpsc::channel(32);
        tokio::spawn(async move {
            openai_compat::pump_sse(response, sender, cancel).await;
        });
        Ok(Box::pin(ReceiverStream::new(receiver)))
    }

    /// 为一个账号签到；查询失败不执行领取，领取传输失败保留结果未知。
    async fn checkin_task(&self, account_id: &str) -> Result<TaskOutcome, AppError> {
        let envelope = self.get_json(account_id, client::TASKS_PATH, true).await?;
        let tasks = envelope
            .get("data")
            .unwrap_or(&envelope)
            .as_array()
            .ok_or_else(|| protocol_error("AutoGLM 任务列表结构不正确"))?;
        let Some(task) = tasks
            .iter()
            .find(|task| task.get("task_id").and_then(Value::as_str) == Some("daily_signin"))
        else {
            return Ok(TaskOutcome::skipped("上游未提供每日签到任务"));
        };
        if task.get("client_triggered").and_then(Value::as_bool) == Some(false) {
            return Ok(TaskOutcome::skipped("上游不允许客户端领取此任务"));
        }
        let credential = self.credential(account_id, false, None).await?;
        let response = match self
            .client
            .request(Method::POST, client::CLAIM_PATH, &credential, true)
            .timeout(Duration::from_secs(20))
            .json(&json!({"task_id":"daily_signin"}))
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => return Ok(unknown_claim()),
        };
        let value = match decode(response).await {
            Ok(value) => value,
            Err(error) if error.status().as_u16() >= 500 => return Ok(unknown_claim()),
            Err(error) => return Err(error),
        };
        let Some(data) = value.get("data").filter(|data| !data.is_null()) else {
            // 官方客户端允许 code=0 且无 data 的幂等签到响应。
            if value.get("code").and_then(Value::as_i64) == Some(0) {
                return Ok(TaskOutcome {
                    status: TaskStatus::AlreadyDone,
                    message: "今日已签到".into(),
                    amount: None,
                });
            }
            return Ok(unknown_claim());
        };
        let already = data.get("already_completed").and_then(Value::as_bool) == Some(true);
        if already {
            return Ok(TaskOutcome {
                status: TaskStatus::AlreadyDone,
                message: "今日已签到".into(),
                amount: None,
            });
        }
        if data.get("success").and_then(Value::as_bool) == Some(true) {
            return Ok(TaskOutcome::succeeded(
                "每日签到成功",
                data.get("reward_points").and_then(Value::as_i64),
            ));
        }
        Ok(TaskOutcome {
            status: TaskStatus::Failed,
            message: "上游未确认签到成功".into(),
            amount: None,
        })
    }
}

#[async_trait]
impl Backend for AutoGlmBackend {
    /// 返回稳定服务标识。
    fn id(&self) -> &str {
        &self.record.id
    }
    /// 返回控制台名称。
    fn name(&self) -> &str {
        &self.record.name
    }
    /// 返回独立客户端类型。
    fn kind(&self) -> &str {
        "autoglm"
    }
    /// 返回已声明的协议能力。
    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }
    /// 使用服务配置的费用层级。
    fn cost_tier(&self) -> CostTier {
        CostTier::parse(&self.record.cost_tier)
    }
    /// 使用服务配置的路由优先级。
    fn priority(&self) -> u8 {
        self.record.priority
    }
    /// 使用服务配置的每日预算。
    fn daily_token_budget(&self) -> u64 {
        self.record.daily_token_budget
    }
    /// 返回白名单用于精确寻址和 auto 路由。
    fn static_models(&self) -> Vec<String> {
        self.models.clone()
    }
    /// 此上游不接受输出上限字段，Auto 必须使用独立的最坏输出预算。
    fn output_enforced(&self) -> bool {
        false
    }
    /// 公布账号池快照，路由会锁定可用账号。
    fn routing_accounts(&self) -> Option<Vec<crate::accounts::AccountState>> {
        Some(self.accounts.snapshot())
    }
    /// 读取路由选中账号的原始积分余额，不使用其他账号代替。
    async fn routing_quota(&self, account_id: &str) -> Result<QuotaInfo, AppError> {
        let account = self.load_account(account_id).await?;
        QuotaReader::read(self, &account).await
    }
    /// 始终读取真实目录，让同步操作能够发现上游新增模型。
    async fn list_models(&self) -> Result<Vec<ModelInfo>, AppError> {
        let account = self.select_account()?;
        let value = self
            .get_json(&account.id, client::MODELS_PATH, false)
            .await?;
        let models = catalog::parse(&value, &self.capabilities);
        if models.is_empty() {
            return Err(protocol_error("AutoGLM 模型目录为空或格式已变化"));
        }
        Ok(models)
    }
    /// 将取消令牌覆盖登录态获取及建流，避免客户端断开后继续请求。
    async fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, AppError> {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(AppError::BadRequest {
                code: "request_cancelled".into(), message: "请求已取消".into(),
            }),
            result = self.chat_inner(request, cancel.clone()) => result,
        }
    }
}

#[async_trait]
impl CredentialRefresher for AutoGlmBackend {
    /// 返回账号所属服务。
    fn backend_id(&self) -> &str {
        &self.record.id
    }
    /// 手动刷新复用自动续期的锁和条件持久化，不需重载注册表。
    async fn refresh(&self, account: &Account) -> Result<Account, AppError> {
        self.credential(&account.id, true, None).await?;
        self.load_account(&account.id).await
    }
}

#[async_trait]
impl QuotaReader for AutoGlmBackend {
    /// 返回钱包所属服务。
    fn backend_id(&self) -> &str {
        &self.record.id
    }
    /// 钱包提供余额，不能将其伪装成套餐总量或已用量。
    async fn read(&self, account: &Account) -> Result<QuotaInfo, AppError> {
        let value = self
            .get_json(&account.id, client::WALLET_PATH, true)
            .await?;
        if value.get("code").and_then(Value::as_i64) != Some(0) {
            return Err(protocol_error("AutoGLM 钱包缺少成功状态"));
        }
        let balance = value
            .get("data")
            .and_then(|data| data.get("total_balance"))
            .and_then(crate::accounts::quota::value_units)
            .ok_or_else(|| protocol_error("AutoGLM 钱包未返回有效余额"))?;
        let remaining = crate::accounts::quota::decimal_string(balance);
        Ok(QuotaInfo {
            routing: Some(crate::accounts::quota::QuotaSnapshot {
                remaining: Some(remaining.clone()),
                unit: "积分".into(),
                quota_scope_id: format!("{}/{}", self.record.id, account.id),
                valid_until: None,
                unlimited: false,
                complete: true,
            }),
            backend_id: self.record.id.clone(),
            account_id: account.id.clone(),
            used: None,
            total: None,
            remaining: remaining.parse().ok(),
            unit: Some("积分".into()),
            reset_at: None,
            confidence: QuotaConfidence::Verified,
            note: Some("上游钱包余额；积分到期与签到奖励以上游为准".into()),
            checked_at: crate::records::now_seconds(),
        })
    }
}

#[async_trait]
impl TaskProvider for AutoGlmBackend {
    /// 声明每日签到。
    fn kind(&self) -> TaskKind {
        TaskKind::Checkin
    }
    /// 返回服务标识。
    fn backend_id(&self) -> &str {
        &self.record.id
    }
    /// 执行必须指定账号，避免把多个账号结果写到同一条记录。
    async fn run(&self, _period: &str) -> Result<TaskOutcome, AppError> {
        Err(AppError::bad_request("AutoGLM 签到需要指定账号"))
    }
    /// 每次只领取当前记录对应账号的积分。
    async fn run_for_account(
        &self,
        account_id: &str,
        period: &str,
    ) -> Result<TaskOutcome, AppError> {
        let credential = self.credential(account_id, false, None).await?;
        match self.client.checkin_protocol(&credential) {
            client::CheckinProtocol::Card => self.checkin_card(account_id, period).await,
            client::CheckinProtocol::Task => self.checkin_task(account_id).await,
        }
    }
}

/// 领取请求已经发出而结果不确定时不推断成功或奖励数。
fn unknown_claim() -> TaskOutcome {
    TaskOutcome {
        status: TaskStatus::Unknown,
        message: "签到结果未知，请在官方客户端核对后再操作".into(),
        amount: None,
    }
}

/// 去掉提供商前缀，仅修改 body；实际路由头始终使用完整模型 ID。
fn body_model(model: &str) -> &str {
    for prefix in [
        "zai_",
        "zai-",
        "zaicoding_",
        "zaicoding-",
        "tdpsk_",
        "tdpsk-",
        "zhipu_",
        "zhipu-",
    ] {
        if let Some(model) = model.strip_prefix(prefix) {
            return model;
        }
    }
    model
}

/// 对 ASCII 身份短语做大小写无关替换，保持其余 Unicode 提示词原样。
fn neutralize_identity(text: &str) -> String {
    let mut text = text.to_string();
    for phrase in [
        "you are claude code",
        "you are cursor",
        "you are cline",
        "you are codex",
        "you are qwen code",
        "you are opencode",
        "you are an interactive cli tool that helps users with software engineering tasks",
        "you are an interactive agent in traecode",
        "claude agent sdk",
    ] {
        let replacement = if phrase == "claude agent sdk" {
            "AutoClaw"
        } else {
            "You are AutoClaw"
        };
        while let Some(index) = text
            .as_bytes()
            .windows(phrase.len())
            .position(|bytes| bytes.eq_ignore_ascii_case(phrase.as_bytes()))
        {
            text.replace_range(index..index + phrase.len(), replacement);
        }
    }
    text
}

/// 复用标准消息与工具编码，仅施加上游网关已知的格式约束。
fn shape_body(request: &ChatRequest, model: &str) -> Value {
    let mut body = openai_compat::OpenAiCompatBackend::build_body(request, body_model(model));
    for key in [
        "max_tokens",
        "max_completion_tokens",
        "max_output_tokens",
        "parallel_tool_calls",
    ] {
        body.as_object_mut()
            .expect("OpenAI body is an object")
            .remove(key);
    }
    // 上游统一用 SSE，入站非流式由现有聚合器收集事件。
    body["stream"] = json!(true);
    body["stream_options"] = json!({"include_usage":true});
    let messages = body["messages"]
        .as_array_mut()
        .expect("OpenAI messages are an array");
    let mut banner_added = false;
    for message in messages
        .iter_mut()
        .filter(|message| message["role"] == "system")
    {
        if let Some(text) = message["content"].as_str() {
            let text = neutralize_identity(text);
            message["content"] = json!(if text.contains(BANNER) {
                text
            } else {
                format!("{BANNER}\n\n{text}")
            });
            banner_added = true;
        }
    }
    if !banner_added {
        messages.insert(0, json!({"role":"system","content":BANNER}));
    }
    body
}
