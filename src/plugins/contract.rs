//! 插件描述与运行实例的组合契约；描述能力不代表账号当前可用。

use super::ScopedHostServices;
use crate::accounts::remote::{CredentialRefresher, QuotaReader};
use crate::backend::Backend;
use crate::core::AppError;
use crate::store::BackendRecord;
use crate::tasks::TaskProvider;
use serde::Serialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub const CONTRACT_VERSION: u32 = 1;

/// 可发现的管理动作；远程副作用由调用方显式授权。
#[derive(Clone, Debug, Serialize)]
pub struct OperationDescriptor {
    pub id: String,
    pub title: String,
    pub effect: String,
    pub idempotency: String,
    pub interaction: Option<String>,
    pub input_schema: Value,
    pub output_schema: Value,
}

/// 授权交互和旧受控视图的地址；预设不能注入脚本或 HTML。
#[derive(Clone, Debug, Serialize)]
pub struct AuthFlowDescriptor {
    pub id: String,
    pub title: String,
    pub interaction: String,
    pub start_path: Option<String>,
    pub poll_path: Option<String>,
    pub cancel_path: Option<String>,
}

/// 插件真实实现支持的能力，不能作为模型或账号权限的证据。
#[derive(Clone, Debug, Default, Serialize)]
pub struct Features {
    pub inference: bool,
    pub catalog: bool,
    pub accounts: bool,
    pub quota: bool,
    pub credential_refresh: bool,
    pub checkin: bool,
}

/// 同一描述用于 HTTP 发现、CLI、UI 和插件契约检查。
#[derive(Clone, Debug, Serialize)]
pub struct PluginDescriptor {
    pub id: String,
    pub version: String,
    pub contract_version: u32,
    pub config_schema_version: u32,
    pub display_name: String,
    pub ui_group: String,
    pub docs: String,
    pub config_schema: Value,
    pub auth_flows: Vec<AuthFlowDescriptor>,
    pub operations: Vec<OperationDescriptor>,
    pub features: Features,
    pub ui: Value,
}

impl PluginDescriptor {
    /// 从实际能力组合生成公共动作；插件可添加受支持的专属动作。
    pub fn new(id: &str, name: &str, group: &str, schema: Value, features: Features) -> Self {
        let mut operations = vec![operation(
            "models.sync",
            "同步模型",
            "read_remote",
            "repeatable",
        )];
        if features.quota {
            operations.push(operation(
                "quota.refresh",
                "更新额度",
                "read_remote",
                "repeatable",
            ));
        }
        if features.credential_refresh {
            operations.push(operation(
                "credential.refresh",
                "刷新凭证",
                "write_remote",
                "fenced",
            ));
        }
        if features.checkin {
            operations.push(operation(
                "checkin",
                "领取每日奖励",
                "write_remote",
                "period",
            ));
        }
        Self {
            id: id.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            contract_version: CONTRACT_VERSION,
            config_schema_version: 1,
            display_name: name.into(),
            ui_group: group.into(),
            docs: format!("docs/providers/{}.md", id.replace('_', "-")),
            config_schema: schema,
            auth_flows: Vec::new(),
            operations,
            features,
            ui: json!({"field_order":["base_url","models","options"],
                "advanced":["options"],"secret_fields":["credentials","options.headers"]}),
        }
    }

    /// 声明沿用现有会话保护的授权流程。
    pub fn with_auth(mut self, interaction: &str, cancellable: bool) -> Self {
        self.auth_flows.push(AuthFlowDescriptor {
            id: "login".into(),
            title: "登录授权".into(),
            interaction: interaction.into(),
            start_path: Some(format!("/api/oauth/{}/start", self.id)),
            poll_path: Some(format!("/api/oauth/{}/poll", self.id)),
            cancel_path: cancellable.then(|| format!("/api/oauth/{}/cancel", self.id)),
        });
        let mut auth = operation("auth.start", "登录授权", "write_remote", "session");
        auth.interaction = Some(interaction.into());
        auth.input_schema = json!({"type":"object","additionalProperties":false});
        self.operations.push(auth);
        self
    }
}

