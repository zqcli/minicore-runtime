# Driver 接收 DriverTurnInput 而不是完整 TurnState

Status: accepted

`TurnState` 是 `SessionRuntime` actor 拥有的内部 run snapshot，用来 pin 住资源、`PromptTurn`、模型状态、工具视图和 context usage；`Driver` 只是 Rig sans-IO 适配器，不应直接看到 `TurnResourceSnapshot`、cwd resource revision、工具治理状态或 context usage。MiniCore 决定让 owner actor 在 run 启动时从 `TurnState` 投影出窄的 `DriverTurnInput` 放进 `DriveRequest`，只包含模型选择、原子 `PromptCallProfile`、thinking level 和 stream options；turn resources 留在 run-scoped owned `SessionDriverHost` 中，完整 `PromptTurn` 继续由 `CurrentRun` pin 住。MVP 拒绝 active turn 中的 model/thinking/stream/active-tools/profile mutation；后续 safe-point rebuild 必须通过 ADR 0021 的私有 `RunLink` 回到 actor，并通过 step snapshot 或明确 step override 原子替换 profile 与 future tool invoker。Driver 使用自己拥有的 profile 调用纯 `prompt::project_model_call(...)`，不需要把 `PromptTurn` 或 resources 扩大进 seam；system prompt 与 active tool schemas 不能独立 patch。

这个决定把“少传上下文”变成类型层面的 seam，而不是实现纪律。代价是 `SessionRuntime` 需要维护一次投影，但收益是 `Driver` 的测试面更小，无法误用资源 snapshot 或会话编排状态，BR-008 中的 `DriveRequest { turn_state: TurnState }` 过宽问题被消除。

后续 Rig 0.40.0 核对确认，active Steer 可以在完整 assistant/tool turn 后通过同一 `RunId` 下的 Rig `AgentRun` segment rollover 实现。该实现不扩大 `DriverTurnInput`：旧 segment 的协议 history 和已 committed steering message 由 Driver 私有 adapter 组合，新 segment 仍使用当前 run 的 owned profile/options；resources、`PromptTurn` 和 session queue state 继续留在 actor/host seam 之后。
