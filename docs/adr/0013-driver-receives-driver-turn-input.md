# Driver 接收 DriverTurnInput 而不是完整 TurnState

Status: accepted

`TurnState` 是 `SessionRuntime` actor 拥有的内部 run snapshot，用来 pin 住资源、`PromptTurn`、模型状态、工具视图和 context usage；`Driver` 只是 Rig sans-IO 适配器，不应直接看到 `TurnResourceSnapshot`、资源 revision、工具治理状态或 context usage。MiniCore 决定让 owner actor 在 run 启动时从 `TurnState` 投影出窄的 `DriverTurnInput` 放进 `DriveRequest`，只包含模型选择、原子 `PromptCallProfile`、thinking level 和 stream options；turn resources 留在 run-scoped owned `SessionDriverHost` 中，完整 `PromptTurn` 继续由 `CurrentRun` pin 住，safe-point rebuild 通过 ADR 0021 的私有 `RunLink` 回到 actor。Driver 使用自己拥有的 profile 调用纯 `prompt::project_model_call(...)`，不需要把 `PromptTurn` 或 resources 扩大进 seam；system prompt 与 active tool schemas 不能独立 patch。

这个决定把“少传上下文”变成类型层面的 seam，而不是实现纪律。代价是 `SessionRuntime` 需要维护一次投影，但收益是 `Driver` 的测试面更小，无法误用资源 snapshot 或会话编排状态，BR-008 中的 `DriveRequest { turn_state: TurnState }` 过宽问题被消除。
