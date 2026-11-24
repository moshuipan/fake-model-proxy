//! 模型目录共用的元数据读取；只接受上游含义明确的字段。

use crate::core::{Capabilities, TokenLimits};
use serde_json::Value;

/// 映射上游明确声明的模型能力；模型级布尔值优先于模态、标签和后端默认值。
pub(crate) fn capabilities(model: &Value, defaults: &Capabilities) -> Capabilities {
    let mut result = defaults.clone();
    let boolean = |keys: &[&str]| {
        std::iter::once(model)
            .chain(model.get("capabilities"))
            .find_map(|source| {
                keys.iter()
                    .find_map(|key| source.get(*key).and_then(Value::as_bool))
            })
    };
    let tag = |names: &[&str]| {
        model
            .get("tags")
            .and_then(Value::as_array)
            .and_then(|tags| {
                tags.iter()
                    .filter_map(Value::as_str)
                    .any(|tag| names.contains(&tag))
                    .then_some(true)
            })
    };
    let input = model
        .pointer("/architecture/input_modalities")
        .or_else(|| model.pointer("/modalities/input"))
        .or_else(|| model.get("input_modalities"))
        .or_else(|| model.get("input"))
        .and_then(Value::as_array);
    let parameters = model.get("supported_parameters").and_then(Value::as_array);
    result.images = boolean(&[
        "images",
        "supportsImages",
        "supports_images",
        "is_vl",
        "support_image",
        "vision",
    ])
    .or_else(|| input.map(|items| items.iter().any(|item| item == "image")))
    .or_else(|| {
        tasks(model)
            .any(|task| {
                matches!(
                    task,
                    "image-text-to-text" | "visual-question-answering" | "image-captioning"
                )
            })
            .then_some(true)
    })
    .or_else(|| tag(&["vision", "image", "image-understanding"]))
    .unwrap_or(result.images);
    result.tools = boolean(&[
        "tools",
        "supportsToolCall",
        "supports_tool_call",
        "tool_call",
    ])
    .or_else(|| parameters.map(|items| items.iter().any(|item| item == "tools")))
    .unwrap_or(result.tools);
    result.parallel_tools = boolean(&["parallel_tools", "supportsParallelToolCall"])
        .or_else(|| parameters.map(|items| items.iter().any(|item| item == "parallel_tool_calls")))
        .unwrap_or(result.parallel_tools)
        && result.tools;
    result.reasoning = boolean(&[
        "reasoning",
        "supportsReasoning",
        "supports_reasoning",
        "is_reasoning",
        "thinking_enabled",
    ])
    .or_else(|| {
        parameters.map(|items| {
            items
                .iter()
                .any(|item| item == "reasoning" || item == "reasoning_effort")
        })
    })
    .or_else(|| tag(&["reasoning", "thinking"]))
    .unwrap_or(result.reasoning);
    result.context_window = token_limits(model).context_window.or(result.context_window);
    result
}

/// 读取官方任务 ID，显示名和模型名称不能作为能力依据。
fn tasks(model: &Value) -> impl Iterator<Item = &str> {
    model
        .get("Tasks")
        .or_else(|| model.get("tasks"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|task| {
            task.as_str()
                .or_else(|| task.get("Name").and_then(Value::as_str))
        })
}

/// 目录声明纯图像、视频等生成任务时，不发布到聊天接口；未知任务仍保留。
pub(crate) fn chat_model(model: &Value) -> bool {
    let mut non_chat = false;
    for task in tasks(model) {
        match task {
            "text-generation"
            | "text2text-generation"
            | "chat"
            | "image-text-to-text"
            | "visual-question-answering"
            | "image-captioning" => return true,
            "text-to-image"
            | "image-to-image"
            | "image-to-video"
            | "text-to-video"
            | "text-to-speech"
            | "automatic-speech-recognition"
            | "sentence-embedding"
            | "text-embedding" => non_chat = true,
            _ => {}
        }
    }
    !non_chat
}

/// 接受正整数字段或数字字符串，拒绝零、负数、小数及 u32 溢出。
pub(crate) fn token_count(value: &Value) -> Option<u32> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
        .and_then(|tokens| u32::try_from(tokens).ok())
        .filter(|tokens| *tokens > 0)
}

/// 只读取明确的窗口、输入和输出字段；不把 max_tokens 猜成模型输出能力。
pub(crate) fn token_limits(model: &Value) -> TokenLimits {
    let read = |keys: &[&str]| {
        std::iter::once(model)
            .chain(model.get("capabilities"))
            .find_map(|source| {
                keys.iter()
                    .find_map(|key| source.get(*key).and_then(token_count))
            })
    };
    TokenLimits {
        context_window: read(&[
            "context_window",
            "contextWindow",
            "context_length",
            "contextLength",
        ]),
        max_input_tokens: read(&["max_input_tokens", "maxInputTokens"]),
        max_output_tokens: read(&["max_output_tokens", "maxOutputTokens"]),
        context_tiers: Vec::new(),
    }
}

