# 模块总览

本目录按可实现的 MiniCore runtime 模块组织文档。每个 md 对应一个需要编写的功能集合，描述它的 seam、interface、职责和不应承担的事情。下游 CLI、TUI、GUI 仓库通过 `AgentRuntimeProtocol` 接入这些模块，但不在本目录实现完整产品 UI。

## 分层图

```text
Downstream CLI     Downstream TUI     Downstream GUI
      │                  │                  │
      ▼                  ▼                  ▼
CLI Adapter       Ratatui Adapter     Tauri/Vue Adapter
      │                  │                  │
      └────── AgentRuntimeProtocol ─────────┘
                         │
                         ▼
                    AgentRuntime
                         │
                         ▼
                 WorkspaceServices
      ┌──────────────┬──────────────┬──────────────┬──────────────┐
      ▼              ▼              ▼              ▼              ▼
SessionManager  ResourceManager  CommandManager Future RuntimeHooks  ModelGateway
      │              │                                           │
      │              ├─ ResourceSnapshotStore                    │
      │              └─ ResourceOverlayPolicy                    │
      ├──────┬──────┐                                            │
      ▼      ▼      ▼                                            ▼
LoadedSessionRuntimes  SessionHandle                       Rig providers
      │             │
      ▼             ▼
SessionRuntime  SessionStorage
      │
      ┌──────────────┬──────────────┬────────┬─────────────┬─────────────┬─────────────┐
      ▼              ▼              ▼        ▼             ▼             ▼             ▼
   Command         Skills         Prompt    Tools      Compaction     Driver      UsageStats
                                                                                         │
                                                                                         ▼
                                                                                        Rig
```

## 模块职责

`AgentRuntime` 是 MiniCore 的 UI 无关运行时门面。它管理工作区、`WorkspaceServices`、`ResourceManager`、事件通道、命令和查询路由，并向下游宿主暴露 `dispatch`、`query`、`subscribe`、`snapshot`。

`SessionManager` 是工作区内会话生命周期 facade。它协调持久化会话目录、`SessionHandle` / `SessionStorage` 和内部 `LoadedSessionRuntimes`；`LoadedSessionRuntimes` 只是已加载 `SessionRuntime` 的 map，不作为独立架构层。

`SessionRuntime` 是单会话产品级编排层。它管理阶段、当前 run、`PromptDelivery` admission、消息队列、结构化 `PendingSessionAction`、资源、模型状态、工具状态、pending stable batch drafts 和事件归约；后期只在自己拥有的安全点接入 Hook。每个 session 固定一个 workspace cwd；每次 run 启动时从 `ResourceManager` 捕获 `TurnResourceSnapshot` 进 `TurnState`，使任一 run 不受客户端 session selection 变化或资源 reload 影响。

`AgentRuntimeEvents` 是运行时事件生命周期模块。它定义 `agent_runtime_protocol::Event { ..., msg }`、事件命名、事件来源、started/delta/finished 配对、commit 后领域事实、重连和常见场景的事件顺序，供下游 UI reducer 消费。

`ResourceManager` 是运行时内部资源子系统，负责资源来源聚合、信任校验、原子刷新候选、级联 snapshot、cwd-over-runtime overlay policy、提示词素材和诊断。它维护 `RuntimeResourceSnapshot`、`CwdResourceSnapshot`、`TurnResourceSnapshot` capture，并预留 `StepResourceSnapshot`；在 skill 层面它决定 roots、trust、分层、overlay 和 current snapshot，但不解析 frontmatter、不拼 `<skill>` 消息。

`CommandSurface` 是跨 UI 的用户命令领域面；实现上拆为共享无状态 `CommandManager` 和 `SessionRuntime` 持有的 session-scoped `Command` facade。它把 `/compact`、`/skill name`、兼容 `/skill:name`、`/{template}`、`/model`、`/usage` 和后续扩展命令通过 nested JSON command tree、dynamic providers、parse/suggest/resolve 和 trusted handlers 映射到 `agent_runtime_protocol::AgentCommand`、受控 query 或 prompt-like 输入，但不直接执行工具或 Agent loop。

