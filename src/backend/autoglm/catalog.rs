//! AutoGLM 目录元数据；路由 ID、官方显示名与明确积分倍率分别保留。

use crate::backend::catalog::{credit_multiplier, numeric_factor};
use crate::core::{Capabilities, ModelInfo};
use serde_json::Value;

/// 读取模型集合或显式模型字段，保留目录顺序并合并重复项缺失的元数据。
pub(super) fn parse(value: &Value, capabilities: &Capabilities) -> Vec<ModelInfo> {
    let mut models = Vec::new();
    visit(value, value.is_array(), capabilities, &mut models);
    models
}

/// 只在模型集合接受通用 id；分组、请求和活动的 id 不进入模型白名单。
fn visit(
    value: &Value,
    model_collection: bool,
    capabilities: &Capabilities,
    models: &mut Vec<ModelInfo>,
) {
    match value {
        Value::Object(object) => {
            let id = ["model_id", "model", "id"].into_iter().find_map(|key| {
                if key == "id" && !model_collection {
                    return None;
                }
                object
                    .get(key)
                    .and_then(Value::as_str)
                    .filter(|id| !id.trim().is_empty() && id.len() < 128)
            });
            if let Some(id) = id {
                if object.get("hidden").and_then(Value::as_bool) == Some(true)
                    || object.get("disabled").and_then(Value::as_bool) == Some(true)
                    || ["enable", "enabled"]
                        .iter()
                        .any(|key| object.get(*key).and_then(Value::as_bool) == Some(false))
                {
                    return;
                }
                let display_name = [
                    "display_name",
                    "displayName",
                    "model_name",
                    "modelName",
                    "name",
                ]
                .into_iter()
                .find_map(|key| {
                    object
                        .get(key)
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                })
                .map(ToOwned::to_owned);
                let price_factor = factor(value, false);
                let original_price_factor = factor(value, true);
                let mut token_limits = crate::backend::catalog::token_limits(value);
                // AutoGLM 的官方目录使用 OpenClaw 模型字段 maxTokens 表示输出能力。
                // 仅在本适配器读取，不把其他目录的请求参数 max_tokens 当模型上限。
                token_limits.max_output_tokens = token_limits.max_output_tokens.or_else(|| {
                    value
                        .get("maxTokens")
                        .and_then(crate::backend::catalog::token_count)
                });
                if let Some(existing) = models.iter_mut().find(|model| model.id == id) {
                    existing.capabilities =
                        crate::backend::catalog::capabilities(value, &existing.capabilities);
                    existing.display_name = existing.display_name.take().or(display_name);
                    existing.price_factor = existing.price_factor.or(price_factor);
                    existing.original_price_factor =
                        existing.original_price_factor.or(original_price_factor);
                    existing.token_limits.context_window = existing
                        .token_limits
                        .context_window
                        .or(token_limits.context_window);
                    existing.token_limits.max_input_tokens = existing
                        .token_limits
                        .max_input_tokens
                        .or(token_limits.max_input_tokens);
                    existing.token_limits.max_output_tokens = existing
                        .token_limits
                        .max_output_tokens
                        .or(token_limits.max_output_tokens);
                    if existing.token_limits.context_tiers.is_empty() {
                        existing.token_limits.context_tiers = token_limits.context_tiers;
                    }
                } else {
                    models.push(ModelInfo {
                        id: id.into(),
                        display_name,
                        price_factor,
                        original_price_factor,
                        price_valid_until: None,
                        capabilities: crate::backend::catalog::capabilities(value, capabilities),
                        token_limits,
                    });
                }
                // 模型对象中的工具、计费等子对象不是独立的模型声明。
                return;
            }
            for (key, child) in object {
                if matches!(
                    key.as_str(),
                    "request"
                        | "requests"
                        | "promotions"
                        | "modelPromotions"
                        | "promotion"
                        | "discount"
                        | "pricing"
                        | "tools"
                        | "capabilities"
                ) {
                    continue;
                }
                visit(
                    child,
                    key == "models" || (key == "data" && child.is_array()),
                    capabilities,
                    models,
                );
            }
        }
        Value::Array(items) => {
            for item in items {
                visit(item, model_collection, capabilities, models);
            }
        }
        Value::String(id) if model_collection && !id.trim().is_empty() && id.len() < 128 => {
            if !models.iter().any(|model| model.id == *id) {
                models.push(ModelInfo {
                    id: id.clone(),
                    display_name: None,
                    price_factor: None,
                    original_price_factor: None,
                    price_valid_until: None,
                    capabilities: capabilities.clone(),
                    token_limits: Default::default(),
                });
            }
        }
        _ => {}
    }
}

