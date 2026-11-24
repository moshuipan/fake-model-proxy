# TRAE 个人版接入

FMP 的 TRAE 后端接入范围限定为面向公众的个人账号，包括 TRAE CN / TraeWork CN。接入使用账号本人的个人版授权和模型权限。

控制台提供 **TRAE CN（个人版）** 预设，后端类型为 `trae_personal`。授权地址固定为 `https://www.trae.cn/authorization`，换票使用 `https://api.trae.cn`，模型调用使用个人版 `https://trae-api-cn.mchost.guru`。授权与调用地址固定，不接受自定义授权服务器。当前仅支持个人 CN 区域，国际版未验证。

服务选项保留 `region` 字段，接受 `"cn"`、空字符串或省略，均使用个人版 CN
网关。旧配置中的 `{"region":"cn"}` 可继续保存和导入；其他区域、非字符串区域和
未知选项会被拒绝。配置保存校验全部服务，错误中的 `/services/<序号>` 是该次完整
配置的数组位置，可能指向未编辑的服务；未知选项的诊断路径会包含具体字段名。

服务可在 `options.client_device` 保存官方客户端的设备上下文。五个必填字段为
`device_id`（官方注册 ID）、`device_brand`（硬件型号）、`device_type`
（`mac` / `windows` / `linux`）、`os_version`（官方完整字符串）和 `app_version`
（`product.appVersion`）。它们分别用于签到和余额请求的 `x-device-id`、
`x-device-brand`、`x-device-type`、`x-os-version`、`x-app-version`，
并复用到网页登录 URL 的对应设备参数。

可选 `machine_id` 使用官方 `telemetry.machineId`，`plugin_version` 使用官方
`product.tronBuildVersion`，`app_type` 默认 `stable`。缺省机器 ID 为服务 ID
派生的稳定 FMP 标识，缺省构建号为 `local`；需要与官方一致时应显式填写。
一份服务代表一台客户端，同服务账号共享该物理设备；不同设备应使用不同服务。
账号凭证、余额和每日记录仍独立。设备信息不是 token，只经 ConfigService
的 validate / plan / apply 持久化，续期不改设备，修改设备或版本会重建连接，
旧实例和等待中的授权不能继续使用已变更的设备配置。

`ide_version` 独立保存官方 `product.version`，即原生 AI 扩展从
`vscode.version` 取得的 IDE 内核版本；它不是 `product.appVersion`。
模型目录和推理请求复用服务设备参数，并按官方扩展的 `encodeURIComponent`
规则编码型号、机器 ID、系统与 OS 参数。`X-Ide-Version` 使用已采集的
`ide_version`；旧设备报告缺此字段时保持未知，省略该头，不再固定填 `3.4.4`。
余额与签到仍发送原始 OS 字符串及 GUI 应用版本，不能混用原生头编码。

原生 API 的 `X-Ide-Version-Code` 来自官方 CN AI 扩展区域配置，当前已核对值为
`20260212`；`X-App-Version` 在该配置中为 `default`。它们不是客户端发布日期、
GUI 版本或构建号，不按日期或应用版本猜测替换。设备采集同时读取
`product.appVersion`（GUI 版本）、`product.version`（IDE 内核）和
`product.tronBuildVersion`（构建号）。客户端升级后重新采集并保存。

未配置 `client_device` 的旧服务仍可使用，但授权和个人 API 使用未注册回退值
`0`，省略未知品牌、OS 和应用版本，不再伪造账号哈希设备或固定样例硬件。
FMP 不在启动或保存配置时读取本机客户端；用户点击采集按钮才执行只读采集。
FMP 不自动注册设备，也不轮换设备绕过签到限制。
设备 ID 应从账号本人使用的官方客户端注册结果获取，不能把 `machineId`、
本地 `aha-...` 备用 ID 或随机数字当成已注册 ID。

控制台添加或编辑 TRAE 个人版服务时，「官方客户端设备信息」区域提供：

- **自动获取本地**：读取 FMP 进程所在机器、当前运行用户的官方 CN 客户端；
  浏览器与 FMP 不在同一机器时，按钮仍读取 FMP 主机。需要 Python 3.9+
  （Windows 使用 `py -3`）。识别运行中进程的 `--user-data-dir` 及标准用户目录；
  多个完整候选由用户选择，不按目录顺序自动覆盖。
