# Async Loop 与 Best-Effort Session Recording 开放问题

日期：2026-07-31
状态：Q1–Q10全部Closed
关联：[ADR 0126](../adr/0126-turn-execution-is-async-and-session-recording-is-best-effort.md)、[ADR 0127](../adr/0127-session-recording-omits-turn-lifecycle.md)

## 已冻结基线

- Codex式`SessionExecutor control actor + one ActiveTurnTask`；
- live state先推进，随后inline await当前entry的best-effort append attempt；
- Recorder不使用后台task、channel或process-local queue；
- 第一次encode/write失败后Degraded并停止后续记录，replay最多恢复有效完整行前缀；
- recording failure不回滚live state、不终止Turn、不使Session Unavailable；
- successful write不表示flush、fsync或power-loss durability；
- cold replay只恢复recorded conversation prefix；
- JSONL不保存Turn start/terminal，Load后的`current_turn`为空；
- 不恢复旧task、waiter、provider stream、Tool process或in-flight append；
- complete Tool exchange仍是下一次Model的硬协议门禁，但由live reducer拥有；
- Rig仍只实现ModelGateway private provider attempt。

## Q1：Recorder队列容量与背压

状态：Closed。

决议：MVP删除Recorder后台queue。每个Session通过`SessionRecorder.record(entry).await`顺序encode并append当前JSONL line，因此不存在per-Session待写积压，也不配置entry count或queued bytes上限。

inline append会延迟同Session的final StateEvent或下一protocol step，但不持有LiveSessionState guard，不使用Runtime-global lock，也不串行化其他Session。recording failure转为Degraded后立即返回，live流程继续。

`max_entry_bytes`仍作为独立wire/storage限制继续freeze；它约束单条StoredSessionEntry和文件膨胀，不承担queue memory治理。

## Q2：公开RecordingHealth形状

状态：Closed。

决议：`SessionSnapshot.recording`继续使用object形式的`SessionRecordingView { state }`，其中state固定为`healthy | degraded`。Q7删除Disabled后不改动object结构，避免在尚无实现收益时再次重塑Snapshot schema。

语义：

- `Healthy`表示当前load尚未观察到record failure，不表示flush/fsync durability；
- `Degraded`表示初始化、encode或append失败，当前load停止后续记录；
- 同一load只允许`Healthy → Degraded`，不增加Disabled、Initializing、Writing或per-entry receipt状态。

first failure先让当前domain StateEvent携带Degraded Snapshot发布，随后补发一次`session_recording_changed`；两者的完整SessionSnapshot都包含Degraded state和至少一条当前脱敏recording diagnostic。允许的公开code为`session_recording_initialization_failed | session_recording_encode_failed | session_recording_append_failed | session_recording_outcome_unknown`。raw I/O error、绝对路径、credential、完整entry和Tool output不进入公开payload。

## Q3：显式Flush命令

状态：Closed。

决议：MVP不提供`FlushSessionRecording` command。Recorder没有后台queue或待drain watermark；`record().await`只等待当前line的`write_all`结果。MiniCore不提供per-entry `fsync`、durable receipt或将Degraded live tail追溯补写为连续ledger的能力。

## Q4：Graceful drain deadline

状态：Closed。

决议：MVP没有Recorder drain deadline。graceful unload等待或取消ActiveTurnTask；task结束后不存在后台record tail。forced process exit可能中断当前append并留下partial final line，tolerant replay忽略该tail。

## Q5：Degraded后的恢复

状态：Closed。

决议：同一loaded Session进入`Degraded`后保持终态，不恢复recording，不创建新segment，也不提供自动retry或live-tail backfill。

MVP不提供：

```text
ResumeSessionRecording
StartRecordingSegment
automatic storage probe/retry timer
unrecorded live tail backfill
Degraded → Healthy state event
```

修复存储后，Host可以：

- 继续当前loaded Session，接受后续内容不保存；
- `Unload + Load`同一Session：只从recorded prefix重建，旧unrecorded live tail丢失；新loaded instance重新尝试初始化Recorder并建立Healthy或Degraded；
- 显式Fork到新Session；source已loaded时从LiveSnapshot保留snapshot capture前已apply的unrecorded tail。

