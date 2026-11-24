//! 管理 API 的 MCP stdio 薄传输；不暴露 shell 和数据库写入。

/// stdio stdout 只输出 MCP JSON-RPC 消息。
#[tokio::main]
async fn main() -> std::process::ExitCode {
    match fakemodelproxy::tooling::mcp::serve_from_env().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}", error.value());
            std::process::ExitCode::from(error.exit_code)
        }
    }
}
