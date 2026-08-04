# DurableState 架构设计

状态：M5.0 design gate 已冻结；Store recovery foundation implementation in progress（ADR 0136、0137）
日期：2026-08-03

## 目的

`DurableState`是MiniCore private deep module：在一个专用、user-private local store root中，拥有Agent/Session entity的物理布局、永久identity reservation、root lease、catalog/head installation、immutable generation、CAS recheck、marker publication、readback、recovery/cleanup、poison/closing状态和filesystem fault seam。它把跨文件durability复杂性收在一个operation owner内，而不是交给lifecycle caller拼装。

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

`DurableStateActor` is the single process-local mutable owner and queue for **every** entity mutation and catalog/head installation. The root OS lease prevents another cooperating process, but is not internal serialization. **No caller may hold an Agent or durable Session lifecycle/mutation gate while awaiting the actor.** Actor status mutations take the exclusive side of the same private per-Agent `AgentLifecycleGate` whose admission side issues short `AgentAdmissionPermit`s; this one gate/epoch is the linearization point against initiating Input live apply. The actor alone acquires private durable cross-entity gates in the fixed `Agent → Session` order and owns the mutation slot. It always reads/rechecks current immutable state and every public CAS/lifecycle precondition before no-op detection: a stale expected value remains stale even when the candidate equals current state. A bounded Create/Update document preparation may run while the actor serializes its operation; publication never becomes a detached job.

Create, Upgrade and Fork acquire their required Agent gate inside the actor. For Session Create specifically, before that gate the actor may receive only parent-independent semantic/canonical candidate fragments containing **neither** the assigned `SessionId` nor an `AgentRevisionRef`. The actor acquires the Agent gate, reads the current exact ref while `Enabled`, and constructs the final `SessionDefinition`, `SessionHeader`, and generation-1 head bytes. It writes/syncs/readbacks that bounded markerless payload, then crosses `DurableCommitBarrier` immediately before `COMMITTED`. It holds that same gate through complete `PUBLISHED`/readback; this intentional bounded-size but potentially unbounded-latency local-publication head-of-line tradeoff prevents a disable/delete race from publishing a new executable reference. Event/fan-out, SessionExecutor work, Recorder work, and host callbacks are forbidden under it. A potentially 1 GiB Fork copy streams outside the actor through an actor-owned and actor-tracked job into the final markerless child path. It first captures the typed source seed/lease and releases the short source residency/lifecycle guard before target I/O; a recorded physical lease itself remains held through copy/readback. The job then returns a validated `PreparedConversationProof` plus final Agent validation/publication work to the actor. The actor owns and joins every blocking job even when the dispatch waiter drops. A join panic/failure after possible publication poisons/starts Runtime close; dropping a `JoinHandle` to detach publication is forbidden.

## Named barriers and runtime shutdown

The following owner barriers are shared by DurableState, lifecycle, SessionExecutor, and Recorder. For durable entity commands, `RuntimeClosing` rejection applies only before the relevant durable barrier. Recorder additionally respects live truth: after a recordable fact is live-applied, pre-write shutdown yields `NotRecorded`/Degraded and cannot retroactively reject the owning command:

- `EntityReservationBarrier` is immediately before permanent reservation. Shutdown/cancellation before it can reject with no burned ID.
- After reservation but before `DurableCommitBarrier`, shutdown/cancellation may stop and clean staging, but the reservation remains burned.
- Markerless final-path staging before `DurableCommitBarrier` is cancellable: directories and payload files may be created, written, synced, and streaming-readback validated, but neither `COMMITTED` nor `PUBLISHED` exists. Cancellation closes handles and removes only the exact operation-produced markerless staging; the reservation remains burned.
- `DurableCommitBarrier` is immediately before `create_new(COMMITTED)`. After crossing it, caller drop or shutdown cannot cancel: the operation settles through marker/readback/completion or the fatal outer error.
- Fork semantic copy and its full bounded-memory streaming validation occur before `DurableCommitBarrier`; cancellation cleans the markerless child and releases its source lease.
- `RecorderWriteBarrier` is after encode/size validation and immediately before the first physical append write. After crossing, append settles and its health transition is installed.

`MiniCoreRuntime::shutdown().await` is host-only and non-wire; the four protocol entry points remain exactly `dispatch`, `query`, `snapshot`, and `subscribe`. It is idempotent: transition to Closing/reject new admission; settle accepted work according to those barriers; stop/unload SessionExecutors and join current Recorder jobs; join DurableState staging/blocking jobs and stop its actor; close conversation handles; release the root lease; then mark Closed. Facade `Drop` only sends a best-effort Closing signal and never blocks; the owner registry retains task handles/self-Arcs so dropping the facade does not detach a raw `JoinHandle`. Hosts must still await shutdown to observe graceful completion and root-lease release before Tokio teardown.

## Root lease and supported store

