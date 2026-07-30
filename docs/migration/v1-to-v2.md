# MiniCore V1 → V2 版本迁移记录

状态：V2目标架构已按ADR 0126重构，生产实现待启动
日期：2026-07-30

## 目的

本文记录V1到V2的目标module、实现顺序和完成门槛。类型与行为以`docs/architecture.md`、`docs/modules/`和Accepted ADR为权威。

## V2目标

```text
MiniCoreRuntime
└─ Agent*
   └─ Session*
      └─ Turn*
         └─ Item*
            └─ Interaction*
```

Runtime拥有四个共享深module：

```text
PromptService
ToolService
SkillService
ModelGateway
```

每个loaded Session：

```text
SessionExecutor control actor
├─ SessionIngress
├─ LiveSessionState
├─ SessionRecorder
├─ SessionSnapshot publisher
└─ optional ActiveTurnTask
```

ActiveTurnTask使用普通async loop：

```text
LiveConversationView
→ PromptSet.assemble
→ ModelCallRequest::new
→ await ModelGateway
→ optional await ToolSet / Interaction
→ live mutation + record attempt
→ next Model or terminal
```

## 2026-07-30重构

ADR 0126取代了此前以下目标机制：

```text
sync sans-I/O AgentLoop
next_action / accept_* effect protocol
single current RunningOperation
SessionWriter strict append/apply
storage-owned committed deltas
ConversationCheckpoint EntryId live proof
append-before-StateEvent/next Model barrier
writer failure → read-only/Unavailable
```

新规则：

- LiveSessionState是current-process truth；
- SessionRecorder只保存有序best-effort JSONL prefix；
- live apply成功后完成inline record attempt，再publish/continue；
- recording failure只更新health；
- cold replay只恢复recorded prefix；
- complete Tool exchange仍是Model visibility门禁，但由live reducer拥有；
- logical retry由ActiveTurnTask使用ConversationRevision验证；
- Compaction先Replace live conversation，再record marker。

## 迁移原则

- module interface优先，不按V1文件名机械搬运；
- provider types只存在于ModelGateway private adapter；
- PromptSet是唯一model context assembly seam；
- Tool policy/approval/sandbox/execution统一经过ToolSet；
- Workspace/Prompt/Skill/Tool/Model objects在Turn admission immutable capture；
- control actor保持Cancel、Interaction和Snapshot响应；
- 同Session最多一个ActiveTurnTask；
- 不恢复旧Future、Tool side effect或Interaction waiter；
- recording不能驱动副作用重放；
- 先用ScriptedProviderAdapter证明真实module spine，再接Rig provider adapter。

## 阶段状态

### 阶段1：领域与文档结构

状态：完成。

- Agent/Session/Turn/Item/Interaction领域层级；
- Workspace属于Session；
- Prompt/Tool/Skill独立module；
- V1文档归档；
- canonical owner/link和INV索引。

### 阶段2：Runtime公开协议

状态：目标设计完成，wire/schema待冻结。

- `dispatch / query / snapshot / subscribe`；
- Command/Query/StateEvent/ProgressEvent分离；
- snapshot-first subscription；
- SessionSnapshot现在描述live state，并以`recording.state = healthy | degraded`公开recording health；所有Session都尝试记录；
- first degradation发布`session_recording_changed`，Snapshot保留当前脱敏recording diagnostic；
- StateEvent不再表示physical Session commit。

待完成：

- serde casing/tag；
- public ID文本格式；
- Timestamp/Money；
- StoredTurnStart/StoredCompaction schema。

### 阶段3：Conversation Recording与Replay

状态：目标设计已按ADR 0126重写，生产实现未开始。

必须实现：

1. `LiveSessionState`和`LiveConversation` typed reducers；
2. `ConversationRevision`；
3. Session-scoped EntryId generator；
4. inline ordered SessionRecorder；
5. first failure → Degraded → stop suffix；
6. by-entry JSONL encoder；
7. tolerant replay；
8. complete/incomplete Tool exchange sanitizer；
9. recorded history tree/query/fork；
10. recording health Snapshot/diagnostics。

完成门槛：

- [ ] ordinary live mutation后inline await当前append attempt；
- [ ] slow append只延迟同Session finalization，不串行化其他Session；
- [ ] first failure后停止suffix并保留可恢复完整行前缀；
- [ ] Degraded在同一loaded instance内不恢复、不创建segment或backfill；
- [ ] Unload/Load只恢复recorded prefix并可建立new Healthy Recorder；
- [ ] crash/failed write留下partial tail时replay不brick，writable Load只截断final unterminated tail；
- [ ] incomplete Tool exchange不进入model input；
- [ ] recording failure不产生SessionUnavailable；
- [ ] no-corruption live/replay sanitizer结果一致。

