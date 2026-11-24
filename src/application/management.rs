//! 外部控制面的授权用例；所有传输只做输入输出映射。

use super::{
    access::{Principal, Scope},
    config_service::{ApplyResult, ConfigPlan, ConfigView},
    config_types::{ConfigChange, ValidationReport, error},
};
use crate::{core::AppError, state::AppState};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// 计划请求必须声明读取过的版本，防止客户端静默覆盖并发更新。
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanInput {
    pub expected_revision: u64,
    pub change: ConfigChange,
}

/// 路由解释只有请求形状，不需要私有对话正文，也不触发远程刷新。
#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ExplainInput {
    pub model: Option<String>,
    pub input_tokens: u32,
    pub output_tokens: Option<u32>,
    pub tools: bool,
    pub images: bool,
}

/// 受权限保护的应用门面；内部后台任务使用自身限定的能力。
pub struct Management<'a> {
    state: &'a AppState,
    principal: &'a Principal,
}

impl<'a> Management<'a> {
    /// 读取默认值目录时合并公开自定义预设。
    pub async fn presets(&self) -> Result<Value, AppError> {
        self.principal.require(Scope::Read)?;
        Ok(
            json!({"presets":crate::presets::catalog(&self.state.store).await?
            .iter().map(|p| p.view()).collect::<Vec<_>>()}),
        )
    }

    /// 导入目录只允许使用实际安装的插件，不变更既有实例。
    pub async fn import_presets(
        &self,
        presets: Vec<crate::presets::ProviderPreset>,
    ) -> Result<Value, AppError> {
        self.principal.require(Scope::ConfigWrite)?;
        if presets.is_empty() || presets.len() > 100 {
            return Err(error("preset_invalid", "每次导入需要 1–100 个预设"));
        }
        crate::presets::validate(&presets)?;
        self.state
            .store
            .import_presets(&presets, &self.principal.id)
            .await?;
        Ok(
            json!({"imported":presets.iter().map(|p| &p.id).collect::<Vec<_>>(),
            "existing_services_changed":false}),
        )
    }
    /// 一个请求携带一个已验证身份。
    pub fn new(state: &'a AppState, principal: &'a Principal) -> Self {
        Self { state, principal }
    }

    /// 读取实际编译安装的插件目录。
    pub fn plugins(&self) -> Result<Vec<crate::plugins::PluginDescriptor>, AppError> {
        self.principal.require(Scope::Read)?;
        Ok(crate::plugins::builtin::registry().descriptors())
    }

    /// 读取配置只返回引用和实际生效版本。
    pub async fn config(&self) -> Result<ConfigView, AppError> {
        self.principal.require(Scope::Read)?;
        self.state.config_service().current().await
    }

    /// 校验属于只读能力，不执行上游探测。
    pub async fn validate(&self, change: ConfigChange) -> Result<ValidationReport, AppError> {
        self.principal.require(Scope::Read)?;
        self.state.config_service().validate(change).await
    }

    /// 只读身份可以拟定计划，应用仍需独立写权限。
    pub async fn plan(&self, input: PlanInput) -> Result<ConfigPlan, AppError> {
        self.principal.require(Scope::Read)?;
        self.state
            .config_service()
            .plan(input.expected_revision, input.change)
            .await
    }

    /// 查询持久计划，不暴露包含引用的未提交原始文档。
    pub async fn get_plan(&self, id: &str) -> Result<ConfigPlan, AppError> {
        self.principal.require(Scope::Read)?;
        Ok(self.state.store.config_plan(id).await?.0)
    }

    /// 外部 apply 在应用层检查权限并记录真实调用者。
    pub async fn apply(&self, id: &str, key: &str) -> Result<ApplyResult, AppError> {
        self.principal.require(Scope::ConfigWrite)?;
        self.state
            .config_service()
            .apply(id, key, &self.principal.id)
            .await
    }

    /// 回滚仍产生计划，随后用统一 apply 创建新 revision。
    pub async fn rollback(&self, expected: u64, target: u64) -> Result<ConfigPlan, AppError> {
        self.principal.require(Scope::Read)?;
        self.state
            .config_service()
            .rollback_plan(expected, target)
            .await
    }

    /// 新秘密写入不可变引用；替换服务凭证须再通过 plan/apply 显式绑定。
    pub async fn write_secret(&self, value: &str) -> Result<Value, AppError> {
        self.principal.require(Scope::SecretWrite)?;
        if value.trim().is_empty() || value.len() > 256 * 1024 {
            return Err(error("secret_invalid", "凭证必须非空且不超过 256 KiB"));
        }
        let reference = format!("store:{}", uuid::Uuid::new_v4());
        self.state
            .store
            .insert_config_secret(&reference, value, &self.principal.id)
            .await?;
        Ok(json!({"secret_ref":reference,"configured":true}))
    }

    /// 解释捕获的完整 generation，读取已有目录，绝不隐式查询上游。
    pub fn explain(&self, input: ExplainInput) -> Result<Value, AppError> {
        self.principal.require(Scope::Read)?;
        if input.input_tokens > 2_000_000 {
            return Err(error("schema_invalid", "input_tokens 不能超过 2000000"));
        }
        let model = input.model.unwrap_or_else(|| "auto".into());
        let (policy, free_only) = crate::routing::RoutePolicy::parse(&model)
            .ok_or_else(|| error("schema_invalid", "路由解释使用 auto 或兼容的 auto 名称"))?;
        let (registry, router, revision) = {
            let _guard = self
                .state
                .generation_lock
                .read()
                .unwrap_or_else(|p| p.into_inner());
            (
                self.state.registry.fork(),
                self.state.router.fork(),
                self.state
                    .active_revision
                    .load(std::sync::atomic::Ordering::Acquire),
            )
        };
        let mut content = vec![crate::core::Message::text(
            crate::core::Role::User,
            "x".repeat(input.input_tokens.max(1) as usize),
        )];
        if input.images {
            content[0].content.push(crate::core::ContentPart::ImageUrl {
                url: String::new(),
                detail: None,
            });
        }
        let request = crate::core::ChatRequest {
            protocol: crate::core::Protocol::Chat,
            model,
            messages: content,
            tools: if input.tools {
                vec![crate::core::ToolDef {
                    name: "preview".into(),
                    description: String::new(),
                    parameters: json!({"type":"object"}),
                }]
            } else {
                vec![]
            },
            tool_choice: None,
            max_tokens: input.output_tokens,
            temperature: None,
            top_p: None,
            stop: vec![],
            reasoning_effort: None,
            stream: false,
            session: None,
        };
        let decision = router.plan(&registry, &request, policy, free_only);
        Ok(json!({
            "active_revision":revision,"strategy":policy.as_str(),"source":"cached_snapshot",
            "reservation":"reservation_unavailable","comparison_scope":decision.comparison_scope,
            "auto_allow_paid_fallback":router.gateway.settings().auto_allow_paid_fallback && !free_only,
            "candidates":decision.candidates.iter().map(|c| json!({
                "service_id":c.backend.id(),"service_name":c.backend.name(),"model":c.model,
                "account_id":c.account_id,"cost_tier":c.cost_tier.as_str(),"pool":c.pool(),
                "priority":c.priority,"quota_known":c.quota_known(),"quota":c.quota,
                "price_factor":c.price_factor,"comparison_scope":c.comparison_scope,"limits":c.limits
            })).collect::<Vec<_>>(),
            "rejected":decision.rejected.iter().map(|r| json!({"model":r.model,"reason":r.reason})).collect::<Vec<_>>()
        }))
    }
}
