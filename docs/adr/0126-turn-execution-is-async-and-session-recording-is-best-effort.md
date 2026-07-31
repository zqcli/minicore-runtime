# ADR 0126：Turn执行使用async loop，Session记录采用inline best-effort append

状态：Partially Superseded by ADR 0127
日期：2026-07-30

> 2026-07-31：[ADR 0132](0132-compaction-derives-markers-from-live-stable-units.md)细化Compaction live-first路径：source是reducer-owned revision-bound stable units；LiveSessionState在record前分配Compaction EntryId并安装rolling-summary origin，marker从plan cut派生。

> 2026-07-31：async Turn loop、LiveSessionState truth、inline best-effort conversation recording与Degraded semantics保留。ADR 0127删除`StoredTurnStart`、recorded Turn terminal、cold recovery closure和fork closure；TurnStatus/terminal StateEvent改为current-process only。

> 2026-07-31：[ADR 0130](0130-user-message-composition-resolves-skills-asynchronously.md)细化Starting/Steer async composition：control actor拥有可取消candidate future，ActiveTurnTask拥有Steer resolve；两者在await后重验control/ConversationRevision，PromptSet不执行Skill load。

## 背景

此前架构把`SessionStorage`定义为durable truth，并要求所有live mutation遵循：

```text
SessionWriter strict append
→ apply storage-owned committed delta
→ publish StateEvent / resume waiter / start next operation
```

AgentLoop因此被设计为同步sans-I/O reducer，只能消费Storage私有构造的`CommittedToolExchangeDelta`和`CommittedSteerDelta`。该设计提供强append-before-observe语义，但形成了`AgentLoop + RunningOperation + TurnExecutionPhase`三份配对状态，并让SessionWriter成为Model、Tool、Interaction、Compaction和terminal推进的硬门禁。

Pi、Codex、Gemini CLI和OpenHands的实现表明，coding-agent产品通常让live async loop或Turn task拥有当前执行控制流，Session文件用于resume、历史和诊断。Pi、Gemini CLI和OpenHands直接顺序append；Codex使用bounded async rollout recorder。MiniCore已经通过ADR 0124放弃same-Turn crash resume、durable Tool start proof和旧Future恢复，因此继续维持同步AgentLoop与durable commit barrier的收益不足以覆盖接口和实现复杂度。

后台Recorder queue还会引入容量、byte accounting、queue full、worker failure、flush和graceful drain等额外策略。MVP没有证据需要用这些机制隐藏一次本地JSONL append的延迟，因此采用Pi式顺序记录，并在Rust async环境中使用inline awaited filesystem append，避免阻塞runtime worker线程。

## 决定

### 1. 执行结构

MiniCore采用Codex式执行结构：

```text
SessionExecutor control actor
├─ owns ingress arbitration, lifecycle, FollowUp, active-task handle
├─ routes Cancel / SecurityRevoked / Interaction resolution
├─ publishes SessionSnapshot and StateEvent
└─ ActiveTurnTask                 one per Running Session
   └─ async run_turn loop
      ├─ await ModelGateway
      ├─ await ToolSet
      ├─ await Interaction resolution
      ├─ run logical retry timer
      ├─ orchestrate Compaction
      └─ return TurnTaskOutcome
```

每个Session最多一个`ActiveTurnTask`。不再实现同步`AgentLoop`、`next_action()`、`AgentLoopAction`、`AgentLoopError`、`CommittedToolExchangeDelta`或`CommittedSteerDelta`推进接口。

Rig继续只实现`ModelGateway` private `ProviderAdapter`的单次provider attempt；Rig不拥有Turn loop、conversation、Tool治理或retry。

### 2. Live truth

当前进程内的`LiveSessionState`是loaded Session的live truth：

- async loop根据validated input直接推进Turn、Item、Interaction和conversation；
- 模型输入来自sanitized `LiveConversationView`；
- complete Tool exchange由live conversation reducer判定；
- incomplete、orphan或abandoned-first Tool exchange不能进入下一次Model；
- StateEvent和Snapshot描述live state，不承诺已fsync或可在crash后恢复；
- process crash后只恢复SessionRecorder实际留下的完整JSONL前缀。

`LiveSessionState`通过private typed methods修改，并私有持有唯一Session-scoped `EntryIdGenerator`。实现可以使用`Arc<Mutex<LiveSessionState>>`或等价actor-owned state，但任何live-state guard不得跨Model、Tool、filesystem、provider或Interaction await。

### 3. SessionRecorder

`SessionWriter`被`SessionRecorder`取代。每个loaded Session拥有一个有序inline best-effort recorder：

```text
validate live mutation
→ LiveSessionState.EntryIdGenerator allocates EntryId and binds parent_id
→ apply to LiveSessionState
→ build immutable StoredSessionEntry
→ await SessionRecorder.record(entry)
→ publish / resume / continue async loop
```

