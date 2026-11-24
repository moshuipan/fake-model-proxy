//! 固定命令和 JSON 输出的 CLI 映射；输入文件是声明，不是第二个配置事实源。

use super::{
    client::{ApiClient, ClientError, outcome_code},
    plugin_dev,
};
use reqwest::Method;
use serde_json::{Value, json};
use std::{collections::BTreeMap, io::Read, path::PathBuf};

const HELP: &str = "fmp [--server URL] [--json] identity | schema | openapi | plugins list/describe ID | presets list/import --file FILE | config get/validate/plan/get-plan/apply/rollback | operations list/run/get/cancel | routes explain --file FILE | secrets write --stdin | plugin scaffold/verify ID [--project DIR]. JSON files accept '-' for stdin. Authentication: FMP_ADMIN_KEY; server: FMP_SERVER (default http://127.0.0.1:8317). plan: --file PlanInput; validate: --file ConfigChange; apply: --plan-id ID --idempotency-key KEY; rollback: --expected-revision N --target-revision N; run: --file OperationRequest. Exit codes: 0 accepted/success, 1 local failure, 2 usage, 3 invalid, 4 conflict, 5 needs user, 6 auth, 7 failed/unknown/cancelled/expired, 8 transport.";

struct Arguments {
    words: Vec<String>,
    options: BTreeMap<String, String>,
}

impl Arguments {
    /// 解析严格选项；拒绝重复、遗漏值及命令不支持的字段。
    fn parse(args: Vec<String>) -> Result<Self, ClientError> {
        let mut words = Vec::new();
        let mut options = BTreeMap::new();
        let mut input = args.into_iter();
        while let Some(arg) = input.next() {
            if let Some(name) = arg.strip_prefix("--") {
                if options.contains_key(name) {
                    return Err(usage());
                }
                let value = if matches!(name, "json" | "stdin" | "help" | "compiled") {
                    String::new()
                } else {
                    input
                        .next()
                        .filter(|v| !v.starts_with("--"))
                        .ok_or_else(usage)?
                };
                options.insert(name.into(), value);
            } else {
                words.push(arg);
            }
        }
        Ok(Self { words, options })
    }
    /// 必填字段不能缺省为空字符串。
    fn required(&self, key: &str) -> Result<&str, ClientError> {
        self.options
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
            .ok_or_else(usage)
    }
    /// 每个命令明确列出允许的参数，避免拼错选项被静默忽略。
    fn allow(&self, keys: &[&str], word_count: usize) -> Result<(), ClientError> {
        if self.words.len() != word_count
            || self.options.keys().any(|key| {
                !matches!(key.as_str(), "json" | "server") && !keys.contains(&key.as_str())
            })
        {
            return Err(usage());
        }
        Ok(())
    }
    /// 从文件或 stdin 读取一个有大小上限的 JSON 文档，不回显非法原文。
    fn file(&self) -> Result<Value, ClientError> {
        let path = self.required("file")?;
        let data = if path == "-" {
            read_stdin()?
        } else {
            let file = std::fs::File::open(path)
                .map_err(|_| ClientError::new("input_unreadable", "无法读取输入文件", 2))?;
            read_limited(file)?
        };
        serde_json::from_slice(&data)
            .map_err(|_| ClientError::new("invalid_json", "输入必须是合法 JSON", 3))
    }
}

/// 将任意本地失败映射为单个 JSON 响应，stdout 无帮助日志。
pub async fn run(args: Vec<String>) -> (Value, u8) {
    match execute(args).await {
        Ok(value) => {
            let code = outcome_code(&value);
            (value, code)
        }
        Err(error) => (error.value(), error.exit_code),
    }
}

