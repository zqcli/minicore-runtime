# MiniCore V1 → V2 版本迁移记录

状态：V2目标架构已推进至ADR 0140；全部普通V4-P0/P1与conditional V4-C0-1已关闭，production Provider/Tool adapters仍未实现
日期：2026-08-11

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
→ recordable conversation mutation + record attempt
→ next Model or live terminal
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
- SessionRecorder只保存有序best-effort conversation JSONL prefix；
- recordable conversation fact live apply成功后完成inline record attempt，再publish/continue；
- recording failure只更新health；
- cold replay只恢复recorded prefix；
- complete Tool exchange仍是Model visibility门禁，但由live reducer拥有；
- logical retry由ActiveTurnTask使用ConversationRevision验证；
- Compaction先Replace live conversation，再record marker。

## 2026-07-31重构

ADR 0127进一步删除持久化Turn lifecycle：

```text
StoredTurnStart
StoredTurnTerminal
TurnCompleted / TurnInterrupted / TurnFailed JSONL events
cold recovery closure
HistoricalFork terminal
```

TurnStatus和terminal StateEvent继续服务current loaded execution。Replay只恢复conversation facts并sanitize incomplete Tool exchange；Load设置`current_turn = None`，Fork原样复制selected conversation path。

ADR 0132关闭Compaction实现门禁：Live reducer发布Session/revision-bound EntryId-bearing stable units；Runtime settings在Turn admission capture；Compaction从source+cut派生marker并使用Prompt/Model exact basis闭合pressure与summary budget；automatic StoredCompaction保存完整safe model-call provenance。

ADR 0133关闭Runtime public payload门禁：CommandCompletion/error mapping、metadata CAS、queued command Snapshot、concrete Query/Snapshot/Item views和safe Interaction形成closed protocol；MVP Prompt body只保留Empty/Text。

ADR 0134关闭wire/storage format门禁：Wire Schema冻结JSON casing/tag、typed IDs/revisions、Timestamp/Duration/Money/path/cursor、ProtocolLimits、canonical BoundedJson与bounded scanner；Conversation JSONL Format V1冻结strict Header、六种flat Stored body、field order、replay relation与Compaction projection；Wire V1 Fixtures冻结public manifest、golden/corruption expectations与boundary/+1 recipes。

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
- 先用ScriptedProviderAdapter证明真实module spine，再接protocol-specific direct provider adapters。

## 阶段状态

### 阶段1：领域与文档结构

状态：完成。

- Agent/Session/Turn/Item/Interaction领域层级；
- Workspace属于Session；
- Prompt/Tool/Skill独立module；
- V1文档归档；
- canonical owner/link和INV索引。

### 阶段2：Runtime公开协议

状态：semantic payload与wire/schema contract完成；Rust typed scalar/value/path carriers已实现，Runtime协议行为尚未实现。

- `dispatch / query / snapshot / subscribe`；
- Command/Query/StateEvent/ProgressEvent分离；
- snapshot-first subscription；
- SessionSnapshot现在描述live state，并以`recording.state = healthy | degraded`公开recording health；所有Session都尝试记录；
- first degradation发布`session_recording_changed`，Snapshot保留当前脱敏recording diagnostic；
- StateEvent不再表示physical Session commit；
- SessionSnapshot完整列出可取消Submit/Steer/FollowUp和Pending Interaction safe request；
- Agent/Session metadata独立CAS，Starting Submit cancel和public errors使用typed completion；
- UserQuestion仅支持non-secret Text/SingleChoice，approval只提交request-scoped option index；
- Query/Snapshot/Item read models为closed concrete payload，MVP Prompt body只有Empty/Text。

已冻结：

- camelCase field、snake_case variant与adjacent `type/data`；
- typed public/storage IDs、revision、u64、Timestamp、Duration、Money、PageCursor与file URI；
- ProtocolLimits、BoundedJson/Schema canonicalization与public diagnostic projection limits；
- format-v1 Stored DTO：ModelResponseSummary、StoredToolOutcome、StoredInteraction request/resolution和StoredCompaction；
- public manifest、JSON/JSONL golden/corruption vectors和all-limit boundary recipes。

当前Rust crate必须继续消费`docs/fixtures/wire-v1/`，不能重新发明第二套serde defaults。

### 阶段3：Conversation Recording与Replay

状态：目标设计已按ADR 0126/0127重写，生产实现未开始。

必须实现：

1. `LiveSessionState`和`LiveConversation` typed reducers；
2. `ConversationRevision`；
3. LiveSessionState-private Session-scoped EntryId generator；
4. inline ordered SessionRecorder；
5. first failure → Degraded → stop suffix；
6. by-entry JSONL encoder；
7. tolerant replay；
8. complete/incomplete Tool exchange sanitizer；
9. recorded history tree/query/fork；
10. recording health Snapshot/diagnostics；
11. conversation-only schema：无StoredTurnStart/terminal，Load无closure。

完成门槛：

- [ ] live owner在apply前分配EntryId/parent，Recorder观察exact same identity；
- [ ] replay/Fork copied IDs seed collision guard，Degraded继续分配fresh ID；
- [ ] ordinary live mutation后inline await当前append attempt；
- [ ] slow append只延迟同Session finalization，不串行化其他Session；
- [ ] first failure后停止suffix并保留可恢复完整行前缀；
- [ ] Degraded在同一loaded instance内不恢复、不创建segment或backfill；
- [ ] Unload/Load只恢复recorded prefix并可建立new Healthy Recorder；
- [ ] crash/failed write留下partial tail时replay不brick，writable Load只截断final unterminated tail；
- [ ] incomplete Tool exchange不进入model input；
- [ ] recording failure不产生SessionUnavailable；
- [ ] no-corruption live/replay sanitizer结果一致；
- [ ] restart后current_turn为空，不推断或追加旧Turn terminal；
- [ ] historical ListTurns/GetTurn按TurnId分组且不返回execution status。