`RuntimeHooks` 是后期内部扩展点系统。当前 MVP 不实现 hook registry / hook invocation；文档只固定 hook/event 边界、capability、typed result 和 owner 分层。后期启用时，它在 runtime 安全点开放 typed decision / patch / replacement，让内置策略、测试 harness、可信 package 或 extension 影响 prompt、context、tools、compaction 和 UI-safe command result；当前设计不定义资源 discovery / reload hook。hook 不直接发布 UI event，不直接读写 session storage，也不直接执行工具。

`Skills` 是平级技能文件能力模块，对应未来的 `skills.rs`。它提供 `SkillMetadata` / `SkillResource` / `SkillCatalog` 数据结构，以及给定目录后的发现、解析、校验和格式化 helper；它不拥有资源生命周期或 overlay。显式技能调用由目标 `PromptTurn` 基于 captured `PromptResourceView` 展开。

`PromptTemplates` 是平级纯模板能力模块，对应未来的 `prompt_templates.rs`。它定义 template metadata/resource/catalog/invocation、frontmatter、参数解析和单次替换规则；ResourceManager 拥有生命周期，CommandManager 只消费 metadata，目标 `PromptTurn` 执行正文展开。

`Prompt` 是无状态提示词组装子系统，对应未来的 `prompt.rs` / `prompt/`。`SessionRuntime` 作为 Pull Master，把 captured `PromptResourceView` 与 tool/model/agent/environment/policy views 交给 `prompt::begin_turn(...)`；immutable `PromptTurn` 负责 intent 展开，并在每次模型调用前把 durable history、protected current input 和 typed transient context 投影为协议安全的 `ModelInputProjection`。它不是长期 `PromptManager` / `ContextManager`。

`Tools` 是 `SessionRuntime` 内部的 session-scoped 工具子系统，对应未来的 `tools.rs` / `tools/`。它封装工具定义、registry、active tools、prompt catalog、policy、approval、grants、execution coordination、sandbox、mutation locks 和 executor implementations；`SessionRuntime` 协调 `DriverHost::invoke_tool_batch(...)` 与 `Tools::invoke_batch(...)`，`Driver` 不直接依赖 `Tools`。

`Compaction` 是平级压缩能力模块，对应未来的 `compaction.rs`。它提供上下文 token 估算、压缩触发判断、cut point 选择、`CompactionSummaryMaterial` 构建、摘要消息格式化和压缩结果类型；它不构造 `ModelCallRequest`。压缩流程、模型调用、事件和 session 写入由 `SessionRuntime` 编排，后期压缩 Hook 也由 `SessionRuntime` 在对应安全点接入。

`UsageStats` 是 token 消耗和上下文占用统计模块。它区分模型调用消耗、run 汇总、会话累计 stats 和当前 context usage；provider usage 归一化、本地估算、UI view 口径和压缩阈值计算都在这里统一说明。

`ModelGateway` 是模型调用治理模块。它复用 Rig provider/client 能力，但在 MiniCore 内负责 provider/model 解析、凭据解析、custom base URL、fallback、usage 归一化和错误分类。后期 provider hook 的 owner 也是 `ModelGateway`。实现顺序上先提供最小稳定 spine，供真实 `Driver` 集成复用；完整 custom provider、fallback 和 usage/context usage 后续扩展。

`Driver` 只负责适配 Rig。Rig 决定 `CallModel`、`CallTools` 和 `Done`；`Driver` 把这些 step 接到产品运行时的 provider、`Tools`、event 和 abort 语义。

## 文档索引

