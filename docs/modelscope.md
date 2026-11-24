# ModelScope 魔搭

控制台「服务接入 → 添加服务 → ModelScope 魔搭」使用官方 OpenAI 兼容 API，
复用现有传输与协议转换。代理端支持 Chat Completions、Responses 和 Messages，
上游统一调用 `/v1/chat/completions`。

1. 在 [魔搭官网](https://modelscope.cn/my/myaccesstoken)登录，绑定已完成实名认证的阿里云账号。
2. 获取 Access Token，在服务的 API Key 字段填写。
3. 使用 Base URL `https://api-inference.modelscope.cn/v1`，保存并启用服务，点击「同步目录」。
4. 在「模型与调试」选择对话模型并试聊。上游模型 ID 保留组织前缀，例如 `Qwen/Qwen3.5-35B-A3B`；客户端使用控制台给出的调用名称。

模型白名单为空时从官方 `GET /v1/models` 发现目录。填写白名单后仅保留指定模型；
如需发现新增模型，先清空白名单再同步。同步还访问公开官方详情
`GET https://modelscope.cn/api/v1/models/{组织}/{模型}`：
`Tasks[].Name=image-text-to-text` 等视觉理解任务声明图片输入能力，
已知图像生成、编辑和视频生成等非聊天任务从目录排除。
详情请求不携带访问令牌，最多并发 8 个，共用 9 秒刷新预算，成功详情缓存五分钟；
详情暂时不可用时保留已知能力，新模型的未知能力不凭名称推断。
工具和并行工具只接受明确声明，不从聊天模板推断。
能力按模型分别保存，控制台目录和参数页可查看；高级配置仍可显式覆盖整个服务。

官方当前规则提供免费体验，调用额度和并发会动态调整，并支持魔粒兑换调用。
早期公告的「每日 2000 次」不作为当前账号额度。
当前公开推理目录只返回模型 ID 等信息，模型详情可补充任务能力，但没有可靠的上下文窗口或输出上限；
模型窗口和输出上限保持未知。本地「今日 Token」是代理侧用量，不是魔粒扣减或账号剩余额度。
未配置可信的上下文窗口时，Auto 会按现有保守规则排除对应候选；仍可显式选择模型试聊。
魔粒余额使用官方 `GET /openapi/v1/magicubes/balance`，展示 `available_balance`，
保留小数；总余额和冻结金额单独说明，不把总余额当套餐总量。服务端每十二小时更新，
可手动更新；每次每日任务后同步当前账号余额。路由快照有效期为 60 秒。

每日任务先查询 `GET /openapi/v1/magicubes/earn/rules`。今日两项奖励已发放则跳过登录；
否则使用官方 SDK 的 `POST /api/v1/login`（`AccessToken`），在本次 Cookie 会话读取
`/api/v1/users/login/info` 和 `/api/v1/users/binding/check`，再查询规则确认实际奖励。
当前每日活跃 200 魔粒，绑定阿里云后另有 50 魔粒；以 `today_earned_amount` 的变化为准。
这两项奖励从发放起有效 24 小时，其他长期魔粒当前有效 90 天。
未绑定阿里云时只确认每日活跃奖励；缺失状态、跨北京时间日期、令牌变化或无法确认的结果
记为未知，禁止本周期自动重放。已领取数量只在消息中说明，不伪装成本次新增奖励。

控制台支持单服务执行、批量执行和现有每日调度。自动签到是否启用沿用原设置。
访问令牌仅存于服务 API Key；账号表自动管理不含令牌的指纹引用，Cookie 不落盘。
旧官方 `openai_compat` 配置在重载时迁移为 `modelscope`，保留模型、别名和其他配置；
首次升级会清除旧控制台生成的三项皆 false 的默认能力覆盖（空格和键顺序不影响识别）。
其他自定义覆盖保留；升级后新设置的覆盖不再被清除。
替换令牌后更换引用账号，旧余额和领取记录不会带到新身份。

核对来源（2026-11-21）：

- [官方 API 推理介绍](https://www.modelscope.cn/docs/model-service/API-Inference/intro)
- [官方使用限制](https://www.modelscope.cn/docs/model-service/API-Inference/limits)
- [官方实时模型目录](https://api-inference.modelscope.cn/v1/models)
- [官方模型任务详情示例](https://modelscope.cn/api/v1/models/Qwen/Qwen3.5-35B-A3B)
- [官方魔粒规则](https://www.modelscope.cn/docs/magicube/intro)
- [官方 SDK 登录实现](https://github.com/modelscope/modelscope/blob/v1.31.0/modelscope/hub/api.py)

真实对话验证需要有效 Access Token 和满足官方账号条件；匿名目录读取成功不代表账号已获推理权限。
