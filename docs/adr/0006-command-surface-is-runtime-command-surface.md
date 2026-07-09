# CommandSurface 是跨 UI 的运行时命令入口

## 状态

Accepted，已由 [ADR 0012](0012-command-manager-is-stateless-session-command-facade.md) 收敛实现形态。

## 决策

MiniCore 需要跨 CLI、TUI 和 GUI 共享同一套用户命令语义。`CommandSurface` 作为领域总称保留，表示 runtime-owned command catalog、command text 解析、catalog selection 解析、动态命令节点和执行前校验边界。具体实现不再是一个有状态 `CommandSurfaceService`，而是共享无状态 `CommandManager` 加 `SessionRuntime` 持有的 session-scoped `Command` facade。

## 背景

pi 把 builtins、prompt templates、extension commands 和 `/skill:name` 合成当前 session 的 autocomplete，并在 session 层展开技能/模板；Codex 把 slash command 建模为带可用性和 inline-args 规则的 enum，再由 TUI dispatch 映射到 app events、用户消息或 runtime actions。

MiniCore 会被多个 UI 宿主复用。如果让各宿主自己解析 `/compact`、`/skill`、`/{template}`、`/model` 或后续配置命令，名称冲突、资源 revision、运行中可用性、结果呈现和权限边界会漂移。因此 command catalog、动态节点、parse/suggest/resolve 和 handler binding 必须在 runtime core 中统一。

## 后果

- UI 可以渲染 slash autocomplete、command palette、嵌套菜单和 picker，但不拥有 command 的权威解析或执行授权。
- `slash command` 降级为 command text 的一种输入语法；同一 command 也可以来自 GUI catalog selection。
- `CommandManager` 不读资源正文、不执行工具、不调用模型、不持有 UI 状态。
- Skill、prompt template、model thinking levels 等动态来源通过 provider materialize 成 command nodes；执行时仍回到对应 owner。
