# OpenAI 兼容服务插件

`openai_compat` 提供 `/chat/completions` 传输和模型目录，复用三个入口的公共协议转换。支持额外请求头、聊天/模型目录路径及显式能力配置。完整字段从 `fmp plugins describe openai_compat` 获取。

普通网关差异使用预设与服务配置，不新增代码插件。地址和模型需要用户确认；兼容协议不证明免费、额度查询、续期、图片或工具权限。费用未知保持 `unknown`，未验证能力不扩大准入。

API 密钥和敏感头通过 `credentials.api_key`、`credentials.headers` 引用服务器 `env:` 或 `store:` 秘密。配置保存走 [管理计划](../guides/management.md)，不要把秘密写入预设或 options 明文头。

通用兼容插件没有专属授权、额度、续期或签到能力；需要这些能力时实现独立服务插件，见 [插件开发](../guides/plugins.md)。

`options.wire_profile` 可选 `compatible` 或 `openai`。官方 `api.openai.com`
缺省使用 `openai`，其他地址缺省使用 `compatible`。
原生模式保留 `developer` 角色，输出上限使用 `max_completion_tokens`；
兼容模式保留旧 `system/max_tokens` 映射，不按模型名称猜测协议。
同时传入两个输出上限且值不一致时返回参数错误。

当前统一请求没有 strict、结构化输出、并行工具控制和多候选的保真表示。
显式提供 `parallel_tool_calls`、`response_format`、函数 `strict: true`、
Responses 的非 text `text.format` 或 `n>1` 会返回
`unsupported_request_constraint`，不能接收后静默删除。
`strict: false` 与默认行为一致，视为未声明该约束。

SSE 按最早 LF/CRLF 分隔符读取，保留 DONE 前的用量尾帧。
OpenAI 的 `refusal` 正文暂以统一文本事件交付，避免空成功；
这一转换保留正文和终止原因，但不保留原生 refusal 字段类型。
参考 OpenAI SDK `7.62.0` 的官方 Chat 类型定义，静态契约验证不等于真实账号实测。
