# DurableState 架构设计

状态：M5.0 production durable foundation与exact historical definition resolution已实现；loaded Ready+Idle SessionExecutor及Runtime residency actor已消费definition/lifecycle durable seams并完成required Snapshot install、unloaded exclusion与post-commit poison；public Create/Load/Submit/Unload/Fork与ordinary Turn replay已消费这些seams，Fork包含全部公开anchor、LiveSnapshot/RecordedHistory、streaming publication/readback与restart recovery；public Session metadata CAS、ordinary Session definition CAS与显式Agent revision upgrade已消费durable seams并经residency/executor完成loaded publication；完整cross-platform native matrix pending（ADR 0136、0137）
日期：2026-08-03

## 目的

`DurableState`是MiniCore private deep module：在一个专用、user-private local store root中，拥有Agent/Session entity的物理布局、永久identity reservation、root lease、catalog/head installation、immutable generation、CAS recheck、marker publication、readback、recovery/cleanup、poison/closing状态和filesystem fault seam。它把跨文件durability复杂性收在一个operation owner内，而不是交给lifecycle caller拼装。

当前 foundation已由同一actor中的concrete Agent Create、ordinary Session Create、Session Fork及Agent status/definition/metadata、Session metadata CAS、Session definition/Agent revision upgrade CAS与Session lifecycle existing-head action tracers消费：sealed lifecycle attempt在reservation成功后生成exact G1 payload；Session Create在private Agent lifecycle gate内读取current Enabled exact ref并持有同一gate至owner-tracked publication child完成；Fork从residency线性化后的LiveSnapshot或opaque same-open RecordedHistory observation捕获anchor-resolved selected path，在同一Agent gate/certainty路径逐行重编码child Header/history、完整流式readback、固定captured retained Agent revision、发布COMMITTED/PUBLISHED、安装catalog并完成close/reopen recovery。**没有 standalone production `reserve` API、reservation token/receipt 或 caller-visible reservation waiter**。Agent status CAS在exclusive Agent gate内执行expected-status recheck、stale-before-no-op、Deleted terminal与status-only G+1 COMMITTED publication；Agent definition CAS在同一exclusive gate内执行expected-revision recheck、stale-before-no-op、Deleted terminal与definition-bearing G+1 COMMITTED publication；Agent metadata CAS保留name Keep/Set与description Keep/Set/Clear patch intent，在gate内按expected metadata revision→Deleted→canonical no-op顺序解析并发布metadata-only G+1。三者均在complete full-chain readback后先install catalog、settle当前请求再释放gate；Session metadata CAS在独立per-Session gate内保留name/description Keep/Set/Clear patch intent，发布metadata-only G+1且不触碰conversation。Session definition CAS在同一per-Session gate内执行expected revision→Open lifecycle→complete Workspace/Model/Prompt replacement→canonical no-op，definition-bearing G+1按semantic Workspace变化决定WorkspaceRevision current或N+1；显式Agent revision upgrade使用独立sealed attempt，在actor内按`Agent → Session`获取typed permits，要求same Agent、Enabled current与retained exact target，并支持current upgrade和historical rollback。两条definition-bearing路径均通过closed Session publication operation保留metadata、lifecycle、fork provenance、entity createdAt与conversation identity，post-COMMITTED worker继续持有所需gate/root lease。Session lifecycle existing-head action依据authoritative current lifecycle执行Archive/Unarchive/Delete closed matrix，无expected lifecycle token，发布head-only G+1；Runtime public façade现消费Agent Create/status/definition/metadata、Session lifecycle与Session metadata outcome，真实变化发布matching safe summary event；loaded Session metadata还经residency gate安装exact executor Snapshot并发布Session-scope event，canonical no-op不重复发布。exact definition resolver对current revision复用catalog内installed Arc且不做filesystem I/O；historical Agent/Session revision通过immutable revision index定位单个bounded `definition.json`，由owner-tracked blocking job在root lease下读取。Session index使用reopen时构建的dense recovered base加small process overlay，head-only recovery流式验证并复用Arc；已索引immutable definition的缺失、错owner/revision、corrupt bytes或worker panic会poison并关闭admission，ordinary read I/O unavailable保持retryable，caller取消不detach worker或fatal settlement。Runtime现可从recovered base加published overlay短capture当前Agent/Session head projection，供`ListAgents`、`ListSessions`与Fork provenance query构造immutable page snapshot；DurableState仍不拥有cursor或loaded state。Runtime owner消费该resolver完成Load/query routing、Workspace Idle/Snapshot publication，并从Session lifecycle的Unloaded检查到durable completion持续排除并发Load。Runtime Agent/Session durable StateEvent已消费Agent Create/status/definition/metadata、Session Create/Fork publication、Load/Unload residency变化、Archive/Unarchive/Delete durable变化、Session metadata CAS及ordinary Session definition CAS（loaded路径经residency gate安装exact executor Snapshot并发布Runtime+Session `SessionDefinitionUpdated`事件，canonical no-op不重复发布）；显式Agent revision upgrade亦已消费：Runtime在publication semaphore内采样单一owner timestamp，经residency per-Session gate路由，unloaded直接调用`upgrade_session_agent`并验证exact head/definition shape，loaded调用executor既有publication slot（executor只precheck installed expected revision/busy/closing，worker只调用durable seam、不调用Workspace resolver），durable Updated经exact checked-successor验证后原子安装并发布matching事件，post-commit install失败保持integrity-fatal poison；readiness与full recovery conformance继续消费同一基础。

