# Durable Store V1

状态：当前权威 physical format specification；M5.0 production recovery/root lease、private permanent reservation foundation、crate-private Agent Create与ordinary Session Create exact G1 COMMITTED/PUBLISHED publication、unloaded RecordedHistory + Genesis Session Fork tracer，以及Agent status/definition/metadata与Session metadata G2 CAS tracers已实现；remaining Fork anchors/LiveSnapshot、Session definition/lifecycle update/CAS、public Runtime command及完整cross-platform native matrix pending
日期：2026-08-03

## Scope

本文冻结DurableState V1的路径、namespace、bytes、canonical JSON、field order、scanner/recovery precedence和limits。operation/CAS/publication/lease semantics由[DurableState](../modules/durable-state.md)拥有；Agent/Session semantic values由[Agent 与 Session 生命周期](../modules/agent-session-lifecycle.md)拥有；SessionHeader/JSONL与replay由[Conversation Storage](../modules/conversation-storage.md)拥有。

V1不使用rename或CURRENT pointer作为correctness，且没有online migration、WAL、physical purge、generation GC或definition GC。

## Fresh-store bootstrap

Bootstrap is idempotent only for an input that is (a) a nonexistent root, (b) an empty user-private root, or (c) a crash-left **markerless empty scaffold**. The latter contains only the permanent `.minicore.lock` and an exact subset of these empty fixed directories: `reservations`, `reservations/agents`, `reservations/sessions`, `agents`, `sessions`. It contains no reservation, entity, payload, unknown entry, alias, link/reparse point, non-UTF-8 name, or nonempty directory. Every other markerless root is `UnsupportedStoreFormat` and open is blocked: V1 must never guess, migrate, delete, or reinterpret durable/unknown/nonempty content.

The opener creates or validates the user-private root, creates/opens the permanent lock file without following a link, acquires the root lease, and then strict-scans this markerless eligibility. It creates each fixed directory **one component at a time**, requiring exact case, directory type, and mode (`0700` Unix), and uses `0600` for newly-created regular files. After every directory creation it syncs that directory's direct parent when directory sync is `Supported`. It then creates and syncs the zero-byte `MINICORE_STORE_V1` **last**, syncs the root when supported, and exact-readbacks the marker before ordinary recovery. Thus an abort before the marker may resume only from the eligible empty scaffold. A marker-present root missing or invalidating any fixed directory is corruption, not a bootstrap candidate.

## Exact layout

```text
<root>/
  .minicore.lock
  MINICORE_STORE_V1
  reservations/
    agents/<AgentId>
    sessions/<SessionId>
  agents/<AgentId>/
    PUBLISHED
    generations/<20-digit StorageGeneration>/
      head.json
      definition.json?
      COMMITTED
  sessions/<SessionId>/
    PUBLISHED
    conversation.jsonl
    generations/<20-digit StorageGeneration>/
      head.json
      definition.json?
      COMMITTED
```

`MINICORE_STORE_V1`, every reservation, `PUBLISHED`, and `COMMITTED` are regular files of exactly zero bytes. They are existence-only; a directory, symlink, nonzero file or any alternative byte representation is invalid. `.minicore.lock` is a permanent regular lock file and is not an existence-only marker. `definition.json` is present only in a generation introducing a new domain definition. This slice enforces its current no-follow/type and platform-observable checks; Unix whole-entity cleanup compares device-plus-inode facts. Full Windows/NTFS identity, reparse, and native-process coverage remains the separate M5.0 pending platform gate.

Recognized names are ASCII and exact-case only:

```text
.minicore.lock  MINICORE_STORE_V1  reservations  agents  sessions
generations  PUBLISHED  COMMITTED  head.json  definition.json  conversation.jsonl
```

An AgentId directory is exactly canonical `agt_<32 lowercase hex>`; a SessionId directory is exactly canonical `ses_<32 lowercase hex>`. Names with different case, lossy/non-UTF-8 spelling, an observed link, or any unexpected recognized namespace entry fail strict recovery. Full Windows reparse-point recognition remains within the pending M5.0 platform gate. Enumerators parse canonical names, sort them bytewise, count all entries toward their relevant cap, then process; filesystem enumeration order is not meaningful.

