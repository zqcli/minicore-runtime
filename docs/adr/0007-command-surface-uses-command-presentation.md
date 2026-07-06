# CommandSurface 使用 CommandPresentation 呈现用户可见结果

CommandSurface 采用 `Parse → Plan → Execute → Present` 四阶段：解析和规划由 `CommandSurface` 完成，真实执行交给 `AgentRuntime`、`SessionRuntime`、`ResourceManager`、`SessionManager` 等后端承接者，用户可见结果再被归一成 `CommandPresentation`。我们不把 `/status`、`/usage`、`/model` picker 或解析错误塞进 `CommandAck`，也不让 UI 自己解释 runtime snapshot；这些结果通过 `command_output_appended`、`command_interaction_requested` 等事件进入 message panel、popup、picker、menu 或 form。

这个决定使下游 CLI、TUI 和 GUI 的 composer / command palette 共享同一套命令语义和结果表达，同时保持 `CommandAck` 只表示协议命令是否被接收，业务事实仍由原有 runtime events 和 snapshot 负责。