[Durable Store V1](../formats/durable-store-v1.md)是路径、bytes、field order和scanner precedence的唯一exact owner；本模块拥有这些bytes的operation语义。

## 非目标

不建立：public adapter hierarchy、generic transaction/mutation-plan、caller-visible `StagedSessionStorage`、caller-selected path/generation/marker、remote backend、database、WAL、production physical purge、committed-generation GC或published-definition GC。`CommandId`不持久化、不传给DurableState，也不能查询durable outcome。

## Ownership

```text
Agent / Session Lifecycle                         Conversation Storage
semantic validation, canonical no-op,             SessionHeader / JSONL, tolerant replay,
domain revisions, timestamps, metadata,           fork semantic seed
lifecycle candidates                                      │
             │                                             │
             └──────────── typed sealed request ───────────┘
                                      │
                              DurableStateActor
                    root lease · reservations · catalog/head
                    immutable generation · CAS recheck · publication
                    recovery/cleanup · poison · filesystem port
                                      │
                         LocalFilesystem / deterministic fault filesystem
```

- Agent/Session Lifecycle owns all domain meaning: validation and canonicalization, domain and metadata revision arithmetic, timestamps, status/lifecycle candidates, and exact cross-entity semantic checks.
- Conversation Storage owns `SessionHeader`, JSONL byte semantics, replay/tree selection and a fork's semantic conversation seed. It never owns an entity directory, `PUBLISHED`, a storage generation, or a filesystem path.
- DurableState owns physical realization only. It validates the sealed candidate against the current immutable head before writing, but never redefines a domain no-op or revision.
- Lifecycle callers never receive a staging object, path, generation, marker handle, writable file handle or publication receipt. Read callers receive immutable entity snapshots/capabilities only.

## Deep interface and actor

Representative crate-private operations are deliberately concrete rather than a generic transaction API:

```rust
pub(crate) struct DurableState;
pub(crate) struct DurableStateActor;

impl DurableState {
    pub(crate) async fn open(
        config: DurableStoreConfig,
        runtime: RuntimeTaskContext,
    ) -> Result<Self, DurableOpenError>;
    pub(crate) async fn create_agent(
        &self,
        attempt: SealedAgentCreateAttempt,
    ) -> Result<DurableAgentHead, DurableMutationError>;
    pub(crate) async fn update_agent(
        &self,
        request: AgentHeadMutation,
    ) -> Result<DurableAgentHead, DurableMutationError>;
    pub(crate) async fn set_agent_status(
        &self,
        request: AgentStatusMutation,
    ) -> Result<DurableAgentHead, DurableMutationError>;
    pub(crate) async fn create_session(
        &self,
        attempt: SealedSessionCreateAttempt,
    ) -> Result<DurableSessionHead, DurableMutationError>;
    pub(crate) async fn fork_session(
        &self,
        attempt: SealedSessionForkAttempt,
    ) -> Result<DurableSessionHead, DurableMutationError>;
    pub(crate) async fn update_session(
        &self,
        request: SessionHeadMutation,
    ) -> Result<DurableSessionHead, DurableMutationError>;
    pub(crate) async fn set_session_lifecycle(
        &self,
        request: SessionLifecycleMutation,
    ) -> Result<DurableSessionHead, DurableMutationError>;
    pub(crate) fn agent_head(&self, id: AgentId) -> Option<Arc<DurableAgentHead>>;
    pub(crate) fn session_head(&self, id: SessionId) -> Option<Arc<DurableSessionHead>>;
    pub(crate) async fn open_conversation_target(
        &self,
        session_id: SessionId,
    ) -> Result<PublishedConversationTarget, DurableReadError>;
    pub(crate) async fn acquire_recorded_fork_lease(
        &self,
        session_id: SessionId,
    ) -> Result<RecordedForkConversationLease, DurableReadError>;
}
```

