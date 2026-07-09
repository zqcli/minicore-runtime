# Driver 接收 DriverTurnInput 而不是完整 TurnState

Status: accepted

`TurnState` 是 `SessionRuntime` 拥有的内部 run snapshot，用来 pin 住资源、system prompt、模型状态、工具视图和 context usage；`Driver` 只是 Rig sans-IO 适配器，不应直接看到 `TurnResourceSnapshot`、资源 revision、工具治理状态或 context usage。MiniCore 决定让 `SessionRuntime` 在 run 启动时从 `TurnState` 投影出窄的 `DriverTurnInput` 放进 `DriveRequest`，只包含模型选择、system prompt、active tool schemas、thinking level 和 stream options；turn resources 留在 per-run `SessionDriverHost` 中，用于工具运行上下文、safe point 决策和未来 `StepResourceSnapshot` parent。

这个决定把“少传上下文”变成类型层面的 seam，而不是实现纪律。代价是 `SessionRuntime` 需要维护一次投影，但收益是 `Driver` 的测试面更小，无法误用资源 snapshot 或会话编排状态，BR-008 中的 `DriveRequest { turn_state: TurnState }` 过宽问题被消除。
