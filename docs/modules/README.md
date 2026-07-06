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
      ┌────────────────────┬────────────────────┬────────────────────┐
      ▼                    ▼                    ▼                    ▼
WorkspaceServices   CommandSurface        RuntimeHooks          SessionManager
      │                                                           ┌──────┴──────┐
      ├─ ResourceManager                                  LoadedSessionRuntimes  SessionHandle
      │   ├─ ResourceSnapshotStore
      │   └─ ResourceOverlayPolicy
      ├─ ProviderRegistry / AuthStore                              │             │
      └─ ModelGateway                                              ▼             ▼
                                                            SessionRuntime  SessionStorage
                                                                  │
                                      ┌──────────────────┬────────┼────────┬─────────────┬─────────────┐
                                      ▼                  ▼        ▼        ▼             ▼             ▼
                                   Skills             Prompt    Tools  Compaction     Driver      UsageStats
                                                                                         │
                                                                                         ▼
                                                                                        Rig

ModelGateway ───────────────────────────────────────────────────────────────────────▶ Rig providers
```

## 模块职责

`AgentRuntime` 是 MiniCore 的 UI 无关运行时门面。它管理工作区、`WorkspaceServices`、`ResourceManager`、事件通道和命令路由，并向下游宿主暴露 `dispatch`、`subscribe`、`snapshot`。

`SessionManager` 是工作区内会话生命周期 facade。它协调持久化会话目录、`SessionHandle` / `SessionStorage` 和内部 `LoadedSessionRuntimes`；`LoadedSessionRuntimes` 只是已加载 `SessionRuntime` 的 map，不作为独立架构层。

`SessionRuntime` 是单会话产品级编排层。它管理阶段、当前 run、队列、资源、模型状态、工具状态、Hook、pending session writes 和事件归约。每个 session 固定一个 workspace cwd；每次 run 启动时从 `ResourceManager` 捕获 `TurnResourceSnapshot` 进 `TurnState`，使后台 run 不受 focused session 切换或资源 reload 影响。

`AgentRuntimeEvents` 是运行时事件生命周期模块。它定义 Codex-like `agent_runtime_protocol::Event { ..., msg }`、事件命名、事件来源、started/delta/finished 配对、保存点、重连和常见场景的事件顺序，供下游 UI reducer 消费。

`ResourceManager` 是运行时内部资源子系统，负责资源来源聚合、信任校验、原子刷新候选、级联 snapshot、cwd-over-runtime overlay policy、提示词素材和诊断。它维护 `RuntimeResourceSnapshot`、`CwdResourceSnapshot`、`TurnResourceSnapshot` capture，并预留 `StepResourceSnapshot`；在 skill 层面它决定 roots、trust、分层、overlay 和 current snapshot，但不解析 frontmatter、不拼 `<skill>` 消息。

`CommandSurface` 是跨 UI 的命令目录、解析、执行映射和呈现协调模块。它把 `/compact`、`/skill:name`、`/{template}`、`/model`、`/usage` 和后续扩展命令经过 Parse / Plan / Execute / Present 四阶段映射到已有 `agent_runtime_protocol::Command`、受控查询、prompt-like 输入或 `CommandPresentation`，但不直接执行工具或 Agent loop。

`RuntimeHooks` 是内部扩展点系统。它在 runtime 安全点开放 typed decision / patch / replacement，让内置策略、测试 harness、可信 package 或后续 extension 影响 prompt、context、tools、compaction 和 command presentation；当前设计不定义资源 discovery / reload hook。hook 不直接发布 UI event，不直接读写 session storage，也不直接执行工具。

`Skills` 是平级技能文件能力模块，对应未来的 `skills.rs`。它提供 `SkillMetadata` / `SkillResource` / `SkillCatalog` 数据结构，以及给定目录后的发现、解析、校验和格式化 helper；它不拥有资源生命周期或 overlay。显式技能调用的正文展开和 message 构造由 `SessionRuntime` 基于 captured `TurnResourceSnapshot` 负责。

`Prompt` 是纯系统提示词构建模块，对应未来的 `prompt.rs`。它消费 `SessionRuntime` 从 captured `TurnResourceSnapshot` 提取的 prompt materials，以及会话持有的 active tools / snippets，输出最终 system prompt；它不直接调用 `ResourceManager`，`ResourceManager` 也不调用 `Prompt`。

`Tools` 是平级工具能力模块，对应未来的 `tools.rs` / `tools/`。它提供工具定义、内置工具、外部工具适配、schema、prompt metadata 和 executor helper；工具状态和工具治理由 `SessionRuntime` 持有。

`Compaction` 是平级压缩能力模块，对应未来的 `compaction.rs`。它提供上下文 token 估算、压缩触发判断、cut point 选择、summary prompt 构建、摘要消息格式化和压缩结果类型；压缩流程、模型调用、Hook、事件和 session 写入由 `SessionRuntime` 编排。

`UsageStats` 是 token 消耗和上下文占用统计模块。它区分模型调用消耗、run 汇总、会话累计 stats 和当前 context usage；provider usage 归一化、本地估算、UI view 口径和压缩阈值计算都在这里统一说明。

`ModelGateway` 是模型调用治理模块。它复用 Rig provider/client 能力，但在 MiniCore 内负责 provider/model 解析、凭据解析、custom base URL、provider hook、fallback、usage 归一化和错误分类。

`Driver` 只负责适配 Rig。Rig 决定 `CallModel`、`CallTools` 和 `Done`；`Driver` 把这些 step 接到产品运行时的 provider、tool gateway、event 和 abort 语义。

## 文档索引

- [AgentRuntime](agent-runtime.md)：UI 无关的运行时门面。
- [SessionRuntime](session-runtime.md)：单会话产品编排模块。
- [AgentRuntimeProtocol](agent-runtime-protocol.md)：命令、事件、快照和下游 adapter 协议。
- [AgentRuntimeEvents](agent-runtime-events.md)：事件命名、`agent_runtime_protocol::Event` / `agent_runtime_protocol::EventMsg`、生命周期、顺序约束、保存点和重连语义。
- [SessionManager / SessionStorage](session-manager.md)：会话生命周期、已加载会话运行时、session tree、JSONL、会话管理与存储接口。
- [ResourceManager](resource-manager.md)：资源来源聚合、级联 snapshot、cwd overlay policy、刷新和诊断。
- [CommandSurface](command-surface.md)：斜杠命令目录、四阶段解析/规划/执行/呈现、来源合并、可用性规则和执行映射。
- [RuntimeHooks](runtime-hooks.md)：内部 hook seam、hook/event 边界、capability、typed result 和安全点。
- [Skills](skills.md)：`skills.rs` 平级模块，提供技能 metadata、catalog、发现、解析、校验和格式化 helper。
- [Prompt](prompt.md)：`prompt.rs` 纯构建模块，拼装最终 system prompt。
- [Tools](tools.md)：`tools.rs` / `tools/` 平级模块，提供工具定义、内置工具、外部工具适配和执行 helper。
- [Compaction](compaction.md)：`compaction.rs` 平级模块，提供压缩准备、摘要 prompt、上下文重建规则和压缩结果类型。
- [UsageStats](usage-stats.md)：token 消耗、run/session stats、context usage、provider usage 归一化和 UI 展示口径。
- [ModelGateway](model-gateway.md)：provider/model/auth 执行边界、custom provider、Rig provider adapter、usage/error/fallback 规则。
- [Driver](driver.md)：Rig 状态机适配模块。

实现路线不属于代码模块，放在 [实现路线图](../implementation-roadmap.md)。事件协议的关键取舍记录在 [ADR 0003](../adr/0003-agent-runtime-events-use-event-msg-and-lifecycle-pairs.md)，hook 边界的关键取舍记录在 [ADR 0008](../adr/0008-runtime-hooks-are-internal-safe-point-seams.md)，provider/model 边界的关键取舍记录在 [ADR 0009](../adr/0009-model-gateway-wraps-rig-providers.md)。

## Rust 文件规划

当前文档约定的拟用 Rust 文件名如下。`Prompt` 和 `Driver` 的文件名分别固定为 `src/prompt.rs` 与 `src/driver.rs`，其余模块文件名保持现状。`AgentRuntimeEvents` 是事件生命周期文档；协议类型仍以 `agent_runtime_protocol.rs` 为权威。

| Rust 文件 | 对应文档 | 说明 |
| --- | --- | --- |
| `src/lib.rs` | [MiniCore 架构](../architecture.md)、[模块总览](README.md) | crate root、模块声明和必要 re-export。 |
| `src/agent_runtime.rs` | [AgentRuntime](agent-runtime.md) | UI 无关运行时门面、工作区服务和命令路由。 |
| `src/runtime_services.rs` | [AgentRuntime](agent-runtime.md)、[AgentRuntimeEvents](agent-runtime-events.md) | `WorkspaceServices`、共享 settings/provider/auth/model gateway wiring。 |
| `src/auth_store.rs` | [AgentRuntime](agent-runtime.md)、[RuntimeHooks](runtime-hooks.md) | 凭据读取边界；不暴露 secret material 给 UI 或 hook。 |
| `src/settings_store.rs` | [AgentRuntime](agent-runtime.md)、[CommandSurface](command-surface.md) | runtime/session 设置读取与更新边界。 |
| `src/provider_registry.rs` | [ModelGateway](model-gateway.md)、[AgentRuntime](agent-runtime.md) | provider/model catalog、custom provider 配置和模型能力摘要；不持有凭据或 provider client。 |
| `src/model_gateway.rs` | [ModelGateway](model-gateway.md)、[Driver](driver.md)、[SessionRuntime](session-runtime.md)、[Compaction](compaction.md)、[UsageStats](usage-stats.md) | provider 调用、凭据解析、payload hook、fallback、usage 归一化和错误分类入口。 |
| `src/model_gateway/rig.rs` | [ModelGateway](model-gateway.md) | 私有 Rig provider adapter；唯一允许接触 `rig::providers::*` 的 provider/client 实现细节位置。 |
| `src/project_trust.rs` | [ResourceManager](resource-manager.md)、[RuntimeHooks](runtime-hooks.md) | workspace trust 判断、记忆和资源加载 gate。 |
| `src/runtime_diagnostics.rs` | [AgentRuntimeEvents](agent-runtime-events.md)、[ResourceManager](resource-manager.md)、[RuntimeHooks](runtime-hooks.md) | runtime/resource/hook diagnostics 聚合与协议投影。 |
| `src/ids.rs` | [AgentRuntimeProtocol](agent-runtime-protocol.md) | `WorkspaceId`、`SessionId`、`RunId`、`CommandId`、`ToolCallId` 等稳定 ID。 |
| `src/error.rs` | [AgentRuntimeProtocol](agent-runtime-protocol.md)、[AgentRuntime](agent-runtime.md) | `RuntimeError`、`SessionError`、`DriverError` 等错误边界。 |
| `src/messages.rs` | [AgentRuntimeProtocol](agent-runtime-protocol.md)、[SessionManager / SessionStorage](session-manager.md)、[Driver](driver.md) | `MessageRecord`、message content、tool call/result message 公共类型。 |
| `src/agent_runtime_protocol.rs` | [AgentRuntimeProtocol](agent-runtime-protocol.md)、[AgentRuntimeEvents](agent-runtime-events.md) | `Command`、`CommandAck`、`Event`、`EventMsg`、`RuntimeSnapshot`、UI view 类型。 |
| `src/agent_runtime_events.rs` | [AgentRuntimeEvents](agent-runtime-events.md) | 事件构造、sequence、生命周期断言和 event bus helper；不重新定义协议 enum。 |
| `src/session_manager.rs` | [SessionManager / SessionStorage](session-manager.md) | 会话生命周期、loaded runtime map、focus/open/fork/delete。 |
| `src/session_storage.rs` | [SessionManager / SessionStorage](session-manager.md) | `SessionHandle`、`SessionStorage` trait、entry/context 重建公共类型。 |
| `src/session_storage/memory.rs` | [SessionManager / SessionStorage](session-manager.md) | `InMemorySessionStorage` / 测试与 MVP 原型。 |
| `src/session_storage/jsonl.rs` | [SessionManager / SessionStorage](session-manager.md) | JSONL session 文件存储。 |
| `src/session_runtime.rs` | [SessionRuntime](session-runtime.md) | 单会话 phase、queue、run 编排、事件归约和 save point。 |
| `src/resource_manager.rs` | [ResourceManager](resource-manager.md) | `ResourceManager`、`ResourceSnapshotStore`、runtime/cwd/turn/step snapshots、overlay policy、reload/recompose、diagnostics、prompt materials。 |
| `src/prompt_templates.rs` | [ResourceManager](resource-manager.md)、[CommandSurface](command-surface.md) | prompt template metadata、解析和显式调用 helper。 |
| `src/command_surface.rs` | [CommandSurface](command-surface.md) | slash catalog、parse/plan/execute/present 协调。 |
| `src/runtime_hooks.rs` | [RuntimeHooks](runtime-hooks.md) | hook registry、capability、typed decision/result。 |
| `src/skills.rs` | [Skills](skills.md) | skill metadata、catalog、frontmatter、format helper。 |
| `src/prompt.rs` | [Prompt](prompt.md) | 最终 system prompt 纯构建模块。 |
| `src/tools.rs` | [Tools](tools.md) | tools public module 和常用类型 re-export。 |
| `src/tools/definition.rs` | [Tools](tools.md) | `ToolDefinition`、schema、risk、display metadata。 |
| `src/tools/registry.rs` | [Tools](tools.md) | `ToolRegistry`、`ActiveToolSet`、`ToolPromptCatalog`。 |
| `src/tools/policy.rs` | [Tools](tools.md) | `ToolPolicy`、policy input/decision/config。 |
| `src/tools/approval.rs` | [Tools](tools.md)、[AgentRuntimeProtocol](agent-runtime-protocol.md) | `ToolApprovalBroker`、pending approval 状态机。 |
| `src/tools/gateway.rs` | [Tools](tools.md)、[Driver](driver.md) | `ToolGateway`、tool invocation/result 归一化。 |
| `src/tools/providers.rs` | [Tools](tools.md) | built-in / external tool provider adapter。 |
| `src/tools/builtin/mod.rs` | [Tools](tools.md) | 内置工具集合声明。 |
| `src/tools/builtin/read.rs` | [Tools](tools.md) | `read` 工具。 |
| `src/tools/builtin/grep.rs` | [Tools](tools.md) | `grep` 工具。 |
| `src/tools/builtin/find.rs` | [Tools](tools.md) | `find` 工具。 |
| `src/tools/builtin/ls.rs` | [Tools](tools.md) | `ls` 工具。 |
| `src/tools/builtin/write.rs` | [Tools](tools.md) | `write` 工具。 |
| `src/tools/builtin/edit.rs` | [Tools](tools.md) | `edit` 工具。 |
| `src/tools/builtin/apply_patch.rs` | [Tools](tools.md) | `apply-patch` 工具。 |
| `src/tools/builtin/bash.rs` | [Tools](tools.md) | `bash` 工具。 |
| `src/compaction.rs` | [Compaction](compaction.md) | 压缩准备、cut point、summary prompt 和结果类型。 |
| `src/usage_stats.rs` | [UsageStats](usage-stats.md) | provider usage 归一化、run/session/context usage helper。 |
| `src/driver.rs` | [Driver](driver.md) | Driver trait/host seam、drive request/result、Rig step 映射主入口。 |
| `src/driver/rig.rs` | [Driver](driver.md) | 当前 Rig sans-IO adapter 实现细节。 |

## 权威归属

为避免同一概念在多个文档中漂移，后续维护按下面的 source of truth 写：

| 概念 | 权威文档 | 其他文档只能做什么 |
| --- | --- | --- |
| AgentRuntimeProtocol 类型：`agent_runtime_protocol::Command`、`agent_runtime_protocol::Event`、`agent_runtime_protocol::EventMsg`、`agent_runtime_protocol::RuntimeSnapshot` | [AgentRuntimeProtocol](agent-runtime-protocol.md) | 引用类型，不复制完整 enum。 |
| 事件顺序、生命周期、所有权、重连和保存点 | [AgentRuntimeEvents](agent-runtime-events.md) | 描述本模块会触发哪些事件，不重新定义事件协议。 |
| 运行时门面、工作区服务、事件通道 | [AgentRuntime](agent-runtime.md) | 只说明如何被调用或被拥有。 |
| 单会话运行编排、phase、queue、hooks、当前 run | [SessionRuntime](session-runtime.md) | 只说明交给会话运行时编排的 seam。 |
| 会话生命周期、已加载会话运行时、session tree、JSONL、上下文重建 | [SessionManager / SessionStorage](session-manager.md) | 只说明何时读写会话，不重复存储规则。 |
| 资源生命周期、`ResourceSnapshotStore`、runtime/cwd/turn/step snapshots、cwd overlay policy、source info、diagnostics、prompt materials | [ResourceManager](resource-manager.md) | 只说明资源输入或输出，不扫描资源。 |
| 斜杠命令目录、四阶段模型、解析优先级、phase policy、source projection 和 command presentation 规则 | [CommandSurface](command-surface.md) | 只说明某个命令会映射到什么运行时能力，不重复解析和呈现规则。 |
| hook/event 边界、hook source/capability、typed result、hook 点和安全策略 | [RuntimeHooks](runtime-hooks.md) | 只说明何时触发 hook，不重复 hook 注册和权限规则。 |
| 技能 metadata、catalog、frontmatter、format helper | [Skills](skills.md) | 只说明如何调用 helper，不拥有技能生命周期。 |
| 最终 system prompt 拼装规则 | [Prompt](prompt.md) | 只说明何时重建，不拼装 prompt。 |
| 工具定义、registry/helper 类型、执行 facade 形态 | [Tools](tools.md) | 只说明工具状态由 `SessionRuntime` 持有。 |
| provider/model/auth 调用边界、`ModelSelection`、`ProviderRegistry`、`ModelGateway`、custom provider、Rig provider adapter | [ModelGateway](model-gateway.md) | 只说明本模块如何选择模型或发起模型调用，不重复 provider/auth 解析规则。 |
| Rig `AgentRun` step 驱动和 host seam | [Driver](driver.md) | 只说明如何进入 driver，不拥有 Rig 协议。 |
| 压缩算法、cut point、summary prompt、summary message | [Compaction](compaction.md) | 只说明压缩流程，不重复摘要 prompt 和 projection 规则。 |
| token 消耗、run/session stats、context usage | [UsageStats](usage-stats.md) | 只引用 usage view，不重新定义估算和累计规则。 |

## 设计判断

将能力拆成模块文档有利于开发，因为不同模块的实现节奏不同：`SessionStorage` 可以先写存储测试，`SessionManager` 可以独立验证 create/open/list/fork 和 loaded runtime lifecycle，`Tools` 可以独立接入只读工具定义和 executor，`Driver` 可以先跑通文本流。总览文档只保留模块关系和边界，具体接口放在各模块文档里维护。
