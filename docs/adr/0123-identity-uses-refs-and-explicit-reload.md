# ADR 0123：执行一致性使用Exact Ref、不可变快照与显式Reload

状态：Partially Superseded by ADRs 0124, 0126 and 0127
日期：2026-07-28

> 2026-07-31：exact refs、immutable capture和explicit reload继续有效；ADR 0127删除`StoredTurnStart`，Agent/Session/Workspace/Model execution metadata不再随Input记录。

> 2026-07-30：exact refs、immutable capture和explicit reload保留；`ConversationCheckpoint.entry_id`不再是live execution proof，改用process-local `ConversationRevision`。

> 2026-07-29修订：immutable Arc、private constructor、explicit reload、same request logical retry和删除fingerprint族保持有效。ADR 0124取代durable Workspace/Model execution refs与proof links、StoredTurnContext、ConversationBoundary、Fork identity remap、ToolExecutionStarted和StoredCompaction scope/provenance条款；AgentRevisionRef与SessionDefinitionRevision仍作为StoredTurnStart历史说明保留。

## 背景

当前V2设计为Workspace、Prompt、Skill、Tool、Model、TurnExecutionContext、模型输入、conversation、Compaction plan/budget/boundary等对象定义了约二十种`*Fingerprint`，并在部分durable schema中保存`ContentHash`。ADR 0122已把Workspace fingerprint收窄为Runtime-local opaque identity，说明其中许多值并非内容指纹，而是generation、binding token或diagnostic label。

pi主要依赖entry id/parent tree、canonical path和current registry；Codex只在需要增量model-visible fragment比较时计算WorldStateHash，其他陈旧检测使用history version；Gemini CLI只为render cache、stable synthetic id和文件缓存选择性计算Hash；OpenHands和Claude Agent SDK主要依赖event/session id与结构化持久状态。它们没有为每层不可变对象建立聚合fingerprint链。

MiniCore已经具备更直接的正确性机制：单SessionExecutor owner、private constructor、Turn-pinned immutable `Arc`对象、exact durable revision/ref、每次append推进的`ConversationCheckpoint.entry_id`、`current_operation + execution_version`、以及logical retry复用同一个`Arc<ModelCallRequest>`。继续维护fingerprint链会重复证明这些事实，并把cache、diagnostics、授权和operation validation耦合成一种机制。

## 决定

1. MVP删除所有命名`*Fingerprint`类型，不新增`WorkspaceResolutionId`、`ToolSetId`、view generation或其他替代identity。`Fingerprint`不再是V2架构术语。
2. durable definition继续使用existing exact revision/ref：`AgentRevisionRef`、`SessionDefinitionRevision`、`WorkspaceRevision`、`ModelDefinitionVersion`及对应immutable definition retention。领域/ledger correlation继续使用`SessionId`、`TurnId`、`ItemId`、`RequestId`、`EntryId`和`ToolCallId`。这些不是fingerprint替代品。
3. Runtime执行一致性由对象所有权保证：active Turn持有同一组immutable `Arc<WorkspaceSnapshot>`、`Arc<PromptResourceView>`、`Arc<SkillView>`、`Arc<ToolSet>`、`Arc<PromptSet>`和`Arc<TurnModelSnapshot>`；private constructors不允许调用方跨capture拼接任意view。
4. logical model retry移动并复用同一个`Arc<ModelCallRequest>`。retry前只验证current Turn仍Running、`execution_version`、exact `ConversationCheckpoint.entry_id`、`current_operation`仍为持有该request的对应retry slot且control basis未改变；不重新assemble，也不比较request/context fingerprint。
5. `ConversationCheckpoint`只保存latest committed `EntryId`。所有append，包括`AdvanceOnly`，都推进checkpoint；因此任何ledger或model-visible变化都会使旧operation source失效。Transcript projection的正确性由shared semantic reducer、append/live-apply/cold-replay等价性和actual typed messages验证，不使用Transcript hash。
6. Compaction operation持有同一个`Arc<CompactionPlan>`及其immutable settings、budget、source、summary prefix和single `first_kept_entry_id` marker，同时持有由该plan组装出的同一个`Arc<ModelCallRequest>`；exact rendered directive随request固定。operation result只能交回持有这两个Arc的current operation slot；append前验证exact source checkpoint、current Turn/version/control和actual typed entries。
7. 当前设计不定义`ConversationBoundary`。Fork复制selected path并保留历史`EntryId`、`TurnId`、`ItemId`、`RequestId`和`ToolCallId`，只分配new SessionId；future append生成fresh ID，并对target执行tolerant structural replay。
8. Tool model disclosure和executor route来自同一个immutable `ToolSet`。`ToolPromptView`只能由parent ToolSet私有投影并随PromptSet捕获，不能由caller伪造或替换；不使用ToolSet binding ID/hash。
9. MVP不保存跨调用Tool grant：approval decision只保留per-call `AllowOnce`或进一步收紧权限的`AllowWith`，删除`AllowForTurn`、`AllowForSession`、`ToolGrantStore`、grant key/scope/suggestion和PolicyRevision绑定。未来若出现真实跨调用授权需求，必须另建ADR定义完整结构匹配、失效和审计语义。
10. Tool side-effect start只存在于current-Runtime owner-local `ToolOperationSlot`，并关联assistant ToolCall/Item、resolved ToolName、exact frozen arguments、ToolRequirements和effective permissions；ledger不保存start event，也不保存invocation/requirements/authorization hash。
11. Prompt/Skill/Tool/Model资源只在Runtime初始化或显式`/reload`后替换current immutable object。watcher最多标记dirty，不自动publication。active Turn继续使用old captured objects；reload成功后admit的future Turn使用new objects；completed Turn不更新。
12. `/reload`对共享Prompt/Skill/Tool/Model资源执行two-phase流程：各deep module只build/validate candidate，不保存current pointer或提供publish方法；`MiniCoreRuntime`在短publication gate下整体替换private `SharedResourceRoots`中的四个Arc。任一required candidate失败时保留完整old roots。Turn admission在同一gate下整体clone该root bundle，后续Context构建不持有gate，因此不会捕获到一半old、一半new的共享资源组合。`SharedResourceRoots`没有ID/version/generation，也不拥有source、cache或module逻辑。
13. `/reload workspace`保持ADR 0121的Session Idle-only规则，非Idle返回`SessionBusy`。Session load、Idle Workspace definition update和`/reload workspace`都必须在publication前完成Workspace resolve以及Workspace-bound Prompt/Skill source capture。initial load没有old Snapshot，失败时进入Unavailable；Idle definition update失败时不commit new definition并保留old Snapshot；Ready状态的`/reload workspace`失败时保留old Snapshot；Unavailable状态的reload/retry失败时保持Unavailable。shared `/reload`不重新读取Workspace-bound source。
14. 为保证“只有reload后生效”，shared Prompt/Skill filesystem source在Runtime initialize或shared `/reload`时读取为immutable captured bytes/content；Workspace-bound Prompt/Skill source在Session load、Idle definition update或`/reload workspace`时捕获。Skill可以lazy parse，但只能解析captured bytes，不能在Turn内按path重新读取current file。reload失败继续使用old bytes。
15. correctness不能依赖cache。initialize/reload/re-resolve可以直接清空相关cache；实现允许内部使用未公开Hash优化cache，但Hash不进入module interface、durable schema、authorization、retry、recovery或架构不变量。
16. 当前设计不写独立`StoredTurnContext`；Input UserMessage内联`StoredTurnStart`，只保存AgentRevisionRef、SessionDefinitionRevision和safe model/generation历史说明。MVP不从该metadata重建旧execution environment。
17. `StoredCompaction`不保存hash、scope、boundaries、protected entries、previous checkpoint或coverage provenance；保留rolling summary、single `first_kept_entry_id` marker，以及automatic SummaryModel路径所需的safe model call provenance、usage、finish reason和logical retry count。
18. O13关闭：不抽取共享pinning/fingerprint value module；一致性由各deep module的private immutable interface保证。O14关闭：不新增`CompactionSummaryDirectiveFingerprint`，directive由Compaction唯一private constructor创建，模板/格式不兼容变化递增`CompactionSummaryFormatVersion`。O15关闭：Prompt正文由explicit reload发布的immutable content承载；不定义PromptFingerprint或跨reload正文identity。

