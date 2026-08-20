# ADR 0148：v0.1 Session Transcript 是Library-only Read Seam

状态：Accepted
日期：2026-08-13

> 实现状态：`c99ccf7 feat: expose paged session transcripts`新增`MiniCoreRuntime::session_transcript`、public `session_transcript` DTO module、Session actor coherent capture、Runtime-owned immutable page cursor与restart/load evidence；`96f3ac4 docs: freeze v0.1 frontend closure`同步canonical current合同；`0e72c06 test: arm fetch cleanup guardrail before cancellation`只修正全量并发门禁暴露的test-only macOS socket guardrail时序，不改变production `fetch_url`或transcript。该实现不修改Public Wire V1、Wire capability manifest、Conversation JSONL V1或Durable Store V1。

## 背景

v0.1前端闭环已经具备Runtime生命周期、Agent/Session管理、Submit/Steer/FollowUp/Cancel、Snapshot、StateEvent/ProgressEvent、Interaction resolution、production provider installation与五个closed/default-off Tools。然而`SessionSnapshot`只投影current Turn与active Items；Turn terminal后，terminal event的Snapshot回到Idle并清空`current_turn`与`active_items`，`SessionEventDetail::TurnTerminal`也只携带Turn identity与terminal状态。

这意味着持续在线的UI可以消费progress，但丢帧、重连或Runtime重启后的TUI/GUI无法只靠Snapshot恢复已完成的User/Assistant文本。Conversation Storage和loaded `LiveSessionState`已经保存canonical selected history，但这些类型全部是crate-private；现有public `RuntimeQuery`只有capabilities、Agent/Session catalog与Fork provenance，没有completed chat timeline read seam。

为了解决基本聊天恢复，不能把Public Wire V1临时扩展一个optional字段，也不应在v0.1实现完整`GetHistoryTree/ListTurns/GetTurn/ListItems`生态。后者需要Tool/Interaction/Compaction展示语义、unloaded direct reads、排序/反向分页、protocol minor与更广的fixture closure，不是基本嵌入式前端的最小阻塞项。

## 决策

1. **增加一个library-only deep read seam。** `MiniCoreRuntime`公开：

   ```rust
   pub async fn session_transcript(
       &self,
       session_id: SessionId,
       page: PageRequest,
   ) -> Result<Page<SessionTranscriptItem>, QueryError>;
   ```

   它属于in-process Rust library interface，不属于Wire V1 `RuntimeQuery/QueryResponse`，不新增wire route、capability、fixture或transport entry point。Wire-compatible entry families仍是`dispatch/query/snapshot/subscribe`。

2. **First page要求Session已loaded。** Runtime经residency定位exact loaded `SessionExecutor`；未loaded返回`QueryErrorCode::SessionNotLoaded + RetryAdvice::UserActionRequired + PublicSubject::Session(session_id)`。前端选择Session时先执行既有`SessionCommand::Load`，不建立第二套unloaded Conversation Storage reader。

3. **Capture在线性化owner内完成。** `SessionExecutor` actor在自己的串行request点短暂锁住`LoadedSessionConversation.live_state`，从`LiveSessionState::selected_entries()`捕获immutable `Arc<[Arc<StoredSessionEntry>]>`。guard不跨await；capture不做I/O、不复制全部正文、不读取DurableState或ambient filesystem。

4. **Current Turn不进入transcript capture。** 若`LiveSessionState.current_turn()`存在，capture排除该Turn的全部entries。当前Turn继续只由`SessionSnapshot.current_turn`、`active_items`、`pending_interactions`与实时events展示，避免同一User/Assistant item同时出现在stable transcript与active projection。

5. **v0.1只投影基础聊天文本。** `SessionTranscriptItem`只包含：

   - User message：`ItemId`、`TurnId`、`UserMessageSource`、safe canonical body、`Timestamp`；多content part按既有active-item规则以单个`\n`连接；
   - Assistant message：每个`StoredAssistantContent::Text`各形成一项，保留`ItemId`、`TurnId`、safe text body与entry `Timestamp`。

   Reasoning、ToolCall、ToolResult、Interaction request/resolution与Compaction marker不进入该DTO。它不是generic history、audit log、Tool trace或full conversation export。

6. **顺序是canonical selected path顺序。** entry按selected root-to-leaf path顺序，Assistant entry内部Text按content顺序。Fork/replay选择仍由Conversation Storage与LiveConversation现有规则拥有；transcript不重新判定tree、relation或Tool exchange validity。

7. **分页绑定首次immutable capture。** `PageRequest.limit`复用selected V1 paging maximum（1..=200）；Runtime `PageCursorStore`保存`SessionTranscriptCapture + SessionId + display-item offset`，使用与既有catalog cursor相同的15分钟TTL、4,096 capacity与one-shot successor语义。cursor不能跨Session或query family复用。

8. **Continuation不重新访问residency。** 首次page后，后续page只从cursor持有的same immutable capture投影；Session随后append、Unload或definition变化都不会污染已开始的分页。成功使用一个cursor后该cursor被消费；cross-Session失败不消费正确Session的cursor。

