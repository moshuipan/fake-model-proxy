//! 可插拔后台任务：签到与额度同步。
//!
//! 任务按 `(backend, account, kind, period)` 幂等：同一周期内重复触发不会
//! 产生第二次上游动作。失败或结果未知保留记录，等待人工核对，不自动重放。

pub mod batch;
pub mod checkin;
pub mod scheduler;

use crate::core::error::AppError;
use crate::store::Store;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// 任务类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    /// 每日签到。
    Checkin,
    /// 额度同步。
    Quota,
}

impl TaskKind {
    /// 返回数据库与接口使用的字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            TaskKind::Checkin => "checkin",
            TaskKind::Quota => "quota",
        }
    }

    /// 从字符串解析。
    pub fn parse(value: &str) -> Option<TaskKind> {
        match value {
            "checkin" => Some(TaskKind::Checkin),
            "quota" => Some(TaskKind::Quota),
            _ => None,
        }
    }
}

/// 任务结果状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// 本次执行成功。
    Succeeded,
    /// 上游表示已领取过。
    AlreadyDone,
    /// 失败，可重试。
    Failed,
    /// 执行结果未知，禁止自动重放。
    Unknown,
    /// 上游没有该能力，跳过。
    Skipped,
}

impl TaskStatus {
    /// 返回数据库字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Succeeded => "succeeded",
            TaskStatus::AlreadyDone => "already_done",
            TaskStatus::Failed => "failed",
            TaskStatus::Unknown => "unknown",
            TaskStatus::Skipped => "skipped",
        }
    }
}

/// 任务执行结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskOutcome {
    pub status: TaskStatus,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<i64>,
}

impl TaskOutcome {
    /// 构造成功结果。
    pub fn succeeded(message: impl Into<String>, amount: Option<i64>) -> Self {
        TaskOutcome {
            status: TaskStatus::Succeeded,
            message: message.into(),
            amount,
        }
    }

    /// 构造跳过结果。
    pub fn skipped(message: impl Into<String>) -> Self {
        TaskOutcome {
            status: TaskStatus::Skipped,
            message: message.into(),
            amount: None,
        }
    }

    /// 领取已发出但结果无法确认时禁止自动重放，不推测奖励数。
    pub fn unknown(message: impl Into<String>) -> Self {
        Self {
            status: TaskStatus::Unknown,
            message: message.into(),
            amount: None,
        }
    }
}

/// 任务执行器。每个后端按需实现。
#[async_trait]
pub trait TaskProvider: Send + Sync {
    /// 任务类型。
    fn kind(&self) -> TaskKind;

    /// 所属后端标识。
    fn backend_id(&self) -> &str;

    /// 执行一次任务。实现方负责凭证与协议细节。
    async fn run(&self, period: &str) -> Result<TaskOutcome, AppError>;

    /// 为指定账号执行任务；旧执行器沿用原入口，新执行器应覆盖此方法。
    async fn run_for_account(
        &self,
        _account_id: &str,
        period: &str,
    ) -> Result<TaskOutcome, AppError> {
        self.run(period).await
    }

    /// 活动窗口与自然日不同的服务需要重新查询上游，避免本地缓存漏领新窗口。
    fn recheck_completed(&self) -> bool {
        false
    }
}

/// 任务执行记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub backend_id: String,
    pub account_id: String,
    pub kind: String,
    pub period: String,
    pub status: String,
    pub message: String,
    pub amount: Option<i64>,
}

