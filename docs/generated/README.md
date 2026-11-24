# 生成的管理契约

这些 JSON 是 2026-11-24 实现的公开契约快照，不包含服务实例配置或账号秘密。Rust 请求类型和已安装插件目录仍是事实源。

- `management-schema.json`：ConfigDocument、变更、服务、计划、操作、解释和预设输入类型。
- `management-openapi.json`：管理 HTTP OpenAPI 3.1。
- `builtin-plugins.json`：已安装内置插件的真实描述、options schema 与动作。

在 Cargo 项目目录离线导出，不需要启动服务、配置账号或连接提供商：

```sh
cargo run --locked --example export_contracts
```

可以传入输出目录：`cargo run --locked --example export_contracts -- /tmp/fmp-contracts`。
CI 将重新导出并与仓库快照比较。不要用手写 JSON 替代生成器。

远程服务器的插件版本可能不同。操作远程服务时仍使用在线 `fmp schema`、
`fmp openapi` 和 `fmp plugins list` 发现当前能力，不能用本地快照替代服务器事实。