/// 根据严格命令分派在线用例；本地开发工具不连接运行服务器。
async fn execute(args: Vec<String>) -> Result<Value, ClientError> {
    let args = Arguments::parse(args)?;
    if args.words.is_empty() || args.options.contains_key("help") || args.words == ["help"] {
        return Ok(json!({"help":HELP}));
    }
    let command = args.words.first().map(String::as_str).unwrap_or("");
    let action = args.words.get(1).map(String::as_str).unwrap_or("");
    let id = || {
        args.words
            .get(2)
            .map(String::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(usage)
    };
    if command == "plugin" {
        let project = args
            .options
            .get("project")
            .map(PathBuf::from)
            .unwrap_or_else(plugin_dev::default_project);
        match action {
            "scaffold" => {
                args.allow(&["project"], 3)?;
                return plugin_dev::scaffold(&project, id()?);
            }
            "verify" => {
                args.allow(&["project", "compiled"], 3)?;
                return if args.options.contains_key("compiled") {
                    plugin_dev::verify_compiled(&project, id()?).await
                } else {
                    plugin_dev::verify(&project, id()?).await
                };
            }
            _ => return Err(usage()),
        }
    }
    let base = args
        .options
        .get("server")
        .cloned()
        .or_else(|| std::env::var("FMP_SERVER").ok())
        .unwrap_or_else(|| "http://127.0.0.1:8317".into());
    let client = ApiClient::new(&base, std::env::var("FMP_ADMIN_KEY").ok())?;
    let (method, path, body) = match (command, action) {
        ("identity", "") => {
            args.allow(&[], 1)?;
            (Method::GET, vec!["identity"], None)
        }
        ("schema", "") => {
            args.allow(&[], 1)?;
            (Method::GET, vec!["schema"], None)
        }
        ("openapi", "") => {
            args.allow(&[], 1)?;
            (Method::GET, vec!["openapi.json"], None)
        }
        ("plugins", "list") => {
            args.allow(&[], 2)?;
            (Method::GET, vec!["plugins"], None)
        }
        ("plugins", "describe") => {
            args.allow(&[], 3)?;
            (Method::GET, vec!["plugins", id()?], None)
        }
        ("presets", "list") => {
            args.allow(&[], 2)?;
            (Method::GET, vec!["presets"], None)
        }
        ("presets", "import") => {
            args.allow(&["file"], 2)?;
            let data = args.file()?;
            let body = if data.is_array() {
                json!({"presets":data})
            } else {
                data
            };
            (Method::POST, vec!["presets"], Some(body))
        }
        ("config", "get") => {
            args.allow(&[], 2)?;
            (Method::GET, vec!["config"], None)
        }
        ("config", "validate") => {
            args.allow(&["file"], 2)?;
            (Method::POST, vec!["config", "validate"], Some(args.file()?))
        }
        ("config", "plan") => {
            args.allow(&["file"], 2)?;
            (Method::POST, vec!["config", "plans"], Some(args.file()?))
        }
        ("config", "get-plan") => {
            args.allow(&["plan-id"], 2)?;
            (
                Method::GET,
                vec!["config", "plans", args.required("plan-id")?],
                None,
            )
        }
        ("config", "apply") => {
            args.allow(&["plan-id", "idempotency-key"], 2)?;
            (
                Method::POST,
                vec!["config", "plans", args.required("plan-id")?, "apply"],
                Some(json!({"idempotency_key":args.required("idempotency-key")?})),
            )
        }
        ("config", "rollback") => {
            args.allow(&["expected-revision", "target-revision"], 2)?;
            let number = |key| args.required(key)?.parse::<u64>().map_err(|_| usage());
            (
                Method::POST,
                vec!["config", "rollback"],
                Some(json!({
                    "expected_revision":number("expected-revision")?, "target_revision":number("target-revision")?
                })),
            )
        }
        ("operations", "list") => {
            args.allow(&[], 2)?;
            (Method::GET, vec!["operations"], None)
        }
        ("operations", "run") => {
            args.allow(&["file"], 2)?;
            (Method::POST, vec!["operations"], Some(args.file()?))
        }
        ("operations", "get") => {
            args.allow(&[], 3)?;
            (Method::GET, vec!["operations", id()?], None)
        }
        ("operations", "cancel") => {
            args.allow(&[], 3)?;
            (Method::POST, vec!["operations", id()?, "cancel"], None)
        }
        ("routes", "explain") => {
            args.allow(&["file"], 2)?;
            (Method::POST, vec!["routes", "explain"], Some(args.file()?))
        }
        ("secrets", "write") => {
            args.allow(&["stdin"], 2)?;
            if !args.options.contains_key("stdin") {
                return Err(usage());
            }
            let value = String::from_utf8(read_stdin()?)
                .map_err(|_| ClientError::new("secret_invalid", "凭证必须是 UTF-8 文本", 3))?;
            (
                Method::POST,
                vec!["secrets"],
                Some(json!({"value":value.trim_end_matches(['\r', '\n'])})),
            )
        }
        _ => return Err(usage()),
    };
    client.request(method, &path, body).await
}

/// 使用固定帮助提示，不将用户输入拼入错误。
fn usage() -> ClientError {
    ClientError::new("usage", "参数无效；运行 fmp --help 查看命令格式", 2)
}

/// stdin 与文件采用相同上限，避免不可控内存分配。
fn read_limited(input: impl Read) -> Result<Vec<u8>, ClientError> {
    let mut data = Vec::new();
    input
        .take(1024 * 1024 + 1)
        .read_to_end(&mut data)
        .map_err(|_| ClientError::new("input_unreadable", "输入读取失败", 2))?;
    if data.len() > 1024 * 1024 {
        return Err(ClientError::new("input_too_large", "输入超过 1 MiB", 3));
    }
    Ok(data)
}

/// 凭证仅从 stdin 传入，防止出现在进程参数中。
fn read_stdin() -> Result<Vec<u8>, ClientError> {
    read_limited(std::io::stdin().lock())
}