9. **只复制当前page正文。** capture只clone stored entry `Arc`；projection以displayable item offset扫描，只有落入当前page的User/Assistant body才被clone。一个Assistant entry包含多个Text时，page不会先复制全部Text再丢弃页外内容。

10. **Restart只恢复recorded prefix。** Runtime shutdown/reopen后，既有`SessionCommand::Load`从Conversation JSONL tolerant replay重建selected entries；`session_transcript`随后可恢复已recorded User/Assistant文本，即使Session因空model catalog投影`ModelUnavailable`。Best-effort Recorder已Degraded或process crash造成的unrecorded live tail仍不可恢复，本seam不提升durability声明。

11. **Terminal event是refresh signal，不内联文本。** 正常在线时UI继续消费Progress/StateEvent；收到`TurnCompleted/Interrupted/Failed`或重连后的snapshot-first baseline时，可以重新开始一次transcript page capture。terminal event、SessionSnapshot与Wire V1不增加Assistant body字段。

12. **复用existing query error与page DTO。** 不新增`TranscriptError` public taxonomy，不建立generic history service、trait、registry或storage adapter。内部executor/residency errors只负责Closing/SessionNotLoaded/Internal映射，public返回existing `QueryError`与`Page<T>`。

13. **Debug与错误不披露正文。** `SessionTranscriptItem` Debug只显示identity、role、source、timestamp与body byte length；capture/slice Debug只显示counts/offset。正文不会进入panic、error message或cursor Debug。

14. **完整history ecosystem冻结为post-MVP。** 以下不阻塞v0.1：Wire/transport transcript route、unloaded direct transcript read、newest-first或reverse pagination、`GetHistoryTree`、`ListTurns/GetTurn/ListItems/GetItem`、Tool/Interaction/Reasoning/Compaction timeline、export/search、event replay与same-Session history checkout。任何一项进入production前必须有独立consumer、bounded contract及必要的protocol/storage migration。

## 可执行证据

- completed Turn之后，limit=1第一页返回exact User、第二页返回exact Assistant；cross-Session cursor fail closed，cursor one-shot；
- continuation在Session Unload后仍从same capture读取，fresh first page返回SessionNotLoaded；
- Runtime正常shutdown后用同一Store V1 root重开并Load，在ModelUnavailable状态下仍恢复recorded User/Assistant transcript；
- current Turn被capture排除，User multi-part以`\n`连接；
- Assistant item offset忽略Reasoning并从多Text entry中只复制命中page的正文；
- transcript item/capture Debug不披露User、Assistant或Reasoning正文；
- stable full library suite、Clippy、format与真实Rust 1.85 focused transcript tests通过。

## Acceptance

2026-08-13在`0e72c06`上从头运行完整门禁：

- `./scripts/check.sh`：main library `1035 passed / 3 ignored`；main integration合计`159 passed / 3 ignored`；standalone provider-gate `25/25`；Clippy、format、current/archive docs、Wire V1 `144 active / 0 pending`与Durable Store fixtures全绿；
- `./scripts/check-msrv.sh`：exact `rustc 1.85.0 (4d91de4e4 2025-02-17)`、隔离`target/msrv-1.85.0`；main library `1035 passed / 3 ignored`，main integration合计`159 passed / 3 ignored`；
- production network tests保持默认离线，两个real-provider smoke保持显式`#[ignore]`；
- stable log：`/tmp/minicore-v0.1-stable-check-rerun.log`；MSRV log：`/tmp/minicore-v0.1-msrv-check.log`。日志只含nonsecret local acceptance evidence，不进入Git。

## 后果

- TUI/GUI现在可以在terminal、重连和cold load后恢复基础聊天文本，不需要解析JSONL或缓存全部progress。
- v0.1保持deep interface：一个method隐藏actor capture、selected-history projection、bounded page copying、cursor ownership与restart replay。
- Wire V1/Store V1继续closed exact；独立transport adapter若要远程暴露transcript，必须设计新protocol minor而不是偷用当前library method。
- long-session newest-first体验、完整Tool/Interaction history与unloaded browsing仍是明确的post-MVP产品工作，不再被current文档误写成已实现的MVP Query。

## 被否决的方案

- **把final Assistant text塞进TurnTerminal detail。** 只能修正常terminal一刻，无法恢复多Turn历史、丢帧或cold load，且会修改Wire V1。
- **把completed items永久留在SessionSnapshot。** Snapshot会随历史无限增长，破坏bounded current-state read model。
- **让前端直接读Conversation JSONL。** 泄露storage format、tree/replay sanitizer与root authority，破坏owner边界。
- **立即实现完整generic history Query。** 扩大DTO、wire、pagination、Tool/Interaction与corruption语义，超过基本聊天恢复所需。
- **每一页重新读取live state。** append/Unload/reload会让页间重复或漏项，不能提供captured pagination consistency。
- **首次capture复制全部正文。** 长Session可接近Store V1 1 GiB上限，违反最小有界内存目标。
