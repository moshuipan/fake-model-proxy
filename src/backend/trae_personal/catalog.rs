//! 从个人账号动态目录建立原生路由，不发布隐藏配置和自定义占位模型。

use crate::core::{AppError, Capabilities, ModelInfo, TokenLimits};
use serde_json::Value;

/// 原生 config_name 与公开模型元数据保持同一快照。
#[derive(Clone)]
pub struct Model {
    pub info: ModelInfo,
    pub config: String,
    pub source: u64,
}

/// 同时兼容目录中 JSON 对象和 JSON 字符串字段。
fn object(value: &Value) -> Value {
    match value {
        Value::String(raw) => serde_json::from_str(raw).unwrap_or(Value::Null),
        value => value.clone(),
    }
}

/// 仅发布启用、可见、属于聊天完成的实际模型。
pub fn parse(value: &Value) -> Result<Vec<Model>, AppError> {
    let configs = value["config_info_list"]
        .as_array()
        .ok_or_else(|| super::error(None, "TRAE 个人模型目录格式已变化", false))?;
    let mut models = Vec::<Model>::new();
    for config in configs {
        let Some(name) = config["config_name"].as_str() else {
            continue;
        };
        if config["usage"] != "chat_completion"
            || config["config_switch"] != true
            || config["is_invisible_to_user"] == true
            || config["display_config"]["is_custom_model"] == true
            || name.starts_with("custom_model_")
        {
            continue;
        }
        let extra = object(&config["extra_config"]);
        for detail in config["model_detail_list"].as_array().into_iter().flatten() {
            let Some(id) = detail["model_name"].as_str().filter(|id| !id.is_empty()) else {
                continue;
            };
            if models.iter().any(|model| model.info.id == id) {
                continue;
            }
            let input = crate::backend::catalog::token_count(&detail["prompt_max_tokens"]);
            let output = crate::backend::catalog::token_count(&detail["max_tokens"]);
            let context = input.zip(output).and_then(|(a, b)| a.checked_add(b));
            // native_function_call 是目录声明；思考输出为个人 raw-chat 已验证的通道。
            let capabilities = Capabilities {
                tools: extra["native_function_call"] == true,
                parallel_tools: extra["parallel_tool_calling"] == true,
                images: false,
                reasoning: true,
                context_window: context,
            };
            models.push(Model {
                info: ModelInfo {
                    id: id.into(),
                    display_name: Some(name.into()),
                    price_factor: None,
                    original_price_factor: None,
                    price_valid_until: None,
                    capabilities,
                    token_limits: TokenLimits {
                        context_window: context,
                        max_input_tokens: input,
                        max_output_tokens: output,
                        context_tiers: Vec::new(),
                    },
                },
                config: name.into(),
                source: config["config_source"].as_u64().unwrap_or(1),
            });
        }
    }
    if models.is_empty() {
        return Err(super::error(None, "TRAE 个人账号没有可用聊天模型", false));
    }
    Ok(models)
}