/// 只读取含义明确的倍率字段或带 x 的 credits，不猜测 price / cost 的单位。
fn factor(model: &Value, original: bool) -> Option<f64> {
    // AutoClaw 2.5.3 的倍率为数值，范围为 0..=1000；顶层字段优先于 metadata。
    let official_key = if original {
        "creditCostFactorOriginal"
    } else {
        "creditCostFactor"
    };
    if let Some(value) = model
        .get(official_key)
        .filter(|value| !value.is_null())
        .or_else(|| model.get("metadata")?.get(official_key))
    {
        return value
            .as_f64()
            .filter(|factor| factor.is_finite() && (0.0..=1000.0).contains(factor));
    }
    let keys = if original {
        ["original_price_factor", "originalPriceFactor"]
    } else {
        ["price_factor", "priceFactor"]
    };
    let credit_keys: &[&str] = if original {
        &["original_credits", "originalCredits"]
    } else {
        &["credits"]
    };
    std::iter::once(model)
        .chain(model.get("pricing"))
        .find_map(|source| {
            keys.iter()
                .find_map(|key| source.get(*key).and_then(numeric_factor))
                .or_else(|| {
                    credit_keys
                        .iter()
                        .find_map(|key| source.get(*key).and_then(credit_multiplier))
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 模型与元数据分离；分组、请求、活动和嵌套工具不能变成模型。
    #[test]
    fn metadata_and_unrelated_ids_are_separated() {
        let value = json!({"id":"envelope","data":{"groups":[{"id":"group","models":[
            {"id":"zai_glm-5","displayName":"GLM-5","credits":"0.2x","tools":[{"id":"tool"}]},
            {"id":"disabled","enabled":false},
            {"model_id":"hidden","hidden":true}
        ]}],"request":{"model":"request-only"},"modelPromotions":[{"model_id":"promotion-only"}]}});
        let models = parse(&value, &Capabilities::default());
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "zai_glm-5");
        assert_eq!(models[0].display_name.as_deref(), Some("GLM-5"));
        assert_eq!(models[0].price_factor, Some(0.2));
    }

    /// 重复项补全元数据而不改动调用 ID，免费与未知倍率必须区分。
    #[test]
    fn duplicates_fill_metadata_without_guessing_currency() {
        let models = parse(
            &json!({"models":[
                "zai_auto", {"model_id":"zai_auto","name":"Auto","price_factor":0,"original_price_factor":"0.5"},
                {"model":"zai_glm-5","name":"GLM-5","credits":0.2,"price":0.2,"cost":"0.2"},
                {"id":"invalid","price_factor":"NaN","credits":"-0.2x"},
                {"model":"explicit","pricing":{"priceFactor":"0.08","originalCredits":"0.2x"}}
            ]}),
            &Capabilities::default(),
        );
        assert_eq!(models.len(), 4);
        assert_eq!(models[0].id, "zai_auto");
        assert_eq!(models[0].display_name.as_deref(), Some("Auto"));
        assert_eq!(models[0].price_factor, Some(0.0));
        assert_eq!(models[0].original_price_factor, Some(0.5));
        assert!(models[1].price_factor.is_none());
        assert!(models[2].price_factor.is_none());
        assert_eq!(models[3].price_factor, Some(0.08));
        assert_eq!(models[3].original_price_factor, Some(0.2));
    }

    /// 原生 Auto 继承已知传输能力，具体模型与重复项的显式能力优先生效。
    #[test]
    fn model_flags_override_defaults_and_duplicate_metadata() {
        let defaults = Capabilities {
            images: true,
            tools: true,
            ..Default::default()
        };
        let models = parse(
            &json!({"models":[
                "auto", "text", {"id":"text","capabilities":{"images":false,"tools":false}},
                {"id":"vision","modalities":{"input":["text","image"]}}
            ]}),
            &defaults,
        );
        assert!(models[0].capabilities.images);
        assert!(!models[1].capabilities.images && !models[1].capabilities.tools);
        assert!(models[2].capabilities.images);
    }

    /// 使用真实目录字段区分 Auto 的视觉输入与具体文本模型，并保留输出能力。
    #[test]
    fn official_input_and_max_tokens_are_model_metadata() {
        let models = parse(
            &json!({"models":[
                {"id":"zai_auto","input":["image","text"],"contextWindow":1048576,"maxTokens":393216,"reasoning":true},
                {"id":"zaicoding_glm-5.3","input":["text"],"contextWindow":1048576,"maxTokens":307200,"reasoning":true}
            ]}),
            &Capabilities {
                images: true,
                ..Default::default()
            },
        );
        assert!(models[0].capabilities.images && models[0].capabilities.reasoning);
        assert!(!models[1].capabilities.images);
        assert_eq!(models[0].token_limits.max_output_tokens, Some(393216));
        assert_eq!(models[1].token_limits.max_output_tokens, Some(307200));
    }

    /// 当前客户端的倍率字段支持免费值、metadata 回落和顶层优先，拒绝越界及字符串。
    #[test]
    fn managed_credit_factors_follow_official_numeric_contract() {
        let models = parse(
            &json!({"models":[
                {"id":"free","creditCostFactor":0,"metadata":{"creditCostFactor":9}},
                {"id":"discount","metadata":{"creditCostFactor":0.2,"creditCostFactorOriginal":0.5}},
                {"id":"invalid","creditCostFactor":1001,"metadata":{"creditCostFactor":0.2}},
                {"id":"string","creditCostFactor":"0.2"}
            ]}),
            &Capabilities::default(),
        );
        assert_eq!(models[0].price_factor, Some(0.0));
        assert_eq!(models[1].price_factor, Some(0.2));
        assert_eq!(models[1].original_price_factor, Some(0.5));
        assert_eq!(models[2].price_factor, None);
        assert_eq!(models[3].price_factor, None);
    }
}
