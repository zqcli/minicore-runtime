# CommandSurface 使用 UI-safe 命令结果表达用户可见反馈

## 状态

Accepted，已由 [ADR 0012](0012-command-manager-is-stateless-session-command-facade.md) 收窄安全边界。

## 决策

用户命令的接收确认和用户可见结果必须分离。`CommandAck` 只表示 `AgentRuntime.dispatch(AgentCommand)` 是否接收协议命令，不承载 `/status`、`/usage`、`/model` 或解析错误的业务结果。

用户可见结果通过 UI-safe 的 command output / command result event 表达；它可以描述文本、表格、状态摘要、候选项或需要进一步输入的语义，但不能携带完整 `AgentCommand`、内部 handler key、resource body、credentials 或 UI 组件实现。

## 背景

旧设计使用 `CommandPresentation`、`CommandOutputAction` 和 `UiInteractionRequest` 表达 message panel 输出、picker、menu 或 form。这一方向保留了“结果不进 `CommandAck`”的优点，但如果 presentation action 能携带完整 protocol command，就会形成绕过 parse/resolve/authorize 的第二执行通道。

## 后果

- UI 可以根据 command result 自行渲染 message panel、toast、modal、command palette 或嵌套菜单。
- UI-visible 对象不能携带完整 runtime mutation command；如果未来需要 action，必须是 runtime-owned opaque action id，并在 submit 时重新校验。
- GUI/TUI 选择 catalog item 后应提交 `ExecuteCatalogCommand` 或 `ExecuteCommandText`，由 `CommandManager.resolve_for_execution` 再次校验。
- 业务事实仍由原有 runtime events 表达，例如 `resources_changed`、`session_model_changed`、`skill_invoked` 和 `run_finished`。
