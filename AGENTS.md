# FMP 项目指南

这是一个 Cargo package，Rust/Axum/SQLite + 原生 JS。当前架构从 `docs/design/architecture.md` 进入；接口字段来自 Rust 生成的 `/api/v2/schema`、`/api/v2/openapi.json` 及插件目录，历史设计不能当实现事实。

## 修改边界

- 兼容 API 接入优先使用 `presets/builtin.json` 或公开预设导入。
- 专有协议增加 `plugins/<provider>`、对应传输实现、`plugins/mod.rs` 声明和 `plugins/builtin.rs` 唯一注册项。可用 `fmp plugin scaffold/verify`。
- 公共应用/调度通过能力接口，不新增厂商名字分支。客户端会话证据位于独立的 `client_plugins`。
- 推理、额度、续期、任务实现使用 `ScopedHostServices`；可信授权控制仍需 AppState 编排，不能宣称第三方沙箱。
- 所有配置写入使用 ConfigService；CLI/MCP 使用在线管理 API，禁止直接改 SQLite 或绕过 revision/CAS。
- 描述中的支持能力不代表账号已授权、模型支持或真实余额。未知费用保持 unknown，不能擅自标免费。

## 核心语义

保持三个入口共用 Gateway、统一 Auto、完整流租约、总重试预算、取消和未知执行不重放。重载捕获完整 generation，保留旧流和稳定实例的健康/绑定/冷却。

秘密只经独立写入接口及 store/env 引用进入配置。账号轮换必须 store 引用、凭证 CAS；正常续期不产生配置 revision。日志、schema 诊断、常规配置、计划和操作结果不包含秘密。

新增函数提供函数级注释。schema 从 Rust 类型导出，跨字段校验由插件及配置服务执行；不要在 CLI、UI、Skill 中复制业务校验。

## 操作和验证

在本目录运行：

```sh
cargo build --locked --bins
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
node --check web/console.js
node --test web/console.test.cjs web/autoglm-login.test.cjs
python3 -B -m unittest discover -s scripts -p 'test_*.py'
python3 -B scripts/check_public_source.py
```

公开源码使用白名单打包，不包含运行数据库、凭证、设备报告、请求记录和构建缓存。
新增源码目录或文件类型需同步 Cargo include 与公开源码检查器。
契约变更后运行 `cargo run --locked --example export_contracts`，维护生成快照。

管理工作按 `fmp plugins describe → config get → validate → plan → apply → config get/routes explain` 执行。用户已授权的普通变更可连续执行 plan/apply；远程授权、签到和生成依据动作 effect 与已有授权处理。waiting_user 交给用户，unknown 查询和判断，不自动重放。

本地验证使用隔离 Store 和假上游，默认不使用真实凭证签到或消耗推理额度。详见 `docs/guides/management.md`、`docs/guides/plugins.md`。
