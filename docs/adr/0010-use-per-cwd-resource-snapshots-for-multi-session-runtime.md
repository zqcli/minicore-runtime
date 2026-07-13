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

`RuntimeResourceSnapshot` 捕获 product / user-global defaults。它在 `OpenWorkspace` 初始化一次，并与 UI host / `AgentRuntime` 生命周期绑定；MVP 不支持 runtime reload，也不使用递增 runtime 级 revision。全局资源变化通过重建 `AgentRuntime` 生效。

`CwdResourceSnapshot` 捕获固定 `(workspace_id, cwd)` 下的 project/cwd/trust/catalog overlay 视图，并持有构建它时使用的 `Arc<RuntimeResourceSnapshot>`。`CwdResourceRevision` 只用于 cwd reload 与 UI 摘要失效。

`TurnResourceSnapshot` 是一次 user turn / work chain 的稳定输入，包含 `session_id`、`user_turn_id`（不是 `RunId`）、`Arc<CwdResourceSnapshot>`、behavior、model、environment、policy 和 fingerprint。`StepResourceSnapshot` 只作为未来每次模型采样前捕获动态资源的类型边界；MVP 定义类型但不执行、不使用。

如果未来启用 `StepResourceSnapshot`，它也只应持有 `Arc<TurnResourceSnapshot>` 作为 parent，并附加 step-scoped 动态输入；它不能在每个 step 重新加载或重新合成 runtime/cwd/turn 级资源，也不能读取 `ResourceManager` 的最新 current pointer 来替换 running turn 已捕获的资源。

`CwdResourceSnapshot` 不是单纯的 cwd-local 增量。它包含由 `ResourceOverlayPolicy` 生成的 resolved effective resource view。cwd/project 资源可以按稳定 `ResourceKey { kind, namespace?, name }` 覆盖同 key 的 runtime/global 资源，例如项目内同名 skill 覆盖 user-global skill。MVP 中 overlay policy 编码在代码内，不暴露为 settings 或配置文件；后续如果需要调整 precedence，只应修改集中的 policy 实现。

每个 `SessionRuntime` 固定一个 workspace cwd。新的显式 user turn / work chain 启动时，`SessionRuntime` 调用 `ResourceManager.capture_turn(...)` 取得 `TurnResourceSnapshot` 并存入 `TurnState`。running turn 不再读取 `ResourceManager.current_*()`。`ReloadResources { workspace_id, cwd }` 会构建新的 `CwdResourceSnapshot`，并只通过 `ResourceSnapshotStore::replace_cwd(...)` 为后续 turn 原子替换当前 cwd pointer；running turn 持有的旧 snapshot 通过 `Arc` 继续有效。

`capture_turn(...)` 不自动发现资源变化，也不兜底读盘。MVP 中 `OpenWorkspace` 保证 runtime snapshot 已初始化；`OpenSession` / `NewSession` 保证目标 cwd snapshot 已存在；turn capture 在稳态只读取 current cwd pointer，并冻结调用方传入的 typed owner projections。下一个新的显式 user turn 会使用读取 current pointer 时看到的 snapshot：如果此前没有 cwd reload，它仍会使用旧 cwd revision；如果 reload 已成功发布新 snapshot，它会使用新 cwd revision。

cwd reload 对后续 turn 生效的唯一线性化点是 `ResourceSnapshotStore::replace_cwd(...)`。`SessionRuntime` 不跨 turn 缓存 `TurnResourceSnapshot`，也不等待 `ResourceManager` 主动推送新资源。若 `SubmitPrompt` 在 `replace_cwd` 之前已经读取 current pointer，它可以使用旧 snapshot；若在 `replace_cwd` 之后读取 current pointer，它必须看到新 snapshot。

并不是所有层都在 turn 开始时才创建。`RuntimeResourceSnapshot` 在 `OpenWorkspace` 创建；`CwdResourceSnapshot` 可以在 session open、new session、首次受控 ensure 或显式 reload 时创建并发布；turn 开始时只创建 `TurnResourceSnapshot`，用来 pin 住当时 current cwd snapshot 链和 typed turn projections。打开 UI 后如果某个 cwd 还没有 current snapshot，`OpenSession` / `NewSession` 的 ensure 必须先创建它；单纯修改磁盘文件不会改变 current snapshot，必须 cwd reload 后新的显式 user turn 才能捕获新 revision。

`TurnResourceFingerprint` 组合 cwd revision + behavior/model/environment/policy versions。具体 canonical 算法归 BR-063 定义。

user turn / work chain capture 规则：automatic retry、overflow compaction recovery、active Steer、同 `RunId` Rig segment rollover 复用原 `TurnResourceSnapshot`；`FollowUp`、`NextTurn` 和新 idle prompt 创建新的 `TurnResourceSnapshot`。

skill 子系统与 `ResourceManager` 的边界也按此决策收敛：`skills.rs` 定义和解析 `SkillMetadata` / `SkillResource` / `SkillCatalog`，但 current skill catalog 的生命周期、runtime/cwd 分层、overlay 和发布都归 `ResourceManager`；显式 `InvokeSkill` 只能从 captured `TurnResourceSnapshot` 读取 selected `SkillResource.body`。

provider settings、auth、custom providers 和 `ModelGateway` 仍是 user-global/runtime-global 服务，不是 project-scoped resources。因此 `CwdScopedServices`、`CwdServiceRegistry` 和 service generation pinning 不属于 MVP runtime shape。隔离来自固定 session cwd、不可变级联资源快照、turn-time capture 和 deterministic overlay policy。

当前决策也明确不考虑热更新、文件监听器或资源回调接口。所有会影响模型行为的 cwd 资源更新都必须通过 `ResourceManager` 的显式 cwd reload 生成新 snapshot；旧 snapshot 不能被原地修改。若未来 MiniCore 支持 daemon-style detach/attach、project-scoped providers、per-session process workers 或另一套资源更新机制，应重新审视本 ADR，但仍必须保持旧 snapshot 不可变。