/// 执行并记录一次任务。
pub async fn run_and_record(
    store: &Store,
    provider: &dyn TaskProvider,
    account_id: &str,
    period: &str,
) -> Result<TaskOutcome, AppError> {
    let _guard = store.lock_tasks().await;
    let existing = store
        .get_task(
            provider.backend_id(),
            account_id,
            provider.kind().as_str(),
            period,
        )
        .await?;
    // 未知领取禁止重放；活动窗口型任务允许重新查询上游确认新窗口。
    if let Some(existing) = &existing
        && (existing.status == "unknown"
            || !provider.recheck_completed()
                && matches!(existing.status.as_str(), "succeeded" | "already_done"))
    {
        return Ok(TaskOutcome {
            status: if existing.status == "unknown" {
                TaskStatus::Unknown
            } else {
                TaskStatus::AlreadyDone
            },
            message: format!("already recorded: {}", existing.message),
            amount: existing.amount,
        });
    }
    // 在外部动作前记录执行意图，进程中断或结果落盘失败时禁止手动、自动重放。
    store
        .upsert_task(&TaskRecord {
            backend_id: provider.backend_id().to_string(),
            account_id: account_id.to_string(),
            kind: provider.kind().as_str().to_string(),
            period: period.to_string(),
            status: TaskStatus::Unknown.as_str().into(),
            message: "任务执行处理中；若服务中断，请核对上游领取结果".into(),
            amount: existing.as_ref().and_then(|record| record.amount),
        })
        .await?;
    let outcome = match provider.run_for_account(account_id, period).await {
        Ok(outcome) => outcome,
        Err(error) => TaskOutcome {
            status: if matches!(&error, AppError::Upstream(upstream) if upstream.execution_unknown)
            {
                TaskStatus::Unknown
            } else {
                TaskStatus::Failed
            },
            message: error.message(),
            amount: None,
        },
    };
    store
        .upsert_task(&TaskRecord {
            backend_id: provider.backend_id().to_string(),
            account_id: account_id.to_string(),
            kind: provider.kind().as_str().to_string(),
            period: period.to_string(),
            status: outcome.status.as_str().to_string(),
            message: outcome.message.clone(),
            // 只查询到已领取时保留本周期已有的实发奖励记录，不把查询当成新奖励。
            amount: if outcome.status == TaskStatus::AlreadyDone {
                outcome
                    .amount
                    .or_else(|| existing.as_ref().and_then(|record| record.amount))
            } else {
                outcome.amount
            },
        })
        .await?;
    Ok(outcome)
}

/// 任务接口同时校验 HTTP 和业务状态，不把错误信封当成空活动或领取成功。
pub(crate) async fn decode_checkin(
    response: reqwest::Response,
    provider: &str,
) -> Result<serde_json::Value, AppError> {
    let status = response.status();
    if !status.is_success() {
        return Err(AppError::Upstream(crate::core::UpstreamError::status(
            status.as_u16(),
            format!("{provider} 签到接口返回 HTTP {}", status.as_u16()),
        )));
    }
    let value: serde_json::Value = response.json().await.map_err(|_| {
        AppError::Upstream(crate::core::UpstreamError::status(
            502,
            format!("{provider} 签到接口返回非 JSON 响应"),
        ))
    })?;
    if let Some(code) = value.get("code") {
        let code = match code {
            serde_json::Value::Number(number) => number.to_string(),
            serde_json::Value::String(code) => code.clone(),
            _ => return Err(checkin_protocol_error(provider)),
        };
        if code != "0" && code != "200" {
            let mut error = crate::core::UpstreamError::status(
                400,
                format!("{provider} 签到接口拒绝请求（业务码 {code}）"),
            );
            error.code = format!("checkin_{code}");
            return Err(AppError::Upstream(error));
        }
    }
    if value.get("success").and_then(serde_json::Value::as_bool) == Some(false) {
        return Err(AppError::bad_request(format!(
            "{provider} 签到接口未通过业务校验"
        )));
    }
    Ok(value)
}

/// 缺失关键结构属于协议错误，不能默认为无活动。
pub(crate) fn checkin_protocol_error(provider: &str) -> AppError {
    AppError::Upstream(crate::core::UpstreamError::status(
        502,
        format!("{provider} 签到响应结构已变化，无法确认活动状态"),
    ))
}

