# ADR 0103: Turn / Item / Interaction 模型

状态：Accepted
日期：2026-07-24

## 背景

MiniCore 需要一个精确的领域模型来定义「一次用户意图」的 durable 边界、其内部语义单元，以及与外部 host 的 durable 请求/回答。同类产品在这三点上形状分裂：Turn 有时等于一次模型调用加 Tool round（pi），ToolCall 与 ToolResult 有时是分离的两条消息，approval 有时是 ephemeral 的内存 waiter（pi、Grok Build 的部分 interaction）。若照搬其中任意一种，会破坏 MiniCore 对 crash recovery、reconnect 与 durable Interaction 的要求；若把 stream delta、retry、progress、phase 等都建模为领域 entity，又会让领域 identity 泛滥。

需要一个决策固定 Turn / Item / Interaction 的边界、identity 与生命周期。权威模型、全部类型、不变量与 Test Matrix 见 [`../modules/turn-item-interaction.md`](../modules/turn-item-interaction.md)；Turn 执行上下文、pinning、逻辑模型调用与 recovery 见 [`../modules/turn-execution-context.md`](../modules/turn-execution-context.md)。

## 决策

- **Turn 边界由 committed entry 线性化**：Turn 从 initiating UserMessage（`source = Input`）entry 成功 append 开始，到唯一 terminal entry（final AssistantMessage、TurnInterrupted 或 TurnFailed）成功 append 结束。admission 在 initiating UserMessage append 前失败不创建 Turn；`Interrupted` 与 `Failed` 都是 terminal，不恢复为 Running。
- **Steer与FollowUp分工**：Steer携带expected TurnId并进入当前Running Turn的普通FIFO；不取消当前Model/Tool step，完整step committed后、下一次模型调用前pop一条并append为Steer UserMessage。FollowUp不是`TurnControl`，它在当前Turn terminal后从独立FIFO pop一条，作为新的`Input` UserMessage开启下一Turn并捕获新Context。
- **Assistant Continue step**：provider返回无ToolCall稳定response但Steer FIFO非空时，该response保存为model-visible、non-terminal `Assistant(Intermediate)`；queue为空时才保存`Assistant(Final)`并结束Turn。含ToolCall的Intermediate仍只在`tool_round_completed`后model-visible。
- **最小 ItemContent 四类**：`ItemContent = UserMessage | AgentMessage | Reasoning | ToolInvocation`。`ItemType` 与 `ItemStatus` 都是从 content discriminant 派生的 read projection，不作为独立存储的第二事实字段。UserMessage/AgentMessage/Reasoning 只在形成稳定值后成为 durable Item 且创建即 Completed。
- **ToolCall 与 ToolResult 合并为同一 ToolInvocation Item**：一个 Item 贯穿 call、approval、execution、result 与 recovery，状态 `Started → Completed | Abandoned`。Completed 必须持有 truthful ToolResult（typed `disposition`，不用单 bool 压平 denied/cancelled/failed）；outcome unknown 时进入 Abandoned，不生成 synthetic ToolResult，不进入模型 conversation。拒绝 sibling ToolCall/ToolResult Items。
- **Interaction 是 Item-owned durable request/resolution**：`InteractionRequest = ToolApproval | UserQuestion`，request/resolution family 必须匹配。遵循 request-before-notify（request append 后才通知 host）与 resolution-before-resume（resolution append 后才唤醒 waiter 或执行受审批保护的副作用）。transport disconnect不自动关闭Interaction；reconnect使用相同RequestId；同一loaded execution内重复Resolve使用相同resolution_key去重，crash recovery则按committed prefix状态判断，不承诺durable key重建。Interaction answer不是UserMessage，不开启新Turn。
- **terminal Turn 领域闭合**：terminal entry 前必须关闭所有 Pending Interaction（Cancelled/Expired）与 Started Item（Completed 已有 truthful result，或 Abandoned）；terminal Turn 不保留 Pending Interaction 或 Started Item，terminal 后不接受 Steer、Item append 或 Interaction response。
- **identity 三分且不混用**：`ItemId`（MiniCore 语义 Item identity）、`ToolCallId`（provider/transcript correlation，原样回显）、`EntryId`（storage entry identity）不要求相等；Interaction 归属于 ItemId，不归属于裸 ToolCallId。
- **不引入总控对象**：不建立 `ItemManager`/`ItemService`、`InteractionManager`/`InteractionService`、`ModelStep`、`ToolRound` entity（ToolRound 只是 conversation promotion 单位，非领域 entity）。SessionStorage 是唯一 durable truth，projection 与 event stream 都不是第二事实来源。

## 后果

- Turn / Item / Interaction 的开始、结束、可见性与 side-effect 都锚定在 committed entry 的 append 线性化点，replay、UI projection 与 recovery 可独立测试。
- 合并的 ToolInvocation 让 approval 归属、outcome-unknown 处理与 UI/replay correlation 只发生在单一 identity 上，避免 orphan result 与永久悬空的 call。
- durable Interaction 使 reconnect、lost acknowledgement 与 host restart 都能从 truth 判断请求状态，代价是引入 resolution_key 幂等与 first-wins CAS 的排序复杂度。
- 「派生而非存储」的 ItemType/ItemStatus 与「无 Manager/Service」的取舍，把复杂性留在 Session execution 与 SessionStorage projection 内，deletion test 成立——移除该模型会让边界与生命周期重新散落。
- 明确不建 ModelStep/ToolRound entity，意味着逻辑模型调用与 conversation promotion 靠 fingerprint、EntryId/parent_id 引用与 `tool_round_completed` event 表达，而非新增领域实体。

## 历史

本 ADR 属 V2 决策集，是 V2 领域模型新增的长期决策，不取代某一条 V1 ADR。相关的 V1 事件形状与生命周期背景（如 ADR 0003：agent-runtime events 使用 event-msg 与 lifecycle pairs）见 [`../archive/v1/adr/`](../archive/v1/adr/)。