`record().await`顺序encode并把一条完整JSONL line交给单Session file append。它不使用后台task、channel或process-local recording queue，也不执行per-entry `fsync`或`sync_data`。

`RecordOutcome::Written`只表示本次`write_all`成功，不是durable commit receipt。调用方不能把它用作Model、Tool、Interaction或Compaction correctness proof。`NotRecorded`也不能触发live rollback、Model retry或Tool重放。

Recorder必须满足：

1. 每个loaded Session只有一个有序append seam，record order来自SessionExecutor/ActiveTurnTask的domain mutation ownership，不依赖mutex竞争顺序；
2. 记录格式仍是header + by-entry JSONL；Recorder只接受已经具有稳定EntryId的immutable entry，不能创建或改写identity；
3. 第一次encode、lease、create、append、partial/unknown write失败后进入`RecordingHealth::Degraded`；
4. Degraded后停止该loaded Session的后续entry append，避免在已知缺口后继续写suffix；
5. failure产生redacted diagnostic，不能把Session置为Unavailable，不能终止active Turn；
6. process crash或failed write可以留下最后partial line，cold replay忽略该tail；
7. restart不恢复旧recorder object或任何in-flight write。

```rust
pub(crate) enum RecordOutcome {
    Written,
    NotRecorded { health: RecordingHealth },
}

pub(crate) enum RecordingHealth {
    Healthy,
    Degraded {
        failed_entry_id: Option<EntryId>,
        reason: SessionRecordingError,
    },
}
```

MVP不提供recording policy、`Disabled`或ephemeral Session。Session Create严格staging初始SessionHeader后发布`Open + Unloaded`，不创建loaded-session Recorder；header staging失败则Create失败且不发布Session。每次Load都尝试初始化SessionRecorder；初始化、lease、权限、磁盘、encode、append或unknown write failure进入`Degraded`。Runtime公开投影固定为`SessionRecordingView { state: healthy | degraded }`；internal reason映射为allowlisted diagnostic，不公开raw error或failed EntryId。first `Healthy → Degraded`原子安装new Snapshot state和当前recording diagnostic；当前domain event先携带该Snapshot发布，随后补发一次`session_recording_changed`。同一load不自动恢复Healthy。

实现应使用async filesystem API。仅Recorder自身用于保护单文件handle的async mutex可以跨append await；任何`LiveSessionState`、Session phase或Tool control guard都必须先释放。RecordingHealth通过独立immutable view发布，使Snapshot不等待file write。不同Session不能共享Recorder锁。

MVP保持单producer domain ownership：Starting/Idle recordable mutation由SessionExecutor拥有，Running Turn mutation由ActiveTurnTask拥有，Interaction request/resolution发生在对应task等待期间。所有路径都必须通过同一个`LiveSessionState` private mutation seam分配EntryId；SessionExecutor和ActiveTurnTask不能各自保存generator。新增并发record producer前必须设计显式domain sequencing，不能用async mutex acquisition顺序定义history。

Replay以文件中全部first-valid EntryId初始化collision guard；loaded Fork target以全部copied EntryId初始化child generator。Degraded不停止ID分配，EntryId不从JSONL line number、storage ordinal或ConversationRevision派生。具体UUID/ULID算法和文本wire留到ID schema freeze。

### 4. Observe与记录顺序

Final StateEvent、waiter resume和下一次Model/Tool遵循：

```text
live mutation applied
→ inline record attempt completed
→ final StateEvent / waiter resume / next protocol step
```

record failure只更新RecordingHealth并继续后续步骤。成功write不代表flush或fsync；Host必须把Snapshot/StateEvent理解为当前进程状态，而不是durable acknowledgement。

ProgressEvent仍可在final live mutation前发布，不进入SessionRecorder。Cancel和SecurityRevoked的sticky emergency publication不等待Session recording。

### 5. Tool exchange

Assistant ToolCall进入live conversation并完成inline record attempt后即可启动Tool。各ToolResult按完成顺序更新live state并顺序完成append attempt；只有全部expected calls得到first terminal `ToolResult`时，conversation reducer生成provider-valid ordered exchange并允许async loop继续Model。

crash可能留下assistant ToolCall而缺少部分或全部ToolResult；cold replay sanitizer排除不完整exchange。MiniCore不宣称Tool exactly-once，也不因recording失败重跑Tool。

### 6. Interaction

Interaction遵循live ordering：

```text
create pending Interaction in LiveSessionState
→ await record InteractionRequested
→ publish request
→ await oneshot
→ validate and apply resolution
→ await record InteractionResolved
→ resume waiter
```

