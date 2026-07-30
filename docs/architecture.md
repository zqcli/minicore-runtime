# MiniCore 架构（V2 当前权威）

本文档是MiniCore原生Agent harness runtime core的架构总入口。详细设计位于[`docs/modules/`](modules/README.md)。

## 版本状态

| 版本 | 状态 |
| --- | --- |
| V1 | 已归档，只保存在[`docs/archive/v1/`](archive/v1/README.md)和Git history中。 |
| V2 | 当前权威架构。ADR 0126已把执行模型重构为async Turn loop与inline best-effort Session recording；生产实现待启动。 |

权威顺序：本文与`docs/modules/` → Accepted ADR → `docs/research/` → `docs/archive/v1/`。

## 设计定位

MiniCore采用Codex式执行结构：每个loaded Session有一个`SessionExecutor` control actor和最多一个`ActiveTurnTask`。ActiveTurnTask使用普通async loop顺序编排Model、Tool、Interaction、logical retry和Compaction；不再实现同步sans-I/O `AgentLoop`、`next_action()`或`RunningOperation` effect协议。

Session的当前进程事实由`LiveSessionState`拥有。`SessionRecorder`在live mutation后inline await当前JSONL line的best-effort append；成功不表示flush或fsync，失败不回滚live state或重放外部操作。process crash后只恢复实际留下的完整行前缀。

Rig只实现`ModelGateway` private `ProviderAdapter`的单次provider attempt。Model resolution、request validation、credential、response validation和provider-neutral terminal result仍由ModelGateway拥有；logical retry由ActiveTurnTask拥有。

下游CLI、TUI和GUI只通过`MiniCoreRuntime`的command、query、snapshot和event interface接入。

## 领域模型

```text
MiniCoreRuntime
└─ Agent*
   └─ Session*
      └─ Turn*
         └─ Item*
            └─ Interaction*
```

核心关系：

- 一个Agent可被多个Session引用；一个Session固定归属一个Agent；
- Workspace属于`SessionDefinition`，active Turn捕获immutable `WorkspaceSnapshot`；
- Prompt、Tool和Skill是独立module；
- Turn/Item/Interaction是领域对象，Model request、provider stream、Tool future和ActiveTurnTask是process-local执行对象；
- Session log用于resume、history和诊断，不证明当前进程全部live事实已经durable；
- restart不恢复旧ActiveTurnTask、provider stream、Tool task、Interaction waiter、retry timer或queue。

## Runtime-Owned共享模块

```rust
pub struct MiniCoreRuntime {
    prompt_service: Arc<PromptService>,
    tool_service: Arc<ToolService>,
    skill_service: Arc<SkillService>,
    model_gateway: Arc<ModelGateway>,
    shared_resources: RwLock<SharedResourceRoots>,
}
```

```text
captured SharedResourceRoots + Session/Agent/Workspace facts
├─ ModelGateway::resolve_for_turn → Arc<TurnModelSnapshot>
├─ SkillService::for_turn         → Arc<SkillView>
├─ ToolService::for_turn          → Arc<ToolSet>
└─ PromptService::for_turn        → Arc<PromptSet>
```

active Turn始终使用admission时捕获的immutable对象。显式reload只影响future Turn。

## Loaded Session结构

```text
MiniCoreRuntime
└─ LoadedSessionExecutors
   └─ SessionExecutor
      ├─ SessionIngress
      ├─ LiveSessionState
      ├─ SessionRecorder
      ├─ SessionSnapshot publisher
      └─ optional ActiveTurnTask
```

`SessionExecutor`拥有Session级control、lifecycle、FollowUp、active-task handle和公开snapshot。`ActiveTurnTask`拥有当前Turn的async control flow、phase、Model/Tool future、retry timer和compaction orchestration。

`LiveSessionState`通过private typed methods更新。若使用锁，guard不得跨任何I/O或await。SessionExecutor与ActiveTurnTask不得分别维护可独立修改的conversation副本。

## Session Recording

live mutation顺序：

```text
validate domain mutation
→ allocate stable IDs
→ apply LiveSessionState / LiveConversation
→ await SessionRecorder.record(entry)
→ publish final StateEvent / resume waiter / continue loop
```

`record().await`顺序encode并执行当前JSONL line的`write_all`，不使用后台queue。成功不表示flush、fsync或power-loss durability。第一次encode/write失败后Recorder进入`Degraded`并停止该loaded Session的后续记录；Turn继续运行。Degraded在同一load内为终态，不retry、不创建segment、不backfill。

