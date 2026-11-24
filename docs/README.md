# 文档索引

首次接入从 [项目 README](../README.md)开始。当前行为以源码、生成契约和以下
使用指南为准，历史 ADR 用于记录设计背景。

## 技术设计

- [当前架构](design/architecture.md)：模块、能力、配置与运行状态的边界。
- [Auto 路由](design/auto-routing.md)：准入、费用池、调度、绑定和安全恢复。
- [客户端会话关联](design/client-sessions.md)：会话证据、历史推断和请求记录。
- [ADR 0001](adr/0001-original-architecture-superseded.md)：初始架构提案及取舍。

## 使用与开发指南

- [服务启动配置](guides/configuration.md)
- [管理 API、CLI 与 MCP](guides/management.md)
- [模型调用名称](guides/model-names.md)
- [插件开发](guides/plugins.md)
- [生成契约](generated/README.md)
- [Windows 打包](../packaging/windows/BUILD.md)
- [贡献流程](../CONTRIBUTING.md)

## 提供商接入

| 提供商 | 文档 |
| --- | --- |
| OpenAI 兼容网关 | [openai-compat](providers/openai-compat.md) |
| ModelScope 魔搭 | [modelscope](providers/modelscope.md) |
| Qoder | [qoder](providers/qoder.md) |
| WorkBuddy | [workbuddy](providers/workbuddy.md) |
| AutoGLM | [autoglm](providers/autoglm.md) |
| MiniMax Code | [minimax-code](providers/minimax-code.md) |
| MonkeyCode | [monkeycode](providers/monkeycode.md) |
| 小浣熊 | [raccoon](providers/raccoon.md) |
| TRAE CN 个人版 | [trae](providers/trae.md) |

文档使用公开协议说明和合成示例，不保存账号、设备报告、私人工作内容或本机实验记录。
