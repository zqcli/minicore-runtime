# Workspace 模型设计稿

日期：2026-07-12

状态：Accepted（已固化为 [ADR 0022](../adr/0022-workspace-is-single-instance-thin-boundary.md) 并同步各权威文档，BR-036 已关闭；本文作为设计推导背景材料保留）

关联：[BR-036](../review/system-blueprint-review-issues-round2.md)、[BR-052](../review/system-blueprint-review-issues-round3.md)、[BR-064](../review/system-blueprint-review-issues-round3.md)、[ADR 0010](../adr/0010-use-per-cwd-resource-snapshots-for-multi-session-runtime.md)、[ADR 0020](../adr/0020-agent-runtime-has-no-current-session.md)

## 1. 问题

BR-036 指出：协议表面已按多 workspace 形状铺开（`ReloadResources { workspace_id }`、`SessionListScope::AllWorkspaces / Workspace{id}`、外层 `Event.workspace_id`、resource snapshot key 含 `workspace_id`、`NewSession { workspace_id }`），但 `RuntimeSnapshot.workspace` 是单个 `Option`，Workspace Lifecycle 状态机是单实例的（`NoWorkspace → Open → …`），`OpenWorkspace` 被第二次调用是替换、并存还是拒绝没有任何说明。

本轮设计前的盘点发现问题比 BR-036 记录的更深，共五个必须一起回答的问题：

- **Q1**：`OpenWorkspace` 重复调用的语义（BR-036 原问题）。
- **Q2**：session 的固定 cwd 与 workspace root 的关系——cwd 必须在 root 之下，还是任意目录？现有文档的示例图（`SessionRuntime A { cwd: repo-a }` 与 `SessionRuntime C { cwd: repo-b }` 并存于同一 workspace）暗示多仓库，但从未声明边界。
- **Q3**：`WorkspaceId` 的身份来源。session metadata 持久化了 `{ workspace_id, cwd }`，`SessionListScope::CurrentWorkspace` 要在重启后仍能把历史会话归到当前项目——如果 `WorkspaceId` 是每次 `OpenWorkspace` 随机分配的，重启后所有历史 session 的 workspace 归属全部失效。这是当前蓝图的一个隐藏缺陷，任何 Q1 的答案都绕不开它。
- **Q4**：持久化 catalog 中"非当前 workspace"的 session（`AllWorkspaces` 能列出它们）被 `OpenSession` 时的行为。
- **Q5**：`WorkspaceServices` 的名实关系——它的成员（EventBus、SessionManager、CommandManager、ResourceManager、user-global provider/auth、ModelGateway、diagnostics）没有一个真正依赖 workspace 身份。

## 2. 现状盘点：workspace 是一个没有语义负载的路由维度

逐项核对当前蓝图中每一类 scope 的真实归属：

| Scope | 实际归属 | 权威出处 |
| --- | --- | --- |
| 项目资源（skills、templates、context files） | per-cwd（`CwdResourceSnapshot.local`） | resource-manager.md、ADR 0010 |
| runtime/global 资源 | user-global（`RuntimeResourceSnapshot`） | resource-manager.md |
| project trust | per-cwd（"未信任 cwd 不加载项目级资源"；trust state 是构建 `CwdResourceSnapshot.local` 的输入） | resource-manager.md 项目信任节 |
| provider / auth / custom provider / settings | user-global/runtime-global | model-gateway.md、agent-runtime.md、BR-025 |
| 工具沙箱边界 | per-session cwd（`ToolSandboxView`） | tools.md |
| 运行时服务生命周期 | 与 runtime/host 同生命周期 | agent-runtime.md、BR-002/BR-044 约束 |
| 客户端 selection | adapter-local，core 不存在 | ADR 0020 |

结论：**当前设计中没有任何安全边界、资源边界或服务边界绑定在 workspace 层。** workspace 只出现在三种位置：命令/查询/事件的路由坐标、resource snapshot store 的 key 前缀、session metadata 的归属字段。这不是缺陷而是机会——它意味着 workspace 可以被定义得非常薄，而不需要重新分配任何已收敛的 scope（BR-003/BR-025/BR-037 的结论全部保持不动）。

## 3. 同类项目调研