`DurableStateActor` is the single process-local mutable owner and queue for **every** Agent and Session mutation, reservation phase, and catalog/head installation. The root OS lease prevents another cooperating process, but is not internal serialization. **No caller may hold an Agent or durable Session lifecycle/mutation gate while awaiting the actor.** Actor status mutations take the exclusive side of the same private per-Agent `AgentLifecycleGate` whose admission side issues short `AgentAdmissionPermit`s; this one gate/epoch is the linearization point against initiating Input live apply. The actor alone acquires private durable cross-entity gates in the fixed `Agent → Session` order and owns the mutation slot. It always reads/rechecks current immutable state and every public CAS/lifecycle precondition before no-op detection: a stale expected value remains stale even when the candidate equals current state. A bounded Create/Update document preparation may run while the actor serializes its operation; publication never becomes a detached job.

Create, Upgrade and Fork acquire their required Agent gate inside the actor. For Session Create specifically, before that gate the actor may receive only parent-independent semantic/canonical candidate fragments containing **neither** the assigned `SessionId` nor an `AgentRevisionRef`. The actor acquires the Agent gate, reads the current exact ref while `Enabled`, and constructs the final `SessionDefinition`, `SessionHeader`, and generation-1 head bytes. It writes/syncs/readbacks that bounded markerless payload, then crosses `DurableCommitBarrier` immediately before `COMMITTED`. It holds that same gate through complete `PUBLISHED`/readback; this intentional bounded-size but potentially unbounded-latency local-publication head-of-line tradeoff prevents a disable/delete race from publishing a new executable reference. Event/fan-out, SessionExecutor work, Recorder work, and host callbacks are forbidden under it. A potentially 1 GiB Fork copy streams outside the actor through an actor-owned and actor-tracked job into the final markerless child path. It first captures the typed source seed/lease and releases the short source residency/lifecycle guard before target I/O; a recorded physical lease itself remains held through copy/readback. The job then returns a validated `PreparedConversationProof` plus final Agent validation/publication work to the actor. The actor owns and joins every blocking job even when the dispatch waiter drops. A join panic/failure after possible publication poisons/starts Runtime close; dropping a `JoinHandle` to detach publication is forbidden.

## Named barriers and runtime shutdown

The following owner barriers are shared by DurableState, lifecycle, SessionExecutor, and Recorder. For durable entity commands, `RuntimeClosing` rejection applies only before the relevant durable barrier. Recorder additionally respects live truth: after a recordable fact is live-applied, pre-write shutdown yields `NotRecorded`/Degraded and cannot retroactively reject the owning command:

- `EntityReservationBarrier` is immediately before the `create_new` of **each** permanent-reservation attempt. Shutdown/cancellation before it can reject a queued operation or the not-yet-crossed attempt with no burned ID. A definite collision settles that attempt; the next candidate, if any, has a new barrier, so shutdown may win before it.
- After reservation but before `DurableCommitBarrier`, shutdown/cancellation may stop and clean staging, but the reservation remains burned.
- Markerless final-path staging before `DurableCommitBarrier` is cancellable: directories and payload files may be created, written, synced, and streaming-readback validated, but neither `COMMITTED` nor `PUBLISHED` exists. Cancellation closes handles and removes only the exact operation-produced markerless staging; the reservation remains burned.
- `DurableCommitBarrier` is immediately before `create_new(COMMITTED)`. After crossing it, caller drop or shutdown cannot cancel: the operation settles through marker/readback/completion or the fatal outer error.
- Fork semantic copy and its full bounded-memory streaming validation occur before `DurableCommitBarrier`; cancellation cleans the markerless child and releases its source lease.
- `RecorderWriteBarrier` is after encode/size validation and immediately before the first physical append write. After crossing, append settles and its health transition is installed.

The same actor serializes Agent and Session reservation phases. During shutdown it may reject still-queued/pre-reservation work, but every crossed reservation attempt settles its exact certainty before the actor stops and joins its owner-tracked jobs. The actor never releases the root lease: only the Runtime/DurableState shutdown owner, after the actor has stopped and all conversation handles have closed, releases it last.

`MiniCoreRuntime::shutdown().await` is host-only and non-wire; the four protocol entry points remain exactly `dispatch`, `query`, `snapshot`, and `subscribe`. It is idempotent: transition to Closing/reject new admission; settle accepted work according to those barriers; stop/unload SessionExecutors and join current Recorder jobs; join DurableState staging/blocking jobs and stop its actor; close all conversation handles; only then the Runtime/DurableState shutdown owner releases the root lease last; then mark Closed. Facade `Drop` only sends a best-effort Closing signal and never blocks; the owner registry retains task handles/self-Arcs so dropping the facade does not detach a raw `JoinHandle`. Hosts must still await shutdown to observe graceful completion and root-lease release before Tokio teardown.

