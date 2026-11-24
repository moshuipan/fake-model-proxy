# OpenAI 兼容服务插件

`openai_compat` 提供 `/chat/completions` 传输和模型目录，复用三个入口的公共协议转换。支持额外请求头、聊天/模型目录路径及显式能力配置。完整字段从 `fmp plugins describe openai_compat` 获取。

普通网关差异使用预设与服务配置，不新增代码插件。地址和模型需要用户确认；兼容协议不证明免费、额度查询、续期、图片或工具权限。费用未知保持 `unknown`，未验证能力不扩大准入。

API 密钥和敏感头通过 `credentials.api_key`、`credentials.headers` 引用服务器 `env:` 或 `store:` 秘密。配置保存走 [管理计划](./management.md)，不要把秘密写入预设或 options 明文头。

通用兼容插件没有专属授权、额度、续期或签到能力；需要这些能力时实现独立服务插件，见 [插件开发](./plugins.md)。