Unix creation uses `0700` directories and `0600` files. The root's user-private coherent-local-filesystem requirement and root lease are specified by DurableState.

## Storage generation

`StorageGeneration` is a private ordinary JSON integer in `1..=1_000_000`, never a string and never a domain revision. Its directory name is its decimal value zero-padded to exactly 20 ASCII digits; e.g. generation `1` is `00000000000000000001`. No leading/sign/alternate form is accepted. A committed chain starts at one, is contiguous, and records `previousStorageGeneration = null` at one or exactly `G - 1` later.

The highest valid canonical `COMMITTED` generation is current. A markerless/uncommitted trailing generation is staging and may be removed only by DurableState cleanup under the root lease. A `COMMITTED` generation with absent/invalid payload, a gap, a head/path/pointer mismatch, duplicate/case alias, or an invalid highest committed generation is corruption; recovery never falls back to an older head.

## Document encoding

Every `head.json` and `definition.json` is canonical compact UTF-8 JSON followed by **exactly one LF**. It has no BOM, CR, trailing spaces or extra line ending. Its total size including LF is at most 1,048,576 bytes. The writer emits fields in the order below; options are present and use JSON `null`. Readers require duplicate-aware parsing and reject duplicate known fields. Unknown fields are not V1-compatible in these closed durable documents.

Preflight limits are depth 64, object members 256, array items 4,096 and decoded UTF-8 string bytes 262,144. IDs/revisions/timestamps use their existing Wire V1 canonical carriers: Agent/Session IDs use typed prefixes, revisions use `ar_N`, `sdr_N`, `amr_N`, `smr_N`, `wr_N`, and timestamps are millisecond-truncated RFC 3339 UTC text. Storage-generation values are the sole new ordinary JSON integer here.

Dynamic JSON is not embedded. Arrays called `promptIds` contain canonical PromptId strings sorted by their UTF-8 bytes and have no duplicates. `additionalRoots` retain semantic definition order.

## Agent documents

An Agent definition has this exact object shape and order:

```json
{"agentId":"agt_11111111111111111111111111111111","revision":"ar_1","promptIds":["base","safety"],"createdAt":"2026-08-03T10:00:00.123Z"}
```

| Field | Rule |
| --- | --- |
| `agentId` | matches the enclosing `agents/<AgentId>` directory |
| `revision` | immutable AgentRevision; definition changes advance exactly N+1 |
| `promptIds` | sorted, duplicate-free Agent prompt selection |
| `createdAt` | definition creation timestamp |

An Agent head has this exact object shape and order:

```json
{"entity":"agent","agentId":"agt_11111111111111111111111111111111","storageGeneration":1,"previousStorageGeneration":null,"currentDefinition":{"revision":"ar_1","storageGeneration":1},"metadata":{"revision":"amr_1","name":"Planner","description":null,"updatedAt":"2026-08-03T10:00:00.123Z"},"status":"enabled","createdAt":"2026-08-03T10:00:00.123Z"}
```

`entity` is literally `agent`; `status` is `enabled`, `disabled` or `deleted`. `currentDefinition` has exactly `revision`, `storageGeneration`, in that order. `metadata` has exactly `revision`, `name`, `description`, `updatedAt`, in that order. `name` is nonempty canonical safe text; `description` is nullable canonical safe text. `metadata.updatedAt` changes only when metadata revision changes. There is intentionally no generic head `updatedAt`.

A definition-changing generation contains that definition file and `currentDefinition.storageGeneration = storageGeneration`. A metadata/status-only generation omits `definition.json` and repeats a pointer to an earlier committed definition generation.

## Session documents

A Session definition has this exact object shape and order:

```json
{"sessionId":"ses_22222222222222222222222222222222","revision":"sdr_1","agent":{"agentId":"agt_11111111111111111111111111111111","revision":"ar_1"},"workspace":{"revision":"wr_1","primaryRoot":{"key":"repo","path":"file:///Users/example/project","requestedAccess":"read_write","sources":{"prompt":true,"skill":true}},"additionalRoots":[],"cwd":{"root":"repo","relativePath":"src"}},"model":{"selection":{"providerId":"openai","modelId":"gpt-5"},"reasoning":"auto","maxOutputTokens":4096},"promptIds":["base","session-notes"],"createdAt":"2026-08-03T10:01:00.456Z"}
```