## Reload语义

```text
filesystem/config change
→ optional dirty diagnostic only
→ current objects不变

/reload
→ build complete Prompt/Skill/Tool/Model candidates
→ validate all required candidates
→ atomic replacement of all four current Arcs
→ future Turn captures new objects

active/completed Turn
→ 不原地更新

/reload workspace
→ Session Idle: resolve Workspace and capture Workspace-bound Prompt/Skill sources
→ atomically replace WorkspaceSnapshot and captured source values
→ Starting/Running/Finishing: SessionBusy
```

## 理由

- 同一个immutable对象已经比其Hash更强：它不仅证明“相等”，还直接携带实际值。
- 单current operation和exact checkpoint足以拒绝迟到结果；plan/request fingerprint没有额外安全收益。
- private constructor和single-owner ownership能让非法组合不可构造，比在过宽interface后补Hash断言更深。
- explicit reload把变化时机收敛为用户可见线性化点，删除watcher publication、generation比较、source drift和跨Turncache一致性协议。
- JSONL是append-only durable truth；Fork和Compaction可以验证actual entries，不需要为未来性能提前建设内容寻址协议。

## 后果

- reload需要读取并保留Prompt/Skill source bytes，内存占用增加；Skill解析仍可lazy。
- Fork/Compaction/replay执行结构验证，不能用Hash快速短路；MVP优先正确和简单。
- historical Turn只保留exact durable refs和safe metadata，不证明当时完整Prompt/Tool/Skill执行bundle的字节级相等。
- provider cache、跨Session内容去重、durable grant、跨设备execution迁移或adversarial ledger tamper detection不进入MVP；真实需求出现时必须以独立深模块和新ADR设计，不能恢复全局fingerprint链。

## 修订关系

本ADR取代ADR 0122；ADR 0122改为Superseded。它修订ADR 0100/0101/0102/0103/0109/0110/0112/0119/0121及Workspace、Prompt、Skill、Tool、Turn execution、Session execution、ModelGateway、Compaction和ConversationStorage中的fingerprint条款。ADR 0124随后取代本ADR的durable Workspace/Model execution refs与proof links、StoredTurnContext、ConversationBoundary、Fork remap、ToolExecutionStarted和StoredCompaction scope/provenance条款；Agent/Session historical exact refs继续保留，immutable Arc、private constructor、explicit reload、same request retry和删除fingerprint族继续有效。
