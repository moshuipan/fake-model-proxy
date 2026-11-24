//! MCP stdio 仅把结构化工具映射到管理 API；权限由服务器身份决定。

use super::client::{ApiClient, ClientError, outcome_code};
use crate::application::api_schema;
use reqwest::Method;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};

/// 动作工具不按厂商复制；schema 来自管理 API 实际输入类型。
pub fn tools() -> Vec<Value> {
    let schema = api_schema::bundle();
    vec![
        tool(
            "inspect",
            "读取插件、配置、schema、预设、权限或操作历史；不刷新上游",
            json!({"type":"object","additionalProperties":false,"properties":{
                "resource":{"type":"string","enum":["identity","plugins","plugin","config","schema","presets","operations"]},
                "id":{"type":"string","minLength":1}},"required":["resource"]}),
            true,
            false,
        ),
        tool(
            "validate_config",
            "校验声明变更，不调用上游",
            schema["change"].clone(),
            true,
            false,
        ),
        tool(
            "plan_config",
            "按读取的 revision 保存变更计划；返回影响摘要，不自动应用",
            schema["plan"].clone(),
            false,
            false,
        ),
        tool(
            "apply_config",
            "应用已审阅的计划，要求 config_write；使用固定幂等键",
            json!({"type":"object","additionalProperties":false,"properties":{
                "plan_id":{"type":"string","minLength":1},"idempotency_key":{"type":"string","minLength":1}},
                "required":["plan_id","idempotency_key"]}),
            false,
            false,
        ),
        tool(
            "rollback_config",
            "根据旧配置创建新计划；不回退用量、轮换凭证或远程副作用",
            schema["rollback"].clone(),
            false,
            false,
        ),
        tool(
            "run_operation",
            "启动已实现动作；按插件 effect 判断副作用，waiting_user 要交给用户，unknown 不重放",
            schema["operation"].clone(),
            false,
            true,
        ),
        tool(
            "get_operation",
            "读取已有操作状态，不重新执行",
            id_schema(),
            true,
            false,
        ),
        tool(
            "cancel_operation",
            "显式取消当前身份的操作，无法撤回已完成的上游副作用",
            id_schema(),
            false,
            true,
        ),
        tool(
            "explain_route",
            "解释缓存快照准入，不发生成请求、不消耗推理额度",
            schema["explain"].clone(),
            true,
            false,
        ),
    ]
}

/// 注解只作为客户端提示，执行权限仍在服务端检查。
fn tool(name: &str, description: &str, input: Value, read_only: bool, open_world: bool) -> Value {
    json!({"name":name,"description":description,"inputSchema":input,
        "annotations":{"readOnlyHint":read_only,"destructiveHint":!read_only,
            "idempotentHint":read_only,"openWorldHint":open_world}})
}

/// 查询和取消统一使用操作 ID，不接受内部授权会话句柄。
fn id_schema() -> Value {
    json!({"type":"object","additionalProperties":false,
        "properties":{"operation_id":{"type":"string","minLength":1}},"required":["operation_id"]})
}

/// 启动时只建立 HTTP 客户端，初始化和工具发现无需网络。
pub async fn serve_from_env() -> Result<(), ClientError> {
    let base = std::env::var("FMP_SERVER").unwrap_or_else(|_| "http://127.0.0.1:8317".into());
    let client = ApiClient::new(&base, std::env::var("FMP_ADMIN_KEY").ok())?;
    let mut input = BufReader::new(tokio::io::stdin());
    let mut output = tokio::io::stdout();
    while let Some(line) = read_message(&mut input).await? {
        let response = match serde_json::from_slice::<Value>(&line) {
            Ok(request) => dispatch(&client, request).await,
            Err(_) => Some(rpc_error(Value::Null, -32700, "Parse error")),
        };
        if let Some(response) = response {
            let mut data = serde_json::to_vec(&response).expect("RPC response is JSON");
            data.push(b'\n');
            output
                .write_all(&data)
                .await
                .map_err(|_| ClientError::new("stdio_error", "MCP 输出失败", 1))?;
            output
                .flush()
                .await
                .map_err(|_| ClientError::new("stdio_error", "MCP 输出失败", 1))?;
        }
    }
    Ok(())
}

/// 有界读取换行分隔的 JSON-RPC，EOF 前的最后一条也可以处理。
async fn read_message(
    input: &mut (impl AsyncBufRead + Unpin),
) -> Result<Option<Vec<u8>>, ClientError> {
    let mut message = Vec::new();
    loop {
        let chunk = input
            .fill_buf()
            .await
            .map_err(|_| ClientError::new("stdio_error", "MCP 输入失败", 1))?;
        if chunk.is_empty() {
            return Ok((!message.is_empty()).then_some(message));
        }
        let newline = chunk.iter().position(|b| *b == b'\n');
        let count = newline.map_or(chunk.len(), |p| p + 1);
        if message.len() + count > 1024 * 1024 {
            return Err(ClientError::new("input_too_large", "MCP 消息超过 1 MiB", 3));
        }
        message.extend_from_slice(&chunk[..count]);
        input.consume(count);
        if newline.is_some() {
            return Ok(Some(message));
        }
    }
}

