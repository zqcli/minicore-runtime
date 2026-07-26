# ADR 0108: Runtime 公开协议

状态：Accepted
日期：2026-07-24

## 背景

MiniCore 已确立单一权威 SessionExecutor、by-entry SessionStorage durable truth、单一 PromptSet assembly seam 和单一 provider-neutral ModelGateway operation。外部 CLI、TUI、Tauri host 或其他 adapter 需要一个稳定的公开接触面，回答：如何调用 Runtime、Command/Query/Snapshot/Event 各自负责什么、Turn 何时返回、异步业务完成如何通知、message tree 如何读取与分支、streaming progress 与可靠状态如何分离、多 loaded Session 如何订阅与恢复，以及哪些内部对象永远不能进入协议。

同类runtime取舍不同：Codex App Server、pi和Claude Code更接近当前状态/transcript恢复加实时通知，不公开observer cursor replay；LangGraph为分布式Run、checkpoint和replay支付更高复杂度。MiniCore首版应优先领域层级identity、短线性化点、snapshot-first实时流与in-process embedding，而不是先做process-global workspace、long-request Turn、durable event replay或wire-first callback contract。

模块设计见 [`../modules/runtime-interface.md`](../modules/runtime-interface.md)。

## 决策

1. `MiniCoreRuntime` 是外部宿主接触 MiniCore 的唯一顶层 facade，公开能力固定为 `dispatch`、`query`、`snapshot`、`subscribe` 四类：Command 修改事实或启动工作，Query 只读，Snapshot 恢复当前读模型，Event 通知状态变化与进度。四者共享 identity/error/view/revision/redaction 类型但职责不重叠；协议不提供 `WaitForIdle` 之类等待原语。
2. 公开领域identity为`AgentId → SessionId → TurnId → ItemId → RequestId`。不定义公开`RunId`、`WorkspaceId`或`SubmissionId`；`CommandId`负责协议命令correlation与in-flight去重，Submit的CommandId同时作为Turn创建前的process-local admission/cancel target。Turn创建后长期execution identity切换为TurnId。两者都不是额外Run entity；`execution_version`、provider attempt id、Tool executor route、Workspace lease id等内部坐标不公开。
3. Command在每个命令语义明确的线性化点返回typed `CommandOutcome`（如`TurnStarted`、`SteerQueued`、`FollowUpQueued`、`SessionForked`、`InteractionResolved`），不使用只有`accepted: bool`的通用acknowledgement。`Submit`在initiating UserMessage append/apply后返回`TurnStarted`；Steer/FollowUp在各自SessionIngress FIFO admission后返回Queued，后续append/start由Event表达。CommandId必须随机且不得复用；当前Runtime内相同CommandId和exact typed command的in-flight重试加入同一completion；同一in-flight CommandId携带不同command时返回`CommandConflict`。CommandId不持久化，不承诺restart后的命令重放；乐观并发用expected revision/status/TurnId表达。
4. CommandSurface 是 MiniCoreRuntime 内部的无状态命令解释模块，`CommandManager` 每次基于显式 optional SessionId 构造 `CommandContext`，不持有 SessionExecutor handle 或 current Session。slash text 与 catalog selection 走同一 materialize/parse/resolve/handler-binding 路径；catalog 是 UI-safe read model 而非执行授权，执行时必须重新 resolve。Handler 只能产出 `Dispatch/Read/Present/Prompt`，不能直连 SessionExecutor、SessionStorage、ModelGateway 或 Tool executor。
5. Runtime scope与每个Session scope使用独立owner、Snapshot与event stream：`RuntimeSnapshot`只覆盖Runtime/Agent/Session summary与loaded membership，`SessionSnapshot`覆盖单个loaded Session的current Turn、active Items、pending Interaction与queues。不建立runtime-global event sequence，也不构造all-loaded stop-the-world barrier；SessionSnapshot从该Session immutable published view读取，不经过mutation/control queue，RuntimeSnapshot不等待所有SessionExecutor。
6. subscribe使用snapshot-first实时流：owner原子注册subscriber并捕获第一帧Snapshot，随后按发送顺序交付实时StateEvent；ProgressEvent可合并或丢弃。subscriber背压、disconnect或restart时stream结束，调用方重新subscribe并从新Snapshot恢复。首版不公开cursor、Gap、event replay或跨restart续订。
7. SessionStorage 拥有 durable message/entry tree。Runtime 通过 `SessionQuery::GetHistoryTree`/`ListTurns`/`ListItems` 暴露分页 read model，通过 `SessionCommand::Fork`（`ForkAnchor` 使用 Genesis/UserMessage/FinalAgentMessage 语义，不接受裸 EntryId）创建新 Session branch。同一 Session 不提供原地 checkout/navigation mutation。
8. 所有改变 MiniCore 事实的 UI 操作（Agent/Session 管理、Submit/Steer/FollowUp/Cancel、Interaction 回答、model/reasoning/workspace/prompts 修改、slash/catalog command）都经过 Runtime facade；UI selection、editor draft、scroll、layout、折叠状态以及UserQuestion的呈现状态等纯 UI 状态留在Presentation Adapter，host 本地 command overlay 不得 shadow 同名 Runtime command 或绕过 mutation。MiniCore仍拥有Interaction request/resolution protocol和durable pending state。
9. 首版不公开 standalone/manual `CompactSession`：CommandSurface 不注册 `/compact`，automatic compaction 由 SessionExecutor 在 `NeedModel` 安全点内部触发。
10. 公开面是in-process Rust interface加transport-neutral serde types；wire adapter只做serialization、correlation、连接生命周期、initialize/version协商与EventStream映射，不承担Session state、Snapshot publication、authorization、retry或storage truth。SessionExecutor、SessionExecutionHandle、SessionWriter、ModelGateway、PromptSet、Tool/Skill executor、credential、Workspace lease 等内部对象永远留在 crate 内部；外部宿主不能取得内部 handle，只依赖 facade。

