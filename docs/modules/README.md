# 模块总览（V2 当前架构）

本目录是MiniCore V2当前权威module设计。ADR 0126已将执行模型更新为async Turn loop与inline best-effort Session recording；ADR 0129/0130已冻结用户消息contribution与async captured-Skill composition；ADR 0131冻结conversation-only recording owner；ADR 0132冻结Compaction stable-unit/settings/provenance contract；ADR 0133冻结snapshot-recoverable Runtime public payload；ADR 0134、exact Format V1与conformance vectors冻结bounded public/storage wire v1。Rust Wire基础已实现typed scalar/value/path carriers；Runtime、Session、Conversation与provider尚未实现。

权威顺序：[`docs/architecture.md`](../architecture.md)与本目录 → Accepted ADR → `docs/research/` → `docs/archive/v1/`。

## 领域关系

```text
MiniCoreRuntime
└─ Agent*
   └─ Session*
      └─ Turn*
         └─ Item*
            └─ Interaction*
```

Runtime持有`PromptService`、`ToolService`、`SkillService`和`ModelGateway`四个共享深module。每个loaded Session由`SessionExecutor` control actor管理，并最多运行一个`ActiveTurnTask`。当前进程的`LiveSessionState`驱动Model、Tool和UI；`SessionRecorder`异步记录可恢复前缀。

## 模块索引

- [Wire Schema与Bounded Decode](wire-schema.md)：public JSON v1、shared scalar carriers、ProtocolLimits、bounded JSON和JSONL scanner floor。
- [Wire V1 Conformance Fixtures](../fixtures/wire-v1/README.md)：public manifest、byte-exact JSON/JSONL、corruption expectations、all-limit recipes与structural verifier。
- [Runtime公开协议](runtime-interface.md)：`dispatch / query / snapshot / subscribe`、公开identity和live observer语义。
- [Agent与Session生命周期](agent-session-lifecycle.md)：definition/revision、create/load/unload/archive/fork与readiness。
- [Workspace](workspace.md)：Session-owned Workspace、trust、authorization和immutable snapshot。
- [Prompt](prompt.md)：PromptIntent、CanonicalUserMessage、safe part-level contribution provenance、`LiveConversationView`和AssembledModelContext。
- [Skills](skills.md)：SkillService、shared SkillResourceView、Turn-pinned SkillView和reload。
- [Tools](tools.md)：ToolSet、policy、approval、sandbox、executor和Session-local file mutation queue。
- [Turn执行上下文](turn-execution-context.md)：immutable capture、ConversationRevision和ModelCallRequest basis。
- [Turn / Item / Interaction](turn-item-interaction.md)：live lifecycle、complete Tool exchange、Interaction和Tool start gate。
- [Conversation JSONL Format V1](../formats/conversation-jsonl-v1.md)：exact Stored DTO envelope、field order、limits与corruption vectors。
- [Conversation Recording与Replay](conversation-storage.md)：SessionRecorder、best-effort JSONL prefix、RecordingHealth、tolerant replay和fork。
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
| PromptIntent、CanonicalUserMessage、contribution provenance与model context assembly | [Prompt](prompt.md) |
| Skill discovery/load/reload | [Skills](skills.md) |
| Tool policy/approval/sandbox/execution | [Tools](tools.md) |
| immutable Turn capture与live execution basis | [Turn执行上下文](turn-execution-context.md) |
| Turn/Item/Interaction与Tool exchange | [Turn / Item / Interaction](turn-item-interaction.md) |
| exact conversation JSONL v1 envelope与Stored DTO projection | [Conversation JSONL Format V1](../formats/conversation-jsonl-v1.md) |
| JSONL semantic recording、RecordingHealth与cold replay | [Conversation Recording与Replay](conversation-storage.md) |
| control actor、ActiveTurnTask与async loop | [Session执行](session-execution.md) |
| provider attempt与response validation | [ModelGateway](model-gateway.md) |
| compaction planning与summary validation | [Compaction](compaction.md) |

## 当前实现顺序

完整阶段、依赖、测试分层与退出条件见[MiniCore V2开发计划](../development-plan.md)。M0文档收敛/质量门禁已经完成，当前暂停于M1.1前；随后依次完成bounded Wire core、public/storage conformance、LiveConversation与ScriptedProvider vertical slice；production Provider与Tool/Sandbox分别受V4-P1-3和V4-C0-1门禁约束。

跨模块高风险规则见[架构总览的不变量索引](../architecture.md#跨模块不变量索引)。
