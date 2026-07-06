# 多 Session Runtime 使用级联资源快照

状态：已接受

MiniCore MVP 运行在一个 UI/runtime 同进程程序中，不支持 UI detach 后 runtime daemon 继续运行，也不支持 daemon reconnect。基于这个生命周期约束，多 session 同时运行不需要 per-cwd 服务容器；`AgentRuntime` 继续在 `WorkspaceServices` 中保留共享运行时服务，而模型可见资源由 `ResourceManager` 子系统管理，并使用级联的不可变 snapshot：

```text
RuntimeResourceSnapshot
  ↓ 被下层 pin 住
CwdResourceSnapshot
  ↓ 被下层 pin 住
TurnResourceSnapshot
  ↓ 作为父级
StepResourceSnapshot
```

`RuntimeResourceSnapshot` 捕获 user-global / runtime-global 资源。`CwdResourceSnapshot` 捕获固定 `(workspace_id, cwd)` 下的资源视图，并持有构建它时使用的 `Arc<RuntimeResourceSnapshot>`。`TurnResourceSnapshot` 只持有 `Arc<CwdResourceSnapshot>`，不再单独持有 runtime 资源，因为 cwd snapshot 已经传递性地 pin 住了对应 runtime snapshot。`StepResourceSnapshot` 只作为未来每次模型采样前捕获动态资源的类型边界；MVP 定义类型但不执行、不使用。

如果未来启用 `StepResourceSnapshot`，它也只应持有 `Arc<TurnResourceSnapshot>` 作为 parent，并附加 step-scoped 动态输入；它不能在每个 step 重新加载或重新合成 runtime/cwd/turn 级资源，也不能读取 `ResourceManager` 的最新 current pointer 来替换 running turn 已捕获的资源。

`CwdResourceSnapshot` 不是单纯的 cwd-local 增量。它包含由 `ResourceOverlayPolicy` 生成的 resolved effective resource view。cwd/project 资源可以按稳定 `ResourceKey { kind, namespace?, name }` 覆盖同 key 的 runtime/global 资源，例如项目内同名 skill 覆盖 user-global skill。MVP 中 overlay policy 编码在代码内，不暴露为 settings 或配置文件；后续如果需要调整 precedence，只应修改集中的 policy 实现。

每个 `SessionRuntime` 固定一个 workspace cwd。user turn 启动时，`SessionRuntime` 调用 `ResourceManager.capture_turn(...)` 取得 `TurnResourceSnapshot` 并存入 `TurnState`。running turn 不再读取 `ResourceManager.current_*()`。`ReloadResources { workspace_id, cwd }` 会构建新的 `CwdResourceSnapshot`，并只为后续 turn 原子替换当前指针；running turn 持有的旧 snapshot 通过 `Arc` 继续有效。runtime/global 资源 reload 会发布新的 `RuntimeResourceSnapshot`；后续 turn capture 时如果发现 cwd snapshot 指向旧 runtime revision，则由 `ResourceManager` 懒惰 recompute cwd snapshot。

`capture_turn(...)` 不自动发现资源变化。MVP 中显式 reload / startup ensure 是资源更新的写入边界；turn capture 是资源使用的读取边界。下一个 turn 会使用当时的 current snapshot：如果此前没有 reload，它仍会使用旧 revision；如果 reload 已成功发布新 snapshot，它会使用新 revision。

reload 对后续 turn 生效的线性化点是 `ResourceSnapshotStore::replace_cwd(...)` 或 `replace_runtime(...)`。`SessionRuntime` 不跨 turn 缓存 `TurnResourceSnapshot`，也不等待 `ResourceManager` 主动推送新资源；每个 user turn 启动时都重新调用 `capture_turn(...)`，从 store 读取当时 current pointer。若 `SubmitPrompt` 在 replace 之前已经开始 capture，它可以使用旧 snapshot；若在 replace 之后开始 capture，它必须看到新 snapshot。

并不是所有层都在 turn 开始时才创建。`RuntimeResourceSnapshot` 和 `CwdResourceSnapshot` 可以在 startup ensure、session open、首次使用 cwd 或显式 reload 时创建并发布；turn 开始时只创建 `TurnResourceSnapshot`，用来 pin 住当时 current cwd snapshot 链。打开 UI 后如果某个 cwd 还没有 current snapshot，第一次 turn 的 ensure 可以从当时磁盘状态加载资源；如果 current snapshot 已经在 turn 前创建，单纯修改磁盘文件不会改变它，必须 reload 后第一次 turn 才能捕获新 revision。

初始化保证有三道防线：`OpenWorkspace` 必须调用 `ensure_runtime_snapshot(...)`；`OpenSession` / `NewSession` 必须调用 `ensure_cwd_snapshot(...)`；`capture_turn(...)` 必须在 turn 启动时兜底执行 `ensure_runtime_snapshot(...)`、缺失 cwd snapshot 时执行 `ensure_cwd_snapshot(...)`，以及 runtime revision 变化时执行 `recompose_cwd(...)`。`ensure_*` 必须幂等，reload 的线性化点必须是 `ResourceSnapshotStore::replace_*`。

例如当前 snapshot 中 `skill-a` 的正文是 v1，用户在本地把 `SKILL.md` 改成 v2 但没有执行 reload，那么后续 turn 调用 `skill-a` 仍使用 v1。只有 reload 成功发布新 snapshot 后，后续 turn 才会使用 v2。

skill 子系统与 `ResourceManager` 的边界也按此决策收敛：`skills.rs` 定义和解析 `SkillMetadata` / `SkillResource` / `SkillCatalog`，但 current skill catalog 的生命周期、runtime/cwd 分层、overlay 和发布都归 `ResourceManager`；显式 `InvokeSkill` 只能从 captured `TurnResourceSnapshot` 读取 selected `SkillResource.body`。

provider settings、auth、custom providers 和 `ModelGateway` 仍是 user-global/runtime-global 服务，不是 project-scoped resources。因此 `CwdScopedServices`、`CwdServiceRegistry` 和 service generation pinning 不属于 MVP runtime shape。隔离来自固定 session cwd、不可变级联资源快照、turn-time capture 和 deterministic overlay policy。

当前决策也明确不考虑热更新、文件监听器或资源回调接口。所有会影响模型行为的资源更新都必须通过 `ResourceManager` 的显式 reload / recompose pipeline 生成新 snapshot；旧 snapshot 不能被原地修改。若未来 MiniCore 支持 daemon-style detach/attach、project-scoped providers、per-session process workers 或另一套资源更新机制，应重新审视本 ADR，但仍必须保持旧 snapshot 不可变。
