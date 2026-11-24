# 管理 API、CLI 与 MCP

管理业务都调用 `/api/v2`。CLI 和 MCP 通过 HTTP 使用同一个在线服务，不直接读写 SQLite。字段事实源是 `GET /api/v2/schema` 和 `GET /api/v2/plugins/{id}`；OpenAPI 3.1 由 `GET /api/v2/openapi.json` 导出。配置 schema 中的自由 options 还必须符合插件 schema。

仓库中的 [生成契约快照](./generated/README.md) 便于离线阅读；管理当前服务器时仍需在线发现它的版本与实际插件。

## 构建与身份

在 Cargo 项目目录运行：

```sh
cargo build --bins
./target/debug/fmp --help
./target/debug/fmp plugins list
./target/debug/fmp plugins describe raccoon --json
./target/debug/fmp config get
```

CLI 默认访问 `http://127.0.0.1:8317`，可用 `FMP_SERVER` 或 `--server` 修改。`FMP_ADMIN_KEY` 是 CLI/MCP 进程使用的管理认证密钥；它可以是服务器主密钥，也可以是服务器 `FMP_ACCESS_KEYS` 中的具名密钥。不提供命令行明文密钥参数。

服务器 `FMP_ACCESS_KEYS` 接受以下 JSON 形状，示例中的密钥必须替换：

```json
[
  {"id":"ai-reader","key":"replace-with-independent-key","scopes":["read"]},
  {"id":"ai-manager","key":"replace-with-another-key","scopes":["read","config_write","operation","secret_write"]}
]
```

密钥至少 16 字符，身份与密钥唯一，身份不能为 `admin`/`local`。主 `FMP_ADMIN_KEY` 代表全权限，缺省复用 `FMP_API_KEY`。只有回环监听且未设置任何管理密钥时允许本地匿名管理。`fmp identity` 可检查实际身份和范围；旧 v1 管理要求全权限。

| 用例 | 所需范围 |
| --- | --- |
| 目录、配置、schema、校验、计划、路由解释 | `read` |
| 应用配置、导入预设 | `config_write` |
| 写入秘密 | `secret_write` |
| 执行动作、取消操作 | `operation` |
| 同步模型目录 | `operation` + `config_write` |
| 开始授权 | `operation` + `config_write` + `secret_write` |

普通具名身份只能读取及取消自己的操作；主管理员和本地身份可查看全部历史。

## 配置闭环

先读取配置的 `desired_revision`、`active_revision` 和 `document`。以当前 revision 创建 `PlanInput` 文件；服务声明必须提供 `api_version`、`kind`、`id`、`name`、`plugin_id`、`plugin_config_version`、`enabled` 和 `config`。不要照抄旧设计文档中省略字段的示意 JSON。

```json
{
  "expected_revision": 0,
  "change": {
    "services": [{
      "api_version": "fmp/v2",
      "kind": "Service",
      "id": "personal-gateway",
      "name": "个人网关",
      "plugin_id": "openai_compat",
      "plugin_config_version": 1,
      "enabled": false,
      "config": {
        "base_url": "https://gateway.example/v1",
        "models": ["chosen-model"],
        "options": {},
        "capabilities": {}
      },
      "credentials": {},
      "policy": {"priority":20,"cost_tier":"unknown","candidate_models":["chosen-model"]}
    }]
  }
}
```

这是待填地址、模型和 revision 的模板，默认停用。`unknown` 不授予免费许可；要加入 Auto，应确认模型能力及实际费用池，再显式启用。更新服务需要提交完整 ServiceSpec；缺省变更数组保持其他服务，删除使用 `remove_services`/`remove_accounts`。

```sh
fmp config validate --file change.json
fmp config plan --file plan.json
fmp config get-plan --plan-id RETURNED_PLAN_ID
fmp config apply --plan-id RETURNED_PLAN_ID --idempotency-key UNIQUE_REQUEST_KEY
fmp config get
fmp routes explain --file request.json
```