- **下载采集脚本**：下载仓库的 [collect-trae-device.py](../../scripts/collect-trae-device.py)。
  该脚本只用 Python 标准库，在官方客户端机器运行：
  `python3 collect-trae-device.py --output trae-device.json`；
  Windows 可使用 `py -3 collect-trae-device.py --output trae-device.json`。
  非默认安装或未运行的自定义用户目录可显式指定
  `--profile "/path/to/user-data-dir" --app "/path/to/application"`；
  `--app` 接受应用包、安装目录、`resources/app` 或 `product.json`。
- **导入 JSON 文件 / 粘贴 JSON 手动导入**：接受脚本报告或完整 `client_device` 对象，
  服务端沿用配置入口的同一设备校验，填入当前表单后仍需预览并应用变更。
  其余服务选项、策略和凭证引用保留；采集和导入不会保存配置、授权或签到。

采集器只提取官方主进程启动日志中的 `ICDRS rdid` 注册结果、
`telemetry.machineId`、安装目录的 `product.appVersion` /
`product.tronBuildVersion` / `product.quality`，及本机型号和 OS。
输出按字段白名单构造，不导出登录令牌、账户信息或请求正文，也不访问网络。
先在该机器启动官方客户端并联网完成设备注册；缺少注册 ID、型号或版本时，
报告列出缺项，FMP 不生成假值。客户端或系统升级后需要重新采集并保存版本。
Windows/Linux 的平台解析使用模拟系统信息进行隔离测试，仍需对应机器实测。
采集器回归可运行 `python3 -m unittest discover -s scripts -p 'test_*.py' -v`，
只使用临时目录和模拟系统数据，不读取真实账号。

插件目录的 `ui.config_helpers` 声明采集、校验、脚本路径及报告 schema。
采集入口 `POST /api/plugins/trae_personal/device/capture` 要求管理身份的
`operation` 权限，导入校验入口 `POST /api/plugins/trae_personal/device/validate`
要求 `read` 权限；保存仍要求配置写入权限。公开脚本下载
`GET /api/plugins/trae_personal/device/collect.py` 不包含任何主机信息。

TRAE 客户端通过 FMP 的 `/clients/trae/v1` 地址调用网关，
客户端会话关联与上游服务插件分别配置。

客户端未提供明确会话 ID 时，FMP 用已交付历史及输出摘要识别续轮，并复用对应的
Auto 账号和模型绑定。流式客户端读到 `[DONE]`、`message_stop`、
`response.completed` 或 `response.incomplete` 后停止读取，仍视为完成；
中途取消或流中失败不保存续轮证据。TRAE 的 `<user_input>` 包装只用于摘要归一，
请求原文照常转发。

这种内容推断要求历史能唯一匹配；相同首轮的新会话保持独立，歧义时拆组，
历史压缩且无完整工具锚点时无法保证识别。索引和绑定仅在内存，FMP 重启后重新建立。
检查请求记录中的上一轮 `outcome` 与下一轮 `session.source` 可区分完成、取消、
新组和历史续轮；明确 `X-FMP-Session` 可避免依赖内容推断。

TRAE 通过 FMP Auto 使用截图工具时，三种协议均保留工具结果中的图片，
按图片能力和每图容量预留进行准入。Responses 的结构化工具输出不把
base64 转成普通文本；Messages 的多个工具结果分别保留调用 ID。
具体上游是否接受 tool 角色图文数组仍取决于供应商协议，假上游回归不证明真实模型支持。

TRAE Responses 的同轮多个函数调用及正文归一成一条 assistant 消息，
随后发送该批次的全部工具结果。Messages 同条消息中的多个结果连续发送，
普通正文保留相对次序并置于结果之后。三协议统一检查历史关联，
调用/结果缺失、重复或错配时在本地返回 `invalid_tool_history` HTTP 400。
客户端裁剪历史应保留完整工具批次；仅保留结果或插入正文会破坏关联。
上游 `tool call result does not follow tool call (2013)` 应先检查这项结构，
不能用请求体上限或 QPS 设置解释。

“请求失败，请创建新会话后重试”本身不能判定来源。排查应提供发生故障的
FMP 地址、失败时间或 `x-request-id`，检查请求详情中的已读字节/字节上限、
图片数、`routing_rejections`、上游状态和终止原因。本地 413、Auto 上下文拒收、
上游 429、流中错误和客户端取消需要分别确认；没有对应请求记录时继续检查
客户端日志及 FMP 前面的网络入口。最新源码的入站上限默认 64 MiB，
实际运行版本与 `FMP_MAX_REQUEST_BODY_MB` 仍以该部署为准。