/// 任务注册表。按后端提供执行器，未注册的后端不产生任务按钮。
#[derive(Default)]
pub struct TaskRegistry {
    providers: Vec<Arc<dyn TaskProvider>>,
}

impl TaskRegistry {
    /// 创建空注册表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个执行器。
    pub fn register(&mut self, provider: Arc<dyn TaskProvider>) {
        self.providers.push(provider);
    }

    /// 列出全部执行器。
    pub fn providers(&self) -> Vec<Arc<dyn TaskProvider>> {
        self.providers.clone()
    }

    /// 查找某后端的签到执行器。
    pub fn checkin_for(&self, backend_id: &str) -> Option<Arc<dyn TaskProvider>> {
        self.providers
            .iter()
            .find(|provider| {
                provider.kind() == TaskKind::Checkin && provider.backend_id() == backend_id
            })
            .cloned()
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use axum::Router;
    use axum::body::{Body, Bytes};
    use axum::extract::State;
    use axum::http::{HeaderMap, Uri};
    use axum::response::Response;
    use serde_json::Value;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// 本地假上游响应队列和收到的无效测试身份，永远不访问厂商。
    pub struct MockServer {
        pub base: String,
        pub calls: Arc<Mutex<Vec<(String, String, usize)>>>,
        handle: tokio::task::JoinHandle<()>,
    }

    struct Mock {
        responses: Mutex<VecDeque<(u16, Value)>>,
        calls: Arc<Mutex<Vec<(String, String, usize)>>>,
    }

    /// 顺序返回预设信封，并记录路径、测试令牌和请求体长度。
    async fn upstream(
        State(mock): State<Arc<Mock>>,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        mock.calls.lock().unwrap().push((
            uri.path().into(),
            headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .into(),
            body.len(),
        ));
        let (status, value) = mock
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or((500, serde_json::json!({"unexpected":true})));
        Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Body::from(value.to_string()))
            .unwrap()
    }

    impl MockServer {
        /// 监听动态回环端口，所有凭证和响应均由测试构造。
        pub async fn new(responses: Vec<(u16, Value)>) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let calls = Arc::new(Mutex::new(Vec::new()));
            let mock = Arc::new(Mock {
                responses: Mutex::new(responses.into()),
                calls: calls.clone(),
            });
            let router = Router::new().fallback(upstream).with_state(mock);
            let handle = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            Self {
                base,
                calls,
                handle,
            }
        }
    }

    impl Drop for MockServer {
        /// 测试结束立即释放监听器。
        fn drop(&mut self) {
            self.handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// 测试执行器：返回固定结果并统计调用次数。
    struct StubProvider {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl TaskProvider for StubProvider {
        /// 固定为签到任务。
        fn kind(&self) -> TaskKind {
            TaskKind::Checkin
        }

        /// 固定后端标识。
        fn backend_id(&self) -> &str {
            "qoder"
        }

        /// 记录一次调用。
        async fn run(&self, _period: &str) -> Result<TaskOutcome, AppError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(TaskOutcome::succeeded("claimed 100", Some(100)))
        }
    }

    /// 同一周期重复执行只调用上游一次。
    #[tokio::test]
    async fn task_is_idempotent_per_period() {
        let store = Store::open(Path::new(":memory:")).await.expect("store");
        let provider = StubProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let first = run_and_record(&store, &provider, "acc", "2026-11-18")
            .await
            .expect("first");
        assert_eq!(first.status, TaskStatus::Succeeded);
        let second = run_and_record(&store, &provider, "acc", "2026-11-18")
            .await
            .expect("second");
        assert_eq!(second.status, TaskStatus::AlreadyDone);
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "upstream must be called once per period"
        );
    }

    /// 未注册的后端不应产生执行器。
    #[test]
    fn registry_only_returns_registered_backend() {
        let mut registry = TaskRegistry::new();
        registry.register(Arc::new(StubProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
        }));
        assert!(registry.checkin_for("qoder").is_some());
        assert!(registry.checkin_for("other").is_none());
    }
}
