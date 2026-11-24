# 插件开发

先判断新增内容需要数据还是执行代码。兼容 API 的端点、模型和默认策略使用公开预设；自定义签名、OAuth、流解析、额度和签到需要真实代码。预设不能安装执行能力，manifest 不能证明模型能力或免费许可。

## 现有契约

`plugins/contract.rs` 的 `ProviderPlugin` 提供描述、无网络校验与 prepare；`PluginDefinition` 用函数组合这些接口。`ProviderRuntime` 必须提供推理，并按实际实现提供额度、续期和任务视图。`AuthControl` 承载受控授权，`AccountControl` 承载派生账号与凭证规范化。

工厂从 `ScopedHostServices` 获取本服务的账号、CAS 凭证写入、命名空间状态、HTTP、时钟与取消。不要给推理/额度/任务适配器传入整个 AppState，不要读写其他服务。可信控制适配器仍使用 AppState 进行宿主编排；这是现有授权边界，不是第三方沙箱。

插件 `features` 描述实现，账号就绪、实际额度、模型权限、未知计费和能力属于运行事实。宿主控制费用许可、优先级、预算、重试与准入。厂商特殊输入/输出上限要在模型目录归一，不在公共调度中按名字分支。

## 脚手架与本地验证

```sh
cargo build --bin fmp
./target/debug/fmp plugin scaffold new_provider
./target/debug/fmp plugin verify new_provider
```

可用 `--project DIR` 指定 Cargo 项目。ID 为 2–64 位小写 Rust 模块名，现有文件不会被覆盖。脚手架创建：

- `src/plugins/new_provider.rs`：真实 OpenAI 兼容传输组合。
- `src/plugins/new_provider.sample.json`：无秘密、本地占位端点的构造校验样例。
- `plugins/mod.rs` 一条模块声明和 `builtin.rs` 一条注册项。

生成结果中的 `verified:false` 要通过 verify 才能完成构造契约验证。新模块初始只实现兼容传输，不能据此声称实现了平台专有协议。若平台本身兼容，通常导入数据预设即可；脚手架用于接下来需要替换传输或补真实能力的开发。

`verify` 编译当前项目中的 `fmp`，使用 Cargo 返回的实际二进制运行已编译注册表检查。完整编译日志保存在项目 `target/fmp-plugin-verify.log`，stdout 仅为 JSON。它检查注册、配置/动作 schema、样例、prepare 和 trait 视图；不登录、不签到、不发生成请求，`remote_protocol_verified` 始终为 false。

需要专有协议时，将传输、授权、额度、任务实现放在该插件目录或已有 backend 传输模块，用一次 prepare 组合共享状态。为本地假上游增加协议集成验证，覆盖事件终态、工具增量、取消、deadline、错误与未知执行不重放。实际平台证据仍需单独确认，禁止占位成功实现。

## 数据预设

内置预设位于 `presets/builtin.json`，运行时公开导入使用 `POST /api/v2/presets` / `fmp presets import`。新预设必须引用已编译的插件并通过插件 config schema，不包含凭证或脚本。分组使用 `ui_group` 元数据；执行通过能力判断。

用 `fmp schema` 获取 ProviderPreset 类型；用 `fmp plugins describe PLUGIN_ID` 获取该插件配置 schema。预设变更不会静默覆盖已有服务，服务更新仍走配置计划。

## 验证与发布

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
node --check web/console.js
node --test web/console.test.cjs
cargo build --bins
```

全量插件契约回归还要与内置注册清单同步，新增已注册插件时不要继续硬编码旧数量。发布代码需要重新构建并重启服务；已有服务实例可通过配置热启停。进程外执行、动态库热加载和不可信插件沙箱未提供。