| 工具 | "容器"概念 | 身份 | trust/安全边界 | 多目录 | 打开第二个项目 |
| --- | --- | --- | --- | --- | --- |
| VS Code | workspace（单 folder 或 `.code-workspace` multi-root） | folder 路径 / workspace 文件 | **per-folder**；multi-root 中任一 folder untrusted 则整窗 Restricted Mode（取最严格） | multi-root：往当前容器加 folder | 新窗口（= 新实例） |
| Claude Code | project = 启动目录 | **目录路径**（per-project state：trust、allowed tools 以路径为 key 存于 `~/.claude.json`；trust 以 git repo root 为 key） | **per-directory**；trust gate 的对象是"仓库可以提交的东西"（project settings 的 allow 规则、`.mcp.json`） | `--add-dir` / `permissions.additionalDirectories`：只扩文件访问，**不改变配置/资源加载来源**（仍锚定主目录） | 新进程；后台 session 懒创建独立 worktree（`.claude/worktrees/`） |
| Codex CLI | working root = `--cd` | 目录路径（per-invocation 固定） | sandbox `workspace-write` + `writable_roots`；已知 `--add-dir` 作为写围栏存在旁路缺陷（apply_patch 绕过，issue #24214） | `--add-dir` / config `writable_roots`：往沙箱加可写根 | 新 invocation/thread |
| Zed | project = worktrees 集合（每个根文件夹一个内部 worktree） | 各根文件夹路径 | per-worktree | 窗口内 add folder | 新窗口 |
| JetBrains | project 容器（多 content roots / modules；支持 attach 第二个项目到同窗口） | project 目录 | per-project trust | content roots / attach | 新窗口或 attach |

**行业共识**（五家全部一致）：

1. **身份 = 目录路径**。没有任何一家用随机 id 做项目身份；per-project 状态（trust、settings、session 历史）全部以规范化路径为 key。
2. **trust/安全边界绑定目录层，不绑定容器层**。VS Code per-folder（容器层取最严格聚合）、Claude Code per-directory、Codex per-writable-root。MiniCore 当前 per-cwd trust 与此一致，应保持。
3. **多目录 = 往当前容器显式加根，不是多容器并存**。`--add-dir`、multi-root、attach 全部是这个形状；且配置/资源来源仍锚定主 root（Claude Code 明确 additionalDirectories 只授文件访问）。
4. **多容器并存 = 多窗口/多实例**。没有任何一家在一个窗口/进程/invocation 里同时打开两个互不相关的容器。
5. Codex 的旁路缺陷是一个直接教训：**附加目录必须进入 sandbox 的单一 source of truth（`ToolSandboxView`），不能由某个工具各自解释**。

**分歧点**：容器是否支持多根（VS Code/Zed/JetBrains 支持，Claude Code/Codex 用 add-dir 弱化形式支持）；trust 聚合方向（VS Code 取最严格 vs Claude Code 按主 repo root 判一次）。

## 4. 设计原则

从现状盘点和调研提炼，作为本设计的裁决依据：

- **P1 身份即路径**：workspace 身份从 canonical root path 确定性派生，跨进程稳定。
- **P2 薄容器**：workspace 不承载 trust、资源、provider/auth、服务生命周期；这些 scope 维持现有归属（user-global / per-cwd）。workspace 只承载：root 边界、session 归属分组、cwd 合法域、UI 展示锚点。
- **P3 一个 runtime 一个 workspace**：多 workspace 并存 = 多 runtime 实例（对应行业的"多窗口"）。单 runtime 内多 workspace 不是演进目标。
- **P4 多目录是加根不是加容器**：未来扩展方向是 workspace 内 additional roots（对齐 --add-dir / multi-root），且附加根必须消化进 `ToolSandboxView` 与 `ResourceManager` 的既有 source of truth。
- **P5 形态已铺开的协议字段全部保留并定死语义**：不删 `workspace_id` 坐标（避免协议返工），但每个出现位置的校验规则必须显式。

## 5. 提案

### D1：单 workspace 实例；重复 `OpenWorkspace` 幂等或拒绝

- `OpenWorkspace { path }` 只在 `NoWorkspace` 状态创建 workspace，先进入 `Opening`；Ack 只表示 admission，成功/失败分别以 `workspace_opened` / `workspace_open_failed` 完成。
- `Opening` 中同 canonical root 重复调用幂等共享初始化，异根拒绝 `WorkspaceOpening`；`Open` 状态下同根幂等、异根拒绝 `WorkspaceAlreadyOpen`。
- 不引入 `CloseWorkspace`，不做 teardown-and-replace。"换项目"的实现方式是 host 丢弃整个 `AgentRuntime` 实例并重建。

