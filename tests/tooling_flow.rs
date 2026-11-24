//! CLI/MCP 使用真实 HTTP 和 SQLite 验证同一控制平面，不接触真实上游。

use fakemodelproxy::{
    application::{
        access::{AccessKey, Scope},
        api_schema,
    },
    backend::Registry,
    config::Config,
    records::RecordLog,
    routing::Router,
    state::AppState,
    store::Store,
    tasks::TaskRegistry,
    tooling::{
        client::{ApiClient, outcome_code},
        plugin_dev,
    },
    usage::UsageLedger,
};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, RwLock},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// 服务不启动远程后台刷新，每项测试拥有独立存储和临时监听端口。
async fn server() -> (String, tokio::task::JoinHandle<()>) {
    let store = Store::open(Path::new(":memory:")).await.unwrap();
    let config = Config {
        admin_key: "tooling-administrator".into(),
        access_keys: vec![AccessKey {
            id: "reader".into(),
            key: "tooling-reader".into(),
            scopes: [Scope::Read].into(),
        }],
        ..Default::default()
    };
    let state = AppState::new(
        Arc::new(config),
        store.clone(),
        Arc::new(Registry::new()),
        Arc::new(RecordLog::new(20)),
        Arc::new(RwLock::new(TaskRegistry::new())),
        Arc::new(Router::new()),
        Arc::new(UsageLedger::new(store)),
    );
    state.reload_backends().await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        axum::serve(listener, fakemodelproxy::build_router(state))
            .await
            .unwrap();
    });
    (address, handle)
}

/// 从真实 CLI 子进程读取单个 JSON，确认 stdout 没有日志。
async fn cli(server: &str, key: &str, args: &[&str]) -> (i32, Value) {
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_fmp"))
        .env("FMP_SERVER", server)
        .env("FMP_ADMIN_KEY", key)
        .args(args)
        .output()
        .await
        .unwrap();
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    (
        output.status.code().unwrap(),
        serde_json::from_slice(&output.stdout).unwrap(),
    )
}