recording失败不阻止request展示或resolution生效。crash可能丢失request或resolution记录；restart不恢复waiter。

### 7. Steer、FollowUp与final arbitration

- Steer由SessionExecutor接收并路由给ActiveTurnTask；
- ActiveTurnTask只在完整assistant/tool step后、下一次Model前FIFO消费；
- candidate final通过owner-local control arbitration与Steer admission排序；
- FollowUp仍由SessionExecutor保存，只在旧ActiveTurnTask完成settlement并返回terminal outcome后启动新Turn；
- live mutation和record outcome不承担final reservation。

### 8. Cancel与Tool start

Cancel继续在sticky epoch发布后立即返回`CancelAccepted`。ActiveTurnTask观察CancellationToken并停止新Model、Tool和Compaction；已Running Tool truthful settle。

Tool side-effect start继续使用owner-local `ToolStartGate`与EmergencyControl first-wins。该安全规则不依赖Session recording，保持INV-401。

### 9. Logical retry

Logical retry移入ActiveTurnTask局部控制流：

```text
terminal retryable ModelCallError
→ verify same Arc<ModelCallRequest>
→ verify Turn/control_generation/live conversation_revision unchanged
→ cancellation-aware sleep
→ invoke ModelGateway again
```

删除`RunningOperation::WaitForModelRetry`和durable `ConversationCheckpoint.entry_id`校验。retry仍复用同一个immutable request，不重新assemble，不retry不确定delivery。

### 10. Conversation revision

`ConversationCheckpoint`不再作为live execution proof。loaded Session维护process-local单调`ConversationRevision`：

- 每次model-visible live conversation mutation递增；
- ModelCallRequest、CompactionPlan和retry捕获exact revision；
- Steer、assistant、Tool exchange completion和Compaction Replace使旧basis失效；
- EntryId只用于recorded history identity和tree relation，不用于当前operation线性化。

### 11. Compaction

ActiveTurnTask从sanitized `LiveConversationView`规划Compaction并调用ModelGateway。summary验证成功后先Replace live conversation，再inline attempt record `StoredCompaction`。record失败时当前进程继续使用summary；restart会恢复较长的旧conversation。tolerant replay遇到坏marker继续忽略该Compaction。

不再重建AgentLoop segment。

### 12. Recovery与Unload

同一loaded Session进入Degraded后保持终态：不probe/retry storage、不创建新segment、不backfill unrecorded suffix，也不发布`Degraded → Healthy`。Host可以继续unsaved execution，或显式Unload后重新Load。

Cold load只恢复recorded JSONL完整行：

- writable open尝试取得exclusive lease；失败建立Degraded loaded Session；成功时只截断final unterminated partial tail，完整newline-terminated entry和中段内容不改写；
- replay继续容忍malformed、duplicate、orphan和invalid relation；
- model view继续排除不完整Tool exchange；
- 未记录的live tail永久丢失；
- JSONL不保存Turn lifecycle，recorded TurnId只用于conversation grouping；
- Load不推断旧Turn outcome、不追加closure，并设置`current_turn = None`；
- Workspace/Prompt/Skill/Tool/Model execution objects仍按current definitions重新capture；
- new loaded instance始终尝试初始化Recorder，并根据storage结果建立Healthy或Degraded，不继承旧health object；
- Unload/Load永久丢弃旧unrecorded live tail，不记录gap marker。

Session recording不可用不再产生read-only loaded Session或writer-poisoned Unavailable。Workspace、安全或定义解析失败仍可产生`SessionReadiness::Unavailable`。

Recorder没有后台queue。graceful unload等待或取消ActiveTurnTask；task结束后不存在待drain recording tail，因此不提供Recorder drain deadline或public flush command。forced process exit仍可能中断当前append并留下partial line。

### 13. Fork source

Fork在source Session的residency/lifecycle synchronization内确定source kind：

- source在linearization point已loaded时固定使用`LiveSnapshot`；
- source在linearization point未loaded时固定使用`RecordedHistory`；
- Unload先赢则使用RecordedHistory；Fork先捕获snapshot则使用LiveSnapshot，后续Unload不改变已捕获source；
- loaded source不根据Recorder health或当前entry是否已写入而降级为RecordedHistory。

LiveSnapshot必须在一次短live-state critical section内同时解析anchor并复制immutable selected path。live mutation在snapshot捕获前已apply时，即使对应`record().await`仍在执行，该事实也进入fork；捕获后才apply的mutation不进入。stream draft、ProgressEvent和其他未apply状态不进入snapshot。

Fork释放source guard后再创建target staging record stream。selected path必须完整写入并通过tolerant replay验证后才能原子发布child；staging失败不发布部分child。conversation tail原样复制，child不继承source current Turn，也不追加fork terminal。Fork不复制ActiveTurnTask、Tool process、Interaction waiter、Steer/FollowUp queue、CancellationToken、Recorder object、in-flight append或authorization capability。

