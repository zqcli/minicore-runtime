# ADR 0103: Turn / Item / Interaction 模型

状态：Partially Superseded by ADRs 0124 and 0126
日期：2026-07-24

> 2026-07-30：Turn/Item/Interaction identity与lifecycle保留；它们先进入LiveSessionState再best-effort record，storage committed delta和append-before-event条款失效。

> 2026-07-29修订：Turn/Item/Interaction identity、ToolInvocation合并模型和durable Interaction保持有效。ADR 0124删除ToolRoundCompleted和durable ToolExecutionStarted；complete Tool exchange由有效matching ToolResult集合自动形成，cold replay允许局部不闭合并产生diagnostics。

## 背景

MiniCore 需要一个精确的领域模型来定义「一次用户意图」的 durable 边界、其内部语义单元，以及与外部 host 的 durable 请求/回答。同类产品在这三点上形状分裂：Turn 有时等于一次模型调用加 Tool round（pi），ToolCall 与 ToolResult 有时是分离的两条消息，approval 有时是 ephemeral 的内存 waiter（pi、Grok Build 的部分 interaction）。若照搬其中任意一种，会破坏 MiniCore 对 crash recovery、reconnect 与 durable Interaction 的要求；若把 stream delta、retry、progress、phase 等都建模为领域 entity，又会让领域 identity 泛滥。

需要一个决策固定 Turn / Item / Interaction 的边界、identity 与生命周期。权威模型、全部类型、不变量与 Test Matrix 见 [`../modules/turn-item-interaction.md`](../modules/turn-item-interaction.md)；Turn 执行上下文、pinning、逻辑模型调用与 recovery 见 [`../modules/turn-execution-context.md`](../modules/turn-execution-context.md)。

## 决策

- **Turn 边界由 committed entry 线性化**：Turn 从 initiating UserMessage（`source = Input`）entry 成功 append 开始，到唯一 terminal entry（final AssistantMessage、TurnInterrupted 或 TurnFailed）成功 append 结束。admission 在 initiating UserMessage append 前失败不创建 Turn；`Interrupted` 与 `Failed` 都是 terminal，不恢复为 Running。
- **Steer与FollowUp分工**：Steer携带expected TurnId并进入当前Running Turn的bounded per-Turn FIFO；不取消当前Model/Tool step，完整step committed后、下一次模型调用前pop一条并append为Steer UserMessage。FollowUp不是`TurnControl`，它在当前Turn terminal后从独立bounded FIFO pop一条，作为新的`Input` UserMessage开启下一Turn并捕获新Context。具体SessionIngress lane由ADR 0111修订。
- **Assistant Continue step**：provider返回无ToolCall稳定response但Steer FIFO非空时，该response保存为model-visible、non-terminal `Assistant(Intermediate)`；queue为空时才保存`Assistant(Final)`并结束Turn。含ToolCall的Intermediate只有在全部matching ToolResult存在时才与ordered results一起model-visible。
- **最小 ItemContent 四类**：`ItemContent = UserMessage | AgentMessage | Reasoning | ToolInvocation`。`ItemType` 与 `ItemStatus` 都是从 content discriminant 派生的 read projection，不作为独立存储的第二事实字段。UserMessage/AgentMessage/Reasoning 只在形成稳定值后成为 durable Item 且创建即 Completed。AgentRun 的 AgentMessage/Reasoning 流式阶段由 SessionExecutor 维护 process-local `StreamingItem`；started/delta 使用稳定 ItemId 走 ProgressEvent，provider final 生成 `FinalItemCandidate`，只有 append/apply 成功后才发布同 ItemId 的 `item_completed` StateEvent。
- **ToolCall 与 ToolResult 合并为同一 ToolInvocation Item**：一个 Item 贯穿 call、approval、execution、result 与 recovery，状态 `Started → Completed | Abandoned`。Completed 必须持有 truthful ToolResult（typed `disposition`，不用单 bool 压平 denied/cancelled/failed）；outcome unknown 时进入 Abandoned，不生成 synthetic ToolResult，不进入模型 conversation。拒绝 sibling ToolCall/ToolResult Items。
- **Interaction 是 Item-owned durable request/resolution**：`InteractionRequest = ToolApproval | UserQuestion`，request/resolution family 必须匹配。遵循 request-before-notify（request append 后才通知 host）与 resolution-before-resume（resolution append 后才唤醒 waiter 或执行受审批保护的副作用）。UserQuestion由Turn-scoped `ToolExecutionControl::request_user_question`在pre-execution ask-user route中发起；Presentation Adapter只负责展示UI-safe view和提交resolution。transport disconnect或长时间无回答不自动关闭Interaction，也不推断Deny；reconnect使用相同RequestId；同一loaded execution内重复Resolve使用相同resolution_key去重，crash recovery则按committed prefix状态判断，不承诺durable key重建。Interaction answer不是UserMessage，不开启新Turn。
- **terminal Turn live闭合**：SessionExecutor在terminal entry前尽力以Cancelled关闭Pending Interaction，并关闭Started Item（Completed已有truthful result，或Abandoned）；live writer拒绝terminal后新work。cold replay遇到缺失closure或冲突terminal时first valid terminal wins、隔离残留并报告diagnostic，不brick整个Session。
- **identity 三分且不混用**：`ItemId`（MiniCore 语义 Item identity）、`ToolCallId`（ModelGateway adapter归一化的provider/transcript correlation）、`EntryId`（storage entry identity）不要求相等；Interaction归属于ItemId，不归属于裸ToolCallId。三者都不作为display sort key；public ordered Vec和new-Item StateEvent顺序直接投影selected path entry与assistant content/call顺序，不增加DisplaySequence或ordinal。
- **不引入总控对象**：不建立 `ItemManager`/`ItemService`、`InteractionManager`/`InteractionService`、`ModelStep`、`ToolRound` entity（ToolRound 只是 conversation promotion 单位，非领域 entity）。SessionStorage 是唯一 durable truth，projection 与 event stream 都不是第二事实来源。

