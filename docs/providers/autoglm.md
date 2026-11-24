# AutoGLM 接入

后端类型为 `autoglm`，使用 AutoGLM 官方客户端账号和积分。代理内置独立 Rust 适配器，支持国内短信登录、国际 OAuth、凭证续期、模型同步、原生工具流、钱包余额和每日签到。

## 控制台使用

1. 打开控制台，选择「添加服务」→「AutoGLM」，保存服务。
2. 点击「登录账号」。
   国内站打开代理提供的短信登录页面，填写手机号后发送验证码。上游要求安全验证时，按页面中的阿里云官方组件提示操作。填写收到的验证码并点击「登录并保存账号」，成功后控制台自动显示新账号。
   国际站选择「使用 Z.ai 授权」或「使用 Google 授权」，按官方提示完成验证和授权，收到回调后自动保存账号。国际站浏览器与代理需运行在同一台电脑。
3. 点击「同步目录」，保存上游模型名称与明确的积分倍率。列表、试聊下拉与详情使用官方名称；外部调用和复制使用持久调用名称，例如 `autoglm-glm-5.3`，可在模型参数中编辑。原始 `服务ID/模型ID` 继续兼容。
4. 「更新账号额度」读取钱包余额；「任务」页可执行每日签到。
5. 在控制台「任务 → 自动签到」设置每日时间和随机范围，保存后开启调度，无需重启。

国内是自建网页调用官方客户端短信接口，国际是官方 OAuth。两种方式成功后都自动保存账号。已有客户端凭证仍可通过服务详情的「手工导入」保存，既有账号可调用和续期。

示例凭证结构：

```json
{
  "access_token": "<访问令牌>",
  "refresh_token": "<刷新令牌>",
  "device_id": "<登录时的 UUID>",
  "user_id": "<用户 ID>",
  "region": "global"
}
```

支持桌面端 `accessToken`、`refreshToken`、`deviceId`、`account`、`expiresAt` 字段。访问令牌可包含 `Bearer ` 前缀。JWT 中的设备 ID、到期时间与账号区域可以自动提取；这些元数据只用于客户端请求，JWT 解码不代表签名验证。

只有访问令牌也能调用；自动续期需要刷新令牌与登录时的设备 ID。不要给导入的账号随机补设备 ID，服务端可能校验原设备身份。

## 实现边界