## Root lease and supported store

Store open creates or reuses the permanent regular `<root>/.minicore.lock`, opens it without following links, and later uses `fs4 = { version = "0.13.1", default-features = false, features = ["sync"] }` `try_lock_exclusive`. One non-cloned handle is retained until every store/recorder job has settled and the store closes. The lock file is never deleted or renamed; its existence and PID have no authority. This global root lease replaces all per-Session OS locks. Lease denial is Runtime `StoreInUse`, never one Session's `Degraded` recording health. Root lease loss or invalid physical identity poisons DurableState and initiates Runtime close.

`ExclusiveWritableConversationLease` has **no public or caller-visible constructor**. `DurableState` now issues a `PublishedConversationTarget` plus its paired proof after exact initial Header and same-open-file physical validation; Recorder/Conversation Storage consumes the opaque pair without receiving a path. The existing `#[cfg(test)]` constructor remains scanner-test-only. Recorder Load/open/write failures with root lease intact remain local to that Session and create `RecordingHealth::Degraded`; loss of root lease/lock-file identity is global poison/close, never Session Degraded.

MVP support is claimed only when host configuration supplies a dedicated user-private coherent root on APFS, NTFS, ext4 or XFS and the native platform suite passes. MiniCore validates current platform-observable facts; it does not pretend to identify every mount technology perfectly. NFS, SMB, WebDAV, FUSE, overlay filesystems, mapped remote drives and hostile concurrent filesystem mutation are unsupported host configurations. The root itself must not be a symlink/reparse point. Unix creation targets use directories `0700` and files `0600`; Windows relies on the trusted inherited user-private ACL. Unix whole-entity cleanup captures and revalidates device-plus-inode facts for directories and zero markers, plus length for regular files. The non-Unix seam is deliberately narrower: **full Windows/NTFS identity, reparse, and native-process coverage remains an M5.0 pending platform gate.** This slice's process-abort/no-hostile-mutation cleanup scope does not claim to close that platform gate.

Every recognized component must have exact ASCII spelling. Recovery rejects observed case-fold aliases, lossy/non-UTF-8 recognized names, recognized symlinks, and unexpected recognized namespace entries. It enumerates then sorts canonical parsed names; `read_dir` order is never semantic.

## Reservation and identity

Reservation is a permanent phase before new-entity staging, not an entity-directory substep. Before lifecycle is asked to generate a candidate or open its candidate source, the actor counts the relevant direct collection: `reservations/agents` for an Agent, `reservations/sessions` for a Session or Fork. These collections are independent. The count includes every reservation entry, including an orphan with no entity after an ordinary burned or later staging failure, cleanup, or crash. It must be below `1,000,000`: `999,999` permits one reservation and then becomes `1,000,000`; `1,000,000` returns `DurableStateTooLarge` before candidate generation, owner-job preregistration, `EntityReservationBarrier`, filesystem write, or burned ID.

After that cap phase, the actor retains the exact canonical reservation-ID inventory behind the count; count alone is insufficient to classify a candidate. Every allocation attempt has this exact owner order:

