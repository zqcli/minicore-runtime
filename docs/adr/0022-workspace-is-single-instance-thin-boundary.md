# ADR 0022: Workspace 是单实例薄边界容器

状态：已接受

关联：本 ADR 关闭 [BR-036](../review/system-blueprint-review-issues-round2.md)；完整设计推导见 [docs/design/workspace-model.md](../design/workspace-model.md)。

## 背景

协议表面已按多 workspace 形状铺开（`ReloadResources { workspace_id }`、`SessionListScope::AllWorkspaces / Workspace{id}`、外层 `Event.workspace_id`、resource snapshot key 含 `workspace_id`、`NewSession { workspace_id }`），但 `RuntimeSnapshot.workspace` 是单个 `Option`、Workspace Lifecycle 状态机是单实例的，`OpenWorkspace` 重复调用的语义从未定义。

盘点后确认一个更根本的事实：当前设计中**没有任何安全边界、资源边界或服务边界绑定在 workspace 层**。项目资源和 project trust 是 per-cwd 的（[ADR 0010](0010-use-per-cwd-resource-snapshots-for-multi-session-runtime.md)、resource-manager.md 项目信任节），provider/auth/settings 是 user-global 的（model-gateway.md、[BR-025](../review/system-blueprint-review-issues-round2.md)），工具沙箱是 per-session-cwd 的（tools.md），运行时服务与 host 同生命周期。workspace 只出现在三处：命令/查询/事件的路由坐标、resource snapshot store 的 key 前缀、session metadata 的归属字段。

同类工具（VS Code、Claude Code、Codex、Zed、JetBrains）的横向调研得出四条行业共识：workspace 身份 = 目录路径（无一用随机 id）；trust/安全边界绑定目录层而非容器层；多目录 = 往当前容器显式加根（`--add-dir` / multi-root）；多容器并存 = 多窗口/多实例。Codex 的 `--add-dir` 写围栏旁路缺陷（apply_patch 绕过白名单，issue #24214）额外提供一条教训：附加目录必须进入沙箱的单一 source of truth，不能由工具各自解释。

## 决策

把 workspace 定义为**单实例的薄边界容器**。它承载四样东西——身份（`WorkspaceId`）、根边界（`workspace_root`）、生命周期状态（`NoWorkspace | Open`）、UI 投影（`WorkspaceSummary`）——不承载 trust、资源 scope、provider/auth/settings 或服务生命周期，这些维持现有归属不动。

**D1 单实例，重复 `OpenWorkspace` 幂等或拒绝。** `OpenWorkspace { path }` 只在 `NoWorkspace` 创建 workspace，并先进入 `Opening`。首次 command 的 `CommandAck` 只表示 admission；初始化成功后进入 `Open` 并发布 `workspace_opened`，失败后回到 `NoWorkspace` 并发布 `workspace_open_failed`。`Opening` 中同 canonical root 的重复 command 幂等 accepted、共享同一次初始化；不同 root 拒绝 `WorkspaceOpening`。`Open` 状态下再次调用：canonical path 与当前 root 相同则幂等成功（不 teardown、不重载资源、不影响 loaded sessions）；不同则拒绝 `WorkspaceAlreadyOpen`。不引入 `CloseWorkspace`，不做 teardown-and-replace。"换项目"的实现方式是 host 丢弃整个 `AgentRuntime` 实例并重建，与 VS Code"换文件夹 = 换窗口"同构。否决替换语义的理由：它必须连带定义全部 loaded session 的强制关闭、in-flight run 的 abort 与 writer commit 竞态、pending approval 清理、跨 workspace 的事件水位连续性——这是一大块状态机复杂度，而 MVP 中 host 与 runtime 同生命周期（[ADR 0020](0020-agent-runtime-has-no-current-session.md)、BR-002/BR-044 约束），重建实例恰好最干净。

**D2 `WorkspaceIdV1` 从 canonical root path 确定性派生。** 同一物理 root 目录，跨进程、跨重启，必须得到同一 `WorkspaceId`。规范算法固定为：确认 path 是现存目录并调用 OS canonicalize；Unix 使用 canonical `OsStr` 原始字节。Windows 先把 `\\?\C:\...` 规范为 `C:/...`，把 `\\?\UNC\server\share\...` 规范为 `//server/share/...`，分隔符统一为 `/`、ASCII drive letter 统一为大写，再编码为 UTF-16LE bytes；不额外 Unicode normalize 或全路径 case-fold。除文件系统 root 外移除尾分隔符。`platform_tag` 固定为 ASCII `unix` 或 `windows`。计算 `SHA-256("minicore-workspace-id:v1\0" || platform_tag || "\0" || normalized_path_bytes)`，取完整 32-byte digest，以 lowercase base32 no-padding 编码并加 `ws1_` 前缀。session metadata 同时持久化 `workspace_id_version = 1` 和 canonical `workspace_root: PathBuf`；读取时若同一 id 对应不同 canonical root，返回 `WorkspaceIdCollision` 并 fail closed。未来算法升级必须显式迁移 catalog，不能静默重算。adapter 不得自行计算 id。规范化后的 canonical root path 也是 [BR-064](../review/system-blueprint-review-issues-round3.md) 中路径展示规则的统一落点。

