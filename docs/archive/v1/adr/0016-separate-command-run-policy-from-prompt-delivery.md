# 命令运行策略与提示词交付方式分离

> **归档（V1）**：本 ADR 属于 MiniCore V1 架构，仅作历史参考，不得作为当前实现或新开发的设计依据。当前权威决策见 `docs/adr/`（0100+）。原文保持历史原貌。

## 状态

Partially superseded by [ADR 0028](0028-runtime-protocol-uses-scoped-state-cursors.md)。Command execution policy与prompt delivery分离保留；SessionExecutor替代SessionRuntime，首版不建立PendingSessionAction/manual compact，公开Turn control使用Submit、Steer和FollowUp typed command。

Amended by [ADR 0025](0025-loaded-session-uses-one-session-executor.md) and [ADR 0027](0027-compaction-uses-strict-stable-suffix.md): SessionExecutor replaces SessionRuntime as execution owner, and first-implementation Compaction does not expose running-time `/compact` or `QueueAfterRun` behavior.

> 以下Decision/Impact保留原始历史形状。涉及NextTurn、PendingSessionAction、QueueAfterRun `/compact`或删除公开Steer/FollowUp的内容已被ADR 0028替代。

## 决策

MiniCore 不引入跨子系统的通用 `InputSchedule`。slash/catalog command 使用 `CommandRunPolicy { Immediate, IdleOnly, QueueAfterRun }` 决定 handler 在 active work 中立即执行、拒绝或保存为 typed `PendingSessionAction`；模型可见输入使用 `PromptDelivery { Steer, FollowUp, NextTurn }` 决定进入当前 run 的下一模型调用、当前 work 后的后续 run 或下一次显式用户 turn。`SessionRuntime` 是两类调度状态的唯一 owner，`CommandManager` 只解析和校验策略，`Driver` 只通过 `before_next_model_call` / `before_run_finish` 暴露消费安全点。

## 影响

公开协议删除独立 `Steer` / `FollowUp` / `NextTurn` 命令，统一由 prompt-producing 入口携带 `PromptDelivery`；`DeliveryMode` 改名为 `PromptDelivery`。`CommandPhasePolicy` 改名并收窄为 `CommandRunPolicy`，其中 `/compact` 使用 `QueueAfterRun`。运行中 `/status` 这类读取命令使用 `Immediate` 并把结果异步输出到 message panel；prompt-producing slash command 先解析为结构化 prompt intent，再按调用方的 `PromptDelivery` 交给 `SessionRuntime`，不能把 raw slash text 放入消息队列或 pending action。
