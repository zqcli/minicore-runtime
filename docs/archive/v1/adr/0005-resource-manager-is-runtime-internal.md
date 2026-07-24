# ResourceManager 是运行时内部资源子系统

> **归档（V1）**：本 ADR 属于 MiniCore V1 架构，仅作历史参考，不得作为当前实现或新开发的设计依据。当前权威决策见 `docs/adr/`（0100+）。原文保持历史原貌。

状态：已由 [ADR 0010](0010-use-per-cwd-resource-snapshots-for-multi-session-runtime.md) 细化。ADR 0010 定义了 runtime / cwd / turn / step 级联 snapshot，以及 cwd-over-runtime overlay policy。

我们保留显式 `ResourceManager`，并把它定义为 `AgentRuntime` 持有的内部资源子系统，而不是 `AgentRuntimeProtocol` 的公开数据结构。公开协议只暴露 `ReloadResources`、`resources_changed`、`RuntimeSnapshot` 中安全的资源摘要，以及受控的资源详情查询；它不能让 UI 提交完整 resource snapshot，也不能绕过来源信息、project trust、diagnostics、overlay policy 和原子发布语义。

这个决定参考 Codex 的 protocol/core 分层：protocol 负责提交、事件和 schema，执行环境、项目上下文和资源状态留在 core runtime 中。MiniCore 有更宽的 pi-like 产品资源面，包括 skills、prompt templates、context files、custom system prompt 和 append system prompt，因此资源来源解析和提示词素材生命周期需要一个独立子系统承载。extension/package/MCP resource discovery 不属于当前决策范围；如果后续引入，应通过新的 ADR 单独定义安全边界。

ADR 0010 进一步细化了这个模块边界：`ResourceManager` 拥有 `RuntimeResourceSnapshot`、`CwdResourceSnapshot`、`TurnResourceSnapshot` capture、预留的 `StepResourceSnapshot` 类型、`ResourceSnapshotStore` 和 `ResourceOverlayPolicy`。ADR 0017 再明确：`SessionRuntime` 从 captured snapshot 取得 `PromptResourceView`，`Prompt` 负责 intent/system/context 的确定性组装，但复用 ResourceManager canonical resource identity，不拥有 reload、overlay 或 catalog lifecycle；UI 和 protocol 只能看到摘要或受控 detail query 结果。