### 阶段4：Prompt、Skill、Tool与Workspace capture

状态：目标设计完成，生产实现未开始。

- PromptSet从LiveConversationView组装；
- PromptContent在candidate build期间完全materialize并由强Arc共享，PromptSet不解析source locator；
- PromptIntent使用`body + skills[]`，MVP body只有Empty/Text，SkillIntent只保存SkillId；
- Skill/Workspace contribution先完成captured source authorization，再按独立顶层part原子规范化并apply live；
- live/JSONL共同使用safe part-level stamp，不保存字符offset、绝对路径或authorization；
- ToolSet immutable并与ToolPromptView同源；
- ToolStartGate独立于Session recording；
- Workspace update Idle-only，SecurityRevoked保持。

Prompt Q1/Q4已分别由ADR 0128/0129关闭。Tool/Sandbox O1/R7已由M13/ADR 0140关闭；production实现仍属于M14。

### 阶段5：Agent/Session lifecycle

状态：目标设计完成，生产实现未开始。

- Agent/Session exact revisions；
- load/unload/archive/fork；
- load只从recorded prefix seed LiveSessionState；
- recording degradation不影响readiness；
- unload等待ActiveTurnTask settlement，无Recorder drain；
- restart不恢复execution objects。

### 阶段6–8：Async模型调用协同交付束

状态：M6–M10 scripted async spine与M12 provider reality gate已完成；Rig已因Rust 1.85被拒绝进入production baseline，两个direct provider adapters仍属于M14。

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

V4-P1-2 wire/storage format门禁已经关闭；全部普通P0/P1（含V4-P1-3）与conditional V4-C0-1均已关闭。Rust typed carriers与scripted async spine已经进入`dev`。后续阶段、依赖与测试分层以[MiniCore V2开发计划](../development-plan.md)为准；production Provider/Tool adapters属于M14并分别遵守ADR 0138/0139与ADR 0140。

共同完成门槛：

- [x] 每Session最多一个ActiveTurnTask；
- [x] control actor在Model/Tool/Interaction await期间响应；
- [x] AgentRun和CompactionSummary只通过ModelCallRequest/ModelGateway；
- [x] SDK retry=0，Gateway single attempt；
- [x] logical retry可被Cancel打断；
- [x] complete Tool exchange按call order进入next Model；
- [x] recording failure不重放Model/Tool；
- [x] recording Degraded/crash时Snapshot可以领先可恢复recorded prefix，Healthy状态无queue lag；
- [x] restart只恢复recorded prefix；
- [x] Rig spike覆盖OpenAI Responses与Anthropic Messages。

## Rig 0.40.0 Spike

静态源码审计与真实M12 loopback spike均已完成：Rig适合作为protocol evidence，不适合作为MiniCore Rust 1.85 production dependency或domain/interface来源。协议合同见[ADR 0138](../adr/0138-production-provider-baseline-uses-verified-rig-contracts.md)，implementation转向见[ADR 0139](../adr/0139-rig-is-evidence-only-under-rust-1-85.md)，delivery/error contract见[M12 fixture](../fixtures/provider-gate-m12/README.md)。

已验证：

- OpenAI Responses instructions/messages/tool schema/reasoning/structured request；
- Anthropic Messages system/thinking/signature/cache control；
- streaming cancellation、fragmented SSE、terminal、drop与early EOF；
- provider-specific finish/terminal、usage和body/header identity；
- 400/401/429/500/529 typed envelope与malformed 200 fail-closed；
- automatic retry为0的single-request evidence；
- base URL override和两协议real mock HTTP server；
- Rig synthetic zero-usage `Final`不能替代protocol terminal；
- Rig只存在于standalone stable-only `provider-gate/` package，不进入root dependency/lockfile、production或public DTO；

## 开放问题

Recorder问题见[`docs/review/async-loop-best-effort-recording-open-questions.md`](../review/async-loop-best-effort-recording-open-questions.md)。Q1–Q10已经全部关闭；Q10由ADR 0127确定conversation-only schema和无closure Load。

其他门禁：

- 第四轮全部普通V4-P0/P1与conditional V4-C0-1已关闭；V4-P1-3由M12/ADR 0138/0139关闭，V4-C0-1由M13/ADR 0140关闭；
- 首个Rust crate已经消费ADR 0134/Format V1/Wire V1 fixtures并实现semantic conformance runner；
- production OpenAI Responses/Anthropic Messages direct adapters仍属于M14，必须消费M12 contract suite；
- production Tool/Sandbox adapters必须消费ADR 0140 contract suite；首个file-mutation adapter另须实现ADR 0116的Session-local queue。

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

ADR 0104、0115已被0126取代。0105、0108、0109、0113、0114、0117、0118、0119、0120、0121、0123、0124和0129被部分修订；ADR 0130–0134分别冻结async Skill composition、conversation JSONL configuration/lifecycle边界、Compaction stable-unit/settings/provenance、snapshot-recoverable Runtime public payload和bounded public/storage wire v1。历史正文保留，但实现必须以current modules与最新Accepted ADR为准。

## 完成定义

V2生产迁移完成需要：

- 第四轮P0/P1实施门禁已按对应module关闭；
- Rust crate、module interfaces和private implementations存在；
- scripted async vertical slices通过；
- recorder failure/replay fixtures通过；
- Rig provider contract tests通过；
- wire schema冻结；
- O1/R7已由ADR 0140关闭，production Sandbox adapters继续证明effective enforcement；
- README、Architecture、CONTEXT、modules、ADR和handoff无current旧术语冲突。
