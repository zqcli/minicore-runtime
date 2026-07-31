# ADR 0105: SessionExecutor 拥有 loaded Session

状态：Partially Superseded by ADRs 0126 and 0127
日期：2026-07-24

> 2026-07-31：control actor、single ActiveTurnTask和live terminal handoff保留；ADR 0127删除restart conservative terminalization，Load从recorded conversation建立Idle view。

> 2026-07-30：SessionExecutor control actor与每Session单active Turn约束保留；single mutable Executor owner、`SessionWriter`、`RunningOperation`和同步AgentLoop改为control actor + `ActiveTurnTask` + live state。

## 背景

一个 loaded Session 必须在 Context 构造、Model 调用或 Tool 执行等待外部 I/O 期间，持续处理 Submit、Steer、FollowUp、Interaction resolution、Cancel、snapshot 和 shutdown 请求。

- 若在 Session 状态 owner 内内联等待整个 Agent run，会阻塞请求处理，并在 approval 流程上产生死锁；
- 若允许 Model/Tool task 直接修改 Session 状态，SessionWriter、projection 与 terminal 顺序会散落到多个 owner；
- 一个 Runtime 需要同时运行多个 loaded Session，且它们共享 Model、Tool 等服务。

需要一个明确、单一的执行期 owner，同时保持异步操作与请求处理解耦。详见 [Session Execution 模块](../modules/session-execution.md)。

## 决策

- 每个 loaded Session 拥有恰好一个 `SessionExecutor`，它是该 Session 执行期 mutable state 的唯一 owner：SessionWriter、committed projections、`CurrentTurnExecution`、execution version 与 `SessionIngress`。
- 外部调用方只持有可克隆的`SessionExecutionHandle`，不能借用或加锁Executor状态。请求经每个Session独立的semantic ingress lane进入；Cancel/SecurityRevoked、lifecycle和Snapshot不与普通work共享单一bounded FIFO。lane划分与仲裁由[ADR 0111](0111-session-ingress-separates-control-and-work-lanes.md)定义。
- 一个 Runtime 允许多个 `SessionExecutor` 同时 `Running`；每个 Session 独立推进，最多一个 Starting/Running Turn。
- 执行期 state 只有 `Idle → Starting → Running → Finishing → Idle`；WaitingApproval/WaitingForUserInput/Sampling/Compacting/ExecutingTools 是 Running Turn 的阶段，不写 SessionStorage。
- Context构造、UserMessage composition、Model调用与Tool执行作为cancellable `RunningOperation`异步运行，但每个Session最多一个current operation。主循环同时poll该future与SessionIngress wakeup，LifecycleControl grace deadline通过wakeup推进；等待UserQuestion时只暂停当前Tool future，不阻塞Executor；旧operation terminal/remove或安全drop并关闭结果路径前，不启动logical retry或下一operation。
- Steer和FollowUp分别位于`SessionIngress`的bounded per-Turn `SteerQueue`与`FollowUpQueue`；各自只保留普通FIFO push/pop/remove语义，不拥有Session状态。Steer不取消Sampling；当前assistant/tool step完整committed后、下一次模型调用前pop一条。FollowUp在Turn terminal后最多pop一条并开启新Turn。
- private `AgentLoop` 只返回 `NeedModel | NeedTools | Finished`，不拥有 storage、Prompt assembly、Tool execution、approval 或 Turn terminal 决策。
- 所有durable动作遵循`SessionWriter.append → apply trusted projections → 依赖动作`；durable/model-visible事实和Interaction通知/恢复分别遵守[INV-001与INV-301](../architecture.md#跨模块不变量索引)。Tool side-effect start独立遵守owner-local[INV-401](../architecture.md#跨模块不变量索引)，不以durable append作为start marker。
- restart 不恢复旧的异步操作、queue 或 waiter；unfinished Turn 按 recovery 规则保守 terminalize（closure、preserve tool messages、ToolAbandoned、TurnInterrupted）。
- multi-session共享ModelGateway但不经过本地模型调用permit；多个Session可直接进入各自provider attempt，provider/backend负责外部容量反馈（ADR 0125）。每个SessionExecutor拥有独立file mutation queue，只协调该Session同批sibling ToolCall；跨Session共享Workspace的并发mutation由host/user隔离或协调（ADR 0116）。
- 上下文压缩由 `SessionExecutor` 编排为 `CompactConversation` operation，不由 Driver 或 AgentLoop 拥有。

## 后果

- 执行顺序与线性化点集中在单一 owner，entry / projection / event 顺序可独立测试。
- 单一权威 owner 加异步 operation 分离，避免 lock-across-await 死锁；semantic lane和sticky emergency signal避免普通work backpressure饿死控制面。
- 多Session可后台并发执行，共享Model服务使用配额；Session-local file mutation queue不造成跨Session等待，UI selection不影响后台执行。
- Pending UserQuestion只暂停所属Session当前Turn的逻辑推进；UI presentation断线不改变durable Interaction，其他Session仍可独立运行。
- 严格串行current operation消除同Session logical retry的本地迟到结果竞态；execution version只验证conversation/control basis。provider端可能继续工作或计费仍不宣称exactly-once。Tool副作用真实性继续由outcome确认规则处理。
- AgentLoop保持为MiniCore自研的crate-private concrete state machine，不触碰storage与I/O顺序；第二个真实实现出现前不建立稳定替换seam，Rig不参与AgentLoop，只用于ModelGateway private ProviderAdapter（ADR 0115）。

## 历史

本 ADR 属 V2 决策集，取代以下 V1 决策，原文见 `docs/archive/v1/adr/`：

- ADR 0025（一个 loaded Session 一个 SessionExecutor）；
- ADR 0021（actor/run 分离的 two-task 形状）；
- ADR 0004（SessionManager 拥有 loaded runtime）；
- ADR 0002（compaction 由 session runtime 编排）。

2026-07-25：[ADR 0111](0111-session-ingress-separates-control-and-work-lanes.md)修订本ADR原有的单一bounded request FIFO细节；单SessionExecutor/Writer ownership不变。
2026-07-25：[ADR 0113](0113-user-question-uses-runtime-protocol-and-ui-presentation.md)补充UserQuestion等待阶段与Presentation Adapter职责；单SessionExecutor/Writer ownership不变。
2026-07-27：[ADR 0115](0115-agent-loop-is-first-party-state-machine.md)关闭Rig/SDK AgentLoop adapter分支；AgentLoop改为自研crate-private状态机，Rig职责收窄到ModelGateway private ProviderAdapter。
2026-07-27：[ADR 0116](0116-file-mutations-use-session-local-queues.md)修订跨Sessioncanonical resource lock结论；改为Session-local file mutation queue，跨SessionWorkspace并发由host/user管理。
2026-07-27：[ADR 0117](0117-async-synchronization-uses-single-owner-and-typed-permits.md)关闭全局lock-rank需求；并发实现采用single owner、短guard、release-before-fan-out和typed permit。
2026-07-27：[ADR 0118](0118-cancel-acknowledges-immediately-and-followup-waits-for-settlement.md)将Cancel command改为sticky epoch发布后立即确认；Finishing期间允许FollowUp排队，旧Turn结构化收口后再启动下一Turn。
