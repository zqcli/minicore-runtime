# ADR 0136：DurableState使用operation-owned immutable generations、permanent reservations与root lease

状态：Accepted
日期：2026-08-03

## 背景

Agent/Session lifecycle已有immutable domain definitions、metadata CAS与logical lifecycle，Conversation Storage已有JSONL/replay语义；但M5此前没有冻结将这些事实安全落到一个local store的owner、跨文件可见性、crash cleanup或跨进程协调。caller staging、per-session lock和按`CommandId`查durable outcome会把物理细节泄漏给多个owner，且无法说明Create/Fork response loss。

## 决策

采用一个private deep module `DurableState`。它拥有Store V1 layout、permanent `create_new` ID reservation、root-wide OS advisory lease、single process-local actor、immutable generation/CAS recheck、COMMITTED/PUBLISHED marker publication、exact readback、recovery/cleanup、poison/closing和同步filesystem port。它不是adapter hierarchy、generic transaction plan、database、remote backend或WAL。

- root使用永久`.minicore.lock`的独占 advisory lease；stale lock file复用、不删除/改名，单非clone handle保持到所有jobs关闭；lease denial是Runtime `StoreInUse`，不是Session Degraded；
- reservation先于entity staging，永久保留且ID绝不复用；`CommandId`不进入DurableState，也不持久化；
- generation先在final path建立可取消的markerless payload并sync/exact-readback；`DurableCommitBarrier`紧邻create-new/sync `COMMITTED`，之后必须settle；new entity再最后创建`PUBLISHED`作为唯一catalog visibility root；没有temp namespace、rename或CURRENT correctness；
- marker/payload certainty has four internal states: `NotPublished`, `Published`, `CommittedCorruptPoisoned` (marker definitely exists but required candidate payload missing/invalid/different), and `IndeterminatePoisoned`; either poison settles joined post-admission dispatch waiters only as existing `RuntimeDispatchError::InternalDispatchUnavailable`, closes Runtime, and cannot fabricate rejection or retry with a new ID; Published plus valid readback completes waiters before a later sync failure begins Closing;
- V1 production claim仅适用于host提供并通过native suite的user-private coherent local APFS/NTFS/ext4/XFS root；MiniCore验证可观察的mode、same-volume/device、links/reparse、case/namespace alias，不声称完美识别mount technology；
- 只清理exact markerless/uncommitted staging；不做production purge/GC，保留reservation、PUBLISHED entity、COMMITTED generation、definition、logical Deleted head、conversation和fork provenance；
- lifecycle仍拥有semantic validation/no-op/revision/timestamp/metadata/lifecycle candidates；Conversation Storage仍拥有Header/JSONL/replay/fork semantic seed。caller绝不收到staging/path/generation/marker handle。

exact contract见[DurableState](../modules/durable-state.md)和[Durable Store V1](../formats/durable-store-v1.md)。

## 后果

Create/Fork在response loss或process crash后不提供restart exactly-once：已经PUBLISHED的随机ID可能不可关联，host必须重新page/query catalog且blind retry可能创建duplicate。future durable idempotency key需要独立决策。

Every accepted command normally completes while Runtime remains operational. If physical certainty becomes committed-corrupt or indeterminate, integrity poison settles joined in-process dispatch waiters as the existing fatal outer error, then closes transport/Runtime without semantic completion; host queries/reopens and does not blind-retry Create/Fork. Published plus valid readback followed by a sync error completes all waiters before Closing. This refines ADR 0133 without a new public wire outcome.

实现成本包括严格scanner/cleanup、native process lock tests、fault adapter和per-operation exact readback；收益是operation boundary集中且catalog永不暴露caller staging或corrupt placeholder。

## 修订关系

This ADR refines ADR 0133's accepted-completion limitation and ADR 0126's historic per-session writable-lease wording; ADR 0137 owns the Tokio barriers/job/settlement/shutdown foundation, while this ADR owns DurableState's operation/persistence semantics. It is read with ADR 0137 and is the M5.0 foundation implementation prerequisite.