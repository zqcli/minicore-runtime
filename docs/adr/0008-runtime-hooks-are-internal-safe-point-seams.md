# RuntimeHooks 是内部安全点扩展缝，不是协议事件或 UI 插件 API

MiniCore 需要 hook seam，因为它是 Agent harness runtime core，而不是单一产品后端。prompt/context 注入、工具策略、压缩、保存点同步和命令呈现都需要可测试、可扩展的干预点；如果没有 hook，下游产品只能 fork runtime，或绕过 `AgentRuntime` / `SessionRuntime` 直接操作工具、凭据和会话文件。当前设计不把资源 discovery/reload 做成 hook seam，资源更新必须经过 `ResourceManager` 的 ensure/reload/recompose pipeline。

我们决定把 `RuntimeHooks` 设计成内部安全点扩展系统。Hook handler 只能在明确 hook point 上运行，只能返回 typed decision / patch / replacement，例如 cancel、deny、patch system prompt、transform context、rewrite tool args、provide compaction result 或 patch command presentation。最终状态变化仍由拥有状态机的 `AgentRuntime` / `SessionRuntime` 应用；UI 可见事实仍由它们归约成 `agent_runtime_protocol::Event`。

这意味着 hook 不是 `AgentRuntimeProtocol`，不是 UI plugin API，也不是 event stream。Hook 不得直接发布 `agent_runtime_protocol::Event`，不得分配 `event_id` / `sequence`，不得直接读写 `SessionStorage`，不得直接执行 `ToolExecutor`，不得读取 raw credentials，也不得绕过 `ResourceManager` 的 trust / diagnostics / source info。工具参数 rewrite 后必须重新 schema validate 并重新走 tool policy；provider payload、context replacement 和 system prompt replacement 这类能力必须是 privileged capability。

结果是：MiniCore 保留 pi coding-agent `ExtensionRunner` 的生产经验，例如 `before_agent_start`、`context`、provider hooks、tool hooks、session compaction/tree hooks 和 message/session observers，但把边界收紧为 Rust typed hooks、capability gate、timeout/cancellation 和 failure policy。资源 discovery/reload hook 不属于当前设计；后续如果需要 extension/package 资源声明，应另起 ADR。下游 CLI/TUI/GUI 仍只消费 `CommandAck`、`agent_runtime_protocol::Event` 和 `RuntimeSnapshot`；hook 影响只能通过最终事实体现，例如 tool denied、message patched、compaction cancelled、command output patched 或 diagnostics updated。
