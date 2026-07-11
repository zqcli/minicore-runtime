# 命令运行策略与提示词交付方式分离

## 状态

Accepted

## 决策

MiniCore 不引入跨子系统的通用 `InputSchedule`。slash/catalog command 使用 `CommandRunPolicy { Immediate, IdleOnly, QueueAfterRun }` 决定 handler 在 active work 中立即执行、拒绝或保存为 typed `PendingSessionAction`；模型可见输入使用 `PromptDelivery { Steer, FollowUp, NextTurn }` 决定进入当前 run 的下一模型调用、当前 work 后的后续 run 或下一次显式用户 turn。`SessionRuntime` 是两类调度状态的唯一 owner，`CommandManager` 只解析和校验策略，`Driver` 只通过 `before_next_model_call` / `before_run_finish` 暴露消费安全点。

## 影响

公开协议删除独立 `Steer` / `FollowUp` / `NextTurn` 命令，统一由 prompt-producing 入口携带 `PromptDelivery`；`DeliveryMode` 改名为 `PromptDelivery`。`CommandPhasePolicy` 改名并收窄为 `CommandRunPolicy`，其中 `/compact` 使用 `QueueAfterRun`。运行中 `/status` 这类读取命令使用 `Immediate` 并把结果异步输出到 message panel；prompt-producing slash command 先解析为结构化 prompt intent，再按调用方的 `PromptDelivery` 交给 `SessionRuntime`，不能把 raw slash text 放入消息队列或 pending action。