/// 构造公共操作的固定发现信息；执行仍必须通过注册实例。
fn operation(id: &str, title: &str, effect: &str, idempotency: &str) -> OperationDescriptor {
    OperationDescriptor {
        id: id.into(),
        title: title.into(),
        effect: effect.into(),
        idempotency: idempotency.into(),
        interaction: None,
        input_schema: if id == "models.sync" {
            json!({"type":"object","additionalProperties":false})
        } else {
            json!({"type":"object","properties":{"account_id":{"type":"string"}},"additionalProperties":false})
        },
        output_schema: json!({"type":"object"}),
    }
}

/// 工厂不持有账号状态；prepare 只构造本地客户端，不执行上游动作。
pub trait ProviderPlugin: Send + Sync {
    /// 插件拥有账号来源和凭证规范化规则，公共入口不判断厂商。
    fn account_control(&self) -> AccountControl {
        AccountControl::MANUAL
    }
    /// 可信控制适配层挂载插件专有页面和兼容授权路径。
    fn control_routes(&self) -> axum::Router<crate::state::AppState> {
        super::control::no_routes()
    }

    /// 返回可跟踪授权的实际执行器，无能力时不提供占位成功实现。
    fn auth_control(&self) -> Option<super::control::AuthControl> {
        None
    }
    /// 返回稳定身份、配置 schema 与实际能力目录。
    fn descriptor(&self) -> PluginDescriptor;

    /// 校验协议相关字段；公共配置语义由配置服务校验。
    fn validate(&self, record: &BackendRecord) -> Result<(), AppError>;

    /// 组合一次服务实例，多个 trait 视图共享同一内部状态。
    fn prepare(
        &self,
        record: &BackendRecord,
        host: ScopedHostServices,
    ) -> Result<ProviderRuntime, AppError>;
}

/// 一次服务 generation 的能力；在途生成持有 Arc，自然排空旧实例。
#[derive(Clone)]
pub struct ProviderRuntime {
    pub inference: Arc<dyn Backend>,
    pub quota: Option<Arc<dyn QuotaReader>>,
    pub credential_refresh: Option<Arc<dyn CredentialRefresher>>,
    pub tasks: Vec<Arc<dyn TaskProvider>>,
}

impl ProviderRuntime {
    /// 创建只含推理的实例，再按真实实现补充可选能力。
    pub fn inference(backend: Arc<dyn Backend>) -> Self {
        Self {
            inference: backend,
            quota: None,
            credential_refresh: None,
            tasks: Vec::new(),
        }
    }
}

/// 内置工厂通过函数组合，新增服务只增加自己的模块与一个注册项。
pub struct PluginDefinition {
    pub describe: fn() -> PluginDescriptor,
    pub validate_record: fn(&BackendRecord) -> Result<(), AppError>,
    pub build: fn(&BackendRecord, ScopedHostServices) -> Result<ProviderRuntime, AppError>,
    pub routes: fn() -> axum::Router<crate::state::AppState>,
    pub auth: Option<super::control::AuthControl>,
    pub accounts: AccountControl,
}

/// 派生账号与人工凭证校验是独立于授权交互的插件能力。
#[derive(Clone, Copy)]
pub struct AccountControl {
    pub managed: bool,
    pub derive: fn(&BackendRecord) -> Vec<crate::accounts::Account>,
    pub normalize: fn(&BackendRecord, &str) -> Result<String, AppError>,
}

impl AccountControl {
    pub const MANUAL: Self = Self {
        managed: false,
        derive: no_derived_accounts,
        normalize: unchanged_credential,
    };
}

/// 默认插件没有从服务凭证派生的账号。
fn no_derived_accounts(_: &BackendRecord) -> Vec<crate::accounts::Account> {
    Vec::new()
}