- [AgentRuntime](agent-runtime.md)：UI 无关的运行时门面。
- [SessionRuntime](session-runtime.md)：单会话产品编排模块。
- [AgentRuntimeProtocol](agent-runtime-protocol.md)：命令、领域分组查询、事件、快照和下游 adapter 协议。
- [AgentRuntimeEvents](agent-runtime-events.md)：事件命名、`agent_runtime_protocol::Event` / `agent_runtime_protocol::EventMsg`、生命周期、顺序约束、commit 后领域事实和重连语义。
- [SessionManager / SessionWriter / SessionStorage](session-manager.md)：会话生命周期、已加载会话运行时、统一 stable batch writer、session tree、JSONL 和上下文重建。
- [ResourceManager](resource-manager.md)：资源来源聚合、级联 snapshot、cwd overlay policy、刷新和诊断。
- [CommandSurface](command-surface.md)：命令领域面、无状态 `CommandManager`、session-scoped `Command`、nested JSON command tree、dynamic providers、handler registry、执行前 resolve 和 UI 安全边界。
- [RuntimeHooks](runtime-hooks.md)：后期内部 hook seam、hook/event 边界、capability、typed result、owner 分层和安全点。
- [Skills](skills.md)：`skills.rs` 平级模块，提供技能 metadata、catalog、发现、解析、校验和格式化 helper。
- [PromptTemplates](prompt-templates.md)：`prompt_templates.rs` 平级模块，定义模板资源、参数语法和单次展开 helper。
- [Prompt](prompt.md)：`prompt.rs` / `prompt/` 无状态组装子系统，定义 `PromptTurn`、`PromptCallProfile` 和最终 model-input projection。
- [Tools](tools.md)：`tools.rs` / `tools/` session-scoped 工具子系统，封装工具定义、registry、active tools、policy、approval、grants、execution coordination、sandbox、mutation locks 和 executors。
- [Compaction](compaction.md)：`compaction.rs` 平级模块，提供压缩准备、摘要 prompt、上下文重建规则和压缩结果类型。
- [UsageStats](usage-stats.md)：token 消耗、run/session stats、context usage、provider usage 归一化和 UI 展示口径。
- [ModelGateway](model-gateway.md)：provider/model/auth 执行边界、custom provider、Rig provider adapter、usage/error/fallback 规则。
- [Driver](driver.md)：Rig 状态机适配模块。

事件协议的关键取舍记录在 [ADR 0003](../adr/0003-agent-runtime-events-use-event-msg-and-lifecycle-pairs.md)，hook 边界的关键取舍记录在 [ADR 0008](../adr/0008-runtime-hooks-are-internal-safe-point-seams.md)，provider/model 边界记录在 [ADR 0009](../adr/0009-model-gateway-wraps-rig-providers.md)，工具子系统边界记录在 [ADR 0011](../adr/0011-tools-are-session-scoped-subsystem.md)，命令体系边界记录在 [ADR 0012](../adr/0012-command-manager-is-stateless-session-command-facade.md)，driver 输入 seam 记录在 [ADR 0013](../adr/0013-driver-receives-driver-turn-input.md)，ModelGateway 实现顺序记录在 [ADR 0014](../adr/0014-model-gateway-spine-precedes-driver-integration.md)，hook owner 分层和延期实现记录在 [ADR 0015](../adr/0015-hook-owners-follow-runtime-boundaries.md)，command run policy 与 prompt delivery 的分离记录在 [ADR 0016](../adr/0016-separate-command-run-policy-from-prompt-delivery.md)，Prompt 的 immutable turn assembly 决策记录在 [ADR 0017](../adr/0017-prompt-uses-immutable-turn-assembly.md)，Command/Query/Event/Snapshot 分离记录在 [ADR 0018](../adr/0018-agent-runtime-separates-command-query-event-and-snapshot.md)，统一 session batch writer 记录在 [ADR 0019](../adr/0019-session-writes-use-one-trusted-batch-writer.md)，headless runtime 不拥有 current session 的决策记录在 [ADR 0020](../adr/0020-agent-runtime-has-no-current-session.md)。行为与接口以各模块文档、协议文档、事件文档和 ADR 为权威，不再维护容易滞后的集中式开发路线图。

## Rust 文件规划

当前文档约定的拟用 Rust 文件名如下。`Prompt` 使用 `src/prompt.rs` facade + `src/prompt/` 内部子模块，`Driver` 入口固定为 `src/driver.rs`。`AgentRuntimeEvents` 是事件生命周期文档；协议类型仍以 `agent_runtime_protocol.rs` 为权威。

