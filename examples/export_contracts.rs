//! 从编译后的类型和注册目录离线导出契约，不启动网关或读取账号数据。

use fakemodelproxy::{application::api_schema, plugins::builtin};
use std::{env, fs, path::Path};

/// 生成格式固定的 JSON 快照，保持与 CI 比较及在线发现共用事实源。
fn main() -> anyhow::Result<()> {
    let mut arguments = env::args_os().skip(1);
    let output = arguments.next().unwrap_or_else(|| "docs/generated".into());
    anyhow::ensure!(
        arguments.next().is_none(),
        "usage: export_contracts [output-directory]"
    );
    let output = Path::new(&output);
    fs::create_dir_all(output)?;
    for (name, value) in [
        ("management-schema", api_schema::bundle()),
        ("management-openapi", api_schema::openapi()),
        (
            "builtin-plugins",
            serde_json::json!({"plugins": builtin::registry().descriptors()}),
        ),
    ] {
        fs::write(
            output.join(format!("{name}.json")),
            format!("{}\n", serde_json::to_string_pretty(&value)?),
        )?;
    }
    Ok(())
}
