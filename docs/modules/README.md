# 模块总览（V2 当前架构）

本目录是MiniCore V2当前权威module设计。ADR 0126–0137冻结async conversation/public wire、DurableState/Store V1/root lease与Tokio owner-tracked foundations。M5.0 durable foundation与exact historical definition resolution已实现；M6.1 Workspace resolver/Snapshot、loaded Ready+Idle publication owner及Runtime-owned residency actor/Load/Unload/lifecycle exclusion foundation已实现；public Runtime command/query/snapshot/event、remaining Fork anchors/LiveSnapshot、replay/Recorder-backed full Load、active-Turn grace Unload、完整cross-platform native matrix、完整SessionExecutor Turn control/ActiveTurnTask、M8 public DTO、M10 compaction与provider/Tool adapter行为仍待实现。

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

完整阶段、依赖、测试分层与退出条件见[MiniCore V2开发计划](../development-plan.md)。M0、M1、M2 minimal Snapshot/Event、M3.1、M3.2、M4与M5.0 design gate已完成；M5.0 durable foundation、M6.1 Workspace resolver/Snapshot、loaded Ready+Idle publication与Runtime residency foundation已实现；public Runtime command/query/snapshot/event、remaining Fork anchors/LiveSnapshot、replay/Recorder-backed full Load、active-Turn grace Unload、完整cross-platform native matrix pending。production Provider与Tool/Sandbox分别受V4-P1-3和V4-C0-1门禁约束。

跨模块高风险规则见[架构总览的不变量索引](../architecture.md#跨模块不变量索引)。
