# ADR 0111: SessionIngress 分离控制与工作 lane

状态：Accepted
日期：2026-07-25

## 背景

V2 原设计让Submit、Steer、FollowUp、Interaction resolution、Tool control、Cancel、authority security interruption、PrepareForUnload和Snapshot共用每个Session的一个bounded FIFO。该形状保留了single owner和简单全序，但把容量和延迟也绑定在一起：大量普通输入或内部Tool control可以占满队列，使Cancel、security cleanup、Interaction resolution和unload只能等待前序工作排空。

Authority hard restriction与普通Cancel都需要out-of-band sticky signal，使新operation立即受gate阻止，同时让Executor完成truthful Tool settlement和`TurnInterrupted`。单一FIFO不能同时满足bounded backpressure、紧急控制响应和公平admission。

本决策只拆分 ingress seam，不拆分状态 owner。`SessionExecutor`、`SessionWriter`、committed projections、current Turn 和 terminal decision 仍各自只有一个 owner。完整设计见 [Session Execution](../modules/session-execution.md)。

## 决策

每个 loaded Session 拥有一个独立的 `SessionIngress`。Ingress按语义路由请求，不建立跨 lane 的全局 FIFO：

| 请求 | lane | 规则 |
| --- | --- | --- |
| `Submit` | `TurnAdmissionQueue` | bounded FIFO |
| `Steer` | `SteerQueue<TurnId>` | bounded per-Turn FIFO |
| `FollowUp` | `FollowUpQueue` | bounded FIFO |
| `CancelQueuedMessage` | `InputMailboxControl` | 按`CommandId`原子删除一条Steer或FollowUp |
| `ResolveInteraction` | `InteractionControlQueue` | bounded FIFO，配置保留容量 |
| `ToolControl` | `ToolControlQueue` | 内部bounded FIFO |
| `Cancel` | `EmergencyControl` | target-scoped sticky、可合并signal；cancel epoch发布后立即返回typed accepted response |
| `SecurityRevoked` | `EmergencyControl` | current SessionExecutionHandle-scoped sticky signal；关闭admission，存在Turn时进入truthful settlement |
| `PrepareForUnload` | `LifecycleControl` | sticky stop-admission signal、有限grace deadline和shared completion waiter |
| `GetSnapshot` | `SnapshotMailbox` | latest-wins/coalesced immutable published view |

各 Session 的 lane 容量完全独立。Session A 的 ingress 满不会占用 Session B 的容量；跨 Session 仍可能在 ModelGateway 配额和共享宿主I/O处等待。File mutation queue按Session独立，不造成跨Session等待（ADR 0116）。

### Emergency 与 Tool side effect

`Cancel`先原子校验current immutable `CancelTarget`，只触发该target绑定的operation cancellation token，不等待普通bounded lane。stale TurnId或Submit CommandId不得取消当前或下一Turn；目标terminal后signal retire，新Turn发布新的active target和token。Executor在启动新Model、预留或继续file mutation ticket、`ToolExecutionStarted` append前、`tool_round_completed`前和terminal Assistant append前观察最新emergency epoch。

`SessionIngress`内部使用一个非持久化`TurnControlGate`使检查与短append可线性化。它不是actor或状态owner，只提供原子CAS式的active target、emergency epoch、Steer admission gate和controlled append reservation：

- Steer admission与final commit reservation first-wins；Steer先赢则candidate final转为Continue，final reservation先赢则拒绝新Steer；
- Cancel/SecurityRevoked signal与controlled append reservation first-wins；signal先赢则append不得开始，reservation先赢则该次短append完成，signal仍立即发布并在append后驱动cleanup；
- reservation只跨一次`SessionWriter.append → apply`，signal发布不阻塞；不再叠加Workspace commit permit。

LifecycleControl的stop-admission transition与Submit/Steer/FollowUp `try_admit`同样原子排序：admission先赢则Unload drain明确拒绝/清理该请求，stop先赢则直接返回stopping。Emergency、required cleanup control和Snapshot仍可进入。

这些gate只依据Executor发布的immutable active target和admission state做容量预留和race排序；领域validation、typed completion、StateEvent publication和所有durable mutation仍由SessionExecutor确认。

`ToolExecutionStarted` append/apply是副作用竞态的真实线性化点：

- Cancel/SecurityRevoked先被观察：拒绝新的execution start；
- `ToolExecutionStarted`先append/apply：副作用可以开始，之后必须保存真实outcome；Cancel只能best-effort取消，不能声称回滚。

Authority/host hard restriction先发布current security/policy fact，再通过Runtime current loaded map向对应`SessionExecutionHandle`设置sticky `SecurityRevoked`并唤醒Executor；存在candidate/current Turn时同时绑定current target。即使Executor尚未处理signal，owner-local admission gate与`TurnControlGate`也会阻止new admission/controlled append/Tool start/Model start。Idle直接resolve，Starting取消candidate，active Turn terminal后resolve；成功后signal retire。old handle关闭后不得把signal转发到new Executor，future Turn使用new Snapshot和新的active target。

### Queued input 与 Cancel

`CancelQueuedMessage`只删除目标`CommandId`对应的一条queued Steer或FollowUp，不清空任何lane。remove与lane `pop_front`原子first-wins：remove先赢则消息不执行，pop先赢或已经append则返回`NotQueued`且不重新入队。

Cancel current Turn时：

- sticky epoch发布后立即返回`CancelAccepted`；最终TurnInterrupted通过StateEvent/Snapshot观察；
- 清理所有仍指向该Turn的queued Steer；
- cancel epoch发布后拒绝新的同Turn Steer；
- 默认保留FollowUp，并在Finishing期间继续接受新的FollowUp；当前Turn terminal后继续正常admission；
- 不清除其他Submit；若产品需要“停止该Session全部工作”，必须定义显式`StopAll`/`ClearQueuedMessages`能力。