## 后果

- Turn / Item / Interaction 的 durable 开始、结束、可见性与 side-effect 都锚定在 committed entry 的 append 线性化点；StreamingItem 只服务当前连接的 observer 体验，不改变 replay truth。
- 合并的 ToolInvocation 让 approval 归属、outcome-unknown 处理与 UI/replay correlation 只发生在单一 identity 上，避免 orphan result 与永久悬空的 call。
- durable Interaction 使 reconnect、lost acknowledgement 与 host restart 都能从 truth 判断请求状态，代价是引入 resolution_key 幂等与 first-wins CAS 的排序复杂度。
- UI presentation与MiniCore interaction protocol分离，使同一UserQuestion可以由TUI、Web、GUI或RPC Adapter呈现而不复制pending state；首版ask-user等待不预留Session-local file mutation ticket，避免阻塞同批sibling ToolCall。
- 「派生而非存储」的 ItemType/ItemStatus 与「无 Manager/Service」的取舍，把复杂性留在 Session execution 与 SessionStorage projection 内，deletion test 成立——移除该模型会让边界与生命周期重新散落。
- 明确不建ModelStep/ToolRound entity；逻辑模型调用由同一个immutable `ModelCallRequest`和exact `ConversationCheckpoint.entry_id`约束，conversation promotion由SessionStorage生成的typed `CommittedToolExchangeDelta`表达，不增加durable completion marker或fingerprint identity。

## 历史

本 ADR 属 V2 决策集，是 V2 领域模型新增的长期决策，不取代某一条 V1 ADR。相关的 V1 事件形状与生命周期背景（如 ADR 0003：agent-runtime events 使用 event-msg 与 lifecycle pairs）见 [`../archive/v1/adr/`](../archive/v1/adr/)。

2026-07-25：[ADR 0111](0111-session-ingress-separates-control-and-work-lanes.md)明确Steer/FollowUp的物理SessionIngress lane、Cancel清理和跨lane仲裁；本ADR的领域分工不变。
2026-07-25：[ADR 0113](0113-user-question-uses-runtime-protocol-and-ui-presentation.md)补充UserQuestion producer seam、WaitingForUserInput和UI presentation职责；本ADR的Item-owned durable Interaction不变。
2026-07-27：[ADR 0116](0116-file-mutations-use-session-local-queues.md)将Tool并发控制收窄为Session-local file mutation queue；跨Session共享Workspace不协调。
2026-07-28：[ADR 0123](0123-identity-uses-refs-and-explicit-reload.md)删除ModelStep/ToolRound之外的fingerprint替代identity叙事；Turn/Item/Interaction公开identity仍为`TurnId`、`ItemId`、`RequestId`和storage `EntryId`，这些不是fingerprint替代品。
