# MiniCore 架构

本文档是 MiniCore 原生 Agent harness runtime core 的总入口。MiniCore 本仓库提供可嵌入的运行时核心、协议、会话、资源、工具、事件和 harness 编排能力；CLI、TUI 和 GUI 产品会在独立仓库中以 MiniCore 为核心接入。详细能力已经按可实现的编程模块拆到 `docs/modules/`，避免把所有设计挤在一个长文档里。

MiniCore 的设计借鉴 pi coding-agent 的实际生产路径：`AgentSessionRuntime` 负责会话切换与宿主生命周期，`AgentSession` 负责单个会话的产品级 Agent 编排，底层 `Agent` / `agent-loop` 负责模型和工具循环。MiniCore 不复制 pi-agent-core 的 `AgentHarness` 作为主架构层，而是把产品级编排能力放入 `SessionRuntime`。

## 设计定位

MiniCore 使用 Rig 作为原生 Agent SDK，但 Rig 必须保持为实现细节。下游 CLI、TUI 和 GUI 宿主只通过运行时命令、运行时事件和运行时快照交互，不依赖 Rig 类型、模型提供方类型或工具实现细节。

MiniCore 不重新实现 Rig 的核心 Agent loop。Rig 负责 `AgentRun` / `AgentRunStep` 的状态机推进；MiniCore 通过 `Driver` 适配 Rig，把工具调用委托给运行时工具网关，并把底层活动映射成产品事件。

MiniCore 也不重新实现 provider HTTP clients。真实模型调用通过 `ModelGateway` 复用 Rig provider system；MiniCore 在该边界内治理 provider/model 解析、凭据、custom base URL、hook、fallback、usage 和错误分类。

## 分层

```text
Downstream CLI     Downstream TUI     Downstream GUI
      │                  │                  │
      ▼                  ▼                  ▼
CLI Adapter       Ratatui Adapter     Tauri/Vue Adapter
      │                  │                  │
      └────── AgentRuntimeProtocol ─────────┘
                         │
                         ▼
                 MiniCore AgentRuntime
       ┌────────────────────┬────────────────────┬────────────────────┐
       ▼                    ▼                    ▼                    ▼
 WorkspaceServices     CwdServiceRegistry   CommandSurface      RuntimeHooks
       │                    │
       │                    └── CwdScopedServices { cwd, generation }
       │                               ├─ ResourceLoader
       │                               └─ ModelGateway
       ▼
 SessionManager
       │
 LoadedSessionRuntimes
       │
 SessionRuntime ── pins CwdScopedServices generation
       ├─ Driver ───────────────▶ Rig AgentRun
       └─ ToolGateway / Prompt ─▶ pinned cwd resources
```

## 文档地图

- [模块总览](modules/README.md)：整体模块关系、Rig / Runtime / 下游 UI 宿主的边界。
- [AgentRuntime](modules/agent-runtime.md)：UI 无关的运行时门面、`WorkspaceServices` / `CwdScopedServices`、会话打开/聚焦和工作区生命周期。
- [SessionRuntime](modules/session-runtime.md)：单会话产品级编排、阶段、队列、Hook、turn state 和 post-run 流程。
- [AgentRuntimeProtocol](modules/agent-runtime-protocol.md)：`agent_runtime_protocol::Command`、`agent_runtime_protocol::Event`、`agent_runtime_protocol::EventMsg`、`agent_runtime_protocol::RuntimeSnapshot` 和下游 adapter 调用方式。
- [AgentRuntimeEvents](modules/agent-runtime-events.md)：Codex-like `agent_runtime_protocol::Event { ..., msg }`、事件生命周期、保存点、重连和跨模块事件顺序。
- [SessionManager / SessionStorage](modules/session-manager.md)：会话生命周期、已加载会话运行时、追加式 session tree、JSONL 存储、会话管理与存储接口、上下文重建。
- [ResourceLoader](modules/resource-loader.md)：资源来源聚合、刷新和资源诊断。
- [CommandSurface](modules/command-surface.md)：跨 UI 的斜杠命令目录、Parse / Plan / Execute / Present 四阶段、运行时命令映射和 command presentation。
- [RuntimeHooks](modules/runtime-hooks.md)：内部 hook seam、hook/event 边界、capability、typed result 和安全点。
- [Skills](modules/skills.md)：`skills.rs` 平级模块，提供技能 metadata、catalog、发现、解析、校验和格式化 helper。
- [Prompt](modules/prompt.md)：`prompt.rs` 纯构建模块，拼装最终 system prompt。
- [Driver](modules/driver.md)：Rig `AgentRun` / `CallModel` / `CallTools` 的适配职责。
- [ModelGateway](modules/model-gateway.md)：provider/model/auth 执行边界、custom provider、Rig provider adapter、usage/error/fallback 规则。
- [Tools](modules/tools.md)：`tools.rs` / `tools/` 平级模块，提供工具定义、内置工具、外部工具适配和执行 helper。
- [Compaction](modules/compaction.md)：`compaction.rs` 平级模块，提供上下文压缩准备、摘要 prompt、压缩摘要消息和自动压缩语义。
- [UsageStats](modules/usage-stats.md)：token 消耗、run/session stats、context usage 和 UI 展示口径。
- [实现路线图](implementation-roadmap.md)：MVP 到后续增强的开发顺序和设计约束。