### 阶段4：Prompt、Skill、Tool与Workspace capture

状态：目标设计完成，生产实现未开始。

- PromptSet从LiveConversationView组装；
- Skill/Workspace contribution先规范化并apply live；
- ToolSet immutable并与ToolPromptView同源；
- ToolStartGate独立于Session recording；
- Workspace update Idle-only，SecurityRevoked保持。

待完成：Prompt Q1/Q4、Tool/Sandbox O1/R7。

### 阶段5：Agent/Session lifecycle

状态：目标设计完成，生产实现未开始。

- Agent/Session exact revisions；
- load/unload/archive/fork；
- load只从recorded prefix seed LiveSessionState；
- recording degradation不影响readiness；
- unload等待ActiveTurnTask settlement，无Recorder drain；
- restart不恢复execution objects。

### 阶段6–8：Async模型调用协同交付束

状态：目标设计完成，生产实现、Rig spike和自动化测试未开始。

共享spine：

```text
SessionExecutor admits Turn
→ ActiveTurnTask
→ PromptSet.assemble(LiveConversationView)
→ ModelCallRequest::new(TurnModelSnapshot, ConversationRevision, context)
→ ModelGateway.generate_model_turn
→ live assistant/tool mutation + await SessionRecorder.record
→ optional CompactionSummary
→ live Replace + inline best-effort marker
```

实现顺序：

1. 创建Rust crate与基础ID/error types；
2. 实现LiveConversation reducer和ScriptedProviderAdapter；
3. 实现SessionExecutor control actor与ActiveTurnTask；
4. ordinary AgentRun：Submit → Model → final candidate；
5. complete Tool exchange：Model → parallel Tools → Model；
6. Interaction oneshot、Cancel和ToolStartGate；
7. logical retry：same request + ConversationRevision；
8. SessionRecorder slow-write/failure/crash fixtures；
9. ContextOverflow → CompactionSummary → live Replace → next AgentRun；
10. RigProviderAdapter与mock-server contract tests。

共同完成门槛：

- [ ] 每Session最多一个ActiveTurnTask；
- [ ] control actor在Model/Tool/Interaction await期间响应；
- [ ] AgentRun和CompactionSummary只通过ModelCallRequest/ModelGateway；
- [ ] SDK retry=0，Gateway single attempt；
- [ ] logical retry可被Cancel打断；
- [ ] complete Tool exchange按call order进入next Model；
- [ ] recording failure不重放Model/Tool；
- [ ] recording Degraded/crash时Snapshot可以领先可恢复recorded prefix，Healthy状态无queue lag；
- [ ] restart只恢复recorded prefix；
- [ ] Rig spike覆盖OpenAI Responses与Anthropic Messages。

## Rig 0.40.0 Spike

已完成静态源码审计：Rig适合作为private ProviderAdapter，不适合作为MiniCore domain/interface来源。

实际spike仍需验证：

- OpenAI Responses instructions/messages/tool schema；
- Anthropic Messages system/thinking/signature/cache control；
- streaming cancellation bridge；
- finish reason提取；
- usage mapping；
- provider-specific error mapping；
- SDK automatic retry确认为0；
- base URL override和mock HTTP server。

## 开放问题

Recorder问题集中在[`docs/review/async-loop-best-effort-recording-open-questions.md`](../review/async-loop-best-effort-recording-open-questions.md)：

- EntryId allocation owner；
- recovery closure recording。

Q1 queue容量、Q2 RecordingHealth wire、Q3 explicit flush、Q4 drain deadline、Q5 Degraded recovery、Q6 Fork source、Q7 recording policy和Q8 event/record顺序已经关闭。

其他门禁：

- wire/schema freeze；
- Prompt Q1/Q4；
- Rig provider spike；
- production Tool/Sandbox adapter前关闭O1/R7。

## 文档治理

横切变更按以下顺序更新：

```text
canonical owner
→ interface consumers
→ architecture INV index
→ ADR supersession
→ review/handoff/migration
→ rg old terminology scan
```

ADR 0104、0115已被0126取代。0105、0108、0109、0113、0114、0117、0118、0119、0120、0121、0123、0124被部分修订。历史正文保留，但实现必须以ADR 0126和current modules为准。

## 完成定义

V2生产迁移完成需要：

- Rust crate、module interfaces和private implementations存在；
- scripted async vertical slices通过；
- recorder failure/replay fixtures通过；
- Rig provider contract tests通过；
- wire schema冻结；
- O1/R7在production Sandbox前关闭；
- README、Architecture、CONTEXT、modules、ADR和handoff无current旧术语冲突。
