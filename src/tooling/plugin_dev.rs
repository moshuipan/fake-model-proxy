//! 本地编译期插件开发工具；prepare/契约检查不发送远程请求。

use super::client::ClientError;
use crate::{
    plugins::{ScopedHostServices, builtin},
    store::{BackendRecord, Store},
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

/// 在仓库容器和 Cargo 项目根目录两种入口中定位当前项目。
pub fn default_project() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if cwd.join("Cargo.toml").is_file() {
        cwd
    } else {
        cwd.join("fakemodelproxy")
    }
}

/// ID 必须能直接成为 Rust 模块，拒绝路径、关键字和已存在目标。
fn valid_id(id: &str) -> bool {
    (2..=64).contains(&id.len())
        && id.as_bytes()[0].is_ascii_lowercase()
        && id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
        && ![
            "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
            "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod",
            "move", "mut", "pub", "ref", "return", "self", "static", "struct", "super", "trait",
            "true", "type", "unsafe", "use", "where", "while", "abstract", "become", "box", "do",
            "final", "macro", "override", "priv", "try", "typeof", "unsized", "virtual", "yield",
            "gen",
        ]
        .contains(&id)
}

/// 确认装配布局来自 FMP，避免在错误项目写文件。
fn project_files(project: &Path) -> Result<(String, String), ClientError> {
    if !project.join("Cargo.toml").is_file() {
        return Err(local("项目缺少 Cargo.toml"));
    }
    let modules = std::fs::read_to_string(project.join("src/plugins/mod.rs"))
        .map_err(|_| local("缺少插件模块声明"))?;
    let builtin = std::fs::read_to_string(project.join("src/plugins/builtin.rs"))
        .map_err(|_| local("缺少内置注册清单"))?;
    if !modules.contains("pub mod builtin;")
        || !builtin.contains("let plugins: Vec<Arc<dyn ProviderPlugin>> = vec![")
    {
        return Err(local("插件装配格式不匹配；请手工接入模块和注册项"));
    }
    Ok((modules, builtin))
}

/// 生成真实兼容传输模块、可校验示例和两个可见注册项；失败恢复装配文件。
pub fn scaffold(project: &Path, id: &str) -> Result<Value, ClientError> {
    if !valid_id(id) {
        return Err(ClientError::new(
            "plugin_id_invalid",
            "插件 ID 必须是 2–64 位小写 Rust 模块名",
            2,
        ));
    }
    let (modules, registrations) = project_files(project)?;
    let source = project.join(format!("src/plugins/{id}.rs"));
    let sample = project.join(format!("src/plugins/{id}.sample.json"));
    if source.exists()
        || project.join(format!("src/plugins/{id}")).exists()
        || sample.exists()
        || modules.contains(&format!("mod {id};"))
        || registrations.contains(&format!("super::{id}::PLUGIN"))
    {
        return Err(ClientError::new(
            "plugin_exists",
            "插件目标已存在，不覆盖",
            3,
        ));
    }
    let code = include_str!("plugin_template.txt").replace("__PLUGIN_ID__", id);
    let fixture = json!({"base_url":"http://127.0.0.1:9/v1","models":["fixture-model"],"options":{},"capabilities":{}});
    let modules_next = format!("{modules}\nmod {id};\n");
    let registrations_next = registrations.replacen(
        "let plugins: Vec<Arc<dyn ProviderPlugin>> = vec![",
        &format!("let plugins: Vec<Arc<dyn ProviderPlugin>> = vec![\n            Arc::new(super::{id}::PLUGIN),"), 1);
    let result = (|| {
        std::fs::write(&source, code)?;
        std::fs::write(
            &sample,
            serde_json::to_vec_pretty(&fixture).expect("fixture is JSON"),
        )?;
        std::fs::write(project.join("src/plugins/mod.rs"), modules_next)?;
        std::fs::write(project.join("src/plugins/builtin.rs"), registrations_next)?;
        Ok::<_, std::io::Error>(())
    })();
    if result.is_err() {
        let _ = std::fs::write(project.join("src/plugins/mod.rs"), modules);
        let _ = std::fs::write(project.join("src/plugins/builtin.rs"), registrations);
        let _ = std::fs::remove_file(&source);
        let _ = std::fs::remove_file(&sample);
        return Err(local("脚手架写入失败；已尝试恢复装配文件"));
    }
    Ok(
        json!({"plugin_id":id,"source":source,"sample":sample,"registered":true,
        "implemented_protocol":"openai_compat","verified":false,
        "next":"运行 fmp plugin verify ID；新协议能力需要实际实现和协议测试"}),
    )
}