否决 teardown 替换的理由：替换语义必须连带定义全部 loaded session 的强制关闭、in-flight run 的 abort 与 writer commit 竞态、pending approval 清理、事件水位与 snapshot 的跨 workspace 连续性——这是一大块状态机复杂度，唯一收益是省去一次实例重建；而 MVP 中 host 与 runtime 同生命周期（BR-002/BR-044 的既有约束），重建实例恰好是最干净、与 VS Code"换文件夹 = 换窗口"同构的路径。等未来出现常驻 daemon（progress 文档路线 B），替换/关闭语义再随 supervisor 设计一起定义。

### D2：`WorkspaceId` 从 canonical root path 确定性派生

契约（这是 D1-D4 全部成立的前提，解决 Q3）：

- 同一物理 root 目录，跨进程、跨重启，必须得到同一 `WorkspaceId`。
- 派生规则使用 ADR 0022 的规范 `WorkspaceIdV1`：canonical platform bytes + versioned namespace，完整 SHA-256 digest，lowercase base32 no-padding，并以 `ws1_` 为前缀；不得自行选择 hash、截断或编码。
- session metadata 持久化 `{ workspace_id, workspace_id_version: 1, workspace_root: PathBuf, cwd: PathBuf }`；id/root 冲突 fail closed，算法升级走显式 catalog migration。
- `WorkspaceId` 仍是 opaque 类型；派生算法是 runtime 内部契约，adapter 不得自行计算。

这与 Claude Code 的 per-project state by path、`~/.claude/projects/<path-slug>` 会话组织同构。

### D3：session cwd 必须位于 workspace root 之下（含 root 本身）

- `NewSession` 补齐 cwd 参数（同时关闭 BR-052 的 (c) 项）：

```rust
NewSession { workspace_id: WorkspaceId, cwd: Option<PathBuf> }
// workspace_id       → 必须等于当前 open workspace
// cwd = None       → session cwd = workspace root（默认，预期覆盖绝大多数使用）
// cwd = Some(path) → canonicalize 后必须等于 root 或位于 root 之下，否则拒绝 CwdOutsideWorkspace
```

- 边界判定在 canonicalize 之后进行，symlink 指向 root 外即拒绝（与 `ToolSandboxView` 的路径规则同一套 canonical 语义）。
- `CreateSession` options（session-manager.md 中悬空的类型）随之定型：携带已校验的 `{ workspace_id, workspace_root, cwd }`。
- 效果：workspace 获得它的第一个真实语义负载——**cwd 合法域**。monorepo 内多 session（`repo/frontend`、`repo/backend` 各一个 session，各自 per-cwd 资源与 trust）天然支持；跨仓库多目录（`repo-a` 与 `repo-b` 并存）MVP 不支持，属于 D7 的 additional roots 演进。agent-runtime.md 与 progress 文档中 `repo-a`/`repo-b` 并存的示例图需要修订（改为 root 下两个子目录，或标注为 additional roots 落地后的形态）。
- `ImportSession.cwd_override`（后续命令）沿用同一边界校验。

### D4：`OpenSession` 只接受当前 workspace 的 session

- 持久化 catalog 是跨项目的：`SessionListScope::AllWorkspaces` / `Workspace{id}` 保留，作为纯 catalog 查询（不加载 runtime），供浏览与未来导入。
- `OpenSession` 在加载资源前复验 persisted workspace id/root/cwd；跨 workspace、metadata root 不一致或 cwd 越界均 fail closed。
- "从另一个项目 resume"的正确形态是 host 重建 runtime 指向那个 root（D1/P3），不是跨容器加载。

### D5：workspace 的领域定义（薄容器）

采纳后写入 `CONTEXT.md` 的词条草案：

> **工作区（Workspace）**：
> 一次 runtime 生命周期内打开的项目上下文容器，由 `OpenWorkspace { path }` 的 canonical root path 定义身份（`WorkspaceId` 确定性派生）。它界定 session cwd 的合法域（root 及其之下）、充当持久化会话目录的分组维度，并投影 `WorkspaceSummary` 供 UI 展示。MVP 单实例、单根；它不承载 project trust（per-cwd）、项目资源 scope（per-cwd）、provider/auth/settings（user-global）或运行时服务生命周期。
> _避免_：VS Code 式多根容器、多 workspace 并存、trust 边界、资源 scope owner、UI 当前项目状态

