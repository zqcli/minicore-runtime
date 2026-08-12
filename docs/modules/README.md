# 模块总览（V2 当前架构）

本目录是MiniCore V2当前权威module设计。ADR 0126–0142冻结async conversation/public wire、DurableState/Store V1/root lease、Tokio owner-tracked foundations、Rust 1.85 provider dependency边界、Tool Sandbox pre-start fail-closed admission与M14 stateless full-request wire policy。M0–M8 foundations/behavior与ToolStartGate first-wins start gate、M9当前control/observation范围、M10完整Compaction vertical slice及M11 Session Fork command/storage、durable catalog/Fork provenance query、Runtime Session membership/lifecycle StateEvent、public Agent lifecycle/definition/metadata、public Session metadata、ordinary Session definition CAS、Agent revision upgrade、Ready-state `ReloadWorkspace` command/event、Workspace/Prompt Unavailable loaded readiness及ReloadWorkspace恢复与Agent readiness fan-out与ModelUnavailable及selected PromptUnavailable load/definition projection及shared-resource reload recovery/fanout与complete shared-root publication及active-Turn graceful Unload（default 30s/≤5min grace config、PrepareForUnload deadline signal与truthful settlement、shutdown broadcast）vertical slices及host security Workspace authority invalidation（`MiniCoreRuntime::invalidate_session_workspace_authority` host-only非wire seam、`SessionExecutorSnapshot.workspace_preparing`→`Preparing`最高readiness优先级、owner-tracked Workspace re-resolve recovery worker、start/finish各发布一次`session_readiness_changed`(command_id None)、close/fatal/reap exactly-once waiter settlement）及RuntimeDependencyUnavailable loaded readiness与probe recovery（唯一producer为loaded Turn admission读pinned historical AgentRevisionRef时的transient StorageUnavailable、owner-tracked无TurnId probe与Submit re-arm恢复）已实现；full recovery scenario/fixture closure已实现并通过统一质量门禁；完整cross-platform native matrix acceptance已通过（全部七个`platform_m5_0`坐标均有对应的production行为与测试覆盖；GitHub Actions run 31433810296四个job全部通过：Ubuntu Rust stable、Ubuntu Rust 1.85.0、cargo test macos-latest、cargo test windows-latest）；crate-private Structured output foundation已实现（`OutputContract::Structured` exact-model contract、schema v1 subset、terminal本地schema validation与ScriptedProviderAdapter conformance），crate-private `ToolOperationSlot`完整生命周期亦已实现（Prepared→Running→Settling→Terminal：per-slot first-wins start gate、typed started proof与PreExecution/Executed/Abandoned truthful settlement、Running cancellation pair与truthful settle），crate-private scripted approval/UserQuestion控制seam亦已完成（typed `ToolExecutionPlan::{Approval, UserQuestion}`拆分、concrete Session-owned `ToolExecutionControl`、move-only `UserQuestionAnswerBinding`、hoisted exclusive question调度与signal-first settlement）。M12/V4-P1-3已由ADR 0138/0139、OpenAI Responses/Anthropic Messages真实Rig standalone loopback evidence、terminal/metadata seam、26-case delivery/error fixture与真实Rust 1.85冷编译关闭；Rig被拒绝进入production baseline。M13/V4-C0-1已由ADR 0140、class-level Sandbox admission、approval revalidation与adapter-independent Session round conformance关闭；M14 OpenAI Responses/Anthropic Messages direct provider adapters、provider-native Structured strict mapping、host-only dynamic credential/catalog installation与explicit ignored live smoke harness已实现，stateless full-request wire policy已由[ADR 0141](../adr/0141-provider-calls-are-stateless-full-request.md)冻结为有意omission（不是pending实现）；实际real-credential live smoke与production Tool/Sandbox adapters仍待实现；production `ask_user` builtin（ToolName/schema与answer→model-visible ToolResult text/render格式已由ADR 0142冻结并实现：closed/default-off/`with_ask_user_tool()` opt-in、零permission、仅UserQuestion/frozen PreExecution plans、deterministic compact JSON answer）已实现；public structured activation、具体Skill composition/source、完整Tool policy/approval enforcement、schema/hooks/policy/Sandbox、Session-local mutation queue/mutation permit attachment to Settling、production ToolService/executor/adapters（返回前须提供有界、可确认cleanup）与public Tool DTO仍待实现。

