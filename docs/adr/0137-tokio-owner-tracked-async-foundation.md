# ADR 0137：Tokio owner-tracked async foundation与deterministic persistent seams

状态：Accepted
日期：2026-08-03

## 背景

M5需要actor、blocking local filesystem、Recorder append、Fork streaming、fault injection和deterministic cancellation test，但不能让DurableState或SessionExecutor创建自己的Runtime、detach publication/append jobs，或把time advance误当成operation settlement。

## 决策

MiniCore选择**显式注入**，不“接收/捕获”ambient runtime：host构造MiniCore时显式传入一个从其runtime克隆的唯一`tokio::runtime::Handle`，MiniCore只保留该注入clone。禁止`tokio::runtime::Handle::current()`，也不创建`Runtime`/`Builder`、nested runtime或`block_on`。初始化立即以该Handle启动、owner-track并join一个timer probe；缺少runtime/time driver导致的probe panic或join failure必须返回typed `RuntimeInitializationError::RuntimeDependencyUnavailable`，不得让panic逃逸。初始化成功后把已验证Handle封入crate-private `RuntimeTaskContext`；DurableState、Conversation Storage、Recorder和其job只接收该context，绝不读取ambient Tokio context。它支持current-thread和multi-thread host，并要求host在Tokio teardown前await MiniCore shutdown。

implementation manifest将使用：

```toml
tokio = { version = "1.53.1", default-features = false, features = ["macros", "rt", "sync", "time"] }
tokio-util = { version = "0.7.19", default-features = false, features = ["rt"] }
fs4 = { version = "0.13.1", default-features = false, features = ["sync"] }
```

这些是compatible caret requirements，不是`=`；implementation commit的Cargo.lock固定Tokio 1.53.1和tokio-util 0.7.19。Tokio与tokio-util均声明`rust-version = 1.71`，MiniCore MSRV 1.85仍兼容。dev-only增加`rt-multi-thread,test-util`；没有真实consumer前不启用Tokio `fs`或`io-util`。

所有DurableState、Conversation Storage和Recorder filesystem work经private synchronous filesystem port，在`RuntimeTaskContext`中已验证、owner-retained Handle 的`spawn_blocking`上运行。crate-private `RuntimeTaskContext::spawn_blocking_tracked(...) -> TrackedBlockingJob<T>`在spawn前原子预留owner-registry slot；returned opaque value只携带registry key/shared settlement，registry始终保留实际`JoinHandle`，因此drop该value也不能detach raw task。spawn failure/panic、worker panic、join cancel/failure和owner close都exactly-once settle并映射到ordinary degradation或integrity poison；caller永远不能取得或drop raw `JoinHandle`来detach。Replay/open、recorded lease read和至多1 GiB的Fork streaming/readback也必须使用该owner-tracked path，不能阻塞current-thread runtime或在shutdown owner之后继续。`CancellationToken`只唤醒tasks—owner state和epoch才是first-wins truth。

The exact named owner barriers are: `EntityReservationBarrier` immediately before reservation (shutdown may reject with no burned ID); after reservation, final-path **markerless** directories/payloads may be created/written/synced/readback and cancellation may stop/clean them while leaving the reservation burned; `DurableCommitBarrier` is immediately before `create_new(COMMITTED)` (after it, drop/shutdown settles through marker/readback/completion/fatal outer error); Fork semantic copy/readback before that barrier is cancellable, cleaned, and releases its source lease; and `RecorderWriteBarrier` after encode/size validation immediately before the first physical write (after it, append settles and installs its health transition). `RuntimeClosing` rejection applies only while the corresponding domain mutation has not already become live: once Submit Input is live-applied, a pre-write shutdown produces `NotRecorded`/Degraded and the truthful `TurnStarted`→interruption sequence, never a retroactive `Rejected(RuntimeClosing)`.

Composite post-admission publication also uses owner settlement: a loaded Workspace update is held by SessionExecutor's registered `SessionDefinitionPublicationTask`, not by the dispatch waiter, from its typed permit through durable commit and required Snapshot installation. Recorder owns exactly one shared in-flight settlement while `RuntimeTaskContext` owns the corresponding tracked job/JoinHandle. Job registration happens before spawn; a short synchronous lock moves the file handle and installs/takes `Ready | InFlight | Degraded`, then releases before await. Spawn failure restores or degrades the handle deterministically and resolves the settlement; it cannot leave `InFlight` forever. `record().await` and SessionExecutor's panic/unload finalizer await that same settlement outside raw guards. Cancel/Unload cannot discard a started `write_all`, and terminal/unload waits settlement. No persistent worker or background queue exists. Root lease/lock-file identity loss is global poison/close; only ordinary conversation file/capability/open/write failure with root lease intact is Session Degraded.

Clocks are injectable synchronous `SystemClock`/`TestClock`; emitted `Timestamp` truncates, never rounds, to milliseconds and is not an ordering proof. Tokio paused time plus `advance` drives monotonic delay, but named typed barriers/fault coordinates and joins prove settlement. The only filesystem adapters are `LocalFilesystem` and `DeterministicPersistentFaultFilesystem`; the latter operates on a real temp root and preserves bytes across abrupt simulated drop/reopen.

Clippy enables `await_holding_lock` and freezes this exact `await-holding-invalid-types` set for the Tokio 1.53.1/MSRV gate (no `parking_lot` path is listed unless that dependency is later added):

```text
std::sync::MutexGuard
std::sync::RwLockReadGuard
std::sync::RwLockWriteGuard
tokio::sync::MutexGuard
tokio::sync::OwnedMutexGuard
tokio::sync::MappedMutexGuard
tokio::sync::OwnedMappedMutexGuard
tokio::sync::RwLockReadGuard
tokio::sync::OwnedRwLockReadGuard
tokio::sync::RwLockWriteGuard
tokio::sync::OwnedRwLockWriteGuard
tokio::sync::RwLockMappedWriteGuard
tokio::sync::OwnedRwLockMappedWriteGuard
```

A typed owner permit may cross await only with a documented narrow scope. The implementation must verify these exact paths in a Rust 1.85/current lint smoke fixture.

## 后果

The design works with both Tokio host schedulers without nested-runtime failure and makes filesystem faults/process-like aborts reproducible. It requires explicit lifecycle/shutdown joins and makes fire-and-forget convenience unavailable. `MiniCoreRuntime::shutdown().await` is host-only/non-wire and idempotently performs Closing/rejection, accepted-work settlement by the named barriers, SessionExecutor stop/unload plus Recorder joins, DurableState job/actor joins, conversation handle close, root-lease release, and Closed. Facade `Drop` only sends a best-effort Closing signal and never blocks; owner-registry self-Arcs retain every task handle so a raw `JoinHandle` is not detached. A host that omits awaited shutdown does not observe graceful completion or root-lease release before process/runtime teardown. This does not create a generic executor abstraction.

## 修订关系

本ADR细化ADR 0117的single-owner/typed-permit/lint rules，并为ADR 0136的actor-owned durable operations提供async/test foundation。