个人版接入流程：

1. 在控制台添加 **TRAE CN（个人版）** 服务并保存，点击「登录账号」。官方验证页面在新标签页打开，同时保留可手动打开的链接。
2. 用户完成自己的个人账号登录，授权回调绑定此次流程；服务端保存个人版凭证及续期所需字段。
3. 同步该账号实际可用的模型、能力与 Token 限制。
4. 使用 FMP 的三种对话协议调用；工具由调用方执行，结果随历史回传。
5. 账号过期前自动续期，管理页面也支持「刷新凭证」。一次刷新结果未知时保留持久围栏，要求重新登录，不重复使用旧刷新票据。

网页登录使用临时本机 HTTP 回调，浏览器与 FMP 需运行在同一台电脑。官网要求固定 `/authorize` 路径，每次授权通过官网回传的随机 `loginTraceID` 校验会话，十分钟后过期；取消、停用或修改服务会结束等待。授权成功后账号与目录自动保存，轮询结果不返回令牌。

个人账号通过网页登录接入。设备采集仅导出元数据，不导入账号登录态。

动态目录只保留启用且可见的 `chat_completion` 模型，排除标题、摘要、补全和自定义占位配置。原生模型标识与 `config_name` 成对保存；输入上限和输出上限分别展示，总上下文为两者之和。目录按个人账号缓存，不复用其他账号的权限。

支持 Chat Completions、Messages、Responses 的流式和非流式入口，原生文本、思考输出、工具调用和工具历史回传共用事件契约。工具 schema 和选择策略按个人 raw-chat 要求编码；工具由调用客户端执行。当前未验证图片和 `reasoning_effort` 控制，显式请求会返回不支持。

个人版支持手动签到、一键签到和每日自动签到。任务页开启自动签到后，复用全局时间与随机窗口，为每个启用账号按北京时间自然日执行；服务需要保持运行。每次先从个人版 `https://api.trae.cn/trae/api/v2/ug/checkin_credits/status` 查询，账号未签到且活动开放时才调用同路径下的 `claim`。
两个接口均发送 `{"req_source":1}`，与本机官方客户端的 native 请求来源一致。

同一账号当天已完成或结果未知的任务不会重复提交，记录跨服务重启保留。设备上下文来自服务配置，续期和重启不会轮换。明确 401 最多续期重试一次；限流 `9074` 停止领取，设备已领取 `9095` 必须再次查询本账号状态，不能直接记成账号已完成。领取超时、服务端错误或无效响应记录为「结果未知」，可在个人客户端核对。
业务拒绝消息区分“状态查询”和“积分领取”，任务及操作结果保留这一阶段信息，
不记录原始响应、令牌或设备标识。`9074` 的限流文案是本地映射；
仅据该码不能确认官方限制针对账号、设备、IP 或具体冷却时长。
设备配置用于对齐官方注册设备、品牌、OS 和应用版本。补齐设备字段与
`req_source` 不能保证解除限流；领取被拒绝时，应在官方客户端核对账号状态。
FMP 的换票接口、设备证明和 HTTP 传输实现可能与当前官方客户端不同，
不承诺完整复现客户端协议。

额度查询使用个人版 `https://api.trae.cn/trae/api/v2/pay/ide_user_ent_usage`，请求通用 IDE 积分（`require_usage: true`、`req_source: 1`）。直接读取官方 `total_balance`，保留十进制小数；账号列表、服务卡片和路由使用同一余额快照。支持手动更新和服务端自动更新，签到完成后查询当前账号余额。查询与签到共用账号续期和固定设备标识，明确 401 最多续期重试一次。

非积分计费账号返回「未知」；缺少余额、无效金额、业务错误或查询失败均不补零。积分包金额和汇总可能有舍入差异，不用积分包合计代替官方余额，也不从累计用量推算套餐总量和重置时间。模型扣费规则尚未验证。

已签到查询返回的积分面额不计为本次新增奖励；只有领取成功响应明确给出的积分才记录奖励数量，缺少数量时保持未知。不从 Token 数或签到面额推测可用余额。模型权限、奖励和套餐额度以个人版当前接口及官方政策为准。