## 核心边界

- 下游 CLI/TUI/GUI 不能导入 Rig 类型。
- 下游 CLI/TUI/GUI 不能直接调用模型提供方、执行工具、读取凭据、扫描技能或读写会话文件。
- Rig 拥有 agent loop 的协议级状态机，不拥有产品级工具治理、会话持久化或 UI 呈现。
- `AgentRuntime` 是下游 CLI/TUI/GUI 共用的稳定 runtime 门面。
- `SessionManager` 协调持久化会话和已加载会话运行时；`LoadedSessionRuntimes` 是它的内部 live runtime map，不作为独立架构层。多个 `SessionRuntime` 可以同时 loaded/running，并各自 pin `CwdScopedServices` generation。
- `SessionRuntime` 是单个会话的产品级编排层。
- `CommandSurface` 属于运行时命令入口：下游 UI 可以渲染 autocomplete / command palette / picker / popup，但不拥有 `/...` 的权威解析、执行映射或用户可见结果语义。
- `RuntimeHooks` 是 MiniCore 内部扩展点系统。Hook 可以在安全点返回 typed decision / patch / replacement，但不能直接发布 `agent_runtime_protocol::Event`、读写 session storage、执行工具或读取凭据。
- `Driver` 是 Rig 状态机和产品运行时之间的执行适配层。
- `ModelGateway` 是真实模型调用边界；`Driver` 只传 `ModelSelection`，不解析 provider、凭据、base URL 或 raw payload。
- 工具注册、活跃工具、审批、沙箱和真实副作用执行由 `SessionRuntime` 持有并通过 `ToolGateway` 统一治理；`Tools` 模块只提供工具定义和执行 helper。
- 上下文压缩由 `SessionRuntime` 编排，`Compaction` 模块提供准备、摘要 prompt 和压缩摘要消息 helper；`Driver` 不执行压缩。
- 技能、提示模板、上下文文件、会话管理与会话存储属于 MiniCore 运行时，不属于下游 UI。

## pi 经验映射

```text
AgentSessionRuntime
  └─ AgentSession
       └─ pi-agent-core Agent
            └─ runAgentLoop / runAgentLoopContinue
```

| pi coding-agent 生产路径 | MiniCore 概念 |
| --- | --- |
| `AgentSessionRuntime` | `AgentRuntime` + `SessionManager` 的会话打开、替换、fork、import 和服务重建能力 |
| `AgentSession` | `SessionRuntime` |
| `pi-agent-core Agent` | `Driver` 周边的运行状态、abort、waitForIdle、queue reducer |
| `runAgentLoop` | Rig 的 `AgentRun` / `AgentRunStep` / `drive_agent` |
| `DefaultResourceLoader` | `ResourceLoader` |
| `SessionManager` | `SessionManager` / `SessionHandle` / `SessionStorage` |
| `ExtensionRunner` | 后续 `RuntimeHooks` / 扩展运行时 |
| extension event hooks | `RuntimeHooks` 内部 hook seam，后续由可信 extension/package 注册 |

## 相关决策

- [ADR 0001：在 UI 无关的 Agent 运行时后使用 Rig](adr/0001-use-rig-behind-agent-runtime.md)
- [ADR 0002：上下文压缩由 SessionRuntime 编排](adr/0002-compaction-is-session-runtime-owned.md)
- [ADR 0003：AgentRuntimeEvents 使用 EventMsg、配对生命周期和单 run 终态](adr/0003-agent-runtime-events-use-event-msg-and-lifecycle-pairs.md)
- [ADR 0004：SessionManager 拥有已加载会话运行时](adr/0004-session-manager-owns-loaded-session-runtimes.md)
- [ADR 0005：ResourceLoader 是运行时内部资源服务](adr/0005-resource-loader-is-runtime-internal.md)
- [ADR 0006：CommandSurface 是跨 UI 的运行时命令入口](adr/0006-command-surface-is-runtime-command-surface.md)
- [ADR 0007：CommandSurface 使用 CommandPresentation 呈现用户可见结果](adr/0007-command-surface-uses-command-presentation.md)
- [ADR 0008：RuntimeHooks 是内部安全点扩展缝，不是协议事件或 UI 插件 API](adr/0008-runtime-hooks-are-internal-safe-point-seams.md)
- [ADR 0009：ModelGateway 包装 Rig providers](adr/0009-model-gateway-wraps-rig-providers.md)