清理accepted queued input时发布带`CommandId + reason`的`queue_updated` StateEvent；该事件只描述process-local队列事实，不把未append消息伪造成durable UserMessage。

### Lifecycle 与 Snapshot

`PrepareForUnload`立即停止新admission，拒绝尚未admit的Submit并清理queued Steer/FollowUp。重复请求订阅同一个completion waiter，effective deadline只可取更早值、不可延长shutdown。grace期内current Turn可以自然完成；deadline到期后取消active pre-Turn Submit或Turn，以Cancelled关闭Pending Interaction，完成truthful Tool settlement和terminal append后卸载。该deadline属于显式Unload lifecycle，不是Interaction inactivity timeout。

`GetSnapshot`不进入mutation/control queue。Executor持续发布immutable view，Snapshot mailbox返回latest完整view。用于持续观察时，subscriber注册与初始Snapshot capture在同一publication synchronization内原子完成，之后只发送实时事件。Snapshot与不同lane之间不宣称全局FIFO。

### State-aware arbitration

Executor在每个安全点按状态仲裁：

1. emergency与lifecycle signal，包括到期的Unload grace deadline；
2. 已完成operation result和terminal cleanup；
3. Interaction control；
4. 当前Tool round依赖的Tool control；
5. 当前Turn安全点消费一条Steer；
6. terminal后在FollowUp与Submit间公平admission；
7. Snapshot独立读取published view。

control lane使用bounded burst，避免control flood永久饿死普通工作。terminal后已accepted FollowUp最多获得一次连续优先；若上一Turn由FollowUp启动且当前有external Submit，则下一次Idle decision先选Submit。Submit ingress不是隐式FollowUp，未选中且Session再次Busy时明确返回`SessionBusy`，不跨整个Turn静默等待。

## 同类产品依据

- pi明确分离Steer与FollowUp内存队列，并通过AbortController直接中止当前run、清空pending input；它证明“输入类别分离 + out-of-band abort”是简单有效的形状，但没有MiniCore的durable Tool settlement和terminal cleanup。
- Codex把active Turn输入放入独立`TurnInputQueue/pending_input`，Interrupt可触发active task取消；其submission通道仍可能bounded，属于部分分流而非完整emergency guarantee。
- Grok Build/ACP把session actor、prompt queue和interjection结构分离，说明交互控制不必和普通prompt共用单一FIFO；公开资料未给出内部容量与SLA。
- Claude Code与Cursor的产品行为可观察到queued prompt、interrupt和approval分别处理，但内部owner、容量和调度未公开，不能作为实现级证据。

MiniCore采用这些产品共同的“输入队列与中断分离”方向，同时保留自身更严格的单 durable owner、append/apply线性化和truthful side-effect settlement。

## 后果

- 普通输入backpressure不能阻塞Cancel/SecurityRevoked signal；D1关闭。
- 不再依赖跨类型全局FIFO；每个race必须由state validation、emergency epoch和durable append线性化点说明。
- lane容量、bounded burst、公平admission和unload deadline都必须成为配置与测试项；duplicate Cancel返回同一accepted epoch且不保存sender，PrepareForUnload继续复用shared completion waiter。
- Snapshot读取更轻，不会因工作队列拥塞而超时；持续观察必须使用snapshot-first subscription，不能假设单独Snapshot与某条mutation或后续subscribe形成顺序。
- 仍只有一个SessionExecutor和一个SessionWriter；没有新增第二个conversation owner或并发mutation actor。

## 修订关系

本ADR修订[ADR 0105](0105-session-executor-owns-loaded-session.md)中“所有请求经同一个bounded FIFO”的ingress细节，并修订[ADR 0108](0108-runtime-public-protocol.md)中“SessionSnapshot经request queue线性化”的一致性表述。单Executor ownership保持不变；公开观察协议后由[ADR 0114](0114-runtime-observation-uses-snapshot-first-streams.md)改为snapshot-first实时流。

2026-07-27：[ADR 0116](0116-file-mutations-use-session-local-queues.md)删除跨SessionTool resource lock；本ADR的lane隔离、emergency与短append仲裁不变。
2026-07-27：[ADR 0117](0117-async-synchronization-uses-single-owner-and-typed-permits.md)明确不建设全局lock-rank系统；普通guard不跨await，controlled append使用typed permit和私有组合helper。
2026-07-27：[ADR 0118](0118-cancel-acknowledges-immediately-and-followup-waits-for-settlement.md)将Cancel response改为sticky epoch发布后立即确认；Finishing期间允许FollowUp排队，最终terminal通过StateEvent/Snapshot观察。
2026-07-27：[ADR 0121](0121-workspace-updates-require-idle.md)删除active-Turn Workspace lease/revoke分支；Workspace definition只在Idle更新，authority hard restriction直接发布`SecurityRevoked` sticky signal。

## 被否决的方案

### 保留单一FIFO并仅扩大容量

容量只能延后拥塞，不能保证Cancel在队列满时获得位置，也放大内存和最坏等待时间。

### 仅为FIFO预留一个Cancel slot

无法同时覆盖revocation、unload和Interaction resolution；重复Cancel或多个waiter也会重新产生容量边界，且Snapshot仍与工作流量耦合。

### 为每个lane建立独立actor或writer

会产生多个mutable owner，使entry、projection、Tool outcome和terminal顺序重新分散。lane只负责ingress，所有语义修改仍由SessionExecutor执行。