writable Load尝试取得exclusive lease；失败建立Degraded loaded Session，成功时只截断replay忽略的最终未换行partial tail，再从replayed recorded head继续append。完整newline-terminated entry保留；不修改中段行，不合成missing entry，不记录gap marker。新load通过latest SessionSnapshot表达new health，不产生旧loaded instance的`Degraded → Healthy`transition。

## Q6：Loaded Session fork的数据源

状态：Closed。

决议：source在Fork linearization point已loaded时固定使用`LiveSnapshot`，未loaded时固定使用`RecordedHistory`。loaded source不根据Recorder health或当前entry是否已经写入而fallback到RecordedHistory。

LiveSnapshot在同一个短live-state critical section内解析anchor并复制immutable selected path。capture前已apply的事实会进入fork，即使对应record attempt尚未返回；capture后才apply的事实不进入。Unload先赢时使用RecordedHistory，Fork先完成snapshot capture时保持LiveSnapshot。

target通过staging建立完整新record stream；selected path未全部materialize或验证失败时不发布child。`ForkSourceKind::LiveSnapshot | RecordedHistory`同时进入`SessionForked`outcome和child durable fork provenance。

## Q7：Recording策略配置

状态：Closed。

决议：MVP不提供recording policy、`Disabled`或ephemeral Session。Session Create必须先完成initial SessionHeader staging再发布`Open + Unloaded`，但不创建SessionRecorder；每次Load都必须尝试初始化SessionRecorder，loaded Session后续所有recordable live mutation都经过inline record attempt。

初始化或写入失败进入`Degraded`并按ADR 0126继续live execution；因此“所有Session都记录”表示没有用户/Host opt-out，不表示每条entry获得fsync或durable commit保证。不保留未使用的policy enum、Create/Load字段或public disabled wire value；出现明确临时Session产品需求后再独立设计。

## Q8：Event与record attempt的微观顺序

状态：Closed。

Recordable conversation fact统一采用：

```text
apply live state → await inline record attempt → publish final StateEvent / resume waiter / continue protocol
```

TurnInterrupted/TurnFailed只apply live并在recordable settlement facts完成后发布terminal StateEvent，不创建terminal entry。record failure更新RecordingHealth并继续publication/protocol。successful write不表示flush或fsync。ProgressEvent不进入SessionRecorder，不受该顺序约束；Cancel和SecurityRevoked sticky emergency publication也不等待recording。

## Q9：EntryId分配owner

状态：Closed。

决议：`LiveSessionState`私有持有Session-scoped `EntryIdGenerator`。每个recordable mutation在domain validation成功后、live apply之前分配EntryId并绑定`parent_id`；Recorder只能验证、encode和append，不能创建、替换或规范化EntryId。

规则：

- loaded replay使用文件中全部first-valid EntryId初始化collision guard，包括selected path之外的valid branch/orphan identity；
- loaded Fork child使用复制到target的全部EntryId初始化自己的generator；future child entry生成fresh ID；
- Degraded不影响ID分配；live Item、Interaction、Compaction和StateEvent继续使用已分配ID；
- generated ID在同一loaded instance内不复用，即使后续mutation因内部invariant失败而未publish；
- EntryId与JSONL line number、Codex式rollout ordinal或ConversationRevision分离；
- Q9当时不冻结具体ID wire；该后续项现由[ADR 0134](../adr/0134-public-and-conversation-wire-use-bounded-v1-schemas.md)关闭为typed prefix + 128-bit CSPRNG lowercase hex。

## Q10：Cold recovery closure

状态：Closed。

决议：采用ADR 0127方案A。Session JSONL只记录稳定conversation facts与必要解释元数据，不保存`StoredTurnStart`或Turn terminal。cold Load不投影`InterruptedByRestart`，不推断旧Turn outcome，也不执行recovery append；replay sanitization后设置`current_turn = None`并进入Idle或WorkspaceUnavailable。

关联规则：

- `StoredSessionEntry.turn_id`只用于conversation grouping和Item/Tool correlation；
- Final Assistant是正常响应的conversation fact，不额外记录`TurnCompleted`；
- Interrupted/Failed保留current-process typed StateEvent，不进入JSONL；
- incomplete Tool exchange继续排除，不合成ToolResult；
- Fork复制selected conversation path，不追加fork-specific terminal；
- `ListTurns/GetTurn`的recorded历史不返回execution status。

Q10因此关闭为Not Applicable；此前“先初始化Recorder再尝试closure”和“live recovery view必须标记Interrupted”的前置规则一并撤销。