Cold replay：

- 顺序读取完整JSONL行；
- skip malformed/duplicate并隔离orphan或invalid relation；
- 重建recorded history和sanitized model conversation；
- incomplete Tool exchange不进入模型conversation；
- recorded unfinished Turn在新进程中按restart interruption处理；
- writable Load只截断final unterminated partial tail，再从replayed recorded head初始化新Recorder；
- Unload/Load只恢复recorded prefix，未record的live tail永久丢失。

## Turn执行

```text
Submit admission
→ capture TurnExecutionContext
→ apply live Input + await inline record attempt
→ spawn ActiveTurnTask
→ async run_turn loop
   ├─ consume safe-point Steer
   ├─ PromptSet.assemble(LiveConversationView)
   ├─ await ModelGateway
   ├─ await ToolSet / Interaction
   ├─ logical retry or Compaction
   └─ final arbitration
→ return TurnTaskOutcome
→ SessionExecutor settles lifecycle and FollowUp
```

同Session只有一个ActiveTurnTask。多个Session可以同时调用共享ModelGateway；Gateway没有本地模型调用permit。

## Conversation与Tool Exchange

模型输入只来自sanitized `LiveConversationView`：

```text
Assistant(tool calls A/B/C) applied live
→ Tool A/B/C may run
→ results applied in completion order
→ all expected calls have first truthful ToolResult
→ reducer exposes ordered Assistant + Result A/B/C exchange
→ next Model allowed
```

complete exchange门禁保留，但owner是live conversation reducer。SessionRecorder或cold projector不再向执行loop签发`CommittedToolExchangeDelta`。

每个assistant或ToolResult live mutation在启动下一protocol step前完成inline record attempt。crash或Degraded仍可能只留下assistant ToolCall或部分结果；replay sanitizer排除整个不完整exchange。Tool不会因记录缺失自动重跑。

## Interaction

```text
apply Pending Interaction live
→ await inline record request attempt
→ publish InteractionView
→ await oneshot resolution
→ validate and apply resolution live
→ await inline record resolution attempt
→ resume waiter / authorize Tool
```

recording failure不阻止Interaction。request/resolution可能在crash后缺失；restart不恢复waiter。

## Cancel与Security

- Cancel发布sticky epoch后立即返回`CancelAccepted`；
- ActiveTurnTask停止新Model、Tool和Compaction；
- Running Tool执行best-effort cancel并truthful settle；
- FollowUp等待旧task terminal后再启动；
- Tool start继续通过`ToolStartGate`与EmergencyControl first-wins；
- Workspace update仍只在Idle；SecurityRevoked后重新resolve失败仍可进入Unavailable。

## Logical Retry

Logical retry是ActiveTurnTask局部流程：

```text
retryable terminal ModelCallError
→ verify Turn/control_generation/conversation_revision
→ cancellation-aware sleep
→ reuse same Arc<ModelCallRequest>
→ invoke ModelGateway again
```

不再存在`RunningOperation::WaitForModelRetry`或基于durable `ConversationCheckpoint.entry_id`的live校验。

## Compaction

ActiveTurnTask从sanitized live conversation构建plan，使用同一PromptSet/ModelGateway路径生成summary。验证后先Replace live conversation，再inline attempt record `StoredCompaction`。marker未写入时，restart恢复未压缩的旧conversation；这是best-effort recording允许的降级。

## 状态与观察

- `SessionExecutionState = Idle | Starting | Running | Finishing`；
- `TurnExecutionPhase = Sampling | ExecutingTools | WaitingApproval | WaitingForUserInput | RetryBackoff | Compacting`；
- StateEvent和Snapshot描述live state；recording Degraded或process crash时，它们可能领先可恢复的recorded prefix；Healthy状态没有后台queue lag；
- ProgressEvent仍可合并或丢弃；
- `SessionSnapshot.recording.state = healthy | degraded | disabled`；first `Healthy → Degraded`先由当前domain event发布Degraded Snapshot，再补发一次`session_recording_changed`；Snapshot保留当前脱敏recording diagnostic；
- StateEvent不是durable acknowledgement。

## 跨模块不变量索引

只有跨至少三个module且影响correctness/security的规则进入本表。