权威顺序：[`docs/architecture.md`](../architecture.md)与本目录 → current/refined ADR → formats + fixtures → development plan → migration + research → archive。

## 领域关系

```text
MiniCoreRuntime
└─ Agent*
   └─ Session*
      └─ Turn*
         └─ Item*
            └─ Interaction*
```

目标Runtime持有`PromptService`、`ToolService`、`SkillService`和`ModelGateway`四个共享深module。后续行为slice中，每个loaded Session将由`SessionExecutor` control actor管理，并最多运行一个`ActiveTurnTask`。M4的`LiveSessionState` reducer提供current-process conversation truth；`SessionRecorder`与可恢复前缀属于M5。

## 模块索引

- [Wire Schema与Bounded Decode](wire-schema.md)：public JSON v1、shared scalar carriers、ProtocolLimits、bounded JSON和JSONL scanner floor。
- [Wire V1 Conformance Fixtures](../fixtures/wire-v1/README.md)：public manifest、byte-exact JSON/JSONL、corruption expectations、all-limit recipes与structural verifier。
- [Runtime公开协议](runtime-interface.md)：`dispatch / query / snapshot / subscribe`、公开identity和live observer语义。
- [Agent与Session生命周期](agent-session-lifecycle.md)：definition/revision、create/load/unload/archive/fork与readiness。
- [Workspace](workspace.md)：Session-owned Workspace、trust、authorization和immutable snapshot。
- [Prompt](prompt.md)：PromptIntent、CanonicalUserMessage、safe part-level contribution provenance、Prompt-owned opaque `ModelMessage` read refs和crate-private AssembledModelContext。
- [Skills](skills.md)：SkillService、shared SkillResourceView、Turn-pinned SkillView和reload。
- [Tools](tools.md)：ToolSet、policy、approval、sandbox、executor和Session-local file mutation queue。
- [Turn执行上下文](turn-execution-context.md)：immutable capture、ConversationRevision和ModelCallRequest basis。
- [Turn / Item / Interaction](turn-item-interaction.md)：live lifecycle、complete Tool exchange、Interaction和Tool start gate。
- [Conversation JSONL Format V1](../formats/conversation-jsonl-v1.md)：exact Stored DTO envelope、field order、limits与corruption vectors。
- [Durable Store V1](../formats/durable-store-v1.md)：exact local entity layout、head/definition bytes、markers与strict recovery。
- [Durable Store V1 Fixtures](../fixtures/durable-store-v1/README.md)：golden documents、crash taxonomy与structural verifier。
- [DurableState](durable-state.md)：private actor、reservation/root lease/CAS/generation/publication/recovery/fault seam。
- [Conversation Recording与Replay](conversation-storage.md)：LiveSessionState reducer transaction/capture、SessionRecorder、best-effort JSONL prefix、RecordingHealth、tolerant replay和fork semantic seed。
- [Session执行](session-execution.md)：SessionExecutor actor、ActiveTurnTask、async run loop、Steer/FollowUp/Cancel。
- [ModelGateway](model-gateway.md)：TurnModelSnapshot、single provider attempt、stream、usage和typed errors。
- [M12 Production Provider Gate Fixtures](../fixtures/provider-gate-m12/README.md)：OpenAI Responses/Anthropic Messages Rig standalone evidence、SDK rejection与26-case delivery/error mapping。
- [Compaction](compaction.md)：revision-bound stable units、Runtime settings、source+cut marker、summary budget与best-effort recording。