`WorkspaceServices` 名称保留：MVP 中 workspace 生命周期 == runtime 生命周期，名实相符。但在 agent-runtime.md 中加一句：未来若出现单 runtime 多 workspace（当前非目标）或 supervisor 拆分，需先把它拆为 runtime-level 与 workspace-level 两层；MVP 不预建该拆分。

### D6：协议表面逐项处置

| 位置 | 处置 | 语义 |
| --- | --- | --- |
| `OpenWorkspace { path }` | 保留 | D1；command accepted 后进入 `Opening`，canonicalize/初始化失败以 `workspace_open_failed` 完成 |
| `Event.workspace_id` | 保留 | `workspace_opened` 与 session/run 级事件必填；`workspace_open_failed` 为 `None` |
| `NewSession` | **修改** | 增加 `cwd: Option<PathBuf>`（D3） |
| `OpenSession` | 保留 | 增加 D4 校验 |
| `ReloadResources { workspace_id, cwd }` | 保留 | `workspace_id` ≠ 当前 → 拒绝 `WorkspaceMismatch`；cwd 必须是 root 之下已有 snapshot 或 loaded session 的 cwd |
| `ResourceQuery::*{ workspace_id, cwd }` | 保留 | 同上校验 |
| `SessionListScope::{CurrentWorkspace, Workspace, AllWorkspaces, Recent}` | 保留 | 纯 catalog 过滤；`CurrentWorkspace` 依赖 D2 的稳定 id 在重启后命中历史会话 |
| `ImportSession { workspace_id, input_path, cwd_override }` | 保留（后续命令） | `cwd_override` 受 D3 边界校验 |
| `RuntimeSnapshot.workspace: Option<WorkspaceSummary>` | 保留单个 | `None` 仅在 `NoWorkspace` 状态 |
| `WorkspaceSummary` | **补定义** | `{ workspace_id: WorkspaceId, root_path: PathBuf, display_name: String }`（display_name 默认取 root 目录名） |
| `ApprovalGrantScope::SameToolInWorkspace` | 暂不启用 | grant scope 单独定型前 MVP 不签发，不能借 workspace 名义跨越 per-cwd trust/sandbox |
| Workspace Lifecycle 状态机（events 文档） | 保留单实例 | 增加 `Opening` 并以 `workspace_opened` / `workspace_open_failed` 表达完成 |

### D7：演进路径（只留缺口，不预建）

1. **MVP（本提案）**：单 workspace、单根。
2. **Additional roots**（需求出现时）：workspace 获得 `roots: Vec<PathBuf>`（主 root 派生身份不变 + 附加根）。协议加 `AddWorkspaceRoot` 类命令或 `OpenWorkspace.additional_roots`；session cwd 合法域扩展为任一 root 之下；**附加根必须同时进入 `ToolSandboxView` 的 write/read roots 和 `ResourceManager` 的 cwd 解析域**（Codex apply_patch 旁路缺陷的直接教训——不允许任何工具自行解释附加目录）；资源/trust 仍 per-cwd，配置来源仍锚定主 root（对齐 Claude Code additionalDirectories 语义）。本提案的 D2（身份从主 root 派生）、D3（边界校验形状）在该阶段全部保持有效。
3. **Supervisor / 多 workspace**（progress 文档路线 B）：多 workspace 并存 = 多 runtime/worker 实例，由 supervisor 编排；`RuntimeSnapshot` 升级为 supervisor 级 projection。薄容器模型不堵死这条路：因为 workspace 不承载服务 scope，把一个 workspace 的全部状态（loaded sessions + cwd snapshots + 事件流）整体搬进独立 worker 时，没有跨 workspace 共享可变状态需要拆分。
4. **明确非目标**：单 runtime 内多 workspace 并存。行业五家无一先例（多容器 = 多窗口/实例）；它会立即重新打开 BR-001/BR-002/BR-034 已关闭的 snapshot/水位/路由问题。

### D8：必测项