## 后果

- 外部宿主有单一稳定 facade，无需了解 Service/provider/storage/execution 内部结构即可驱动全部领域能力。
- 短线性化点加事件流天然支持后台多 Session、Steer、Interaction 与断线恢复；transport request lifetime 不与领域 Turn lifetime 耦合，但调用方需要订阅 Event 才能得到 Turn 完成结果。
- typed CommandOutcome让调用方立即拿到revision/TurnId/Queued/resolution，无需靠额外event猜测命令结果；代价是每个命令都要定义明确的线性化点与outcome类型。
- snapshot-first stream避免公开cursor、epoch、Gap和replay-window复杂度；代价是disconnect或背压后必须重新取得Snapshot，不能增量补发缺失StateEvent。
- StateEvent/ProgressEvent分离保证durable truth不被streaming、retry与UI状态污染，final view和重新订阅后的Snapshot可校正丢失的progress；代价是发布端要维护两类observer通道。
- SessionSnapshot不会被普通work lane背压阻塞；它只表示读取时的完整view，不承诺与不同ingress lane形成全局FIFO。
- message tree 只经 Fork 分支、只读分页暴露，UI 无法直接改写 JSONL 或 leaf pointer；同一 Session 原地 navigation/checkout 留待真实产品需求出现后再设计。
- 未公开 manual compaction、in-process-only transport 与最小 capability 集降低首版面积；JSON-RPC/WebSocket adapter、privileged debug interface、history checkout 等扩展必须由真实领域能力驱动并经 capability 协商加入。

## 历史

本ADR属V2决策集，取代并整合V1的多条Runtime边界决策：ADR 0028（旧scoped cursor方案）、ADR 0018（Command/Query/Event/Snapshot分离）、ADR 0006与0007（CommandSurface）、ADR 0012（无状态CommandManager）、ADR 0020（无current session）、ADR 0001（Rig置于UI无关运行时之后），以及hook边界ADR 0008与0015。保留的核心原则是：Runtime是唯一facade，mutation与只读/Snapshot/Event分离，CommandSurface由Runtime拥有且无状态，内部执行对象绝不进入公开协议。原文见`docs/archive/v1/adr/`。

2026-07-25：[ADR 0111](0111-session-ingress-separates-control-and-work-lanes.md)修订SessionSnapshot和Session ingress的具体一致性机制。
2026-07-25：[ADR 0113](0113-user-question-uses-runtime-protocol-and-ui-presentation.md)补充UserQuestion的Runtime protocol与UI presentation分工。
2026-07-26：[ADR 0114](0114-runtime-observation-uses-snapshot-first-streams.md)删除公开cursor/replay，改为snapshot-first实时流。
2026-07-26：删除独立`SubmissionId`；Submit的`CommandId`统一承担Turn创建前的admission correlation与精确取消。
