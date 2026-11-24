# MonkeyCode 接入

2026-11-20 实现并完成真实账号验证。控制台选择 **添加服务 → MonkeyCode**。

## 官网登录

在服务详情选择「登录账号」，FMP 直接在代理所在电脑打开独立 Chrome 窗口，登录页使用带地址栏和标签栏的普通标签页。完成官方短信、扫码或验证码后，FMP 验证 `GET /api/v1/users/status` 返回的身份，通过官方模型密钥接口保存账号，并在后台同步和保存模型目录，控制台展示完成结果。

会话有效期 10 分钟。取消等待、关闭登录窗口、停用或修改服务区域都会终止未完成登录。重复轮询不重复创建密钥，同一用户再次登录会更新已有账号并复用模型密钥。账号已保存但目录同步失败时，可以点击「重试同步模型」。

需要代理监听本地回环地址，并在代理电脑安装 Chrome。可以用 `FMP_LOGIN_BROWSER` 指定 Chrome 可执行文件的完整路径。网页登录只读取本次临时窗口，不读取用户既有 Chrome 配置或 MonkeyCode 桌面客户端文件。

原有「打开官网登录」是普通外链，官网没有回调到 FMP，网页登录成功无法自动添加账号。此入口已替换为具有会话、身份确认和账号保存的登录流程。

此前应用窗口的外观来自 Chrome `--app` 参数，现已改成普通浏览器窗口。独立窗口仍是当前登录方案的要求：其他提供商通过 OAuth 或设备码回调交付登录结果，MonkeyCode 的官方流程依赖 HttpOnly Cookie。仅在当前浏览器打开官网新标签页，FMP 无法读取跨域 Cookie，也没有官方回调可以接收登录结果。

已删除桌面客户端导入的入口、文件上传逻辑和后端实现；`POST /api/monkeycode/import-client` 返回 404。模型密钥和 Cookie 通过官网登录自动保存；Cookie 失效后选择「重新登录」。

## 协议和模型

| 区域 | 账号接口 | 模型网关 |
|---|---|---|
| 国内 | `https://monkeycode-ai.com` | `https://proxy.monkeycode-ai.com/v1` |
| 国际 | `https://monkeycode-ai.net` | `https://proxy.monkeycode-ai.net/v1` |

模型目录来自 `GET /api/v1/users/models`，读取完整分页。出站 `model` 使用目录的 `model` 字段，配置 UUID 不作为模型名。FMP 对外提供持久调用名称，可在控制台复制或通过 `/v1/models` 查询。

每个模型按 `interface_type` 选择 Chat Completions、Messages 或 Responses 出站。三个 FMP 入站协议共用该适配器，支持文本、思考、工具调用、工具结果和流式响应。上游统一请求流式结果，非流式入站由现有事件收集器汇总。

`X-OhMyAgent-Signature` 是 `v1=` 加 HMAC-SHA256 十六进制签名，签名对象是实际发出的系统提示。请求没有非空系统消息时使用 `You are a helpful assistant.`；已有系统消息按目标协议编码并签名。不会加入官方 Agent 的工具定义，也不会在代理执行客户端工具。

隐藏模型、占位档位、未知协议以及超出当前会员等级的 `monkeycode-pro` / `monkeycode-ultra` 模型不进入可调用目录。目录按账号缓存五分钟，重新登录会重建后端快照。

`context_limit`、`output_limit` 分别记录为总窗口和输出上限；图片与思考能力来自对应目录字段。模型名称不作为能力证据。Responses 模型明确拒绝无法表达的 `stop` 参数。

## 额度

钱包来自 `GET /api/v1/users/wallet`，加入现有十二小时自动更新及手动「更新账号额度」流程。

积分余额使用十进制定点保存；每日免费模型的 `daily_token_balance` / `daily_token_limit` 是另一种资源，在账号详情独立说明，不与积分相加。未保存有效登录态或查询失败时额度保持未知。

费用类型默认「费用未知」，官方未提供倍率时保留未知。可用目录可能同时包含基础会员模型和按积分计费模型，不能将整个服务标为无限免费。

网页登录、注册、短信或交互式验证码在官方界面完成。

## 每日签到

控制台「后台任务」中支持单服务执行、一键签到和每日自动调度。国内和国际区域沿用各自官方账号接口，使用当前账号的网页登录态。

执行前调用 `GET /api/v1/users/wallet/checkin`，只有明确返回 `checked_in: false` 才领取。已签到直接记录「今日已签到」。领取时按官方客户端协议获取计算式验证码：`POST /api/v1/public/captcha/challenge` → 本地 PoW 求解 → `POST /api/v1/public/captcha/redeem` → `POST /api/v1/users/wallet/checkin`。计算在后台线程运行，整组最多 500 万次哈希、20 秒，且不超过上游有效期；任务取消会停止计算。

