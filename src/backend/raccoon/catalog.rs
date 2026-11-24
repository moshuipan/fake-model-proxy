//! 小浣熊目录以 name 为调用 ID，UUID、描述和积分倍率不混用。

use crate::core::{Capabilities, ModelInfo};
use serde_json::Value;

/// 只读取官方 categories[].models[]，能力和 token 限额不凭模型名猜测。
pub(super) fn parse(data: &Value, defaults: &Capabilities) -> Vec<ModelInfo> {
    let mut models = Vec::new();
    for category in data["categories"].as_array().into_iter().flatten() {
        for model in category["models"].as_array().into_iter().flatten() {
            let Some(id) = model["name"]
                .as_str()
                .map(str::trim)
                .filter(|id| !id.is_empty() && id.len() < 128 && !id.contains([',', '\n', '\r']))
            else {
                continue;
            };
            if model["hidden"].as_bool() == Some(true)
                || model["disabled"].as_bool() == Some(true)
                || model["enabled"].as_bool() == Some(false)
                || models.iter().any(|existing: &ModelInfo| existing.id == id)
            {
                continue;
            }
            let mut capabilities = crate::backend::catalog::capabilities(model, defaults);
            let mut token_limits = crate::backend::catalog::token_limits(model);
            token_limits.context_window = token_limits
                .context_window
                .or_else(|| limit(model, "context_window"));
            // 此官方目录的 max_tokens 表示输出上限，不能推广到其他提供商目录。
            token_limits.max_output_tokens = token_limits
                .max_output_tokens
                .or_else(|| limit(model, "max_tokens"));
            capabilities.context_window = token_limits.context_window;
            models.push(ModelInfo {
                id: id.into(),
                display_name: model["display_name"]
                    .as_str()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(ToOwned::to_owned),
                price_factor: model
                    .get("billing_effective_multiplier")
                    .or_else(|| model.get("billing_multiplier"))
                    .and_then(crate::backend::catalog::numeric_factor),
                original_price_factor: None,
                price_valid_until: None,
                capabilities,
                token_limits,
            });
        }
    }
    models
}

/// 参数可位于顶层或 params，拒绝零、负数、非整数和超出 u32 的窗口。
fn limit(model: &Value, key: &str) -> Option<u32> {
    [model, &model["params"]].into_iter().find_map(|source| {
        let value = source.get(key)?;
        let integer = value.as_u64().or_else(|| value.as_str()?.parse().ok())?;
        u32::try_from(integer).ok().filter(|value| *value > 0)
    })
}
