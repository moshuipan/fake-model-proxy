# WorkBuddy 模型目录与积分倍率

## 客户端协议基线

2026-11-24 对照官方 CN 桌面 `5.11.3.43806934-ecace0f7`：
GUI 版本为 `5.11.3`，包内 CLI 为 `2.176.0`，授权 platform 为 `workbuddy`。
这些值分别来自 `package.json`、`cli/package.json`、`cli/product.json`，不能互换。
官方 DMG SHA-256 为 `f3b99997b0aebfe22af4f8c0070b37946b9f839b51cf9510eb6a97c9e45aa639`。

目录 `/v3/config` 单独发送 `X-Product=SaaS`；可用 `options.deployment_type` 覆盖，
推理的 `X-Product=WorkBuddy` 保持独立。授权请求和账号请求的 `X-Domain`
缺省取对应区域的 authority，明确账号 domain 优先。

推理分别发送会话、用户提交、模型请求 ID。会话级 ID 按账号与下游 session 维护，
空闲 30 分钟后轮换；轮次级 ID 在一次 user turn 内稳定，空闲 10 分钟后轮换；
消息级 ID 为时间有序的 UUIDv7。对话请求不带浏览器 `Origin`/`Referer`，并补齐
`X-Agent-Intent=craft`、`X-Agent-Type=main`、`acp-connection-id` 以及共享同一
traceId 的 `traceparent`、B3 与 `X-Trace-ID`；trace 每次请求新建，不冒充真实链路。
`options.private_data` 缺省为 true，通过 `X-Private-Data=true` 表达禁用模型优化。

以上是单一官方包的静态对照。授权轮询的分阶段恢复、刷新后的账号核对、
`X-Client-*`、runtime、产品 feature gate 和 Turing 设备证明尚未完整复现；
`acp-connection-id` 是按会话生成的声明值，未执行真实 ACP connect。
上游 11128 渠道校验仍可能随机拦截，普通元数据修正不能代替官方渠道授权。

## 每日签到

2026-11-19 已修正真实签到信封解析：`checkin-activity-status` 返回
`{code:0, data:{active, today_checked_in, ...}}`。状态位于 `data` 内，
此前从顶层读取时会把可签到账号误判成无活动。

活动开启且今天未签到时调用 `daily-checkin`；已签到显示「今日已签到」，
活动关闭显示「当前签到活动未开放」，企业账号跳过个人活动。
HTTP 与业务码均校验，缺少状态字段报协议错误，不默认当作无活动。
奖励读取成功响应的 `data.credit`，支持数字和数字字符串；缺失时只查询签到状态确认，
奖励保持未知，不估算为零。

每次任务只执行对应账号。手动触发与调度器共享互斥，每次先查询上游当天状态，
已签到不会重复提交；上游新日活动不会被代理日期缓存阻挡。
领取已提交但结果不确定时记录「未知」，不自动重试。
重复查询已签到时保留本周期已有的实发奖励记录。

## 模型目录与倍率

在服务详情点击「同步目录」，从官方 `/v3/config` 保存可见模型、名称与积分倍率。模型列表、试聊下拉和详情展示官方名称，外部调用与复制使用持久调用名称，例如 `workbuddy-glm-5.3`。原始模型 ID 保留在上游请求、白名单和请求证据中。

目录按 craft / default / cli agent 的可用模型过滤，再与 `availableModels` 求交集；隐藏、禁用、本地、自定义 URL 与图片或视频专用模型不会进入聊天白名单。

`models[].credits` 中明确的 `x0.69`、`0.08x` 等值作为积分消耗倍率，零倍率保留为 `0×`。未提供倍率或字段无法识别时显示「倍率未知」，裸积分金额不被误当倍率。

`modelPromotions` 中仅应用匹配模型、启用且当前有效的折扣。优先采用 `discount.discountedCredits`，否则将基本倍率乘以 `discount.factor`；优惠前倍率同步保存，高于当前倍率时划线展示。只有徽标而没有折扣的活动不影响报价。

活动有效期按 RFC3339 起止时间判断，每日跨午夜时间窗按声明时区判断。当前支持上海、香港、新加坡、东京与 UTC；未知时区或周规则不能据此确认折扣。上游提供的价格优先于本地按折扣比例计算的结果。

缓存同时记录最早价格变化边界 `price_valid_until`（Unix 秒），包括优惠开始、结束及每日时间窗切换。到达边界后，页面显示「倍率待刷新」并移除旧优惠划线；返回页面也会重新检查。点击「同步目录」确认新倍率，页面定时器不会自动向厂商发起请求。

名称、当前倍率、优惠前倍率和有效期整体保存到 SQLite，服务重新装配后仍可用于控制台展示。积分倍率是目录快照，实际扣费以上游为准；现有自动路由仍按服务配置的费用层级排序。
