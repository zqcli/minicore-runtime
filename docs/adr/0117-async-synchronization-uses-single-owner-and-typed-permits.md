# ADR 0117：异步同步使用单 Owner、短临界区与 Typed Permit

状态：Accepted
日期：2026-07-27

## 背景

第一版评审O5认为Agent lifecycle、Session lifecycle/residency、`TurnControlGate`、当时设计的Workspace commit permit和SessionWriter可能缺少覆盖全系统的锁获取总序，并把Turn start与Agent disable、controlled append与Workspace revoke描述为潜在AB/BA死锁。ADR 0121随后删除了Workspace commit permit/revoke模型。

复核当前设计后，没有发现可构造的现行循环等待：每个Session只有一个`SessionExecutor` mutable owner；`TurnControlGate` reservation是非阻塞操作；Cancel/SecurityRevoked直接发布不等待普通lane的sticky signal；跨Agent/Session durable operation已有`Agent lifecycle → Session lifecycle`固定顺序；跨SessionTool resource lock已由ADR 0116删除。

同类Agent harness通常也不维护覆盖全系统的lock-rank表。pi和Gemini CLI依赖单线程event loop、run/scheduler gate与queue；Codex使用channel/single owner、短锁作用域，并通过`await_holding_lock`检查避免普通Mutex guard跨异步调用；有意的one-at-a-time异步串行使用单permit semaphore表达。MiniCore采用相同的结构化规避方式。

## 决策

1. **不建立Runtime-global lock hierarchy、lock-rank manager或运行时死锁检测器**。O5不再作为缺少全局锁序的P1架构缺陷。
2. **Session mutable state保持单Owner**。只有`SessionExecutor`修改Session执行状态、writer和projections；异步operation、lane和外部service不能直接取得Session mutable guard。
3. **普通Mutex/RwLock guard不得跨任意`.await`、跨owner同步调用、event publication、fan-out通知或host callback**。共享内存锁只保护可在同步短作用域内完成的读取、clone、CAS或状态替换。
4. **有意跨`.await`的一次一个操作使用语义明确的typed permit/semaphore**。允许跨越的范围必须在类型和文档中固定，例如Agent final-status check到initiating append outcome的start-commit permit、Model concurrency permit和file mutation permit。不得用通用`Mutex<()>`隐藏长临界区。
5. **跨Agent/Session durable operation保留局部固定顺序`Agent lifecycle → Session lifecycle`**。实现不得在持有Agent lifecycle guard/permit时等待SessionExecutor响应、Unload completion或其他host-visible completion；durable mutation完成后先释放gate，再向loaded Session fan-out readiness invalidation。
6. **controlled append保留局部固定顺序**：非阻塞`TurnControlGate` reservation → 一次bounded `SessionWriter.append/apply` sequence → release。所有通知和后续operation在reservation释放后执行，不叠加第二个Workspace permit。
7. **SecurityRevoked不反向等待Session控制面**。authority/host先发布current security fact，再设置Turn-targeted sticky signal并唤醒Executor；不等待TurnControl、普通lane或terminal cleanup确认。
8. **长等待发生时零持有短状态guard**。Model permit、Session-local file mutation ticket、Tool execution、approval、UserQuestion、Sandbox、provider I/O和外部通知等待期间不得持有Agent/Session lifecycle guard或TurnControl reservation。已经取得file mutation permit后可以请求短`ToolExecutionStarted` controlled append；任何路径都不得持有TurnControl reservation反向等待mutation permit。
9. **实现使用私有组合操作收口顺序**。至少提供等价于`commit_turn_start(...)`和`append_controlled(...)`的crate-private helper，调用方不能自由拼装同步原语。
10. **以lint和竞态测试防回归**。Rust实现启用Clippy `await_holding_lock`，并将Tokio Mutex/RwLock guard加入`await_holding_invalid_type`；确需跨await的typed permit显式allow并附作用域理由。

## 理由

- 死锁需要hold-and-wait与循环等待。当前single-owner、非阻塞reservation和release-before-fan-out已经切断关键环节。
- 全局rank系统会把本来局部、语义不同的signal、CAS、permit、queue和mutex强行建模成同一种锁，增加实现与测试复杂度。
- 局部组合helper比文档中的长锁序表更可执行；调用者无法绕过固定顺序，改动集中在深模块内部。
- typed permit明确表达“有意跨await串行”，使代码审查和lint可以区分安全边界与普通持锁错误。

## 后果

- 同一Agent下多个Session的Turn start可能在短Agent start-commit permit上串行，这是可接受的短暂吞吐取舍，不属于死锁。
- 不承诺任意未来同步原语自动安全；新增跨owner等待前必须证明不持有普通guard、不形成反向等待，并优先使用message passing或single-owner状态机。
- O5关闭；相关规则作为实现不变量和P2防回归检查维护，不建设独立领域对象或公开接口。

## 测试要求

- Turn start与Agent Disable/Delete竞态产生唯一first-wins结果，Disable不在持gate时等待SessionExecutor；
- Agent/Session upgrade与Archive/Delete遵守`Agent → Session`局部顺序；
- controlled append与Cancel/SecurityRevoked竞态产生append或signal唯一first-wins结果，双方不循环等待；
- `ToolExecutionStarted`与Cancel/SecurityRevoked竞态保持已有truthful outcome规则；
- 两个Session同时从同一Agent启动Turn只发生有限短等待；
- lint拒绝普通Mutex/RwLock guard跨`.await`；
- event publication、fan-out和host callback发生在相关状态guard/permit释放后。

## 修订关系

本ADR关闭`docs/review/v2-design-review.md`的O5，并补充[ADR 0105](0105-session-executor-owns-loaded-session.md)的single-owner规则、[ADR 0111](0111-session-ingress-separates-control-and-work-lanes.md)的non-blocking signal/reservation规则和`session-execution.md`的controlled append顺序。Agent disable、Cancel和append线性化语义不变；[ADR 0121](0121-workspace-updates-require-idle.md)删除了Workspace lease/commit permit分支。