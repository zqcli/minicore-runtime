# ADR 0108: Runtime 公开协议

状态：Accepted
日期：2026-07-24

## 背景

MiniCore 已确立单一权威 SessionExecutor、by-entry SessionStorage durable truth、单一 PromptSet assembly seam 和单一 provider-neutral ModelGateway operation。外部 CLI、TUI、Tauri host 或其他 adapter 需要一个稳定的公开接触面，回答：如何调用 Runtime、Command/Query/Snapshot/Event 各自负责什么、Turn 何时返回、异步业务完成如何通知、message tree 如何读取与分支、streaming progress 与可靠状态如何分离、多 loaded Session 如何订阅与恢复，以及哪些内部对象永远不能进入协议。

同类 runtime 取舍不同：Codex App Server 使用 process-global workspace 与大量 transport method；Agent Client Protocol 用一个长请求持有整个 Turn；Claude Agent SDK 把 callback 与 SDK 对象句柄固化为 contract；LangGraph 支付分布式 Run/队列/replay 成本。首版应优先领域层级 identity、短线性化点加事件流、scoped cursor 与 in-process embedding，而不是先做 process-global workspace、long-request Turn、durable 分布式 replay 或 wire-first callback contract。

模块设计见 [`../modules/runtime-interface.md`](../modules/runtime-interface.md)。

## 决策

1. `MiniCoreRuntime` 是外部宿主接触 MiniCore 的唯一顶层 facade，公开能力固定为 `dispatch`、`query`、`snapshot`、`subscribe` 四类：Command 修改事实或启动工作，Query 只读，Snapshot 恢复当前读模型，Event 通知状态变化与进度。四者共享 identity/error/view/revision/redaction 类型但职责不重叠；协议不提供 `WaitForIdle` 之类等待原语。
2. 公开领域 identity 为 `AgentId → SessionId → TurnId → ItemId → RequestId`。不定义公开 `RunId` 或 `WorkspaceId`；`CommandId` 只做协议命令 correlation 与幂等，`SubmissionId` 只做 Turn 创建前的 process-local admission control，均不是领域 entity。`execution_version`、provider attempt id、Tool executor route、Workspace lease id 等内部坐标不公开。
3. Command 在每个命令语义明确的线性化点返回 typed `CommandOutcome`（如 `TurnStarted`、`SteerApplied`/`SteerQueued`、`SessionForked`、`InteractionResolved`），不使用只有 `accepted: bool` 的通用 acknowledgement。`Submit` 在 initiating UserMessage append/apply 后返回 `TurnStarted`，Turn 的长期完成（`Completed | Interrupted | Failed`）、Item 生命周期与 Interaction request/resolution 由 Event 发布。相同 CommandId+payload 幂等，携带不同 payload 返回 `CommandConflict`；乐观并发用 expected revision/status/TurnId 表达。
4. CommandSurface 是 MiniCoreRuntime 内部的无状态命令解释模块，`CommandManager` 每次基于显式 optional SessionId 构造 `CommandContext`，不持有 SessionExecutor handle 或 current Session。slash text 与 catalog selection 走同一 materialize/parse/resolve/handler-binding 路径；catalog 是 UI-safe read model 而非执行授权，执行时必须重新 resolve。Handler 只能产出 `Dispatch/Read/Present/Prompt`，不能直连 SessionExecutor、SessionStorage、ModelGateway 或 Tool executor。
5. Runtime scope 与每个 Session scope 使用独立 owner、cursor 与 snapshot：`RuntimeSnapshot` 只覆盖 Runtime/Agent/Session summary 与 loaded membership，`SessionSnapshot` 覆盖单个 loaded Session 的 current Turn、active Items、pending Interaction 与 queues。不建立 runtime-global event sequence，也不构造 all-loaded stop-the-world barrier；`SessionSnapshot` 经该 Session 的 request queue 线性化，`RuntimeSnapshot` 不等待所有 SessionExecutor。
6. 可靠 `StateEvent` 与可合并/丢弃的 `ProgressEvent` 分离。StateEvent 在 scope 内 cursor 严格单调、分配后不得静默丢弃、payload 携带完整 final view，durable fact 从 append/apply 后的 committed entry 派生；ProgressEvent 不占用 cursor，可按 Session/Turn/Item 合并或在背压时丢弃，缺失不触发 gap。recovery cursor 只覆盖 StateEvent；subscriber buffer 不足返回 `Gap`，调用方以 Snapshot 恢复。
7. SessionStorage 拥有 durable message/entry tree。Runtime 通过 `SessionQuery::GetHistoryTree`/`ListTurns`/`ListItems` 暴露分页 read model，通过 `SessionCommand::Fork`（`ForkAnchor` 使用 Genesis/UserMessage/FinalAgentMessage 语义，不接受裸 EntryId）创建新 Session branch。同一 Session 不提供原地 checkout/navigation mutation。
8. 所有改变 MiniCore 事实的 UI 操作（Agent/Session 管理、Submit/Steer/FollowUp/Cancel、Interaction 回答、model/reasoning/workspace/prompts 修改、slash/catalog command）都经过 Runtime facade；UI selection、editor draft、scroll、layout、折叠状态等纯 UI 状态留在 adapter，host 本地 command overlay 不得 shadow 同名 Runtime command 或绕过 mutation。
9. 首版不公开 standalone/manual `CompactSession`：CommandSurface 不注册 `/compact`，automatic compaction 由 SessionExecutor 在 `NeedModel` 安全点内部触发。
10. 公开面是 in-process Rust interface 加 transport-neutral serde types；wire adapter 只做 serialization、correlation、连接生命周期、initialize/version 协商与 EventStream 映射，不承担 Session state、cursor 生成、authorization、retry 或 storage truth。SessionExecutor、SessionExecutionHandle、SessionWriter、ModelGateway、PromptSet、Tool/Skill executor、credential、Workspace lease 等内部对象永远留在 crate 内部；外部宿主不能取得内部 handle，只依赖 facade。