| Rust 文件 | 对应文档 | 说明 |
| --- | --- | --- |
| `src/lib.rs` | [MiniCore 架构](../architecture.md)、[模块总览](README.md) | crate root、模块声明和必要 re-export。 |
| `src/agent_runtime.rs` | [AgentRuntime](agent-runtime.md) | UI 无关运行时门面、工作区服务、command/query 路由。 |
| `src/runtime_services.rs` | [AgentRuntime](agent-runtime.md)、[AgentRuntimeEvents](agent-runtime-events.md) | `WorkspaceServices`、共享 settings/provider/auth/model gateway wiring。 |
| `src/auth_store.rs` | [AgentRuntime](agent-runtime.md)、[RuntimeHooks](runtime-hooks.md) | 凭据读取边界；不暴露 secret material 给 UI 或后期 hook。 |
| `src/settings_store.rs` | [AgentRuntime](agent-runtime.md)、[CommandSurface](command-surface.md) | runtime/session 设置读取与命令动态候选输入边界。 |
| `src/provider_registry.rs` | [ModelGateway](model-gateway.md)、[AgentRuntime](agent-runtime.md) | provider/model catalog、custom provider 配置和模型能力摘要；不持有凭据或 provider client。 |
| `src/model_gateway.rs` | [ModelGateway](model-gateway.md)、[Driver](driver.md)、[SessionRuntime](session-runtime.md)、[Compaction](compaction.md)、[UsageStats](usage-stats.md) | `ModelCallPurpose` / `ModelCallRequest` 权威边界、provider 调用、凭据解析、fallback、usage 归一化和错误分类入口；后期 provider hook owner。 |
| `src/model_gateway/rig.rs` | [ModelGateway](model-gateway.md) | 私有 Rig provider adapter；唯一允许接触 `rig::providers::*` 的 provider/client 实现细节位置。 |
| `src/project_trust.rs` | [ResourceManager](resource-manager.md)、[RuntimeHooks](runtime-hooks.md) | workspace trust 判断、记忆和资源加载 gate；后期 hook capability 可读取 trust summary。 |
| `src/runtime_diagnostics.rs` | [AgentRuntimeEvents](agent-runtime-events.md)、[ResourceManager](resource-manager.md)、[RuntimeHooks](runtime-hooks.md) | runtime/resource diagnostics 聚合与协议投影；后期包含 hook diagnostics。 |
| `src/ids.rs` | [AgentRuntimeProtocol](agent-runtime-protocol.md) | `WorkspaceId`、`SessionId`、`RunId`、`CommandId`、`ToolCallId` 等稳定 ID。 |
| `src/error.rs` | [AgentRuntimeProtocol](agent-runtime-protocol.md)、[AgentRuntime](agent-runtime.md) | `RuntimeError`、`SessionError`、`DriverError` 等错误边界。 |
| `src/messages.rs` | [AgentRuntimeProtocol](agent-runtime-protocol.md)、[SessionManager / SessionStorage](session-manager.md)、[Driver](driver.md) | `MessageRecord`、message content、tool call/result message 公共类型。 |
| `src/agent_runtime_protocol.rs` | [AgentRuntimeProtocol](agent-runtime-protocol.md)、[AgentRuntimeEvents](agent-runtime-events.md) | `AgentCommand` / `CommandAck`、领域分组 `RuntimeQuery` / `QueryResponse`、`Event` / `EventMsg`、`RuntimeSnapshot`、UI view 类型。 |
| `src/agent_runtime_events.rs` | [AgentRuntimeEvents](agent-runtime-events.md) | 事件构造、sequence、生命周期断言和 event bus helper；不重新定义协议 enum。 |
| `src/session_manager.rs` | [SessionManager / SessionStorage](session-manager.md) | 会话生命周期、loaded runtime map、open/fork/delete/close。 |
| `src/session_storage.rs` | [SessionManager / SessionWriter / SessionStorage](session-manager.md) | `SessionHandle`、`SessionWriter` / `SessionStorage` traits、stable batch 和 context 重建公共类型。 |
| `src/session_storage/memory.rs` | [SessionManager / SessionStorage](session-manager.md) | `InMemorySessionStorage` / 测试与 MVP 原型。 |
| `src/session_storage/jsonl.rs` | [SessionManager / SessionWriter / SessionStorage](session-manager.md) | 一行一个 committed batch 的 JSONL session adapter。 |
| `src/session_runtime.rs` | [SessionRuntime](session-runtime.md)、[Driver](driver.md) | 单会话 phase、message queues、`PendingSessionAction`、run/post-run 编排、`TurnState -> DriverTurnInput` 投影、事件归约、稳定 batch commit，以及 per-run `SessionDriverHost` wrapper。 |
| `src/resource_manager.rs` | [ResourceManager](resource-manager.md) | `ResourceManager`、`ResourceSnapshotStore`、runtime/cwd/turn/step snapshots、overlay policy、reload/recompose、diagnostics、prompt materials。 |
| `src/prompt_templates.rs` | [PromptTemplates](prompt-templates.md)、[ResourceManager](resource-manager.md)、[CommandSurface](command-surface.md) | prompt template metadata/resource/catalog/invocation、frontmatter、参数解析和单次展开 helper；不拥有资源生命周期。 |
| `src/command.rs` | [CommandSurface](command-surface.md) | command public module 和常用类型 re-export。 |
| `src/command/manager.rs` | [CommandSurface](command-surface.md) | `CommandManager`：共享无状态 materialize / parse / suggest / resolve 管理器。 |
| `src/command/session.rs` | [CommandSurface](command-surface.md)、[SessionRuntime](session-runtime.md) | `Command`：`SessionRuntime` 持有的 session-scoped facade，构造 `CommandContext` / `SessionCommandHost`。 |
| `src/command/manifest.rs` | [CommandSurface](command-surface.md) | nested JSON command pack schema、加载和校验。 |
| `src/command/definition.rs` | [CommandSurface](command-surface.md) | `CommandNodeSpec`、`nodeType`、args、dynamic children 数据结构。 |
| `src/command/materialize.rs` | [CommandSurface](command-surface.md) | manifest + `CommandContext` -> transient catalog。 |
| `src/command/provider.rs` | [CommandSurface](command-surface.md) | `CommandCandidateProvider` trait 和 provider registry。 |
| `src/command/parse.rs` | [CommandSurface](command-surface.md) | command text parser。 |
| `src/command/suggest.rs` | [CommandSurface](command-surface.md) | command/path/arg suggestions。 |
| `src/command/resolve.rs` | [CommandSurface](command-surface.md) | `resolve_for_execution`：重新校验 selection、bindings、args、phase、trust、capability 和 handler binding。 |
| `src/command/handler.rs` | [CommandSurface](command-surface.md) | `CommandHandler` trait 和 trusted handler registry。 |
| `src/command/host.rs` | [CommandSurface](command-surface.md)、[SessionRuntime](session-runtime.md) | `SessionCommandHost` 窄接口。 |
| `src/command/result.rs` | [CommandSurface](command-surface.md) | `CommandResult` / `CommandError` / UI-safe result view。 |
| `src/command/handlers/` | [CommandSurface](command-surface.md) | builtin command handler 实现：help、status、model、thinking、resources、skills、prompt templates、tools。 |
| `src/runtime_hooks.rs` | [RuntimeHooks](runtime-hooks.md) | 后期 hook registry、capability、typed decision/result；不在当前 MVP 阶段实现。 |
| `src/skills.rs` | [Skills](skills.md) | skill metadata、catalog、frontmatter、format helper。 |
| `src/prompt.rs` | [Prompt](prompt.md) | Prompt public facade、`begin_turn()`、常用类型 re-export。 |
| `src/prompt/turn.rs` | [Prompt](prompt.md) | `PromptTurn`、`PromptCallProfile`、typed turn views 和 fingerprint。 |
| `src/prompt/system.rs` | [Prompt](prompt.md) | 确定性 system prompt section rendering。 |
| `src/prompt/intent.rs` | [Prompt](prompt.md)、[Skills](skills.md)、[PromptTemplates](prompt-templates.md) | `PromptIntent -> ResolvedPromptInput`，组合 skill/template/attachments。 |
| `src/prompt/projection.rs` | [Prompt](prompt.md)、[Driver](driver.md) | durable/current/transient lanes -> `ModelInputProjection`。 |
| `src/prompt/validation.rs` | [Prompt](prompt.md)、[Compaction](compaction.md) | tool protocol、dedup、required contribution、budget 和 persistence 校验。 |
| `src/prompt/provenance.rs` | [Prompt](prompt.md)、[ResourceManager](resource-manager.md) | contribution stamps 和 prompt/model-input fingerprint；复用 canonical resource identity。 |
| `src/tools.rs` | [Tools](tools.md) | tools public module 和常用类型 re-export。 |
| `src/tools/definition.rs` | [Tools](tools.md) | `ToolDefinition`、schema、risk、display metadata。 |
| `src/tools/subsystem.rs` | [Tools](tools.md) | `Tools` session-scoped 子系统主结构和深接口。 |
| `src/tools/registry.rs` | [Tools](tools.md) | `ToolRegistry`、`RegisteredTool`、工具来源和冲突。 |
| `src/tools/active.rs` | [Tools](tools.md) | `ActiveToolSet`、active tool selection。 |
| `src/tools/prompt.rs` | [Tools](tools.md)、[Prompt](prompt.md) | `ToolPromptCatalog`、provider schemas、snippets/guidelines projection。 |
| `src/tools/policy.rs` | [Tools](tools.md) | `ToolPolicy`、policy input/decision/config；保持纯判断。 |
| `src/tools/approval.rs` | [Tools](tools.md)、[AgentRuntimeProtocol](agent-runtime-protocol.md) | `ToolApprovalBroker`、`ApprovalRequestId`、pending approval 状态机。 |
| `src/tools/grants.rs` | [Tools](tools.md) | `ToolApprovalGrantStore`、approval modes、remembered grants。 |
| `src/tools/planner.rs` | [Tools](tools.md) | schema validate、args canonicalize、sandbox check、approval preview、prepared invocation。 |
| `src/tools/coordinator.rs` | [Tools](tools.md)、[Driver](driver.md) | batch execution coordination、parallel/sequential、approval wait、按 `call_index` 稳定回填。 |
| `src/tools/executor.rs` | [Tools](tools.md) | `ToolExecutor` trait、executor registry、result/error 归一化。 |
| `src/tools/events.rs` | [Tools](tools.md)、[AgentRuntimeEvents](agent-runtime-events.md) | 工具内部 update 类型和 sink adapter。 |
| `src/tools/sandbox.rs` | [Tools](tools.md) | `ToolSandboxView`、路径/进程/网络边界和 check result。 |
| `src/tools/mutation.rs` | [Tools](tools.md) | `ToolMutationKey`、file/resource mutation queue。 |
| `src/tools/builtin/mod.rs` | [Tools](tools.md) | 内置工具集合声明。 |
| `src/tools/builtin/read.rs` | [Tools](tools.md) | `read` 工具。 |
| `src/tools/builtin/grep.rs` | [Tools](tools.md) | `grep` 工具。 |
| `src/tools/builtin/find.rs` | [Tools](tools.md) | `find` 工具。 |
| `src/tools/builtin/ls.rs` | [Tools](tools.md) | `ls` 工具。 |
| `src/tools/builtin/write.rs` | [Tools](tools.md) | `write` 工具。 |
| `src/tools/builtin/edit.rs` | [Tools](tools.md) | `edit` 工具。 |
| `src/tools/builtin/apply_patch.rs` | [Tools](tools.md) | `apply-patch` 工具。 |
| `src/tools/builtin/bash.rs` | [Tools](tools.md) | `bash` 工具。 |
| `src/compaction.rs` | [Compaction](compaction.md) | 压缩准备、cut point、`CompactionSummaryMaterial` 和结果类型；不构造 `ModelCallRequest`。 |
| `src/usage_stats.rs` | [UsageStats](usage-stats.md) | provider usage 归一化、run/session/context usage helper；消费 `ModelCallPurpose`，不定义 `UsagePurpose`。 |
| `src/driver.rs` | [Driver](driver.md) | `DriverTurnInput`、`DriverHost` seam、drive request/result、Rig step 映射主入口。 |
| `src/driver/rig.rs` | [Driver](driver.md) | 当前 Rig sans-IO adapter 实现细节。 |