## 权威归属

| 概念 | Canonical Owner |
| --- | --- |
| public/storage JSON representation、shared scalar carriers、ProtocolLimits与bounded decode | [Wire Schema与Bounded Decode](wire-schema.md) |
| 公开command/query/event/snapshot semantic payload | [Runtime公开协议](runtime-interface.md) |
| Agent/Session lifecycle与revision | [Agent与Session生命周期](agent-session-lifecycle.md) |
| Workspace与authority | [Workspace](workspace.md) |
| PromptIntent、CanonicalUserMessage、contribution provenance、opaque ModelMessage与model context assembly | [Prompt](prompt.md) |
| Skill discovery/load/reload | [Skills](skills.md) |
| Tool policy/approval/sandbox/execution | [Tools](tools.md) |
| immutable Turn capture与live execution basis | [Turn执行上下文](turn-execution-context.md) |
| Turn/Item/Interaction与Tool exchange | [Turn / Item / Interaction](turn-item-interaction.md) |
| exact conversation JSONL v1 envelope与Stored DTO projection | [Conversation JSONL Format V1](../formats/conversation-jsonl-v1.md) |
| local entity physical layout、root lease、reservation/generation/marker publication、catalog recovery | [DurableState](durable-state.md) / [Durable Store V1](../formats/durable-store-v1.md) |
| Live reducer transaction/capture、JSONL semantic recording、RecordingHealth与cold replay | [Conversation Recording与Replay](conversation-storage.md) |
| control actor、ActiveTurnTask与async loop | [Session执行](session-execution.md) |
| provider attempt与response validation | [ModelGateway](model-gateway.md) |
| compaction planning与summary validation | [Compaction](compaction.md) |

## 当前实现顺序

完整阶段、依赖、测试分层与退出条件见[MiniCore V2开发计划](../development-plan.md)。M0–M13已实现并通过统一review/check；M11关闭Session Fork command/storage、durable Agent/Session catalog/Fork provenance query、Runtime Session membership StateEvent、public Agent mutation、public Session metadata、ordinary Session definition CAS、Agent revision upgrade、Ready-state `ReloadWorkspace`、Workspace/Prompt/Agent/Model/selected Prompt/RuntimeDependency loaded readiness与恢复、shared-resource reload recovery/fanout、active-Turn graceful Unload、host security Workspace authority invalidation及full recovery scenario/fixture closure。Structured output foundation已实现，provider-native schema strict mapping亦已实现而public activation仍pending；crate-private `ToolOperationSlot`完整生命周期已实现（Prepared→Running→Settling→Terminal：per-slot first-wins gate、typed started proof、Running cancellation pair与truthful settle）；crate-private scripted approval/UserQuestion控制seam已实现（typed `ToolExecutionPlan::{Approval, UserQuestion}`、private concrete Session-owned `ToolExecutionControl`复用既有Interaction owner、Tools-owned move-only/redacted `UserQuestionAnswerBinding`、hoisted exclusive question调度与signal-first settlement）；M14 production `ask_user` builtin已由ADR 0142冻结并实现（closed/default-off/`with_ask_user_tool()` opt-in、零permission、仅UserQuestion/frozen PreExecution plans、deterministic compact JSON answer）；完整cross-platform native matrix实现已交付（统一native matrix acceptance已通过：GitHub Actions run 31433810296四job全绿）。V4-P1-3 provider gate已关闭并拒绝Rig进入production baseline；V4-C0-1 Tool/Sandbox gate也已由ADR 0140关闭。两个direct production Provider adapters与stateless full-request wire policy（ADR 0141）已实现；实际real-credential live smoke与production Tool/Sandbox adapters仍属于M14，首个file-mutation adapter另须满足ADR 0116。

跨模块高风险规则见[架构总览的不变量索引](../architecture.md#跨模块不变量索引)。