**D3 session cwd 必须位于 workspace root 之下（含 root 本身）。** `NewSession { workspace_id, cwd: Option<PathBuf> }` 的 `workspace_id` 必须等于当前 open workspace，否则拒绝 `WorkspaceMismatch`；`cwd = None` 时使用 workspace root，`Some(path)` canonicalize 后必须等于 root 或位于 root 之下，否则拒绝 `CwdOutsideWorkspace`。边界判定在 canonicalize 之后进行，symlink 逃逸到 root 外即拒绝，与 `ToolSandboxView` 同一套 canonical 语义。这给了 workspace 它的第一个真实语义负载——cwd 合法域。monorepo 内多 session（各子目录各自 per-cwd 资源与 trust）天然支持；跨仓库多目录 MVP 不支持，属于 additional roots 演进。同时关闭 [BR-052](../review/system-blueprint-review-issues-round3.md) 的 new session cwd 来源空洞。

**D4 `OpenSession` 只接受当前 workspace 的 session。** 持久化 catalog 跨项目，`SessionListScope::{AllWorkspaces, Workspace}` 保留为纯 catalog 查询（不加载 runtime，供浏览与未来导入）。加载前必须复验 persisted `workspace_id`、`workspace_root` 和 `cwd`：id/root 必须匹配当前 workspace，cwd canonicalize 后仍位于当前 root 之下；不匹配分别拒绝 `SessionOutsideWorkspace` / `SessionWorkspaceCorrupt` / `CwdOutsideWorkspace`，且不得先调用 `ensure_cwd_snapshot`。"从另一个项目 resume"的正确形态是 host 重建 runtime 指向那个 root。

**D5 薄容器不承载 scope。** workspace 不承载 project trust（per-cwd）、项目资源 scope（per-cwd）、provider/auth/settings（user-global）或运行时服务生命周期。`WorkspaceServices` 名称保留：MVP 中 workspace 生命周期 == runtime 生命周期，名实相符。

**D6 已铺开的协议 workspace 坐标全部保留并定死语义。** 不删任何 `workspace_id` 坐标（避免协议返工），但每个位置的校验规则显式化：`NewSession`、`ReloadResources` / `ResourceQuery` 的 `workspace_id` 不匹配当前 workspace 拒绝 `WorkspaceMismatch`；`workspace_opened` 与 session/run 级事件外层 `workspace_id` 恒等于当前 workspace id；`workspace_open_failed` 的外层 `workspace_id = None`；补齐被引用却未定义的 `WorkspaceSummary` 和 workspace open completion events。`ApprovalGrantScope::SameToolInWorkspace` 不由本 ADR 解释为跨 cwd 授权：在 grant scope 单独定型前 MVP 不签发该 scope，避免跨越 per-cwd trust/sandbox。

## 后果

- workspace 获得一个真实语义（cwd 合法域）而不重新分配任何已收敛的 scope，BR-003 / BR-025 / BR-037 的结论保持不动。
- 变更相对当前蓝图协议是加法 + 定死语义：`NewSession` 加一个可选 cwd 字段、补一个缺失类型定义、加运行时校验与拒绝码，类型形状几乎不动。它同时消灭了一个未来会 breaking 的持久化迁移（随机 id → 路径派生 id）。
- adapter 必须本地维护 selected session/workspace 并在 dispatch 前填入 `SessionId` / `workspace_id`；all-loaded snapshot 比单 session projection 大。这些代价 MVP 接受。

## 演进方向（只留缺口，不预建）

1. **Additional roots**（需求出现时）：workspace 获得附加根（主 root 派生身份不变）。session cwd 合法域扩展为任一 root 之下；附加根必须**同时**进入 `ToolSandboxView` 的 read/write roots 和 `ResourceManager` 的 cwd 解析域（Codex 旁路缺陷的直接教训——不允许任何工具自行解释附加目录）；配置来源仍锚定主 root。D2/D3 的形状在该阶段全部保持有效。
2. **跨 session 物理隔离 / worktree**：当多个 session 在同一 cwd 并发写同一文件时，session 内的 mutation lock（session-scoped）不提供跨 session 仲裁。物理隔离方案（每个写 session 一个 git worktree，参考 Claude Code `.claude/worktrees/`）可由未来 `WorkspaceServices` 层新增的 `WorktreeManager` 承担——它是跨 session 协调者，负责按 root 派生 worktree、分配给 session、回收孤儿。**worktree 不进 workspace 薄容器，也不进单个 `SessionRuntime`**。引入后 session 的物理 cwd 会与逻辑 cwd 分裂，其与 D3 边界的交互需另立 issue/ADR；它不属于只解决单 session actor/run 死锁的 BR-047。
3. **Supervisor / 多 workspace**（progress 文档路线 B）：多 workspace 并存 = 多 runtime/worker 实例，由 supervisor 编排。薄容器不堵死这条路：workspace 不承载服务 scope，把一个 workspace 的全部状态整体搬进独立 worker 时没有跨 workspace 共享可变状态需要拆分。
4. **明确非目标**：单 runtime 内多 workspace 并存。行业五家无一先例（多容器 = 多窗口/实例），它会立即重开 BR-001/BR-002/BR-034 已关闭的 snapshot/水位/路由问题。