签到任务沿用本地每日幂等记录。提交后的网络错误、坏 JSON 或状态缺失只允许再查询一次签到状态；仍不能确认时保留「结果未知」，手动、批量和自动任务都不会重放领取。验证期间停用服务、切换区域或重新登录会阻止旧任务提交。

完成或已签到后只更新该账号钱包，快照版本递增；保留原有十二小时全量刷新计划。额度查询失败会单独提示，不改变已确认的签到结果。官方领取接口不返回实发奖励数，任务不硬编码奖励或用钱包差额推测奖励。

## 验证

离线验证：`cargo test --test monkeycode_flow`，覆盖九种入站/出站组合、目录分页和会员筛选、并行工具参数、截断流、通用账号凭证验证、跨区域凭证拒绝、凭证脱敏与小数积分，以及桌面客户端导入接口已移除。

签到验证：`cargo test --test monkeycode_checkin --test quota_refresh`，覆盖正常领取、并发幂等、已签到、未知结果不重放、只读状态核对、验证码拒绝及过期、配置和账号归属校验，以及单账号余额发布与全量刷新并发。PoW 使用独立 Python hashlib 参考向量验证盐、目标和最小 nonce。

本次签到接入后，`cargo test --all-targets` 312 项通过（1 项依赖本机 Chrome 的测试默认忽略），前端 27 项通过，Clippy 无警告。8317 已更新；内置浏览器执行真实账号签到，官网返回「今日已签到」，本地记录为 `already_done`，奖励数为空。钱包查询时间更新，快照版本从 5 升到 6，原有下次全量刷新时间保持不变。真实账号当日已完成签到，因此新的领取分支由离线 HTTP 测试验证。

网页登录验证：`cargo test --lib monkeycode`，覆盖身份接口短暂失败后重试、自动保存账号和目录、重复轮询与再次登录复用密钥、取消及配置变更后撤销迟到密钥、会话归属、管理鉴权、超时和目录失败后的保留账号与重试。真实 Chrome 离线验证命令：`cargo test --lib isolated_browser_reads_only_its_own_http_only_cookie -- --ignored`，验证 HttpOnly Cookie 读取、窗口之间隔离、域过滤和临时目录清理。

此前全量 Rust 测试 276 项通过。本次删除客户端导入、调整浏览器窗口后，网页登录专项 5 项、协议与管理专项 4 项、前端测试 27 项、真实 Chrome 专项 1 项通过，Clippy 无警告。8317 已更新；内置浏览器确认点击「登录账号」直接进入等待状态、没有客户端导入入口或文件选择框，取消等待正常。官网短信或扫码登录仍由用户完成。

本机真实账号同步到 14 个可调用模型。已验证：

- `monkeycode-basic/qwen3.8-flash`：Responses 出站，经控制台 Chat 入站返回“MonkeyCode 连接成功”。
- `monkeycode-basic/glm-5.3-flash`：Messages 入站返回文本；Responses 入站返回完整 SSE 与完成事件。
- Messages 和 Responses 两类真实模型均完成 `echo` 工具调用、结果回传和续答，最终返回 `monkeycode-ok`。工具执行方为调用客户端。
- 钱包成功读取，积分及每日 Token 分开呈现。

## 官方协议依据

仓库：[chaitin/MonkeyCode](https://github.com/chaitin/MonkeyCode/)，核对提交 `4c8c030f986fb02d377e23b398ba347894cfe06e`。

- `desktop/src/baizhi/mod.rs`：国内/国际模型网关及客户端密钥文件。
- `desktop/src/baizhi/monkeycode.rs`：模型清单、会员权限、钱包和模型密钥接口。
- `desktop/src/baizhi/pow.rs`、`backend/pkg/captcha/captcha.go`：计算式验证码算法、challenge/redeem 和有效期。
- `desktop/src/baizhi/cookies.rs`：客户端 Cookie 文件格式、域/路径/有效期匹配。
- `desktop/src/baizhi/weblogin.rs`：独立窗口、HttpOnly Cookie 和官方身份确认流程。
- `backend/biz/llmproxy/ohmyagent_prompt.go`：三种协议的系统提示提取和签名验证。
- `backend/domain/model.go`：模型目录字段。

实现位置：`src/backend/monkeycode/`、`src/admin/monkeycode.rs`、`tests/monkeycode_flow.rs`。