## 权威归属

为避免同一概念在多个文档中漂移，后续维护按下面的 source of truth 写：

| 概念 | 权威文档 | 其他文档只能做什么 |
| --- | --- | --- |
| AgentRuntimeProtocol 类型：`agent_runtime_protocol::AgentCommand`、`agent_runtime_protocol::Event`、`agent_runtime_protocol::EventMsg`、`agent_runtime_protocol::RuntimeSnapshot` | [AgentRuntimeProtocol](agent-runtime-protocol.md) | 引用类型，不复制完整 enum。 |
| 事件顺序、生命周期、所有权、commit 后领域事实和重连 | [AgentRuntimeEvents](agent-runtime-events.md) | 描述本模块会触发哪些事件，不重新定义事件协议。 |
| 运行时门面、工作区服务、事件通道 | [AgentRuntime](agent-runtime.md) | 只说明如何被调用或被拥有。 |
| 单会话运行编排、phase、queue、当前 run、后期 run safe-point hooks | [SessionRuntime](session-runtime.md) | 只说明交给会话运行时编排的 seam。 |
| 会话生命周期、统一 writer、session tree、JSONL、上下文重建 | [SessionManager / SessionWriter / SessionStorage](session-manager.md) | 只说明何时提交稳定 batch，不重复 writer 契约。 |
| 资源生命周期、`ResourceSnapshotStore`、runtime/cwd/turn/step snapshots、cwd overlay policy、source info、diagnostics、prompt materials | [ResourceManager](resource-manager.md) | 只说明资源输入或输出，不扫描资源。 |
| 命令领域面、`CommandManager` / `Command` 命名、nested JSON manifest、dynamic provider、handler registry、command catalog/suggestion/result 和 UI 安全边界 | [CommandSurface](command-surface.md) | 只说明某个命令会映射到什么运行时能力，不重复解析、resolve 和结果规则。 |
| 后期 hook/event 边界、hook source/capability、typed result、hook 点和安全策略 | [RuntimeHooks](runtime-hooks.md) | 只说明何时触发 hook，不重复 hook 注册和权限规则。 |
| 技能 metadata、catalog、frontmatter、format helper | [Skills](skills.md) | 只说明如何调用 helper，不拥有技能生命周期。 |
| 提示模板 metadata/resource/catalog、参数语法和纯展开 helper | [PromptTemplates](prompt-templates.md) | 只说明 roots、catalog metadata 或 delivery，不复制模板语法。 |
| `PromptTurn`、`PromptCallProfile`、PromptIntent 展开、context lanes、最终 `ModelInputProjection` | [Prompt](prompt.md) | 只说明何时调用、由谁提供输入，不重复组装顺序和校验规则。 |
| session-scoped 工具子系统、registry、active tools、policy、approval、grants、execution coordination、sandbox、mutation locks、executors | [Tools](tools.md) | 只说明 `SessionRuntime` 如何协调 `Driver` 与 `Tools`，不复制工具治理 pipeline。 |
| provider/model/auth 调用边界、`ModelSelection`、`ProviderRegistry`、`ModelGateway`、custom provider、Rig provider adapter | [ModelGateway](model-gateway.md) | 只说明本模块如何选择模型或发起模型调用，不重复 provider/auth 解析规则。 |
| Rig `AgentRun` step 驱动、`DriverTurnInput`、`DriverHost` trait seam、`SessionDriverHost` wrapper 代码形态 | [Driver](driver.md) | 只说明如何进入 driver，不拥有 Rig 协议；具体 session 编排仍看 [SessionRuntime](session-runtime.md)。 |
| 压缩算法、cut point、summary prompt、summary message | [Compaction](compaction.md) | 只说明压缩流程，不重复摘要 prompt 和 projection 规则。 |
| token 消耗、run/session stats、context usage | [UsageStats](usage-stats.md) | 只引用 usage view，不重新定义估算和累计规则。 |

## 设计判断

将能力拆成模块文档有利于开发，因为不同模块的实现节奏不同：`SessionStorage` 可以先写存储测试，`SessionManager` 可以独立验证 create/open/list/fork 和 loaded runtime lifecycle，`Tools` 可以独立接入只读工具定义和 executor，`Driver` 可以先跑通文本流。总览文档只保留模块关系和边界，具体接口放在各模块文档里维护。