/// 编译项目中的最新注册项，再运行新二进制中的契约检查；完整输出保存为日志。
pub async fn verify(project: &Path, id: &str) -> Result<Value, ClientError> {
    if !valid_id(id) {
        return Err(ClientError::new("plugin_id_invalid", "插件 ID 无效", 2));
    }
    project_files(project)?;
    let project = project
        .canonicalize()
        .map_err(|_| local("项目路径不可读取"))?;
    let output = tokio::process::Command::new("cargo")
        .current_dir(&project)
        .args(["build", "--bin", "fmp", "--message-format=json"])
        .output()
        .await
        .map_err(|_| local("无法运行 cargo build"))?;
    std::fs::create_dir_all(project.join("target")).map_err(|_| local("无法创建验证日志目录"))?;
    let log_path = project.join("target/fmp-plugin-verify.log");
    let mut log = output.stdout.clone();
    log.extend_from_slice(&output.stderr);
    std::fs::write(&log_path, log).map_err(|_| local("无法保存完整编译日志"))?;
    if !output.status.success() {
        return Ok(json!({"valid":false,"plugin_id":id,"code":"build_failed","log":log_path}));
    }
    let executable = output
        .stdout
        .split(|b| *b == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .find(|v| {
            v["reason"] == "compiler-artifact"
                && v["target"]["name"] == "fmp"
                && v["executable"].is_string()
        })
        .and_then(|v| v["executable"].as_str().map(str::to_owned))
        .ok_or_else(|| local("cargo 未返回 fmp 二进制路径"))?;
    let output = tokio::process::Command::new(executable)
        .current_dir(&project)
        .args(["plugin", "verify", id, "--compiled", "--project"])
        .arg(&project)
        .output()
        .await
        .map_err(|_| local("无法启动编译后的验证器"))?;
    let mut value: Value =
        serde_json::from_slice(&output.stdout).map_err(|_| local("契约验证器未返回 JSON"))?;
    value["log"] = json!(log_path);
    if !output.status.success() {
        value["valid"] = json!(false);
    }
    Ok(value)
}

/// 检查编译后的身份、schema、prepare 和真实 trait 视图，保留尚未验证的远程协议事实。
pub async fn verify_compiled(project: &Path, id: &str) -> Result<Value, ClientError> {
    let plugin = builtin::registry()
        .get(id)
        .map_err(|_| ClientError::new("plugin_not_installed", "插件没有进入编译注册表", 3))?;
    let descriptor = plugin.descriptor();
    let sample_path = project.join(format!("src/plugins/{id}.sample.json"));
    let sample = if sample_path.is_file() {
        serde_json::from_slice::<Value>(
            &std::fs::read(sample_path).map_err(|_| local("示例文件不可读"))?,
        )
        .map_err(|_| local("插件示例必须是合法 JSON"))?
    } else {
        let preset = crate::presets::PRESETS
            .iter()
            .find(|p| p.plugin_id == id)
            .ok_or_else(|| local("插件缺少 sample.json；请提供无秘密的连接配置示例"))?;
        serde_json::to_value(&preset.config).map_err(|_| local("预设序列化失败"))?
    };
    let validator = jsonschema::validator_for(&descriptor.config_schema)
        .map_err(|_| local("插件配置 schema 无效"))?;
    if !validator.is_valid(&sample) {
        return Ok(json!({"valid":false,"code":"sample_schema_invalid","plugin_id":id}));
    }
    for operation in &descriptor.operations {
        jsonschema::validator_for(&operation.input_schema)
            .map_err(|_| local("动作输入 schema 无效"))?;
        jsonschema::validator_for(&operation.output_schema)
            .map_err(|_| local("动作输出 schema 无效"))?;
    }
    let record = BackendRecord {
        id: format!("verify-{id}"),
        kind: id.into(),
        name: id.into(),
        base_url: sample["base_url"].as_str().unwrap_or("").into(),
        api_key: "fixture-only".into(),
        models: sample["models"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(","),
        options: serde_json::to_string(&sample["options"]).unwrap_or_else(|_| "{}".into()),
        capabilities: serde_json::to_string(&sample["capabilities"])
            .unwrap_or_else(|_| "{}".into()),
        ..Default::default()
    };
    let store = Store::open(Path::new(":memory:"))
        .await
        .map_err(|_| local("无法创建隔离验证存储"))?;
    let host = ScopedHostServices::legacy(&record, &[], store, Duration::from_secs(1));
    let runtime = builtin::registry()
        .prepare(&record, host)
        .map_err(|_| local("插件 prepare 失败；检查无网络构造和配置语义"))?;
    let valid = descriptor.features.inference
        && runtime.inference.id() == record.id
        && descriptor.features.quota == runtime.quota.is_some()
        && descriptor.features.credential_refresh == runtime.credential_refresh.is_some()
        && descriptor.features.checkin != runtime.tasks.is_empty()
        && descriptor.auth_flows.is_empty() != plugin.auth_control().is_some();
    Ok(
        json!({"valid":valid,"plugin_id":id,"contract_version":descriptor.contract_version,
        "checked":["compiled_registration","config_schema","operation_schemas","prepare","capability_views"],
        "remote_protocol_verified":false,"descriptor":descriptor}),
    )
}

/// 开发工具失败不输出原始配置或编译器中可能包含的输入。
fn local(message: &str) -> ClientError {
    ClientError::new("plugin_verification_failed", message, 1)
}