- 国内 2.5.3 参考客户端的账号和模型服务缺省使用 `autoglm-acceleration-api.zhipuai.cn`；国际账号保留 `autoglm-api.autoglm.ai`。`options.region` 显式覆盖区域；缺省按账号 JWT 的 `source_id` 推断。`account_base` 可显式保留其他账号域。
- 网页授权使用 `/userapi/overseasv1/{zai|google}-oauth-url` 与 `-oauth-login`。回调只监听本机 `127.0.0.1`，从官方端口池 `18432 / 19654 / 19723 / 53699` 选择可用端口；回跳地址为 `http://localhost:<端口>/auth/callback-<提供商>`，不能用于代理与浏览器分处两台电脑的部署。
- 2.4.0 官方流程先读取 `oauth-captcha-config`。启用阿里云验证时，界面加载阿里云官方 SDK，由用户完成验证；回执作为 `ali_captcha_verify_param` 交由上游校验后才取得授权地址。官方国际 Z.ai 域名为 `chat.z.ai`，Google 为 `accounts.google.com`。
- 国内入口创建十分钟随机会话及同源短信页面。配置、发码和登录使用同一设备 UUID。未知验证配置拒绝发码；官方确认发送成功后绑定手机号，至少六十秒才能重发。登录响应必须包含访问令牌、刷新令牌和用户 ID。
- 国内短信操作校验 Origin、Host 和 `Sec-Fetch-Site`，页面不能被嵌入外站，不缓存会话，也不读取管理密钥。取消、超时及服务配置变更后不能保存迟到结果。保存和取消共用终态锁；重复登录或轮询只读取已保存结果，重登录保留原账号名称。
- 每次海外网页授权有独立会话 ID、设备 UUID、期限和 `state`。只接受 HTTPS 官方域名、唯一且匹配的 `state`、一次有效回调；取消、十分钟超时、删除或停用服务时不再保存迟到结果。后台自动兑换并保存；重复轮询只读取结果。授权开始、兑换和后续续期使用同一个设备 ID。
- 每个请求生成客户端时间签名并设置身份头。`X-Request-Model` 保留完整模型 ID；请求体剥掉提供商前缀。
- 目录保留 `display_name` / `displayName` / `name` 等名称字段，重复模型补齐缺失元数据。分组、请求和活动的通用 `id` 不进入模型白名单。
- 目录支持 2.5.3 的顶层或 `metadata.creditCostFactor`、`creditCostFactorOriginal`，要求数值有限且在 0 至 1000 内，顶层优先。也保留明确的 `price_factor` / `priceFactor` 或带 `x` / `×` 的 `credits`；裸 `price` / `cost` / `credits` 不推断单位。倍率说明和门控尚未进入统一模型契约。
- 对外支持 Chat Completions、Responses、Messages，复用统一事件流和原生工具协议。上游统一请求 SSE，非流式入口聚合事件。
- 当前适配器仍省略 `max_tokens` 系列字段、剥掉模型前缀并改写系统身份。这些是既有兼容策略；本次 2.5.3 静态研究没有确认它们是普通推理通道的必需条件，不能据此宣称官方接口一律拒绝输出上限。输出硬限制仍不能保证。
- Auto 使用模型级输入和窗口限制，并预留目录声明的最大输出量；目录未提供输出上限时，需要显式设置 `options.routing.provider_output_reserve`。两者都未知时返回 `provider_output_limit_unknown`，仍可用明确的模型名称调用。显式预留是部署的容量预算，不会使上游支持输出硬限制；预算必须覆盖上游可能产生的最大输出。
- 钱包余额保留原始积分单位和最多十八位小数，不先取整。零或负余额退出 Auto，未知余额不伪装成零。默认只在同一服务、同一积分单位范围内比较余额和倍率，最终调用固定选中的账号。完整规则见 [Auto 路由](../design/auto-routing.md)。
- 在目录、额度或生成请求中检查到期时间，距到期五分钟且具备刷新令牌时续期；确定的 401 在续期后重试一次。后台目录和额度读取也使用这条路径，不是独立的无条件 token 刷新定时器。新令牌条件写回 SQLite；并发续期及注册表重载共享同一把锁，人工编辑和删除不会被旧续期结果覆盖。
- 主刷新接口仅在明确业务码 `400002` 时回落到 `agent-refresh`；网络失败、坏响应和不完整令牌对不再次提交。访问和刷新令牌均有效后才条件写回，不能沿用旧刷新令牌掩盖轮换结果缺失。
- 403 表示上游禁止访问，不单独证明凭证失效或区域错误；可能涉及模型权限、客户端版本或区域策略，不自动无差别续期，也不让整个账号进入鉴权失效冷却。连接检查失败返回真实 `upstream_status`，错误文案区分 401/403，不回显上游正文。
- 积分只展示上游钱包 `total_balance`，不会伪造套餐总量、已用量、固定奖励或到期时间。未知余额保留 Unknown；确认的零余额显示 0。
- 国内缺省签到使用 `/asset-operation/api/v1/checkin/card` 与 `/checkin/claim`，先核对当天 `biz_date/card_visible/claim_status`，一次领取携带一个 `client_request_id`，之后只读取卡片。`claimed_amount` 是实发奖励；部分成功、处理中和不确定结果保留 Unknown。国际保留旧 task 协议；`checkin_protocol:"task"` 可显式保留旧接口，失败不自动跨协议领取。
- 自动路由采用服务配置的费用层级、优先级及日预算。免费预设指账号奖励积分，实际可用量仍以上游钱包及当前活动规则为准；付费账号应修改费用层级。

## 配置与管理 API

`options` 支持 `region`（`cn` / `global`）、`client_version`（国内默认 `2.5.3`，国际保留 `2.4.0`）、`platform`（默认 `mac`）、`account_base` 与 `checkin_protocol`（`card` / `task`）。版本覆盖仍生效；签到协议由区域缺省或显式选项决定，不能仅修改版本头来切协议。自定义 `base_url` 为聊天及模型目录的裸域地址。客户端签名参数升级可通过 `FMP_AUTOGLM_APP_KEY` 设置。

