//! 管理传输共享的请求类型和生成契约；HTTP、CLI、MCP 不各自定义配置字段。

use super::{
    config_types::{self, ConfigChange, ConfigDocument, ServiceSpec},
    management::{ExplainInput, PlanInput},
    operation_service::OperationRequest,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplyInput {
    pub idempotency_key: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RollbackInput {
    pub expected_revision: u64,
    pub target_revision: u64,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SecretInput {
    pub value: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PresetInput {
    pub presets: Vec<crate::presets::ProviderPreset>,
}

/// 直接从执行器实际使用的 Rust 请求类型生成独立 schema。
pub fn bundle() -> Value {
    json!({
        "api_version":"fmp/v2",
        "configuration":config_types::schema::<ConfigDocument>(),
        "change":config_types::schema::<ConfigChange>(),
        "service":config_types::schema::<ServiceSpec>(),
        "plan":config_types::schema::<PlanInput>(),
        "apply":config_types::schema::<ApplyInput>(),
        "rollback":config_types::schema::<RollbackInput>(),
        "secret":config_types::schema::<SecretInput>(),
        "preset_import":config_types::schema::<PresetInput>(),
        "operation":config_types::schema::<OperationRequest>(),
        "explain":config_types::schema::<ExplainInput>(),
        "preset":config_types::schema::<crate::presets::ProviderPreset>()
    })
}

/// 导出 OpenAPI 3.1；动态插件 options 由插件目录中的 schema 补充。
pub fn openapi() -> Value {
    let mut schemas = bundle();
    schemas
        .as_object_mut()
        .expect("schema bundle is object")
        .remove("api_version");
    for (name, schema) in schemas.as_object_mut().expect("schema bundle is object") {
        rebase_refs(schema, &format!("#/components/schemas/{name}/"));
    }
    let mut paths = serde_json::Map::new();
    for (path, method, scope, body) in [
        ("/api/v2/identity", "get", "", None),
        ("/api/v2/plugins", "get", "read", None),
        ("/api/v2/plugins/{id}", "get", "read", None),
        ("/api/v2/schema", "get", "read", None),
        ("/api/v2/openapi.json", "get", "read", None),
        ("/api/v2/presets", "get", "read", None),
        (
            "/api/v2/presets",
            "post",
            "config_write",
            Some("preset_import"),
        ),
        ("/api/v2/config", "get", "read", None),
        ("/api/v2/config/validate", "post", "read", Some("change")),
        ("/api/v2/config/plans", "post", "read", Some("plan")),
        ("/api/v2/config/plans/{id}", "get", "read", None),
        (
            "/api/v2/config/plans/{id}/apply",
            "post",
            "config_write",
            Some("apply"),
        ),
        ("/api/v2/config/rollback", "post", "read", Some("rollback")),
        ("/api/v2/secrets", "post", "secret_write", Some("secret")),
        ("/api/v2/operations", "get", "read", None),
        ("/api/v2/operations", "post", "operation", Some("operation")),
        ("/api/v2/operations/{id}", "get", "read", None),
        ("/api/v2/operations/{id}/cancel", "post", "operation", None),
        ("/api/v2/routes/explain", "post", "read", Some("explain")),
    ] {
        let mut operation = json!({
            "operationId":format!("{}_{}", method, path.trim_start_matches('/').replace(['/', '{', '}'], "_")),
            "security":[{"adminKey":[]}],
            "x-fmp-scope":scope,
            "responses":{
                "200":{"description":"结构化结果；配置只包含凭证引用",
                    "content":{"application/json":{"schema":{"type":"object"}}}},
                "400":{"$ref":"#/components/responses/Error"},
                "401":{"$ref":"#/components/responses/Error"},
                "403":{"$ref":"#/components/responses/Error"},
                "409":{"$ref":"#/components/responses/Error"}
            }
        });
        if path.contains("{id}") {
            operation["parameters"] = json!([{"name":"id","in":"path","required":true,"schema":{"type":"string","minLength":1}}]);
        }
        if let Some(name) = body {
            operation["requestBody"] = json!({"required":true,"content":{"application/json":{"schema":{"$ref":format!("#/components/schemas/{name}")}}}});
        }
        paths.entry(path.to_owned()).or_insert_with(|| json!({}))[method] = operation;
    }
    json!({
        "openapi":"3.1.0","info":{"title":"FMP Management API","version":"2"},
        "paths":paths,
        "components":{
            "schemas":schemas,
            "securitySchemes":{"adminKey":{"type":"apiKey","in":"header","name":"x-admin-key"}},
            "responses":{"Error":{
                "description":"稳定错误码，不回显请求中的秘密",
                "content":{"application/json":{"schema":{
                    "type":"object",
                    "properties":{"error":{
                        "type":"object",
                        "properties":{"code":{"type":"string"},"message":{"type":"string"}}
                    }}
                }}}
            }}
        }
    })
}

/// 独立 JSON Schema 的根引用转为 OpenAPI 组件引用，保留嵌套定义地址。
fn rebase_refs(value: &mut Value, prefix: &str) {
    match value {
        Value::Object(object) => {
            if let Some(reference) = object.get_mut("$ref")
                && let Some(suffix) = reference.as_str().and_then(|s| s.strip_prefix("#/"))
            {
                *reference = json!(format!("{prefix}{suffix}"));
            }
            for child in object.values_mut() {
                rebase_refs(child, prefix);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| rebase_refs(v, prefix)),
        _ => {}
    }
}