`ForkSourceKind::LiveSnapshot | RecordedHistory`进入child durable fork provenance和`SessionForked`command outcome。Host不需要根据recording health推断Fork是否包含live tail。

## 跨模块不变量变更

- `INV-001`改为live owner为recordable conversation fact在apply前分配稳定EntryId并绑定parent，apply后完成inline record attempt再publication/protocol continuation；TurnStatus只apply/publish live；Recorder不得改写identity；
- `INV-002`保留tolerant replay，并明确只恢复recorded prefix；
- `INV-003`canonical owner移动到live conversation reducer，cold projector只负责恢复期sanitization；
- `INV-004`新增Fork source规则：loaded使用同一LiveSnapshot解析anchor并复制path，unloaded使用RecordedHistory，source kind进入durable provenance和command outcome；
- `INV-101`改为每Session一个control actor和最多一个ActiveTurnTask；
- `INV-102`语义保留，consumer从AgentLoop改为ActiveTurnTask；
- `INV-201`保持；
- `INV-301`改为live Interaction apply/inline-record-attempt-before-notify/resume；
- `INV-401`保持。

## 被取代或修订的ADR

本ADR取代ADR 0115，并取代ADR 0104关于SessionStorage durable truth、strict commit barrier和read-only writer admission的决定。

本ADR部分修订ADR 0105、0109、0113、0114、0117、0118、0119、0120、0121、0123、0124。它不修改ModelGateway single-attempt、provider error taxonomy、Workspace immutable capture、Tool sandbox治理和ModelGateway no-local-permit决定。

## 后果

收益：

- Model→Tool→Model由普通async控制流表达；
- 删除pull polling、issued marker、AgentLoop/RunningOperation配对和retry wait slot；
- 删除Recorder worker、channel、容量、queue full、flush和drain策略；
- 删除同load恢复、新segment、storage probe/retry和live-tail backfill策略；
- Interaction、Cancel和provider/Tool await使用常规task、channel、oneshot和CancellationToken；
- recording failure不会回滚live state、终止Turn或重放外部操作。

代价：

- 每次recordable conversation mutation承担一次本地JSONL encode和append延迟；
- 同Session对应final event和下一protocol step等待该append attempt返回；TurnInterrupted/TurnFailed没有terminal append；
- Host观察到的live状态仍可能因Degraded、partial write或process crash无法完整恢复；
- Tool副作用与Interaction conversation facts可能缺失；Turn terminal本来就不记录；
- source Session的crash resume仍只恢复recorded prefix；显式loaded Fork可以把已apply但未record的live tail写入独立child record stream；
- live fork需要捕获并materialize完整selected path，target staging失败时整个Fork失败；
- MiniCore不再宣称Transcript-First或“已观察事实必可恢复”。

## 测试要求

- Session Create严格stage SessionHeader，失败或publication前crash不发布partial Session；
- 每次Load都尝试初始化Recorder，初始化失败建立Degraded loaded Session；
- ordinary async Model→Tool→Model loop；
- live owner在apply前分配EntryId，Recorder观察exact same ID；
- replay/Fork copied IDs seed collision guard，Degraded继续分配fresh ID；
- parallel Tool completion形成ordered complete exchange；
- incomplete exchange永不进入下一次Model；
- inline recorder按live mutation顺序写出；
- slow append只延迟同Session finalization，不串行化其他Session或阻止sticky EmergencyControl；
- first record failure后停止suffix，Turn继续；
- crash/partial-line replay丢弃未记录live tail，writable Load只截断final unterminated tail；
- Degraded后修复storage不会恢复当前loaded instance；
- Unload/Load可以建立新Healthy Recorder，但旧unrecorded live tail丢失；
- loaded Fork包含snapshot linearization point前已apply的unrecorded tail，unloaded Fork只包含RecordedHistory；
- Fork与Unload竞态按source residency linearization选择且回报稳定`ForkSourceKind`；
- live Fork target staging失败不发布partial child；
- Cancel、SecurityRevoked与ToolStartGate first-wins；
- Interaction request/resolution在recording degraded时仍可完成；
- retry sleep可被Cancel打断并复用exact request；
- Compaction record丢失后replay恢复旧conversation而不brick；
- Snapshot以`healthy | degraded`暴露recording health，不能被解释为fsync acknowledgement；
- first failure的domain event先携带Degraded Snapshot，随后只发布一次`session_recording_changed`，Snapshot保留当前脱敏diagnostic；
- Create必须先完成SessionHeader staging再发布Unloaded Session；Load始终尝试初始化Recorder，不存在Disabled或ephemeral Session路径。
