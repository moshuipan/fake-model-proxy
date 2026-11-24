//! 宿主持有服务策略；协议插件不能替代用户配置的费用、优先级和预算。

use crate::{
    backend::{Backend, CostTier},
    core::{AppError, Capabilities, ChatRequest, ModelInfo, TokenLimits, stream::EventStream},
    store::BackendRecord,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// 连接身份排除名称与宿主调度策略，用于实例复用与迟到写入围栏。
pub fn connection_identity(record: &BackendRecord) -> serde_json::Value {
    let mut options: serde_json::Value =
        serde_json::from_str(&record.options).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(object) = options.as_object_mut() {
        object.remove("routing");
    }
    let capabilities: serde_json::Value =
        serde_json::from_str(&record.capabilities).unwrap_or_else(|_| serde_json::json!({}));
    serde_json::json!({"kind":record.kind,"url":record.base_url,"key":record.api_key,
        "models":record.models,"options":options,"capabilities":capabilities})
}

/// 目录来自连接与账号，修改模型白名单不使已经查询到的元数据失效。
pub fn catalog_identity(record: &BackendRecord) -> serde_json::Value {
    let mut identity = connection_identity(record);
    identity
        .as_object_mut()
        .expect("connection object")
        .remove("models");
    identity
        .as_object_mut()
        .expect("connection object")
        .remove("capabilities");
    identity
}

pub struct ConfiguredBackend {
    inner: Arc<dyn Backend>,
    record: BackendRecord,
    qps: u32,
}

impl ConfiguredBackend {
    /// 配置策略与协议实例组合，旧 generation 保持自己的策略。
    pub fn new(record: &BackendRecord, inner: Arc<dyn Backend>) -> Self {
        let qps = serde_json::from_str::<serde_json::Value>(&record.options)
            .ok()
            .and_then(|options| options.get("routing").cloned())
            .and_then(|routing| {
                serde_json::from_value::<crate::routing::planner::RouteSettings>(routing).ok()
            })
            .map_or(0, |routing| routing.qps);
        Self {
            inner,
            record: record.clone(),
            qps,
        }
    }
}

#[async_trait::async_trait]
impl Backend for ConfiguredBackend {
    /// 稳定实例标识由宿主配置提供。
    fn id(&self) -> &str {
        &self.record.id
    }
    /// 显示名属于配置，不影响协议连接身份。
    fn name(&self) -> &str {
        &self.record.name
    }
    /// 插件身份仅用于描述和审计。
    fn kind(&self) -> &str {
        &self.record.kind
    }
    /// 能力事实由实现提供。
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    /// 不允许插件扩大用户授予的费用许可。
    fn cost_tier(&self) -> CostTier {
        CostTier::parse(&self.record.cost_tier)
    }
    /// 调度优先级由宿主管理。
    fn priority(&self) -> u8 {
        self.record.priority
    }
    /// 当日用量预算不由插件自行修改。
    fn daily_token_budget(&self) -> u64 {
        self.record.daily_token_budget
    }
    /// 当前 generation 的服务限频策略独立于协议实例。
    fn qps(&self) -> u32 {
        self.qps
    }
    /// 静态模型沿用插件已校验的模型列表。
    fn static_models(&self) -> Vec<String> {
        self.inner.static_models()
    }
    /// 保留动态目录的真实能力和价格。
    async fn list_models(&self) -> Result<Vec<ModelInfo>, AppError> {
        self.inner.list_models().await
    }
    /// 宿主配置不能覆盖私有传输的表达限制。
    fn validate_request(&self, request: &ChatRequest) -> Result<(), String> {
        self.inner.validate_request(request)
    }
    /// 输出限制是否透传由协议实现决定。
    fn output_enforced(&self) -> bool {
        self.inner.output_enforced()
    }
    /// 厂商目录和输入限制在适配器中归一。
    fn model_limits(&self, model: &ModelInfo) -> TokenLimits {
        self.inner.model_limits(model)
    }
    /// 核心继续使用已有账号健康与冷却。
    fn routing_accounts(&self) -> Option<Vec<crate::accounts::AccountState>> {
        self.inner.routing_accounts()
    }
    /// 额度事实保留原始来源和未知语义。
    async fn routing_quota(
        &self,
        account_id: &str,
    ) -> Result<crate::accounts::remote::QuotaInfo, AppError> {
        self.inner.routing_quota(account_id).await
    }
    /// 已选账号与取消令牌透明传递，不增加重试。
    async fn chat_on_account(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
        account_id: Option<&str>,
    ) -> Result<EventStream, AppError> {
        self.inner
            .chat_on_account(request, cancel, account_id)
            .await
    }
    /// 原有流式协议和终态语义保持不变。
    async fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, AppError> {
        self.inner.chat(request, cancel).await
    }
}