/// 未定义特殊格式的插件沿用现有凭证，不推测 token 类型。
fn unchanged_credential(_: &BackendRecord, value: &str) -> Result<String, AppError> {
    Ok(value.into())
}

impl ProviderPlugin for PluginDefinition {
    /// 返回同注册项维护的账号来源和校验能力。
    fn account_control(&self) -> AccountControl {
        self.accounts
    }
    /// 挂载同插件目录维护的授权端点。
    fn control_routes(&self) -> axum::Router<crate::state::AppState> {
        (self.routes)()
    }
    /// 能力执行器与描述共用同一注册项。
    fn auth_control(&self) -> Option<super::control::AuthControl> {
        self.auth
    }
    /// 生成机器可读目录。
    fn descriptor(&self) -> PluginDescriptor {
        (self.describe)()
    }
    /// 执行插件的无网络语义检查。
    fn validate(&self, record: &BackendRecord) -> Result<(), AppError> {
        (self.validate_record)(record)
    }
    /// 返回该服务的完整能力组合。
    fn prepare(
        &self,
        record: &BackendRecord,
        host: ScopedHostServices,
    ) -> Result<ProviderRuntime, AppError> {
        (self.build)(record, host)
    }
}

/// 各服务配置共享基本形状，专属 options schema 来自 Rust 类型。
pub fn config_schema<T: schemars::JsonSchema>() -> Value {
    let mut options =
        serde_json::to_value(schemars::schema_for!(T)).expect("JSON schema serializes");
    let mut definitions = options
        .as_object_mut()
        .and_then(|object| object.remove("$defs"))
        .unwrap_or_else(|| json!({}));
    let mut routing = serde_json::to_value(schemars::schema_for!(
        crate::routing::planner::RouteSettings
    ))
    .expect("JSON schema serializes");
    if let Some(extra) = routing
        .as_object_mut()
        .and_then(|object| object.remove("$defs"))
        .and_then(|v| v.as_object().cloned())
    {
        definitions
            .as_object_mut()
            .expect("schema definitions are objects")
            .extend(extra);
    }
    options["additionalProperties"] = json!(false);
    // v1 把宿主策略放在 options.routing；v2 写入时仍可确定性转换。
    options["properties"]["routing"] = routing;
    json!({
        "$schema":"https://json-schema.org/draft/2020-12/schema",
        "$defs": definitions,
        "type":"object", "additionalProperties":false,
        "properties":{
            "base_url":{"type":"string","format":"uri","title":"服务地址"},
            "models":{"type":"array","items":{"type":"string"},"uniqueItems":true,"title":"候选模型"},
            "options":options,
            "capabilities":{"type":"object","additionalProperties":false,"properties":{
                "tools":{"type":"boolean"},"parallel_tools":{"type":"boolean"},
                "images":{"type":"boolean"},"reasoning":{"type":"boolean"},
                "context_window":{"type":"integer","minimum":1}
            }}
        },
        "required":["base_url"]
    })
}

/// 无专属配置的插件拒绝未知 options 字段。
#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EmptyOptions {}

/// 公共静态语义检查；不读取凭证，也不发送网络请求。
pub fn validate_record(record: &BackendRecord) -> Result<(), AppError> {
    if record.id.trim().is_empty() || record.name.trim().is_empty() {
        return Err(AppError::bad_request("service id and name are required"));
    }
    if !record.base_url.is_empty() {
        let url = reqwest::Url::parse(&record.base_url)
            .map_err(|_| AppError::bad_request("base_url must be an absolute HTTP URL"))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(AppError::bad_request(
                "base_url must be HTTP(S) without embedded credentials or fragment",
            ));
        }
    }
    if !matches!(
        record.cost_tier.as_str(),
        "free" | "subscription" | "paid" | "unknown"
    ) {
        return Err(AppError::bad_request("invalid cost_tier"));
    }
    Ok(())
}