Store open creates or reuses the permanent regular `<root>/.minicore.lock`, opens it without following links, and later uses `fs4 = { version = "0.13.1", default-features = false, features = ["sync"] }` `try_lock_exclusive`. One non-cloned handle is retained until every store/recorder job has settled and the store closes. The lock file is never deleted or renamed; its existence and PID have no authority. This global root lease replaces all per-Session OS locks. Lease denial is Runtime `StoreInUse`, never one Session's `Degraded` recording health. Root lease loss or invalid physical identity poisons DurableState and initiates Runtime close.

`ExclusiveWritableConversationLease` has **no public or caller-visible constructor**. `DurableState` is its sole future production issuer/factory: while holding the root lease it binds the SessionId and same-open-file physical observation into the opaque proof. The existing `#[cfg(test)]` constructor remains M3-only until this implementation exists. Recorder Load/open/write failures with root lease intact remain local to that Session and create `RecordingHealth::Degraded`; loss of root lease/lock-file identity is global poison/close, never Session Degraded.

MVP support is claimed only when host configuration supplies a dedicated user-private coherent root on APFS, NTFS, ext4 or XFS and the native platform suite passes. MiniCore validates current platform-observable facts; it does not pretend to identify every mount technology perfectly. NFS, SMB, WebDAV, FUSE, overlay filesystems, mapped remote drives and hostile concurrent filesystem mutation are unsupported host configurations. The root itself must not be a symlink/reparse point. Unix creation targets use directories `0700` and files `0600`; Windows relies on the trusted inherited user-private ACL. Unix whole-entity cleanup captures and revalidates device-plus-inode facts for directories and zero markers, plus length for regular files. The non-Unix seam is deliberately narrower: **full Windows/NTFS identity, reparse, and native-process coverage remains an M5.0 pending platform gate.** This slice's process-abort/no-hostile-mutation cleanup scope does not claim to close that platform gate.

Every recognized component must have exact ASCII spelling. Recovery rejects observed case-fold aliases, lossy/non-UTF-8 recognized names, recognized symlinks, and unexpected recognized namespace entries. It enumerates then sorts canonical parsed names; `read_dir` order is never semantic.

## Reservation and identity

The lifecycle owner obtains a CSPRNG candidate source; all-zero IDs are forbidden. DurableState atomically reserves a candidate with `create_new reservations/agents/<AgentId>` or `reservations/sessions/<SessionId>`. At most 32 **definite** reservation collisions consume attempts. Entropy or storage ambiguity consumes no further collision attempt. A process-local sealed attempt retains the exact candidate and canonical values; once reservation succeeds it never switches identity.

Reservations survive ordinary failure, process crash, logical deletion, staging cleanup and any future purge design. IDs are burned and never reused. Before `EntityReservationBarrier`, the single actor counts the relevant reservation collection and requires it to contain fewer than 1,000,000 entries; cap exhaustion returns `DurableStateTooLarge` without burning a candidate or writing anything. Thirty-two definite collisions return the allocation failure without attempting a thirty-third reservation. An unpublished entity directory may be removed during cleanup, but its reservation remains. There is no V1 restart exactly-once Create/Fork: a crash or response loss after publication can leave a catalog-visible generated ID that the host did not receive. Hosts must page/query the catalog and treat blind retry as potentially creating a duplicate. Durable idempotency keys are a future design problem.

## CAS, generations and publication

`StorageGeneration` is private, nonzero, checked and bounded to `1..=1_000_000`; it is neither a domain revision nor public. A new mutation writes exactly `G + 1`, never reuses a removed/failed path until exact cleanup has completed. The actor:

```text
read current immutable head + recheck CAS/lifecycle
→ let lifecycle owner decide canonical no-op (no write of any kind)
→ cross EntityReservationBarrier; create permanent reservation when needed
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

A marker syscall success or ambiguity is reconciled under the root lease by exact marker plus exact canonical payload readback. For a new entity, a generation-1 `COMMITTED` sync error with exact valid generation readback is not yet catalog completion: the owner continues the mandatory `PUBLISHED` attempt while remembering close-required. If `PUBLISHED` is then proven, it installs/Completes before Closing; if PUBLISHED is proven absent, it settles ordinary failure only after exact cleanup and then closes; indeterminate/corrupt observation poisons. Once the applicable catalog/head marker plus exact payload prove `Published`, any remembered or later marker/directory-sync error has this mandatory order: install the immutable catalog, fulfill **all** joined command waiters with `Completed`, then transition mutation-disabled/Closing and run shutdown. Closing must not overtake that completion. `CommittedCorruptPoisoned` and `IndeterminatePoisoned` settle every joined in-process post-admission dispatch waiter as `Err(RuntimeDispatchError::InternalDispatchUnavailable)`—the sole integrity-fatal outer result, which does not claim the mutation was absent or rejected. Transport sends it if possible and then closes; otherwise it closes the connection. The host must query/reopen and must not blind-retry Create/Fork. Later requests after close receive `RuntimeClosed`; no new wire outcome, error variant, or code exists.

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