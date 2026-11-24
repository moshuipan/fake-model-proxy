//! v2 配置契约：对象、数组与凭证引用；内容不保存运行进度或余额。

use crate::{
    core::AppError, gateway::GatewaySettings, model_names::ModelAlias,
    routing::planner::RouteSettings, tasks::scheduler::ScheduleInput,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// 引用服务器环境变量或宿主凭证库；不接收客户端文件路径。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SecretRef {
    pub secret_ref: String,
}

/// API 密钥和请求头凭证只通过引用进入声明配置。
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServiceCredentials {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<SecretRef>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, SecretRef>,
}

/// 连接与协议设置；厂商 options 使用插件导出的 schema 校验。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub base_url: String,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default = "empty_object")]
    pub options: Value,
    #[serde(default = "empty_object")]
    pub capabilities: Value,
}

/// 宿主拥有调度策略，插件不能扩展费用许可。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ServicePolicy {
    pub priority: u8,
    pub cost_tier: String,
    pub daily_token_budget: u64,
    #[serde(flatten)]
    pub routing: RouteSettings,
}

impl Default for ServicePolicy {
    /// 未确认费用保持 unknown。
    fn default() -> Self {
        Self {
            priority: 100,
            cost_tier: "unknown".into(),
            daily_token_budget: 0,
            routing: RouteSettings::default(),
        }
    }
}

/// 可单独提交的服务声明；版本变化不能被旧客户端静默忽略。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServiceSpec {
    pub api_version: String,
    pub kind: String,
    pub id: String,
    pub name: String,
    pub plugin_id: String,
    pub plugin_config_version: u32,
    pub enabled: bool,
    pub config: ServiceConfig,
    #[serde(default)]
    pub credentials: ServiceCredentials,
    #[serde(default)]
    pub policy: ServicePolicy,
}

/// 账号身份和启停进入配置，正常 token 轮换只更新所引用的秘密。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountSpec {
    pub id: String,
    pub service_id: String,
    pub name: String,
    pub enabled: bool,
    pub credential: SecretRef,
}

/// 一次可原子提交的完整配置，不包含连接进度或额度缓存。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigDocument {
    pub api_version: String,
    pub kind: String,
    pub services: Vec<ServiceSpec>,
    pub accounts: Vec<AccountSpec>,
    pub model_aliases: Vec<ModelAlias>,
    pub gateway: GatewaySettings,
    pub checkin: ScheduleInput,
}

/// 配置变更采用明确的 upsert/remove，缺省数组表示保持已有内容。
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigChange {
    #[serde(default)]
    pub services: Vec<ServiceSpec>,
    #[serde(default)]
    pub remove_services: Vec<String>,
    #[serde(default)]
    pub accounts: Vec<AccountSpec>,
    #[serde(default)]
    pub remove_accounts: Vec<String>,
    #[serde(default)]
    pub model_aliases: Vec<ModelAlias>,
    #[serde(default)]
    pub gateway: Option<GatewaySettings>,
    #[serde(default)]
    pub checkin: Option<ScheduleInput>,
}

/// 校验错误包含稳定字段路径与修复方向，不回显非法输入值。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: String,
    pub path: String,
    pub message: String,
    pub hint: String,
}

/// 校验与计划共用相同诊断。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationReport {
    pub valid: bool,
    pub diagnostics: Vec<Diagnostic>,
}

/// 创建默认空对象，避免对外使用嵌套 JSON 字符串。
fn empty_object() -> Value {
    json!({})
}

/// 机器稳定错误码；详细字段诊断放在 validate 返回值中。
pub fn error(code: &str, message: impl Into<String>) -> AppError {
    AppError::BadRequest {
        code: code.into(),
        message: message.into(),
    }
}

/// 从 Rust 类型生成 HTTP、CLI 与界面共用的结构 schema。
pub fn schema<T: JsonSchema>() -> Value {
    let mut value = serde_json::to_value(schemars::schema_for!(T)).expect("schema is JSON");
    close_objects(&mut value);
    value
}

/// 有固定 properties 的类型拒绝未知字段，自由映射保留自身规则。
fn close_objects(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if object.contains_key("properties") && !object.contains_key("additionalProperties") {
                object.insert("additionalProperties".into(), json!(false));
            }
            for child in object.values_mut() {
                close_objects(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(close_objects),
        _ => {}
    }
}

impl ConfigDocument {
    /// 在完整文档上合并声明变更；删除服务一并删除其账号，历史名称仍保留。
    pub fn merge(&mut self, change: ConfigChange) -> Result<(), AppError> {
        for service in change.services {
            self.services.retain(|old| old.id != service.id);
            self.services.push(service);
        }
        for account in change.accounts {
            self.accounts.retain(|old| old.id != account.id);
            self.accounts.push(account);
        }
        self.services
            .retain(|s| !change.remove_services.contains(&s.id));
        self.accounts.retain(|a| {
            !change.remove_accounts.contains(&a.id)
                && !change.remove_services.contains(&a.service_id)
        });
        for alias in change.model_aliases {
            if self
                .model_aliases
                .iter()
                .any(|old| old.alias == alias.alias && old.address() != alias.address())
            {
                return Err(error("alias_conflict", "历史调用名称不能重新绑定其他模型"));
            }
            if alias.is_primary {
                for old in &mut self.model_aliases {
                    if old.address() == alias.address() {
                        old.is_primary = false;
                    }
                }
            }
            self.model_aliases.retain(|old| old.alias != alias.alias);
            self.model_aliases.push(alias);
        }
        if let Some(gateway) = change.gateway {
            self.gateway = gateway;
        }
        if let Some(checkin) = change.checkin {
            self.checkin = checkin;
        }
        self.normalize();
        Ok(())
    }

    /// 规范化无序集合，使摘要不依赖提交顺序。
    pub fn normalize(&mut self) {
        self.services.sort_by(|a, b| a.id.cmp(&b.id));
        self.accounts.sort_by(|a, b| a.id.cmp(&b.id));
        self.model_aliases.sort_by(|a, b| a.alias.cmp(&b.alias));
        for service in &mut self.services {
            service.config.models.sort();
            service.config.models.dedup();
            service.policy.routing.candidate_models.sort();
            service.policy.routing.candidate_models.dedup();
        }
    }
}