## 后果

- 外部宿主有单一稳定 facade，无需了解 Service/provider/storage/execution 内部结构即可驱动全部领域能力。
- 短线性化点加事件流天然支持后台多 Session、Steer、Interaction 与断线恢复；transport request lifetime 不与领域 Turn lifetime 耦合，但调用方需要订阅 Event 才能得到 Turn 完成结果。
- typed CommandOutcome 让调用方立即拿到 revision/TurnId/Applied-Queued/resolution，无需靠额外 event 猜测命令结果；代价是每个命令都要定义明确的线性化点与 outcome 类型。
- scoped cursor/snapshot 避免慢 Session 拖住全 Runtime 与无关 gap，但放弃了跨 scope 可比较的全局 total order；reducer 必须按 scope 组织。
- StateEvent/ProgressEvent 分离保证 durable truth 不被 streaming、retry 与 UI 状态污染，final view 可校正丢失的 progress；代价是发布端要维护两条通道。
- message tree 只经 Fork 分支、只读分页暴露，UI 无法直接改写 JSONL 或 leaf pointer；同一 Session 原地 navigation/checkout 留待真实产品需求出现后再设计。
- 未公开 manual compaction、in-process-only transport 与最小 capability 集降低首版面积；JSON-RPC/WebSocket adapter、privileged debug interface、history checkout 等扩展必须由真实领域能力驱动并经 capability 协商加入。

## 历史

本 ADR 属 V2 决策集，取代并整合 V1 的多条 Runtime 边界决策：ADR 0028（scoped state cursors）、ADR 0018（Command/Query/Event/Snapshot 分离）、ADR 0006 与 0007（CommandSurface）、ADR 0012（无状态 CommandManager）、ADR 0020（无 current session）、ADR 0001（Rig 置于 UI 无关运行时之后），以及 hook 边界 ADR 0008 与 0015。保留的核心原则是：Runtime 是唯一 facade，mutation 与只读/快照/事件分离，CommandSurface 由 Runtime 拥有且无状态，Runtime 与每个 Session 使用独立 scoped cursor，内部执行对象绝不进入公开协议。原文见 `docs/archive/v1/adr/`。