/// CLI 跨进程 plan/apply 使用持久计划，权限和 revision 均来自真实服务。
#[tokio::test]
async fn cli_plan_apply_conflict_permissions_and_strict_options() {
    let (server, handle) = server().await;
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("plan.json");
    std::fs::write(
        &input,
        json!({"expected_revision":0,"change":{}}).to_string(),
    )
    .unwrap();
    let (code, plan) = cli(
        &server,
        "tooling-reader",
        &["config", "plan", "--file", input.to_str().unwrap()],
    )
    .await;
    assert_eq!(code, 0, "{plan}");
    let id = plan["id"].as_str().unwrap();
    let args = [
        "config",
        "apply",
        "--plan-id",
        id,
        "--idempotency-key",
        "same-request",
    ];
    let (code, denied) = cli(&server, "tooling-reader", &args).await;
    assert_eq!(code, 6);
    assert_eq!(denied["error"]["code"], "forbidden");
    for _ in 0..2 {
        let (code, applied) = cli(&server, "tooling-administrator", &args).await;
        assert_eq!(code, 0, "{applied}");
        assert_eq!(applied["active_revision"], 1);
    }
    let (code, conflict) = cli(
        &server,
        "tooling-administrator",
        &["config", "plan", "--file", input.to_str().unwrap()],
    )
    .await;
    assert_eq!(code, 4);
    assert_eq!(conflict["error"]["code"], "revision_conflict");
    assert_eq!(
        cli(
            &server,
            "tooling-reader",
            &["plugins", "list", "--typo", "value"]
        )
        .await
        .0,
        2
    );
    let invalid = root.path().join("invalid.json");
    std::fs::write(&invalid, r#"{"services":[{"password":"never-echo-this"}]}"#).unwrap();
    let (code, error) = cli(
        &server,
        "tooling-reader",
        &["config", "validate", "--file", invalid.to_str().unwrap()],
    )
    .await;
    assert_eq!(code, 3);
    assert!(!error.to_string().contains("never-echo-this"));
    handle.abort();
}

/// 秘密经 stdin 送入真实接口，stdout 只返回引用，普通配置不包含原文。
#[tokio::test]
async fn cli_stdin_secret_never_appears_in_output() {
    let (server, handle) = server().await;
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_fmp"))
        .env("FMP_SERVER", &server)
        .env("FMP_ADMIN_KEY", "tooling-administrator")
        .args(["secrets", "write", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"private-stdin-fixture\n")
        .await
        .unwrap();
    let output = child.wait_with_output().await.unwrap();
    assert!(output.status.success());
    let secret: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(secret["secret_ref"].as_str().unwrap().starts_with("store:"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private-stdin-fixture"));
    assert!(output.stderr.is_empty());
    let (code, config) = cli(&server, "tooling-reader", &["config", "get"]).await;
    assert_eq!(code, 0);
    assert!(!config.to_string().contains("private-stdin-fixture"));
    handle.abort();
}

/// stdio 初始化、工具发现和权限拒绝不产生额外输出；MCP 共享 CLI 的在线身份。
#[tokio::test]
async fn mcp_stdio_protocol_and_permission_boundary() {
    let (server, handle) = server().await;
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_fmp-mcp"))
        .env("FMP_SERVER", &server)
        .env("FMP_ADMIN_KEY", "tooling-reader")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap()).lines();
    for (id, method, params) in [
        (
            1,
            "initialize",
            json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}),
        ),
        (2, "tools/list", json!({})),
        (
            3,
            "tools/call",
            json!({"name":"inspect","arguments":{"resource":"plugins"}}),
        ),
        (
            4,
            "tools/call",
            json!({"name":"apply_config","arguments":{"plan_id":"any","idempotency_key":"denied"}}),
        ),
        (
            5,
            "tools/call",
            json!({"name":"explain_route","arguments":{"input_tokens":1,"unknown":"never-echo"}}),
        ),
    ] {
        stdin
            .write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let value: Value =
            serde_json::from_str(&stdout.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(value["id"], id);
        match id {
            1 => assert_eq!(value["result"]["protocolVersion"], "2025-06-18"),
            2 => assert!(
                value["result"]["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|t| t["name"] != "shell")
            ),
            3 => assert_eq!(
                value["result"]["structuredContent"]["plugins"]
                    .as_array()
                    .unwrap()
                    .len(),
                9
            ),
            4 => {
                assert_eq!(value["result"]["isError"], true);
                assert_eq!(
                    value["result"]["structuredContent"]["error"]["code"],
                    "forbidden"
                );
            }
            5 => {
                assert_eq!(value["error"]["code"], -32602);
                assert!(!value.to_string().contains("never-echo"));
            }
            _ => unreachable!(),
        }
    }
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();
    stdin.write_all(b"invalid-json\n").await.unwrap();
    let value: Value = serde_json::from_str(&stdout.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(value["error"]["code"], -32700);
    drop(stdin);
    let output = child.wait_with_output().await.unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert!(stdout.next_line().await.unwrap().is_none());
    handle.abort();
}

/// OpenAPI 中所有局部引用都能解析到实际类型，避免独立 schema 嵌入后的根漂移。
#[test]
fn generated_openapi_resolves_every_local_reference() {
    let document = api_schema::openapi();
    check_refs(&document, &document);
    assert!(document["paths"]["/api/v2/config/plans"]["post"]["requestBody"].is_object());
}

/// 递归检查生成契约的所有引用。
fn check_refs(value: &Value, document: &Value) {
    match value {
        Value::Object(map) => {
            if let Some(reference) = map
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|s| s.strip_prefix('#'))
            {
                assert!(document.pointer(reference).is_some(), "{reference}");
            }
            for child in map.values() {
                check_refs(child, document);
            }
        }
        Value::Array(values) => values.iter().for_each(|v| check_refs(v, document)),
        _ => {}
    }
}

/// 本地脚手架写入真实模块与唯一注册项，重复或非法目标不改已有文件。
#[test]
fn scaffold_preserves_existing_registration_on_collision() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("src/plugins")).unwrap();
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname=\"fixture\"\n",
    )
    .unwrap();
    std::fs::write(root.path().join("src/plugins/mod.rs"), "pub mod builtin;\n").unwrap();
    std::fs::write(
        root.path().join("src/plugins/builtin.rs"),
        "let plugins: Vec<Arc<dyn ProviderPlugin>> = vec![\n];",
    )
    .unwrap();
    let result = plugin_dev::scaffold(root.path(), "fixture_plugin").unwrap();
    assert_eq!(result["verified"], false);
    let modules = std::fs::read_to_string(root.path().join("src/plugins/mod.rs")).unwrap();
    assert!(modules.contains("mod fixture_plugin;"));
    assert!(
        std::fs::read_to_string(root.path().join("src/plugins/fixture_plugin.rs"))
            .unwrap()
            .contains("OpenAiCompatBackend::new_with_timeout")
    );
    assert!(plugin_dev::scaffold(root.path(), "fixture_plugin").is_err());
    assert!(plugin_dev::scaffold(root.path(), "../bad").is_err());
    assert_eq!(
        modules,
        std::fs::read_to_string(root.path().join("src/plugins/mod.rs")).unwrap()
    );
    assert!(ApiClient::new("https://user:secret@example.test", None).is_err());
    assert_eq!(outcome_code(&json!({"status":"waiting_user"})), 5);
    assert_eq!(outcome_code(&json!({"status":"unknown"})), 7);
}