1. the lifecycle owner generates one canonical CSPRNG AgentId or SessionId candidate; all-zero IDs are forbidden;
2. immediately after generation, capture whether that candidate is present in the actor's exact pre-attempt inventory, then before owner-job preregistration or a barrier run candidate-dependent semantic preflight. For a Fork, `candidate == sourceSessionId` is a terminal ordinary unburned rejection: it increments no collision counter and does not preregister a job, cross a barrier, enter the filesystem, or request a replacement candidate. Other candidate-dependent semantic invariants use this same pre-filesystem position; no additional invariant is defined here;
3. before filesystem work, the actor pre-registers its owner-retained job/shared settlement. No candidate filesystem work may start through an untracked or detachable job;
4. the actor crosses a fresh `EntityReservationBarrier` immediately before that attempt's `create_new reservations/agents/<AgentId>` or `reservations/sessions/<SessionId>`;
5. it performs the format-owned zero-file/handle-validation/file-sync/direct-collection-sync/readback sequence in [Durable Store V1](../formats/durable-store-v1.md#directory-sync-ordering-and-certainty), then settles the attempt by the following mutually exclusive table before requesting another candidate or starting entity work.

| Observation after reconciliation under the root lease | Attempt classification and required effect |
| --- | --- |
| The candidate was present in the exact pre-attempt inventory; `create_new` reports `AlreadyExists`; no-follow exact readback proves the same canonical valid reservation marker | The sole **definite collision**: the existing ID is already globally burned. This attempt creates no marker and does not increment the collection count, but increments this request's collision counter. Below 32, only this row may request a next candidate; the 32nd returns allocation failure with no candidate 33. |
| The candidate was absent from the exact pre-attempt inventory; `create_new` succeeds; returned-opened-handle validation, file `sync_all`, direct-parent sync when `Supported`, and final no-follow proof of the same-identity regular/non-link zero-byte required-mode marker all succeed | `Reserved`: permanently retain the marker, insert the ID into the actor inventory, increment the relevant reservation count by one, and continue the same Create/Fork request into entity staging. |
| The candidate was absent from the exact pre-attempt inventory; `create_new` returns a noncollision error before any successful create or opened handle has been observed; exact no-follow readback proves the marker absent | Ordinary unburned failure. Return the ordinary error and request no replacement candidate. The error itself neither poisons nor triggers Closing; absent an independent shutdown, Runtime remains running. |
| The candidate was absent from the exact pre-attempt inventory; `create_new` returns a noncollision error before any successful create or opened handle has been observed; exact no-follow readback proves a canonical valid marker now present | Ordinary burned failure: under the root lease and no-hostile-mutation support contract, this is the fail-after-side-effect case. Permanently retain it, insert the ID into the actor inventory, increment the relevant reservation count by one, and return the ordinary error without retry/new ID. The error itself neither poisons nor triggers Closing; absent an independent shutdown, Runtime remains running. |
| The candidate was already present in the exact pre-attempt inventory; `create_new` returns a noncollision error; exact no-follow readback still proves the same canonical valid marker | Ordinary storage failure with no new burn. Keep the existing inventory/count unchanged and return the ordinary error without retry/new ID. The error itself neither poisons nor triggers Closing; absent an independent shutdown, Runtime remains running. |
| The candidate was absent from the exact pre-attempt inventory; `create_new` succeeds and its returned handle validates; then file sync or—when `Supported`—direct-parent sync errors; final no-follow proof establishes the same-identity regular/non-link zero-byte required-mode marker | Ordinary burned failure. Permanently retain it, insert the ID into the actor inventory, increment the relevant reservation count by one, and return the ordinary error without retry/new ID. The error itself neither poisons nor triggers Closing; absent an independent shutdown, Runtime remains running. |
| Any required pre-attempt membership, presence/absence, handle/path identity, regular/non-link type, zero-length, or mode proof is unavailable or ambiguous | `IndeterminatePoisoned`: do not delete, issue a new ID, retry, or fabricate rejection; poison/close. |
| An available required proof is contradictory or invalid: including success for a candidate already in the pre-attempt inventory; `AlreadyExists` for a candidate absent from that inventory; successful create followed by an absent final path or different identity; an absent/invalid marker for an inventory-present candidate; or observed link/type/zero-length/mode invalidity | `DurableStateCorruptPoisoned` (the internal `DurableStateCorrupt` integrity path, not a new public result), not a collision: do not delete, issue a new ID, or retry; poison/close through the existing fatal `InternalDispatchUnavailable` path. |

Thus create, file-sync, and supported-directory-sync errors are never ignored: each is reconciled by the table. Known `Unsupported` skips only the direct directory sync and still requires file sync and exact readback. Entropy failure, candidate-source failure, candidate-dependent rejection, storage ambiguity, and every noncollision error consume no collision count and never request another candidate.

Every `Reserved` and newly ordinary-burned result permanently retains its marker, inserts its ID into the exact actor inventory, and increments the actor's relevant count; later entity staging failure, process crash, logical deletion, cleanup, and any future purge design do not remove it. IDs are never reused. A process-local sealed attempt retains the exact candidate and canonical values; once reservation succeeds it never switches identity. An unpublished entity directory may be removed during cleanup, but its reservation remains. There is no standalone production `reserve` API, reservation token/receipt, or caller-visible reservation path: successful reservation continues only as the same actor-owned Create/Fork request. There is no V1 restart exactly-once Create/Fork: a crash or response loss after publication can leave a catalog-visible generated ID that the host did not receive. Hosts must page/query the catalog and treat blind retry as potentially creating a duplicate. Durable idempotency keys are a future design problem.

## CAS, generations and publication

`StorageGeneration` is private, nonzero, checked and bounded to `1..=1_000_000`; it is neither a domain revision nor public. A new mutation writes exactly `G + 1`, never reuses a removed/failed path until exact cleanup has completed. The actor:

```text
read current immutable head + recheck CAS/lifecycle
→ let lifecycle owner decide canonical no-op (no write of any kind)
→ for a new entity only: exact-count reservation collection before candidate/source; run the closed per-attempt reservation phase until Reserved or terminal result
→ create final markerless generation/entity directories and canonical payload files
→ sync_all payloads + exact bounded/streaming candidate readback
→ cross DurableCommitBarrier immediately before create_new(COMMITTED)
→ create_new zero-byte COMMITTED; sync it
→ supported directory sync and exact COMMITTED + all generation-payload readback
→ for a new entity, create_new/sync/readback PUBLISHED and complete-entity proof
→ install new immutable catalog/head and return
```

`StorageGeneration` exhaustion at `1,000,000` happens before any write and returns the internal too-large/user-action-required mapping. [Durable Store V1](../formats/durable-store-v1.md#directory-sync-ordering-and-certainty) owns the exact directory-sync sequence; the actor must use it, including `Supported | Unsupported` root-local classification.

No rename or CURRENT pointer is a correctness mechanism. `head.json` records `storageGeneration = G`, `previousStorageGeneration = null` for 1 and exactly `G - 1` afterwards. A definition-changing generation includes `definition.json` and points `currentDefinition.storageGeneration` to `G`; metadata/status/lifecycle-only generations omit it and retain the earlier pointer. A domain canonical no-op writes no revision, metadata revision, storage generation, timestamp, marker or event.

Actor recovery and mutation validation reconstructs the semantic owner across every adjacent committed generation, not by raw string diff. It enforces the [Durable Store V1 exact matrix](../formats/durable-store-v1.md#adjacent-generation-domain-validation): generation 1 has all storage/domain/metadata/workspace revisions at 1 and the exact initial timestamps/status/lifecycle/provenance; later Agent generations are exactly definition, metadata, or status; later Session generations are exactly definition, metadata, or lifecycle. Definition changes advance only their domain revision by one and carry `definition.json`; metadata changes advance only metadata revision by one with canonical content change; lifecycle/status changes preserve pointers/metadata and allow only the listed transitions. Session definition preserves one AgentId forever and advances WorkspaceRevision exactly with Workspace semantic change. CreatedAt/provenance changes, revision jumps/reuse, mixed categories, and canonical no-ops are corruption.

Initial publication is stricter:

```text
permanent reservation
→ create final ID directory without PUBLISHED
→ create generation 1 and write/sync/readback head + definition
→ for Session, write/sync valid Header; for Fork, semantic stream/re-readback child Header/JSONL into `PreparedConversationProof`
→ final cross-entity recheck while the required gate remains held
→ cross DurableCommitBarrier
→ create/sync generation-1 COMMITTED
→ exact COMMITTED + head + definition readback
→ create_new/sync PUBLISHED last
→ exact complete-entity readback, binding the same conversation file identity/length/Header to `PreparedConversationProof`
→ install catalog → event/completion
```

`PUBLISHED` alone is catalog visibility. A missing/invalid required file after it exists is committed corruption, never a fallback. For Session Create, the actor has already acquired the Agent gate before it constructs the final `SessionDefinition`, `SessionHeader`, and generation-1 head bytes from the current Enabled exact ref; the “final cross-entity rechecks” above are rechecks while **that same gate remains held**, not a later acceptance of an already-`COMMITTED` stale Header. Restart cleanup applies the same distinction to a committed unpublished Session: ordinary Create requires the Agent's current Enabled exact ref, while Fork requires current Enabled status plus retention of its captured exact revision and need not equal the Agent's current revision. It crosses `DurableCommitBarrier` only after that construction and releases the gate only after complete `PUBLISHED`/readback. The Agent permit may cover only bounded local durable marker/document work; it cannot cover Recorder/SessionExecutor/event/fan-out/host callback.

Fork captures the exact source definition and either `LiveForkConversationSeed` or a `RecordedForkConversationLease` issued by DurableState/source-residency ownership. The physical recorded lease binds the exact source file observation and blocks Load, append and tail truncation; Conversation Storage consumes it to replay, resolve the anchor and stream the selected semantic path. The live seed needs no long file lease after capture. Provenance must name that actual source and must not self-reference the child. The short source residency/lifecycle guard is released before target I/O; a recorded physical lease remains through copy/readback, is then released, and never overlaps an Agent permit. After markerless child materialization/validation, final publication acquires the Agent gate, verifies the captured exact Agent definition still exists and Agent is Enabled, crosses `DurableCommitBarrier`, and holds the gate through `PUBLISHED`/readback. The child is `Open + Unloaded`, has child-local timestamps, workspace/definition revision 1, `name = null`, `description = null`, metadata revision 1, and durable provenance; later source mutation cannot alter it. The public Fork result stays `session_id + source`; callers use `GetSession` for tokens.

## Publication certainty and crash contract

Physical publication has only internal states:

- `NotPublished`: exact marker observation proves absent; return the ordinary typed error.
- `Published`: exact marker and canonical payload readback prove present; complete the command.
- `CommittedCorruptPoisoned`: a required candidate marker definitely exists but the required payload is missing, invalid, or different. It is not semantic completion; poison, close, and make restart store-open fail.
- `IndeterminatePoisoned`: neither fact can be established; never delete/rollback, reserve a new ID, retry another mutation, or fabricate a Rejected semantic completion. Poison DurableState and terminate Runtime.

A marker syscall success or ambiguity is reconciled under the root lease by exact marker plus exact canonical payload readback. For a new entity, a generation-1 `COMMITTED` sync error with exact valid generation readback is not yet catalog completion: the owner continues the mandatory `PUBLISHED` attempt while remembering close-required. If `PUBLISHED` is then proven, it installs/Completes before Closing; if PUBLISHED is proven absent, it settles ordinary failure only after exact cleanup and then closes; indeterminate/corrupt observation poisons. A `create_new(PUBLISHED)` error reported after the marker side effect is the narrow exception: when exact marker/entity readback and every supplemental marker/entity/collection sync all succeed, the ambiguity is fully repaired and the command is `Completed + running`; a failed supplemental step is remembered close-required. Once the applicable catalog/head marker plus exact payload prove `Published`, any remaining remembered or later marker/directory-sync error has this mandatory order: install the immutable catalog, fulfill **all** joined command waiters with `Completed`, then transition mutation-disabled/Closing and run shutdown. Closing must not overtake that completion. `CommittedCorruptPoisoned` and `IndeterminatePoisoned` settle every joined in-process post-admission dispatch waiter as `Err(RuntimeDispatchError::InternalDispatchUnavailable)`—the sole integrity-fatal outer result, which does not claim the mutation was absent or rejected. Transport sends it if possible and then closes; otherwise it closes the connection. The host must query/reopen and must not blind-retry Create/Fork. Later requests after close receive `RuntimeClosed`; no new wire outcome, error variant, or code exists.

The only crash guarantee is MiniCore process abort/SIGKILL while kernel, mount and filesystem continue operating: restart observes a complete old/new state or invisible entity staging. It excludes OS/kernel crash, power loss, controller cache, remote server, lying sync and hostile edits. `sync_all` is required before a marker; zero-byte marker sync is attempted. At open, directory sync is classified `Supported` or `Unsupported`; only the latter is tolerated. POSIX uses directory sync where classified supported; Windows std offers no promised directory sync. An unexpected directory-sync I/O error before marker means no publication; after marker, exact readback means Published plus Runtime close. Errors are never generically ignored.

## Recovery, retention and bounds

Startup acquires the root lease, runs the exact fresh-store bootstrap/markerless eligibility rule in [Durable Store V1](../formats/durable-store-v1.md#fresh-store-bootstrap), validates the format/root namespace, caps before allocation/sort, and then completes exact markerless/uncommitted cleanup before mutations are ready. Cleanup closes handles, never follows links and may remove only exact operation-produced unmarked staging with its matching permanent reservation; no reservation or malformed shape is corruption, not recursively removable. A same-process pre-marker failure removes its entire exact uncommitted generation/entity before another mutation; it may not skip/reuse that generation path until exact cleanup succeeds. Any cleanup failure blocks store open at startup, or transitions the current process to mutation-disabled/Closing—never silently to read-only operational mode. An uncommitted trailing generation is ignored for current head then cleaned; an invalid/missing payload behind `COMMITTED`, a generation gap, pointer mismatch, invalid cross-generation domain transition, duplicate/case alias or corrupted highest committed generation fails the entire store open. Committed generations are contiguous from 1 and the highest canonical committed generation is current. Public catalog has no corrupt placeholder.

For an **unpublished new Agent or Session**, `PUBLISHED` absence is the sole physical visibility classification: an exact operation-produced markerless entity is invisible and cleanup-only, while an observed (even malformed) `PUBLISHED` name always stays on the published corruption path. This intentionally cannot distinguish a manual `PUBLISHED` deletion from process-abort staging under the supported no-hostile-mutation contract; DurableState never guesses or republishes. It accepts one store-wide cleanup candidate only—one published trailing G+1 *or* one whole unpublished entity—and reports any combination of two as `DurableStateCorrupt` after every direct/nested cap-first physical scan has completed. Within the entered Agent/Session entity physical-scan phase, a later `DurableStateTooLarge` overrides the first other physical corruption/unavailable result; root and reservation collection cap rules are unchanged. It does not delete anything in that case.

The unpublished shape, committed-G1 semantic requirements, and exact cleanup ordering are format-owned in [Durable Store V1](../formats/durable-store-v1.md#unpublished-new-entity-recovery-and-cleanup). DurableState's recovery order is deliberate: recover the complete published Agent catalog; recover the complete published Session catalog and its Fork graph; validate one unpublished committed Session against those published facts and its same-open strict conversation classifier; close scanner handles; then execute the sole deferred plan. An unpublished child never enters either catalog. Its Fork source must name a published Session, but recovery does not cross-check its anchor against source conversation bytes. The strict classifier is cleanup-only and is not a `PreparedConversationProof`.

The cleanup plan is a narrow private enum (`published trailing generation | unpublished whole entity`) containing only owned paths and exact current-platform-observable shape facts, with redacted `Debug`; it owns no `ReadDir`, `DirEntry`, raw metadata, or `File`. A whole-entity plan captures the initial reservation, entity, `generations`, G1, zero-byte `COMMITTED`, and regular payload/conversation facts; Unix revalidation compares device-plus-inode for each node and also compares regular-file length. It preserves the initial generation collection cap for revalidation rather than silently switching to a production constant. Before deletion it revalidates the permanent reservation, explicit marker absence, and every planned entity/G1/conversation/payload fact. It removes `COMMITTED` first, then payload/conversation, and non-recursively removes G1, `generations/`, and the entity while syncing each direct parent once when directory sync is supported. It never deletes reservations and never calls production `remove_dir_all`. A partial deletion is not accepted by the old plan: a later open freshly classifies the exact remaining uncommitted prefix and resumes cleanup, while file-removal uncertainty stays unavailable.

V1 retains every reservation, PUBLISHED entity, COMMITTED generation, published definition, logical Deleted head, conversation and fork provenance. It has no physical purge or GC; a future reachability/retention ADR is required.

`head.json` and `definition.json` including LF are each at most 1,048,576 bytes. Their JSON preflight uses depth 64, object members 256, arrays 4,096, strings 262,144 and duplicate-aware typed decode. The exact root/reservation/entity/payload/collection caps and count-before-fail behavior are owned by [Durable Store V1](../formats/durable-store-v1.md#scanner-and-recovery-precedence) and map to `DurableStateTooLarge`. Session conversation must exist and be regular for every PUBLISHED Session; its strict Header/content scan is Conversation Storage's Load concern and may make only that Session `DurableStateCorrupt` or `DurableStateTooLarge`.

## Filesystem and error seams

There are exactly two filesystem adapters: production `LocalFilesystem` and test-only `DeterministicPersistentFaultFilesystem`, both behind the private synchronous filesystem port. The persistent adapter uses a real temporary root and injects fail-before, partial write, fail-after-side-effect, sync, marker, enumeration and cleanup faults; it also models operation-succeeded-but-caller-did-not-observe and abrupt drop-without-cleanup then reopen of the same bytes. It is not a generic backend or executor hierarchy.

Owner-local errors include `StoreInUse`, `UnsupportedStoreFormat`, `DurableStateCorrupt`, `DurableStateTooLarge`, ordinary unavailable/I/O errors, stale CAS/lifecycle rejection, `NotPublished`, `CommittedCorruptPoisoned`, `IndeterminatePoisoned`, and poison/closing. Public mapping stays at Runtime's existing closed error codes: DurableState post-admission integrity poison uses the same sole fatal outer result `RuntimeDispatchError::InternalDispatchUnavailable` that Runtime also reserves for required post-commit live-publication poison; no new completion outcome is introduced. Root lease/lock-file identity integrity errors fail store open or close Runtime globally; only ordinary conversation capability/open/write failure with the root lease intact degrades the affected loaded Session.

DurableState errors are redacted at every non-private boundary: `Debug`, `Display`, and public diagnostics never expose a store root/path, raw head/definition/JSONL bytes, metadata text, OS source strings, or a secret. Exact paths and raw OS sources may appear only in private controlled logs when that deployment explicitly allows them; they never enter a Runtime error, command outcome, Snapshot, StateEvent, fixture diagnostic, or source chain exposed to a host.

## Test matrix

Implementation must use named typed barriers/fault coordinates and prove job settlement by join, not merely Tokio time advance. The closed authoritative fixture coordinates cover bootstrap, permanent reservation/collision/cap/ambiguity, representative payload create/partial/sync failures, `COMMITTED`/`PUBLISHED` boundaries and readback, new-entity Create/Fork complete-or-invisible, existing-head update old/new visibility, Completed-before-Closing, poison, response loss, caller drop/join failure, source-lease races, cleanup/namespace/caps, loaded Workspace publication, and Recorder settlement/replay. Owner property/race tests additionally traverse each operation's internal syscall sequence without claiming that the fixture file enumerates every OS syscall. Native process tests run on Linux, macOS and Windows.

The authoritative table inputs are [Durable Store V1 Fixtures](../fixtures/durable-store-v1/README.md).