The field order is `sessionId`, `revision`, `agent`, `workspace`, `model`, `promptIds`, `createdAt`. Nested orders are exactly shown:

```text
agent: agentId, revision
workspace: revision, primaryRoot, additionalRoots, cwd
root: key, path, requestedAccess, sources
sources: prompt, skill
cwd: root, relativePath
model: selection, reasoning, maxOutputTokens
selection: providerId, modelId
```

`maxOutputTokens` is an integer or `null`; non-null is in `1..=u32::MAX`. `reasoning` is `auto`, `disabled`, `low`, `medium` or `high`. `requestedAccess` is `read_only` or `read_write`. Workspace root `path` is the **Wire-owned [CanonicalFileUri](../modules/wire-schema.md#workspace-paths) carrier by reference**, exact canonical text and never an ambient/native parser result. Semantic `WorkspaceRootSpec` still owns a native `PathBuf`: DurableState's encoder explicitly converts its lossless current-host path to `CanonicalFileUri`; decode checked-lowers that URI for the current host. An unsupported family, non-lossless path or invalid native path fails current-definition load. No ambient URI/path parser is permitted. `WorkspaceRootSpec` remains a Workspace semantic type rather than a second durable DTO.

Every Session definition's Agent ref must resolve to the retained exact Agent definition. A Session's revisions must retain its one AgentId; ref revisions may differ only through the lifecycle's explicit same-Agent upgrade rule.

A Session head has this exact object shape and order:

```json
{"entity":"session","sessionId":"ses_22222222222222222222222222222222","storageGeneration":1,"previousStorageGeneration":null,"currentDefinition":{"revision":"sdr_1","storageGeneration":1},"metadata":{"revision":"smr_1","name":null,"description":null,"updatedAt":"2026-08-03T10:01:00.456Z"},"lifecycle":"open","forkProvenance":null,"createdAt":"2026-08-03T10:01:00.456Z"}
```

The order is `entity`, `sessionId`, `storageGeneration`, `previousStorageGeneration`, `currentDefinition`, `metadata`, `lifecycle`, `forkProvenance`, `createdAt`. `entity` is literally `session`; `lifecycle` is `open`, `archived` or `deleted`. `forkProvenance` is always present: `null` for an ordinary Session, otherwise exactly:

```json
{"sourceSessionId":"ses_33333333333333333333333333333333","source":"recorded_history","anchor":{"type":"after_user_message","data":{"itemId":"itm_44444444444444444444444444444444"}}}
```

Its field order is `sourceSessionId`, `source`, `anchor`; `source` is `live_snapshot` or `recorded_history`. `sourceSessionId` is the exact Session bound to the captured source lease/seed and must differ from the child SessionId. `anchor` is the closed mixed adjacent enum: Genesis is **exactly** `{"type":"genesis"}` and omits `data`; `data:null` and `data:{}` are invalid. `before_user_message`, `after_user_message`, `before_final_agent_message`, and `after_final_agent_message` are exactly `{"type":"...","data":{"itemId":"..."}}`. It stores no source path, lease, token, current source lifecycle, secret or conversation bytes.

As with Agent, metadata has no generic head timestamp and `metadata.updatedAt` changes only with metadata revision. A lifecycle-only generation omits `definition.json` and retains the exact existing current-definition pointer.

## Conversation path and publication consistency

For every PUBLISHED Session, `sessions/<SessionId>/conversation.jsonl` must exist, be regular, and be neither a link nor reparse point. Its exact contents are [Conversation JSONL Format V1](conversation-jsonl-v1.md); its Header session ID must match the enclosing Session. It is materialized and synced before generation-1 `COMMITTED`, and a Fork child is **semantic streaming re-encoded**, never raw source JSONL byte-copied: it has a new child Header and every selected entry is validated/re-encoded with only `sessionId` rebound to the child. Conversation semantic/header failure is reported per Session at Load; required entity head/document failure is whole-store corruption.

At the entity level, `PUBLISHED` is the sole catalog root. Before it exists, an exact markerless entity is invisible staging and removable, while its reservation stays permanent. After it exists, all required files, contiguous committed generations, document identity, definition pointers, cross-entity Agent refs, and for Session the regular conversation file must be complete; absence is not recoverable by fallback.

### Unpublished new-entity recovery and cleanup

`PUBLISHED` is the **sole** visibility root. Under V1's process-abort/no-hostile-mutation contract, a missing `PUBLISHED` plus one of the exact operation-produced shapes below is classified as invisible staging. A host or user who manually removes `PUBLISHED` is physically indistinguishable from that crash state; recovery classifies by marker absence and exact shape, removes the invisible entity, and never attempts to republish it. An observed `PUBLISHED` name always takes the published-marker validation path: an invalid type, link, mode, length, or incomplete published layout is corruption and cannot be downgraded to staging.

An unpublished Agent may be exactly one of: an empty entity; an entity containing only an empty `generations/`; an entity containing only `generations/00000000000000000001/`; that G1 with any subset of `head.json` and `definition.json` and no `COMMITTED`; or that G1 with exactly `head.json`, `definition.json`, and a zero-byte `COMMITTED`. No other generation number, second generation, child, type, link/reparse point, mode, or document-size violation is staging. An unpublished Session has the same entity/G1 possibilities plus an optional physical `conversation.jsonl`. That file is private regular/non-link and at most 1 GiB by metadata; it may exist only when G1 exists, and it is required when G1 has `COMMITTED`. Before `COMMITTED`, recovery never reads conversation or partial head/definition bytes.

Every unpublished entity must have its exact permanent zero-byte reservation. For a G1 containing `COMMITTED`, Agent recovery decodes G1 and applies the ordinary generation-one Agent semantic validation. Session recovery first completes the published Agent and Session catalog and validates the published Fork graph, then decodes/validates G1 against that catalog. Every such candidate requires the referenced Agent's current head to be `enabled`. An ordinary Create candidate (`forkProvenance = null`) must additionally pin that Agent's current exact `AgentRevisionRef`; a Fork candidate must instead retain its captured exact revision in that Agent's retained-definition index, and may therefore reference an older retained revision. It reconstructs the expected `SessionHeader { formatVersion: 1, sessionId, createdAt, initialAgent, initialDefinitionRevision }` from durable G1 facts and strict-classifies the same-open observed conversation file: ordinary Sessions require Header-only; Forks require a canonical linear file (including Header-only). A Fork source must be in the final **published** Session catalog. This cleanup classifier neither adds the child to the catalog nor validates anchors/source bytes, and is not `PreparedConversationProof`.

One startup accepts at most one operation-staging candidate across the entire store: either one published entity's trailing markerless G+1 or one unpublished Agent/Session entity. Any two candidates, including Agent+Session or unpublished+published-tail combinations, are `DurableStateCorrupt` and cause zero cleanup. Recovery completes every Agent/Session direct-entity and nested generation/payload cap-first physical scan before reporting that multiplicity, so a later `DurableStateTooLarge` is not hidden by an earlier second candidate. All published catalog, semantic, and Fork validation succeeds before any cleanup or catalog installation.

Whole-entity cleanup owns only paths and exact current-platform-observable physical-shape facts; it retains no scanner iterator, `DirEntry`, file handle, or raw metadata. Immediately before deletion it rechecks the permanent reservation, explicit `PUBLISHED` absence (`NotFound` only), every entity/generation/G1/conversation child and its initial type/mode/size/identity facts. On Unix these facts include device-plus-inode for the entity, `generations`, G1, reservation, and zero-byte `COMMITTED` nodes, and device-plus-inode plus length for regular payload/conversation files; same-shape replacement is corruption with zero deletion. The non-Unix seam is intentionally limited to currently observable facts: full Windows/NTFS identity, reparse, and native-process coverage remains an M5.0 pending platform gate, not a claim closed by this slice. Added, missing, changed, or unknown children, a changed `COMMITTED`, or a newly present `PUBLISHED` are corruption with zero deletion; an absence observation other than `NotFound` is unavailable. Production never uses recursive deletion. It deletes, skipping already-absent layers only on a later fresh recovery classification, in this order:

1. `COMMITTED`, if present, before every other child;
2. `definition.json`, then `head.json`, then `conversation.jsonl`;
3. sync G1, remove G1, sync `generations/`;
4. for an empty `generations/`, sync it before removal; remove it and sync the entity;
5. remove the entity and sync its `agents/` or `sessions/` collection.

Each directory sync is performed at most once where supported and follows the direct child-namespace removal it makes durable. A `remove_dir` error is reconciled only by an immediate no-follow `NotFound` observation; file-removal errors remain unavailable. Thus a crash/fault after `COMMITTED` removal or any later known-prefix removal is retried only by a fresh open of the remaining exact uncommitted prefix. The reservation is never removed.

## Adjacent-generation domain validation

Recovery reconstructs semantic owners from all committed adjacent generations; it is not a raw JSON-string diff. Generation 1 is exact create only: storage/domain/metadata/workspace revisions are all `1`; Agent status is `enabled`; Session lifecycle is `open`; head `createdAt`, initial definition `createdAt`, and metadata `updatedAt` are equal; Session fork provenance is immutable `null` or exact child provenance.

Every later generation is exactly one category, and no committed generation may combine categories, change entity `createdAt` or fork provenance, jump/reuse a revision, or encode a canonical no-op:

| Entity/category | Required adjacent change; all unlisted persisted facts remain unchanged |
| --- | --- |
| Agent definition | AgentRevision is exactly N+1; `definition.json` is present; current-definition pointer moves to G; metadata/status/entity `createdAt` unchanged. |
| Agent metadata | no `definition.json`; current-definition pointer/status/`createdAt` unchanged; AgentMetadataRevision exactly N+1 and canonical metadata content changes; `updatedAt` may equal prior value because milliseconds truncate. |
| Agent status | no `definition.json`; pointer/metadata/`createdAt` unchanged; only Enabled↔Disabled or Enabled/Disabled→Deleted; a request that would persist the same status writes no generation; Deleted terminal. |
| Session definition | SessionDefinitionRevision exactly N+1 and `definition.json` present; metadata/lifecycle/fork provenance/entity `createdAt` unchanged; one AgentId forever; WorkspaceRevision changes exactly N+1 iff Workspace semantic value changes, otherwise remains unchanged. |
| Session metadata | no `definition.json`; pointer/lifecycle/fork provenance/`createdAt` unchanged; SessionMetadataRevision exactly N+1 and canonical content changes; `updatedAt` may equal prior value. |
| Session lifecycle | no `definition.json`; pointer/metadata/fork provenance/`createdAt` unchanged; only Open→Archived, Archived→Open, or Archived→Deleted; a request that would persist the same lifecycle writes no generation; Deleted terminal. |

A later head's `previousStorageGeneration` must be exactly its immediate predecessor. Recovery validates the whole chain and fails the whole store on any violation. Public completion semantics remain lifecycle-owned: repeated Enable/Disable/Archive/Unarchive completes `NoChange` with no event; Delete against an already Deleted entity returns typed `AgentDeleted`/`SessionDeleted`, also with no write.

## Scanner and recovery precedence

Under the root lease, recovery proceeds in this precedence order:

1. classify directory sync with a root-local capability probe as `Supported` or known `Unsupported`; only the latter is tolerated. The probe creates **no namespace entry**: POSIX opens and syncs the root directory itself, while the Windows adapter reports known `Unsupported`. No probe artifact can violate bootstrap eligibility or root caps;
2. enumerate the markerless/present-marker root, **cap before allocation/sort**, and return `DurableStateTooLarge` at cap+1 before format classification; markerless scaffold eligibility applies the same cap-first rule to every entered fixed directory. Then perform fresh-store bootstrap only under the preceding eligibility rule, or validate the permanent lock and exact zero-byte `MINICORE_STORE_V1`;
3. sort and validate the exact root namespace; reject observable aliases, links, non-UTF-8 recognized names and unexpected entries. Full Windows/NTFS reparse, volume-identity, and native-process validation remains the pending M5.0 platform gate;
4. scan capped reservations (`agents` and `sessions`) and every capped Agent/Session direct entity plus nested generation/payload namespace before candidate multiplicity or semantic recovery; every published or unpublished entity ID must have its matching permanent reservation. Within this entered entity physical-scan phase, recovery attempts every reachable entity scan and gives any observed `DurableStateTooLarge` precedence over the first other physical corruption/unavailable result; this narrow precedence does not change the root/reservation collection cap rules;
5. classify an entity by `PUBLISHED`: published entities scan capped generation names in numerical order, require a contiguous committed chain, validate every retained referenced definition and every adjacent semantic transition; exact unpublished entities use the closed G1 shapes in [Unpublished new-entity recovery and cleanup](#unpublished-new-entity-recovery-and-cleanup). Never fall back from corruption;
6. after the complete published catalog, semantic validation, and published Fork graph succeed and all scanner handles close, remove the one allowed exact markerless operation candidate using its matching permanent reservation and non-recursive revalidation/deletion order. Missing reservation, malformed staging, multiple candidates, or cleanup failure blocks open; then install the immutable catalog.

Every enumerated directory is capped before allocation/sort: root max **5** entries; reservations root max **2**; Agent entity max **2**; Session entity max **3**; generation payload directory max **3**; each reservation/entity/generation collection max **1,000,000**. Unknown, alias, non-UTF-8, and observed link entries count first and then fail. The current implementation enforces exact-case plus its platform-observable no-follow link/type/mode checks; complete Windows/NTFS reparse-point and identity recognition remains covered by the preceding pending M5.0 platform gate. The scanner stops at cap+1 before unbounded allocation. All count/size/structural bound failures map to `DurableStateTooLarge`; during the entered Agent/Session entity physical-scan phase, any such observation overrides the first non-size physical failure after reachable scans complete. Malformed identity/layout/payload failures otherwise map to `DurableStateCorrupt`; a marker absent outside eligible bootstrap maps to `UnsupportedStoreFormat`. V1 has no automatic migration.

The permanent-reservation phase starts by the single actor exact-counting and retaining the canonical ID inventory of the direct relevant collection, `reservations/agents` for an Agent and `reservations/sessions` for a Session/Fork. Count alone is not enough to classify one candidate. The collections and caps are independent. Every entry counts, including a valid permanent reservation with no entity (an orphan left by an ordinary burned or later staging failure, cleanup, or crash). This inventory/count is established before lifecycle candidate generation or opening its CSPRNG source, owner-job preregistration, `EntityReservationBarrier`, and every reservation/entity filesystem write. Count `999,999` permits one reservation to reach exactly `1,000,000`; count `1,000,000` rejects with `DurableStateTooLarge` without a candidate, barrier crossing, write, or burn. `StorageGeneration` exhaustion at `1,000,000` likewise performs no writes and maps internally to too-large/user-action-required.

## Directory sync ordering and certainty

The root-local capability probe creates no artifact: POSIX opens/syncs the root directory, and the Windows adapter returns known `Unsupported`. It must not create a file or directory that could violate fresh-store bootstrap eligibility or any root cap. `Unsupported` skips only directory sync; file `sync_all` and exact readback remain mandatory.

For a **new Agent, Session, or Fork child**, the permanent-reservation phase is chronologically before every markerless entity step. After candidate generation, DurableState captures whether the candidate is present in the exact actor inventory, then runs candidate-dependent semantic preflight before owner-job preregistration, `EntityReservationBarrier`, or filesystem work. A Fork `candidate == sourceSessionId` is a terminal ordinary unburned rejection: no collision count, preregistration, barrier, filesystem work, or replacement candidate. Other candidate-dependent invariants use the same pre-filesystem position; no additional invariant is defined here. The exact chronological order is:

1. complete the permanent reservation for the attempt. Its exact physical sequence is: `create_new` the private zero-byte `reservations/agents/<AgentId>` or `reservations/sessions/<SessionId>` file without following the final path; validate the returned opened handle; `sync_all` that file; when directory sync is `Supported`, sync exactly that reservation collection as its direct parent; then no-follow exact-readback the canonical path and require it to resolve to the same physical identity as the opened handle, be a regular non-link zero-byte file, and have the required mode (`0600` on Unix). `Unsupported` skips **only** that direct-parent directory sync; it never skips file sync, handle validation, or identity/type/zero/mode readback. Every create, validation, file-sync, directory-sync, and readback error is reconciled by DurableState's reservation certainty rules before entity work may begin;
2. create the markerless entity directory, then sync its `agents/` or `sessions/` collection parent when supported;
3. create `generations/`, then sync the entity directory;
4. create generation 1, then sync `generations/`;
5. create/write/sync `head.json` and `definition.json`; for Session/Fork also create/write/sync `conversation.jsonl`; sync their direct directories and exact-readback the candidate generation payload. Conversation Storage performs a bounded-memory streaming reread/semantic validation and returns an opaque `PreparedConversationProof` bound to child SessionId, same file identity/length, Header and selected path;
6. perform final cross-entity checks while the required private gate remains held;
7. cross `DurableCommitBarrier` immediately before `create_new(COMMITTED)`;
8. `create_new`/sync `COMMITTED`, sync generation 1, then exact-readback `COMMITTED`, `head.json` and `definition.json`;
9. `create_new`/sync `PUBLISHED`, sync the entity directory and entity-collection parent, then exact-readback the complete entity. For Session/Fork this binds the same conversation identity/length/Header to `PreparedConversationProof`; it does not allocate or reread the complete 1 GiB a second time;
10. install the immutable catalog head and fulfill completion.

DurableState's mutually exclusive certainty table is also bound to the candidate's exact pre-attempt inventory membership. `Reserved` requires inventory absence plus successful create, returned-handle validation, file sync, direct-parent sync when `Supported`, and final same-identity regular/non-link/zero/mode proof. Ordinary unburned is inventory absence plus a noncollision create error before a successful create/opened handle and exact final absence. A newly ordinary-burned result is inventory absence plus either (a) that noncollision create error with an exact canonical valid marker now present—the fail-after-side-effect case under the root lease and no-hostile-mutation support contract—or (b) a successful create followed by file-sync or—when `Supported`—direct-parent-sync error, with a final same-identity regular/non-link/zero/mode marker. Both newly burned forms insert the ID, increment the collection count, and never retry with another ID. An inventory-present candidate plus a noncollision create error and the same valid marker is an ordinary storage failure with no new burn or count change. Neither ordinary error itself poisons or triggers Closing; absent an independent shutdown, Runtime remains running. Success for an inventory-present candidate, `AlreadyExists` for an inventory-absent candidate, or another available membership/path/identity contradiction is corruption poison; unavailable or ambiguous required membership/presence/absence/identity/type/zero/mode proof is indeterminate poison. Only an inventory-present candidate with `AlreadyExists` and the same valid marker is a definite collision: this attempt creates no marker and does not increment the collection count, but increments the request collision counter; its 32nd collision is terminal, with no candidate 33.

For an **existing entity update**, the exact order is:

1. create markerless generation G+1, then sync `generations/`;
2. create/write/sync the generation payload, sync its directory, and exact-readback the complete canonical candidate (`head.json` plus optional `definition.json`);
3. cross `DurableCommitBarrier`;
4. `create_new`/sync `COMMITTED`, sync generation G+1, then exact-readback `COMMITTED` and **all** generation payload bytes;
5. install the new immutable current head and fulfill completion.

A different/missing post-`COMMITTED` payload is `CommittedCorruptPoisoned`, never ordinary failure or fallback. An unexpected directory-sync error before `COMMITTED` is `NotPublished`. For new entities, a generation-1 COMMITTED marker/sync error with exact valid generation readback records close-required but does not complete before PUBLISHED: owner settlement continues to the PUBLISHED attempt. PUBLISHED proven present then means Completed first and Closing; PUBLISHED proven absent means ordinary failure after exact cleanup and then Closing; ambiguity/corruption poisons. For updates, COMMITTED is the applicable head marker, so exact valid readback means Completed first and Closing. The V1 crash guarantee still excludes power loss and related storage-stack failures.

## Conformance fixtures

[Durable Store V1 Fixtures](../fixtures/durable-store-v1/README.md) provide byte-exact golden documents and a crash table. `python3 docs/fixtures/durable-store-v1/verify.py` verifies their closed structural contract.