/// 接受非负有限数值或数字字符串，用于上游明确标为倍率的字段。
pub(crate) fn numeric_factor(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        .filter(|value| value.is_finite() && *value >= 0.0)
}

/// credits 只有明确带 x / × 才视为倍率，裸数字可能是固定积分金额。
pub(crate) fn credit_multiplier(value: &Value) -> Option<f64> {
    let text = value.as_str()?.trim().to_ascii_lowercase();
    let text = text
        .strip_suffix("credits")
        .or_else(|| text.strip_suffix("credit"))
        .unwrap_or(&text)
        .trim();
    let number = text
        .strip_prefix(['x', '×'])
        .or_else(|| text.strip_suffix(['x', '×']))?
        .trim();
    if number.is_empty() || !number.chars().all(|ch| ch.is_ascii_digit() || ch == '.') {
        return None;
    }
    number
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value >= 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 非整数字段或超大值必须保持未知，不能截断为较小窗口。
    #[test]
    fn token_counts_reject_invalid_or_overflowing_values() {
        for raw in [
            json!(0),
            json!(-1),
            json!(1.5),
            json!("1.5"),
            json!(4294967296_u64),
            json!("4294967296"),
            Value::Null,
            json!("200K"),
        ] {
            assert_eq!(token_count(&raw), None, "{raw}");
        }
        assert_eq!(token_count(&json!(" 200000 ")), Some(200000));
        assert_eq!(token_count(&json!(4294967295_u64)), Some(u32::MAX));
    }

    /// 目录中的输出限制独立于请求参数，输入值也不能补成总窗口。
    #[test]
    fn explicit_limits_keep_input_output_and_context_distinct() {
        let limits = token_limits(&json!({
            "maxInputTokens":"200000", "maxOutputTokens":8192, "max_tokens":4096
        }));
        assert_eq!(limits.context_window, None);
        assert_eq!(limits.max_input_tokens, Some(200000));
        assert_eq!(limits.max_output_tokens, Some(8192));
        assert_eq!(
            token_limits(&json!({"max_tokens":4096})),
            TokenLimits::default()
        );
    }

    /// 后端不同字段必须映射为同一能力，显式 false 不能被标签或默认值覆盖。
    #[test]
    fn model_capabilities_follow_explicit_flags_modalities_and_tasks() {
        for metadata in [
            json!({"supportsImages":true,"supportsToolCall":true,"supportsReasoning":true}),
            json!({"is_vl":true,"tools":true,"is_reasoning":true}),
            json!({"support_image":true,"tool_call":true,"thinking_enabled":true}),
            json!({"vision":true,"tool_call":true,"reasoning":true}),
            json!({"modalities":{"input":["text","image"]},"tool_call":true,"reasoning":true}),
            json!({"architecture":{"input_modalities":["text","image"]},"supported_parameters":["tools","reasoning"],"reasoning":true}),
            json!({"capabilities":{"images":true,"tools":true,"reasoning":true}}),
            json!({"input":["text","image"],"tools":true,"reasoning":true}),
        ] {
            let capabilities = capabilities(&metadata, &Capabilities::default());
            assert!(
                capabilities.images && capabilities.tools && capabilities.reasoning,
                "{metadata}"
            );
        }
        assert!(
            capabilities(
                &json!({"Tasks":[{"Name":"image-text-to-text"}]}),
                &Capabilities::default()
            )
            .images
        );
        let defaults = Capabilities {
            images: true,
            tools: true,
            parallel_tools: true,
            reasoning: true,
            ..Default::default()
        };
        let mapped = capabilities(
            &json!({
                "images":false,"tools":false,"reasoning":false,
                "tags":["vision","reasoning"], "modalities":{"input":["image"]}
            }),
            &defaults,
        );
        assert!(!mapped.images && !mapped.tools && !mapped.parallel_tools && !mapped.reasoning);
        assert!(!capabilities(&json!({"input_modalities":["text"]}), &defaults).images);
        assert!(!capabilities(&json!({"id":"qwen-vision-image"}), &Capabilities::default()).images);
        assert!(!chat_model(&json!({"Tasks":[{"Name":"image-to-image"}]})));
        assert!(chat_model(
            &json!({"Tasks":[{"Name":"image-to-image"},{"Name":"image-text-to-text"}]})
        ));
    }
}
