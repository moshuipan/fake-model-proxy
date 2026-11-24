//! 非交互管理 CLI；stdout 永远只含 JSON。

/// 执行在线 API 或本地开发命令，将稳定退出码交给调用方。
#[tokio::main]
async fn main() -> std::process::ExitCode {
    let (value, code) = fakemodelproxy::tooling::cli::run(std::env::args().skip(1).collect()).await;
    println!(
        "{}",
        serde_json::to_string(&value).expect("CLI result is JSON")
    );
    std::process::ExitCode::from(code)
}