`validate` 输入为 `ConfigChange`（上例的 `change` 对象），`plan` 输入为完整 `PlanInput`。`request.json` 可为 `{"model":"auto","input_tokens":100,"output_tokens":200}`。解释按所给文本量构造保守请求形状，核心继续叠加协议、工具、图片及厂商余量；它不代表真实 tokenizer 计数。解释仅使用缓存快照，目录/额度查询另行执行。

计划有效期 15 分钟。`revision_conflict` 要重新读取当前配置并重建计划，不能自动用新 revision 覆盖旧意图。apply 幂等键绑定计划和摘要；网络超时后先查配置/计划，再用同一键重试。不要每次重试生成新键。

回滚先生成计划，仍需显式 apply：

```sh
fmp config rollback --expected-revision CURRENT_REVISION --target-revision OLD_REVISION
```

秘密只从 stdin 写入：

```sh
fmp secrets write --stdin
```

可从已有受保护文件或管道接入 stdin；不要把明文加入进程参数。返回 `store:...` 引用，把它放到 `credentials.api_key.secret_ref` 或账号 `credential.secret_ref`，再 plan/apply。命令去除末尾换行，保留正文。`env:` 引用在服务器环境解析，与 CLI 本机环境不同；需要轮换的账号只能使用 `store:`。常规配置、计划、操作和导出不回显秘密。

## 操作与预设

从插件 `operations` 读取动作、effect、输入 schema、幂等和交互要求。运行操作输入：

```json
{
  "service_id":"SERVICE_ID",
  "action":"models.sync",
  "input":{},
  "idempotency_key":"UNIQUE_OPERATION_KEY"
}
```

```sh
fmp operations run --file operation.json
fmp operations get RETURNED_OPERATION_ID
fmp operations list
fmp operations cancel RETURNED_OPERATION_ID
fmp presets list
fmp presets import --file presets.json
```

预设文件为数组或 `{"presets":[...]}`。每项使用 `fmp/preset/v1`，完整字段查 `schema.preset`。它只定义公开默认数据，拒绝秘密、脚本和未知字段，且必须引用已安装插件。导入不会修改既有服务。

操作 `pending/running` 表示已受理，需查询终态；`waiting_user` 返回 `next_action`，交给用户完成后继续查询。`unknown` 可能已影响上游，禁止自动重放；`cancelled/expired/failed` 也不能据此断言上游没有副作用。操作幂等不等于远程恰好一次。

CLI 始终输出一个 JSON 文档，`--json` 为兼容显式选项，不改变格式。`--file -` 使用 stdin。固定退出码：

| 码 | 含义 |
| --- | --- |
| 0 | 成功或已受理；长期动作仍需查询 |
| 1 | 本地初始化、编译或工具故障 |
| 2 | 命令参数/文件读取错误 |
| 3 | JSON/schema/配置无效 |
| 4 | revision 或幂等冲突 |
| 5 | 需要用户完成授权 |
| 6 | 身份或权限拒绝 |
| 7 | 操作失败、未知、取消或过期 |
| 8 | HTTP/网络失败，提交结果可能需查询 |

## MCP stdio

构建后的 `fmp-mcp` 以换行 JSON-RPC 在 stdin/stdout 通信，stdout 不混日志。支持 `2024-11-05`、`2025-03-26`、`2025-06-18` 协议协商。客户端配置示例：

```json
{
  "mcpServers": {
    "fmp": {
      "command": "/absolute/path/to/fakemodelproxy/target/debug/fmp-mcp",
      "env": {"FMP_SERVER":"http://127.0.0.1:8317"}
    }
  }
}
```

需要认证时，通过客户端受保护的环境配置传入 `FMP_ADMIN_KEY`。工具为 `inspect`、`validate_config`、`plan_config`、`apply_config`、`rollback_config`、`run_operation`、`get_operation`、`cancel_operation`、`explain_route`。无 shell、任意 URL、任意数据库或凭证读取工具。秘密写入使用 CLI stdin，再由 MCP 配置引用。

`inspect.resource` 支持 `identity/plugins/plugin/config/schema/presets/operations`，只有 `plugin` 要求 `id`。配置和动作 schema 与 HTTP 共用 Rust 类型，服务端最终执行权限、版本、插件能力及字段校验。MCP 工具注解是客户端提示，不能代替权限。
