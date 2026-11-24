//! 数据化服务预设；只引用已安装插件，不创造协议能力、不加载脚本。

use crate::{
    application::config_types::{ServiceConfig, ServicePolicy, ServiceSpec, error},
    core::AppError,
    store::{BackendRecord, Store},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::LazyLock,
};

/// 可导入的公开配置默认值；凭证不允许出现在预设中。
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderPreset {
    pub api_version: String,
    pub id: String,
    pub name: String,
    pub plugin_id: String,
    pub plugin_config_version: u32,
    pub config: ServiceConfig,
    #[serde(default)]
    pub policy: ServicePolicy,
    pub docs: String,
}

/// 公开预设随编译发布；增加兼容平台只需改数据。
pub static PRESETS: LazyLock<Vec<ProviderPreset>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../presets/builtin.json"))
        .expect("built-in preset data must match its Rust schema")
});

/// 按稳定 id 查找内置预设。
pub fn find(id: &str) -> Option<&'static ProviderPreset> {
    PRESETS.iter().find(|preset| preset.id == id)
}

impl ProviderPreset {
    /// 创建未绑定凭证的服务草稿，不自动启用或覆盖已配置实例。
    pub fn draft(&self, id: &str) -> ServiceSpec {
        ServiceSpec {
            api_version: "fmp/v2".into(),
            kind: "Service".into(),
            id: id.into(),
            name: self.name.clone(),
            plugin_id: self.plugin_id.clone(),
            plugin_config_version: self.plugin_config_version,
            enabled: false,
            config: self.config.clone(),
            credentials: Default::default(),
            policy: self.policy.clone(),
        }
    }

    /// v1 响应保留旧字段，增加 canonical 数据供新界面读取。
    pub fn view(&self) -> Value {
        let group = crate::plugins::builtin::registry()
            .get(&self.plugin_id)
            .map(|p| p.descriptor().ui_group)
            .unwrap_or_default();
        let mut value = serde_json::to_value(self).expect("preset is JSON");
        value["kind"] = json!(self.plugin_id);
        value["ui_group"] = json!(group);
        value["base_url"] = json!(self.config.base_url);
        value["models_hint"] = json!(self.config.models.join(","));
        value["priority"] = json!(self.policy.priority);
        value["cost_tier"] = json!(self.policy.cost_tier);
        value
    }
}

/// 校验公开默认值与实际插件 schema；缺少秘密允许作为待绑定的草稿。
pub fn validate(presets: &[ProviderPreset]) -> Result<(), AppError> {
    let mut ids = BTreeSet::new();
    for preset in presets {
        if preset.api_version != "fmp/preset/v1"
            || preset.id.is_empty()
            || preset.id.len() > 100
            || !preset
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
            || !ids.insert(&preset.id)
            || preset.name.trim().is_empty()
        {
            return Err(error("preset_invalid", "预设版本、身份或名称不合法"));
        }
        let plugin = crate::plugins::builtin::registry().get(&preset.plugin_id)?;
        let descriptor = plugin.descriptor();
        if preset.plugin_config_version != descriptor.config_schema_version {
            return Err(error(
                "plugin_incompatible",
                "预设要求的插件配置版本不受支持",
            ));
        }
        let schema = jsonschema::validator_for(&descriptor.config_schema)
            .map_err(|_| error("plugin_incompatible", "插件 schema 无效"))?;
        if !schema.is_valid(&serde_json::to_value(&preset.config).expect("config is JSON")) {
            return Err(error("schema_invalid", "预设配置不符合插件 schema"));
        }
        if preset.config.options.get("headers").is_some() || contains_secret(&preset.config.options)
        {
            return Err(error(
                "preset_secret_forbidden",
                "预设不得包含凭证或请求头秘密",
            ));
        }
        let docs = reqwest::Url::parse(&preset.docs)
            .map_err(|_| error("preset_invalid", "预设文档需要 HTTP(S) 地址"))?;
        if !matches!(docs.scheme(), "http" | "https")
            || docs.password().is_some()
            || !docs.username().is_empty()
        {
            return Err(error("preset_invalid", "预设文档地址无效"));
        }
        let record = BackendRecord {
            id: preset.id.clone(),
            name: preset.name.clone(),
            kind: preset.plugin_id.clone(),
            base_url: preset.config.base_url.clone(),
            models: preset.config.models.join(","),
            options: preset.config.options.to_string(),
            capabilities: preset.config.capabilities.to_string(),
            enabled: false,
            priority: preset.policy.priority,
            cost_tier: preset.policy.cost_tier.clone(),
            daily_token_budget: preset.policy.daily_token_budget,
            ..Default::default()
        };
        crate::plugins::contract::validate_record(&record)?;
        let policy_schema = crate::application::config_types::schema::<ServicePolicy>();
        if !jsonschema::validator_for(&policy_schema)
            .map_err(|_| error("plugin_incompatible", "策略 schema 无效"))?
            .is_valid(&serde_json::to_value(&preset.policy).expect("policy is JSON"))
        {
            return Err(error("schema_invalid", "预设策略不符合配置 schema"));
        }
        plugin.validate(&record)?;
    }
    Ok(())
}

/// 敏感位置由代码拒绝，不允许通过嵌套对象藏入公开默认值。
fn contains_secret(value: &Value) -> bool {
    match value {
        Value::Object(object) => object.iter().any(|(key, value)| {
            matches!(
                key.to_ascii_lowercase().as_str(),
                "api_key"
                    | "token"
                    | "access_token"
                    | "refresh_token"
                    | "credential"
                    | "password"
                    | "authorization"
                    | "cookie"
                    | "secret_ref"
            ) || contains_secret(value)
        }),
        Value::Array(values) => values.iter().any(contains_secret),
        _ => false,
    }
}

/// 合并持久自定义预设；同 id 只影响以后创建的草稿，不改已有服务。
pub async fn catalog(store: &Store) -> Result<Vec<ProviderPreset>, AppError> {
    let mut catalog = PRESETS
        .iter()
        .cloned()
        .map(|p| (p.id.clone(), p))
        .collect::<BTreeMap<_, _>>();
    for preset in store.custom_presets().await? {
        catalog.insert(preset.id.clone(), preset);
    }
    Ok(catalog.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    /// 所有内置数据必须满足真实插件 schema 与无秘密规则。
    #[test]
    fn builtins_match_installed_plugins() {
        for preset in PRESETS.iter() {
            validate(std::slice::from_ref(preset))
                .unwrap_or_else(|error| panic!("{}: {error}", preset.id));
        }
    }
    /// 找不到的预设不隐式回退为其他服务。
    #[test]
    fn finds_by_id() {
        assert!(find("bailian").is_some());
        assert!(find("missing").is_none());
    }
}