| ID | 不变量摘要 | Canonical Owner |
| --- | --- | --- |
| INV-001 | live mutation先apply，再完成inline record attempt，随后publish final state或推进协议；record outcome不提供durable execution permit | [Conversation Recording · Live Mutation](modules/conversation-storage.md#live-mutation-and-recording) |
| INV-002 | cold replay只恢复recorded完整行前缀，局部skip/isolate并返回diagnostics，不恢复process-local对象 | [Conversation Recording · Tolerant Replay](modules/conversation-storage.md#tolerant-replay) |
| INV-003 | 含ToolCall的assistant只有在全部matching truthful results形成provider-valid complete exchange后才model-visible | [Turn / Item / Interaction · Complete Tool Exchange](modules/turn-item-interaction.md#complete-tool-exchange) |
| INV-101 | 每个loaded Session只有一个control actor和最多一个ActiveTurnTask；同Session不得并行运行两个Turn task | [Session Execution · Ownership](modules/session-execution.md#ownership) |
| INV-102 | Steer只在完整assistant/tool step后、下一次Model前FIFO消费 | [Session Execution · Steer](modules/session-execution.md#steer) |
| INV-201 | active Turn只使用admission时captured immutable Workspace/Prompt/Skill/Tool/Model对象 | [Turn Execution Context · Context Capture](modules/turn-execution-context.md#context-capture) |
| INV-301 | Interaction live request在notify前apply并完成inline record attempt；resolution在resume前apply并完成inline record attempt | [Turn / Item / Interaction · Interaction Ordering](modules/turn-item-interaction.md#interaction-ordering) |
| INV-401 | Tool side-effect start由ToolStartGate与EmergencyControl first-wins；Running后只能truthful settle | [Turn / Item / Interaction · Tool Side-Effect Start](modules/turn-item-interaction.md#tool-side-effect-start) |

## 模块地图

- [Runtime公开协议](modules/runtime-interface.md)：dispatch/query/snapshot/subscribe和live observer语义。
- [Agent与Session生命周期](modules/agent-session-lifecycle.md)：definition、revision、load/unload/archive/fork。
- [Workspace](modules/workspace.md)：Session-owned Workspace、authority和immutable snapshot。
- [Prompt](modules/prompt.md)：PromptSet、CanonicalUserMessage和live model context assembly。
- [Skills](modules/skills.md)：SkillService、SkillView和reload。
- [Tools](modules/tools.md)：ToolSet、policy、approval、sandbox和executor。
- [Turn执行上下文](modules/turn-execution-context.md)：immutable capture和ConversationRevision basis。
- [Turn / Item / Interaction](modules/turn-item-interaction.md)：live lifecycle、Tool exchange和Interaction。
- [Conversation Recording与Replay](modules/conversation-storage.md)：JSONL recorder、recording health、tolerant replay和fork。
- [Session执行](modules/session-execution.md)：control actor、ActiveTurnTask、async loop和queues。
- [ModelGateway](modules/model-gateway.md)：single provider attempt和response taxonomy。
- [Compaction](modules/compaction.md)：live rolling summary与best-effort marker。

## 相关决策

核心当前决策：

- [ADR 0126：Turn执行使用async loop，Session记录采用inline best-effort append](adr/0126-turn-execution-is-async-and-session-recording-is-best-effort.md)
- [ADR 0125：ModelGateway不设置本地模型调用Permit](adr/0125-model-gateway-has-no-local-call-permits.md)
- [ADR 0124：Session replay宽容恢复](adr/0124-session-replay-is-tolerant-and-links-are-minimal.md)，其strict writer/committed-delta条款已被ADR 0126取代
- [ADR 0123：Exact Ref、immutable capture与explicit reload](adr/0123-identity-uses-refs-and-explicit-reload.md)，其durable checkpoint条款已被ADR 0126取代
- [ADR 0119：Session logical retry](adr/0119-model-calls-use-session-logical-retries.md)，owner已改为ActiveTurnTask
- [ADR 0118：Cancel立即确认并等待settlement](adr/0118-cancel-acknowledges-immediately-and-followup-waits-for-settlement.md)
- [ADR 0116：文件mutation使用Session-local queue](adr/0116-file-mutations-use-session-local-queues.md)

ADR 0104的durable truth/commit barrier、ADR 0105的single mutable Executor owner和ADR 0115的同步AgentLoop已被ADR 0126取代。