/// JSON-RPC 协议处理与应用工具调用分开；通知从不占用响应流。
pub async fn dispatch(client: &ApiClient, request: Value) -> Option<Value> {
    if !request.is_object() || request["jsonrpc"] != "2.0" || !request["method"].is_string() {
        return Some(rpc_error(Value::Null, -32600, "Invalid Request"));
    }
    let id = request.get("id")?;
    if !(id.is_string() || id.is_number()) {
        return Some(rpc_error(Value::Null, -32600, "Invalid Request ID"));
    }
    let method = request["method"].as_str().expect("method is string");
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    let result = match method {
        "initialize" => {
            let version = params["protocolVersion"].as_str().unwrap_or("2025-06-18");
            let version = if ["2024-11-05", "2025-03-26", "2025-06-18"].contains(&version) {
                version
            } else {
                "2025-06-18"
            };
            json!({"protocolVersion":version,"capabilities":{"tools":{"listChanged":false}},
                "serverInfo":{"name":"fmp-management","version":env!("CARGO_PKG_VERSION")},
                "instructions":"读取真实插件 schema 后生成配置。plan/apply 使用 revision 和幂等键。凭证从 CLI stdin 写入，仅引用进入配置。waiting_user 交给用户；unknown 不自动重放。权限和动作 effect 由在线服务检查。"})
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools":tools()}),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or("");
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let Some(tool) = tools().into_iter().find(|tool| tool["name"] == name) else {
                return Some(rpc_error(id.clone(), -32602, "Unknown tool"));
            };
            let valid = jsonschema::validator_for(&tool["inputSchema"])
                .map(|v| v.is_valid(&arguments))
                .unwrap_or(false);
            if !valid {
                return Some(rpc_error(
                    id.clone(),
                    -32602,
                    "Tool arguments do not match schema",
                ));
            }
            match call(client, name, arguments).await {
                Ok(value) => {
                    json!({"content":[{"type":"text","text":serde_json::to_string(&value).expect("tool result is JSON")}],
                    "structuredContent":value,"isError":matches!(outcome_code(&value),3|7)})
                }
                Err(error) => json!({"content":[{"type":"text","text":error.value().to_string()}],
                    "structuredContent":error.value(),"isError":true}),
            }
        }
        _ => return Some(rpc_error(id.clone(), -32601, "Method not found")),
    };
    Some(json!({"jsonrpc":"2.0","id":id,"result":result}))
}

/// 仅映射有限用例，不添加本地命令、任意 URL 或数据库入口。
async fn call(client: &ApiClient, name: &str, args: Value) -> Result<Value, ClientError> {
    let id = |key| args[key].as_str().unwrap_or("");
    let (method, path, body) = match name {
        "inspect" => {
            let resource = id("resource");
            let path = match resource {
                "plugin" if !id("id").is_empty() => vec!["plugins", id("id")],
                "plugin" => return Err(ClientError::new("usage", "读取单个插件需要 id", 2)),
                "identity" | "plugins" | "config" | "schema" | "presets" | "operations" => {
                    if args.get("id").is_some() {
                        return Err(ClientError::new("usage", "仅 plugin 资源接受 id", 2));
                    }
                    vec![resource]
                }
                _ => return Err(ClientError::new("usage", "未知资源", 2)),
            };
            (Method::GET, path, None)
        }
        "validate_config" => (Method::POST, vec!["config", "validate"], Some(args.clone())),
        "plan_config" => (Method::POST, vec!["config", "plans"], Some(args.clone())),
        "apply_config" => (
            Method::POST,
            vec!["config", "plans", id("plan_id"), "apply"],
            Some(json!({"idempotency_key":id("idempotency_key")})),
        ),
        "rollback_config" => (Method::POST, vec!["config", "rollback"], Some(args.clone())),
        "run_operation" => (Method::POST, vec!["operations"], Some(args.clone())),
        "get_operation" => (Method::GET, vec!["operations", id("operation_id")], None),
        "cancel_operation" => (
            Method::POST,
            vec!["operations", id("operation_id"), "cancel"],
            None,
        ),
        "explain_route" => (Method::POST, vec!["routes", "explain"], Some(args.clone())),
        _ => return Err(ClientError::new("usage", "未知工具", 2)),
    };
    client.request(method, &path, body).await
}

/// 协议错误消息固定，避免把非法参数和秘密写回会话。
fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}
