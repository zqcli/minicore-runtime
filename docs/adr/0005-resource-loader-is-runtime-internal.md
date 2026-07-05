# ResourceLoader 是运行时内部资源服务

我们保留显式 `ResourceLoader`，并把它定义为 `AgentRuntime` 持有的内部 runtime service，而不是 `AgentRuntimeProtocol` 的一组公开数据类型。公开协议只提供 `ReloadResources`、`resources_changed`、snapshot resource summaries 和后续受控 detail query；它不能让 UI 直接提交完整 `RuntimeResources` 或绕过 source info、project trust、diagnostics 和 atomic reload 语义。

这个决定参考 Codex 的 protocol/core 分层：protocol 负责 submission/event/schema，真实执行环境和项目上下文留在 core runtime。由于本项目有 pi-like 的 skills、prompt templates、context files、custom system prompt、append system prompt 和未来 extension/package/MCP resource discovery，资源来源聚合和提示词素材生命周期需要独立服务承载；否则职责会散落到 `AgentRuntime`、`SessionRuntime`、`Prompt` 或 UI adapter 中。