2026-11-24 参考主源为官方国内 AutoClaw 2.5.3 安装包的 `dist/main.cjs`，
SHA-256 `3ab7c34676fe2527627fc27f30362b2b330513828a8b02781293aac34ea9ea79`。
函数 `Obt`、`RLu/XPa`、`_0n/wFu` 分别支持签到、倍率和刷新修正。
这是单一官方实现来源，不是线上验证；国际包内部协议、模型 `api/gates`、
managed session/billing 事件和普通推理最终 body 仍有证据缺口。

网页授权先完成上游启用的安全验证，再获取授权地址。前端遵循[阿里云 V2 接入契约](https://help.aliyun.com/zh/captcha/web-and-h5-client-v2-architecture-access)，每次授权用独立页面初始化 SDK，回执只在当前页面与代理间传递。

国内短信协议使用官方客户端的 `/userapi/v1/agent-send-code-captcha-config`、`agent-send-code` 与无尾斜杠的 `agent-login`；验证码按数值提交。验证配置接受明确关闭或阿里云 `cn` / `sgp` 区域配置。

| API | 功能 |
|---|---|
| `POST /api/oauth/autoglm/captcha-config` | 海外 `{backend_id}`，返回公开的官方验证配置和此次设备 UUID |
| `POST /api/oauth/autoglm/start` | 国内 `{backend_id}` 返回 `kind=sms_browser`、同源 URL；海外提供 `provider:"zai"\|"google"` 返回 `kind=oauth`、官方 URL。两者都返回独立 `session_id`、600 秒期限及 `automatic_account_save=true` |
| `POST /api/oauth/autoglm/poll` | `{backend_id, session_id}`，幂等返回 pending / 已保存结果 |
| `POST /api/oauth/autoglm/cancel` | `{backend_id, session_id}`，取消短信会话或释放海外回调端口 |
| `GET /auth/autoglm/{session_id}` | 独立国内短信页面，不携带管理密钥 |
| `POST /auth/autoglm/{session_id}/captcha-config` | 同源读取公开的官方短信验证配置 |
| `POST /auth/autoglm/{session_id}/send-code` | 同源提交 `{phone, ali_captcha_verify_param?}`，明确成功后绑定手机号 |
| `POST /auth/autoglm/{session_id}/login` | 同源提交 `{phone, code:"短信验证码"}`，交换完整凭证并保存 |
| `POST /auth/autoglm/{session_id}/status` | 同源读取当前会话状态，不返回令牌 |
| `POST /auth/autoglm/{session_id}/cancel` | 同源取消当前会话 |
| `POST /api/accounts` | 手工导入账号凭证 |
| `POST /api/accounts/{id}/refresh` | 手工续期并写回 |
| `POST /api/backends/{id}/models/sync` | 同步真实模型目录 |
| `GET /api/quota` | 读取各账号余额，`remaining` 与套餐字段分开 |
| `POST /api/tasks/{backend_id}/checkin` | 为启用账号签到并记录 |

管理 API 使用已有管理密钥验证，账号列表及登录响应不返回令牌。

旧直接管理短信接口 `/api/oauth/autoglm/send-code`、`/api/oauth/autoglm/login` 保持关闭，短信操作统一经过独立网页会话。

## 验证

`tests/autoglm_flow.rs` 使用本地假上游验证启动不自动发码、客户端身份头、并发持久续期、401 重试、三协议工具调用与结果回传、目录去重、名称与倍率持久化、重同步移除旧优惠、多账号签到、未知结果不重放，以及零余额与未知余额的区别。它不访问真实厂商。

`tests/autoglm_oauth.rs` 使用假上游和真实本机回调验证多窗口与跨服务隔离、state 拒收、重复回调、稳定设备、后台自动落盘、幂等轮询、不完整登录态拒收、取消后的迟到结果以及删除/停用期间的条件保存。

`tests/autoglm_browser.rs` 使用假国内上游验证独立页面、完整短信协议、稳定设备、上游发码确认、安全验证、幂等保存、名称保留、同源与管理鉴权、区域边界及取消/配置变更后丢弃迟到结果。前端 Node 回归验证弹窗轮询、发码失败不解锁、手机号绑定、iframe 来源及保存/取消后停止请求。

真实账号的短信登录、海外授权、模型生成、工具能力和签到可用性，需要用户操作后验证。本地模拟通过不代表真实短信已发送，也不能保证厂商接口长期兼容。
