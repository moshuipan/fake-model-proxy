# 贡献指南

欢迎提交问题、文档改进、协议适配和测试修复。提交贡献即表示你有权提供该内容，
并同意以项目的 [MIT 许可证](LICENSE)发布。第三方代码需保留原有许可证和归属声明。

## 开始开发

安装 Rust 1.94.0、Node.js 22 和 Python 3.11 或更高版本。
Rust 版本与组件由 `rust-toolchain.toml` 指定。

```sh
cargo build --locked --bins
cargo run --locked
```

前端资源嵌入 Rust 二进制，修改 `web` 后需要重新构建。
运行数据位于 `FMP_DATA_DIR`，请为测试使用独立目录及本地假上游。
模块职责见 [当前架构](docs/design/architecture.md)，编码边界见 [AGENTS.md](AGENTS.md)。

## 修改约定

- 一个 PR 解决一个明确问题，说明触发场景、修改后的行为及验证方式。
- 函数提供职责注释；新增公共接口同时更新文档与生成契约。
- 兼容 API 使用预设，专有协议使用插件。先读 [插件开发](docs/guides/plugins.md)。
- 保持三个推理入口共用网关、配置 revision/CAS、流取消与未知执行不重放。
- 使用合成账号、凭证和设备数据。测试不能依赖维护者的登录态、路径或真实额度。
- 不提交运行数据库、请求记录、设备报告、私人配置、企业资料或编译产物。

## 验证

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
node --check web/console.js
node --test web/console.test.cjs web/autoglm-login.test.cjs
python3 -B -m unittest discover -s scripts -p 'test_*.py'
python3 -B scripts/check_public_source.py
```

有平台要求的浏览器测试默认忽略；仅在隔离的本机环境中主动运行。
不要把假上游回归通过描述成真实提供商兼容性已验证。

修改契约后运行 `cargo run --locked --example export_contracts`，
检查 `docs/generated` 的差异。使用说明位于 [文档索引](docs/README.md)。
更新依赖与 `Cargo.lock` 后运行 `python3 -B scripts/export_dependency_notices.py`，
审阅许可变化。CI 会检查契约与依赖声明是否仍可复现。

## 问题与安全报告

普通问题使用 Issue 模板，提供版本、复现步骤、预期与实际结果，以及脱敏日志。
安全问题按照 [安全政策](SECURITY.md)私下报告，避免公开凭证或可利用的攻击细节。
