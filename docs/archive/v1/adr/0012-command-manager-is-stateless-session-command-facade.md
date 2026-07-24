# 命令体系使用无状态 CommandManager 和 session-scoped Command

> **归档（V1）**：本 ADR 属于 MiniCore V1 架构，仅作历史参考，不得作为当前实现或新开发的设计依据。当前权威决策见 `docs/adr/`（0100+）。原文保持历史原貌。

## 状态

Partially superseded by [ADR 0028](0028-runtime-protocol-uses-scoped-state-cursors.md)。Stateless CommandManager、explicit SessionId和执行前resolve保留；SessionRuntime-held长期Command facade被Runtime per-call CommandContext替代。

> 以下正文保留原始历史决定。`WorkspaceServices` ownership、SessionRuntime-held Command facade和AgentRuntimeProtocol命名不再指导目标实现。

## 背景

MiniCore 需要同时服务 TUI slash input、GUI command palette、菜单项和未来快捷入口。旧设计把这组能力统一称为 `CommandSurfaceService`，并倾向把 catalog、parse、plan、interaction、presentation 和 execution routing 放在同一个 runtime service 中；同时协议层使用 `agent_runtime_protocol::Command` 作为外部命令枚举，容易与命令子系统本身的 `Command` 混淆。

随着设计推进，命令来源不再只有固定 builtin：skill、prompt template、model provider/model、thinking level、tools 和 session list 都可能来自当前 session 的动态状态或 resource snapshot。命令也不应被限制为一级 command + 二级 subcommand；它需要递归 command tree，并允许动态 children。与此同时，用户明确希望 command 子系统无状态，避免维护 current catalog、pending menu 或 UI interaction state。

## 决策

MiniCore 将 `CommandSurface` 保留为领域总称，但实现拆成两层：

- `CommandManager` 是 `WorkspaceServices` 持有的共享、无状态命令管理器。它持有只读 command packs、candidate provider registry 和 handler registry；每次 `catalog`、`suggest`、`execute` 都基于调用方传入的 `CommandContext` 临时 materialize catalog，并执行 parse / suggest / resolve。
- `Command` 是 `SessionRuntime` 持有的 session-scoped 命令入口。它不缓存 catalog，只从当前 session 组装 `CommandContext` 和 `SessionCommandHost`，再调用共享 `CommandManager`。

协议层将原 `agent_runtime_protocol::Command` 改名为 `agent_runtime_protocol::AgentCommand`，表示下游 adapter 发给 `AgentRuntime` 的协议级用户意图。`Command` 这个短名留给 command 子系统入口。

命令定义使用多层 JSON manifest 作为 authoring format，内部临时 normalize 成 flat/indexed `MaterializedCommandCatalog`。Manifest 使用 `CommandNode` / `CommandPath` / `children` 表达任意深度命令树，使用 `dynamic.provider + nodeTemplate` 把动态候选投影成节点。`subcommand` 不作为核心协议名。

UI-visible catalog、suggestion、interaction 和 output 不能携带完整 `AgentCommand`、internal handler key、resource body 或 credentials。UI 选择 catalog item 后只能提交 `ExecuteCommandText` 或 `ExecuteCatalogCommand`；runtime 必须重新 materialize 当前 catalog 并执行 `resolve_for_execution`。

## 影响

- `SessionRuntime` 增加 session-scoped `command: Command` 子系统，与 `Tools`、`Driver` 并列。
- `WorkspaceServices` 持有共享 `CommandManager`，不是有状态 `CommandSurfaceService`。
- `AgentRuntimeProtocol` 使用 `AgentCommand`，并引入 `ExecuteCommandText` / `ExecuteCatalogCommand`。高权限内部 mutation 移出公开协议。
- skill 是动态 command node，不只是参数补全；执行时仍由 `SessionRuntime` 捕获 `TurnResourceSnapshot` 后读取 skill body。
- builtin command 也可以动态，例如当前模型支持的 thinking levels。
- handler 逻辑放在 `src/command/handlers/`；trait 和 registry 放在 `src/command/handler.rs`。

## 后果

优点：

- TUI 和 GUI 共享 command 语义，同时 UI 不拥有执行授权。
- `CommandManager` 无状态，避免 catalog cache、UI menu state 和 session state 混在一起。
- 任意深度 command path 和动态 children 统一表达 skill、model、tools、sessions 等来源。
- `AgentCommand` 与 `command::Command` 命名边界清晰。

代价：

- 每次 command query / execute 需要临时 materialize catalog；未来如有性能压力，需要在外层按 revision 做 memoization，但不能让 `CommandManager` 持有业务状态。
- Dynamic provider 和 handler binding 必须有严格 UI-safe / trust / capability 校验。
- 旧的 `CommandPresentation` action 模型需要收窄，不能再携带完整 protocol command。