- 同一 canonical root 重复 `OpenWorkspace` 幂等（不重载资源、loaded sessions 不受影响）；不同 root 拒绝 `WorkspaceAlreadyOpen`。
- `WorkspaceIdV1` golden vectors：同一 canonical root 两次进程启动得到同一 id；相对/绝对、尾分隔符和 Windows verbatim 前缀按规范归一；id/root collision fail closed。
- `NewSession` cwd 边界：`None` → root；root 下嵌套子目录接受；root 外拒绝 `CwdOutsideWorkspace`；经 symlink 逃逸到 root 外拒绝。
- `OpenSession` 非当前 workspace 的 session 拒绝 `SessionOutsideWorkspace`。
- `SessionQuery::List` 三种 scope 过滤正确，且重启后 `CurrentWorkspace` 命中上次同 root 创建的历史会话（依赖 D2）。
- `ReloadResources` / `ResourceQuery` 的 `workspace_id` 不匹配拒绝 `WorkspaceMismatch`。
- `workspace_opened` 与 session/run 级事件外层 `workspace_id` 必填；`workspace_open_failed` 外层 `workspace_id = None`。

## 6. 备选方案与否决理由

| 备选 | 否决理由 |
| --- | --- |
| **A. 删除 workspace 维度**（session 只有 cwd） | 最诚实反映"空壳维度"现状，但协议返工面大（Event 坐标、ResourceQuery、session metadata、snapshot store key 全动）；且"打开一个项目"作为产品锚点真实存在（UI 需要 root 与分组维度）；丢失 additional roots / supervisor 的自然挂点。薄容器以更低成本获得同样的诚实度。 |
| **B. teardown-and-replace** | 见 D1 否决理由：一大块关闭/abort/水位状态机复杂度，唯一收益是省一次实例重建；且与 BR-002/BR-044 的同生命周期假设无谓耦合。 |
| **C. 现在就做 multi-root** | 无当前产品需求；写沙箱与资源域的扩展面（Codex 旁路教训）值得单独一轮设计；D2/D3 的形状已保证它是纯增量演进。 |
| **D. 单 runtime 多 workspace 并存** | 行业无先例；立即重开 BR-001/BR-002/BR-034 已关闭的问题；与 ADR 0020 的"多 session 已由显式 SessionId 路由"叠加后，workspace 维度的并存没有换来任何隔离收益（隔离已由 per-cwd 完成）。 |

## 7. 采纳后的文档修订清单

1. 新增 ADR 0022（本提案 D1-D6 的决策记录）。
2. `CONTEXT.md`：新增"工作区（Workspace）"词条（D5）；`RuntimeSnapshot` 词条不变。
3. `agent-runtime-protocol.md`：`NewSession` 加 `cwd` 参数；补 `WorkspaceSummary` 定义；`OpenWorkspace` / `OpenSession` / `ReloadResources` 补校验语义与 `WorkspaceAlreadyOpen` / `CwdOutsideWorkspace` / `SessionOutsideWorkspace` / `WorkspaceMismatch` 结果。
4. `agent-runtime.md`：运行时服务节补 workspace 生命周期语义（D1）与 `WorkspaceServices` 名实说明（D5）；示例图 `repo-a`/`repo-b` 修订为 root 下子目录。
5. `agent-runtime-events.md`：Workspace Lifecycle 补 `Open` 状态的幂等/拒绝分支；必测项并入 Test Matrix（D8）。
6. `session-manager.md`：`CreateSession` options 定型（D3）；session metadata 补 `workspace_root` 字段（D2）；`SessionListFilter` 形状统一时（BR-059）一并落 scope 语义。
7. `docs/review/system-blueprint-review-issues-round2.md`：BR-036 标记 Resolved，处理记录指向 ADR 0022。
8. `docs/review/system-blueprint-review-issues-round3.md`：BR-052 的 (c) 项、BR-064 的 canonical path 规则部分标注由本设计关闭。

## 8. 调研来源

- Claude Code settings / scopes / trust：https://code.claude.com/docs/en/settings
- Codex CLI sandbox / writable roots：https://developers.openai.com/codex/concepts/sandboxing 、https://developers.openai.com/codex/cli/reference 、https://developers.openai.com/codex/config-reference
- Codex `--add-dir` 写围栏旁路：https://github.com/openai/codex/issues/24214
- VS Code multi-root workspace 与 Workspace Trust、Zed worktrees、JetBrains content roots / attach：各官方文档（稳定的长期设计，正文表格已述）。
