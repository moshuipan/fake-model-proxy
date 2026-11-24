//! 对外模型调用名称，与内部路由地址及显示名称独立保存。

use crate::core::AppError;
use crate::store::BackendRecord;
use serde::{Deserialize, Serialize};

/// 名称一经分配便绑定原模型；历史名称继续可调用。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelAlias {
    pub alias: String,
    pub backend_id: String,
    pub model_id: String,
    pub is_primary: bool,
}

impl ModelAlias {
    /// 返回内部精确路由地址。
    pub fn address(&self) -> String {
        format!("{}/{}", self.backend_id, self.model_id)
    }
}

/// 将官方名称转为常见 SDK 可直接使用的 ASCII 名称。
pub fn slug(value: &str) -> String {
    let mut result = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' {
            result.push(ch.to_ascii_lowercase());
        } else if !result.is_empty() && !result.ends_with('-') {
            result.push('-');
        }
    }
    result.trim_matches(['-', '.', '_']).to_string()
}

/// 生成首次分配的候选名称；后续配置改名不重新执行分配。
pub fn default_alias(record: &BackendRecord, model_id: &str, display_name: Option<&str>) -> String {
    let prefix = match record.kind.as_str() {
        "minimax_code" => "minimax-code".into(),
        "qoder" | "workbuddy" | "autoglm" => record.kind.clone(),
        _ => slug(&record.name),
    };
    let prefix = if prefix.is_empty() {
        "service"
    } else {
        &prefix
    };
    let model = slug(
        display_name
            .filter(|name| !slug(name).is_empty())
            .unwrap_or(model_id),
    );
    let model = if model.is_empty() { "model" } else { &model };
    let result: String = format!("{prefix}-{model}").chars().take(80).collect();
    if validate_alias(&result).is_ok() {
        result
    } else {
        format!("service-{result}")
    }
}

/// 校验调用名称，避免协议分隔符和自动路由保留名称。
pub fn validate_alias(value: &str) -> Result<(), AppError> {
    if value.is_empty()
        || value.len() > 96
        || !value
            .bytes()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || b"._-".contains(&ch))
        || !value.as_bytes()[0].is_ascii_alphanumeric()
        || value == "auto"
        || value.starts_with("auto-")
        || value.starts_with("auto.")
        || value.starts_with("auto_")
    {
        return Err(AppError::bad_request(
            "调用名称须为 1–96 位小写英文字母、数字、点、下划线或短横线，以字母或数字开头，auto 系列为保留名称",
        ));
    }
    Ok(())
}

/// 不能用别名遮蔽其他模型原有的裸模型名。
pub fn check_native_collision(
    alias: &ModelAlias,
    records: &[BackendRecord],
) -> Result<(), AppError> {
    if records.iter().any(|record| {
        record.models.split(',').map(str::trim).any(|model| {
            model == alias.alias && (record.id != alias.backend_id || model != alias.model_id)
        })
    }) {
        return Err(AppError::bad_request("调用名称与另一个模型的原始名称冲突"));
    }
    Ok(())
}
