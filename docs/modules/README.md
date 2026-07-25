# 模块总览（V2 当前架构）

本目录是 **MiniCore V2 当前权威架构**的模块文档集合。每篇对应一个可实现的运行时子系统，描述它拥有的领域事实、窄 interface、职责边界和不应承担的事情。下游 CLI、TUI、GUI 仓库通过 `MiniCoreRuntime` 公开协议接入这些模块，但不在本目录实现完整产品 UI。

> **权威顺序**：当前架构文档（`docs/architecture.md` 与本目录）→ 当前 ADR（`docs/adr/`，0100+）→ `docs/research/` → `docs/archive/v1/`（非权威，仅历史参考）。正式文档不链接 V1 归档；只有 `docs/migration/v1-to-v2.md` 和新 ADR 的历史依据部分可以引用它。

## 领域关系

```text
MiniCoreRuntime
└─ Agent*
   └─ Session*
      └─ Turn*
         └─ Item*
            └─ Interaction*
```

`MiniCoreRuntime` 是外部宿主接触 MiniCore 的唯一顶层门面，并在 Runtime 生命周期内拥有三个长生命周期深模块 `PromptService`、`ToolService`、`SkillService`。Turn执行边界捕获或产生独立不可变的有效对象（`PromptSet` / `ToolSet` / `SkillView`）。领域基础模型见 [MiniCore 架构](../architecture.md)。

当前仓库仍处于设计完成、生产实现待启动阶段，没有Rust crate或自动化测试。下一实现里程碑是[迁移记录](../migration/v1-to-v2.md#阶段-6-8-模型调用协同交付束)中的阶段6–8协同交付束：SessionExecutor、ModelGateway和Compaction通过同一scripted vertical slice共同落地。

## 模块索引

- [Runtime 公开协议](runtime-interface.md)：`MiniCoreRuntime` 的 `dispatch / query / snapshot / subscribe` 四类能力、公开领域 identity、scoped cursor/snapshot 和协议边界。
- [Agent 与 Session 生命周期](agent-session-lifecycle.md)：Agent 定义与 `AgentRevision`、Session 定义与 `SessionDefinitionRevision`、create/load/unload/archive/fork、durable lifecycle 与 loaded execution state 的分离。
- [Workspace](workspace.md)：Session-owned Workspace definition、roots/cwd 合法域、trust 与 source authorization、filesystem capability，以及 Prompt/Tool/Skill 消费的窄只读 view。
- [Prompt](prompt.md)：`PromptService`共享`PromptResourceView`，各Turn独立构建`PromptSet`；`compose_user_message`产出`CanonicalUserMessage`，`assemble(...) -> AssembledModelContext`是模型可见上下文组装的唯一seam。
- [Skills](skills.md)：`SkillService`发布reloadable `SkillView`，与`LoadedSkill`分离；metadata discovery和正文按需加载。
- [Tools](tools.md)：`ToolService` 通过 `for_turn(...) -> ToolSet` 原子绑定模型可见 ToolSpec 与 executor route；registry、policy、approval、grants、sandbox、mutation lock 与 executor。
- [Turn 执行上下文](turn-execution-context.md)：`TurnExecutionContext` 的 capture 依赖图、fingerprint、reload 线性化、cancellation/Steer/FollowUp 与 AgentLoop 分界。
- [Turn / Item / Interaction](turn-item-interaction.md)：Turn 边界、`ItemContent`、`ToolInvocation` 合并 identity、`Interaction` request/resolution、UI/MiniCore职责、terminal cleanup 与保守恢复。
- [Conversation 与 SessionStorage](conversation-storage.md)：per-session append-only by-entry JSONL tree、`SessionWriter::append` 唯一写 seam、entry parent tree、conversation projection、fork 与 recovery。
- [Session 执行](session-execution.md)：一个loaded Session一个`SessionExecutor`、per-session semantic `SessionIngress` lanes、严格串行current `RunningOperation`、per-Turn Steer/FollowUp FIFO、`WaitingForUserInput`、sticky emergency/lifecycle control、AgentLoop `NeedModel | NeedTools | Finished`和multi-session并发。
- [ModelGateway](model-gateway.md)：`resolve_for_turn(...)` 固定 `TurnModelSnapshot`、`generate_model_turn(...)` 唯一真实模型调用、private Rig adapter、stream/retry/auth/usage/cache/continuation。
- [Compaction](compaction.md)：portable rolling summary、stable-unit safe cut、leading summary、per-instruction-segment active-Turn checkpoint、model-aware summary budget、`Compacting` 执行阶段与 `StoredCompaction` 恢复规则。

## 相关决策

长期架构决策记录在 [`docs/adr/`](../adr/)（0100+）：领域与 ownership、Workspace ownership、Prompt/Tool/Skill 边界、Turn/Item/Interaction、SessionStorage durable truth、SessionExecutor ownership、ModelGateway、Compaction、Runtime 公开协议以及UserQuestion的UI/Runtime职责分离。行为与接口以各模块文档、协议文档和 ADR 为权威。

## 权威归属

为避免同一概念在多个文档中漂移，按下面的 source of truth 维护：

| 概念 | 权威文档 |
| --- | --- |
| 公开协议类型、领域 identity、command/query/event/snapshot 分离 | [Runtime 公开协议](runtime-interface.md) |
| Agent/Session revision、durable/runtime lifecycle、fork | [Agent 与 Session 生命周期](agent-session-lifecycle.md) |
| Workspace definition、roots、trust、authorization、filesystem capability | [Workspace](workspace.md) |
| PromptSet、CanonicalUserMessage、PromptIntent 展开、AssembledModelContext | [Prompt](prompt.md) |
| SkillService、SkillView、LoadedSkill、lazy load、reload、cache | [Skills](skills.md) |
| ToolService、ToolSet、policy、approval、grants、sandbox、executor | [Tools](tools.md) |
| TurnExecutionContext capture、fingerprint、reload 线性化 | [Turn 执行上下文](turn-execution-context.md) |
| Turn/Item/Interaction identity、lifecycle、terminal cleanup | [Turn / Item / Interaction](turn-item-interaction.md) |
| UserQuestion producer seam与ask-user Tool route | [Tools](tools.md) |
| TurnExecutionPhase与WaitingForUserInput状态语义 | [Agent 与 Session 生命周期](agent-session-lifecycle.md) |
| UserQuestion公开view、Presentation Adapter与resolution protocol | [Runtime 公开协议](runtime-interface.md) |
| durable truth、entry tree、JSONL、conversation projection、recovery | [Conversation 与 SessionStorage](conversation-storage.md) |
| 单Session执行owner、SessionIngress lanes、唯一current RunningOperation、Steer/FollowUp FIFO、emergency/lifecycle control、multi-session并发 | [Session 执行](session-execution.md) |
| TurnModelSnapshot、generate_model_turn、provider adapter、stream/retry/usage | [ModelGateway](model-gateway.md) |
| 压缩触发、stable cut、summary directive、StoredCompaction | [Compaction](compaction.md) |

内存 projection、cache、snapshot 和 UI read model 只能由权威事实派生，不能成为并列 source of truth。

> Prompt Templates 目前作为 Prompt 子系统的内部能力，Usage Stats 目前作为 projection helper，均不单独设立正式模块。
