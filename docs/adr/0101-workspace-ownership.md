# ADR 0101: Workspace 属于 Session

状态：Accepted
日期：2026-07-24

## 背景

MiniCore 后端需要统一解释工作目录、文件访问域、trust 与 source authorization，但“Workspace”一词在同类产品中含义分裂（Turn-bound roots、Session cwd、IDE window、远程 workspace server 等）。若只在 Session 上放裸 `cwd` 与 `roots`，canonicalization、containment、per-root trust、read/write ceiling、Prompt/Skill source authorization、Sandbox root projection、Turn snapshot 与 revocation 会重新散落到多个调用方；若建立 Runtime-global `WorkspaceService`，则在没有 shared mutable aggregate、独立 lifecycle 或 remote backend 的前提下只会引入浅转发与不必要的共享可变状态。需要一个决策明确 Workspace 的归属、边界与执行语义。

权威模型与全部字段、类型、Test Matrix 见 [`../modules/workspace.md`](../modules/workspace.md)。

## 决策

- **Workspace 是 Session-owned definition**：属于 immutable `SessionDefinition.workspace`，不是独立 entity、Runtime-global service、registry 或 lifecycle aggregate；`SessionId` 即 owner identity，当前不定义 `WorkspaceId`。
- **三种精确 identity 分工**：`WorkspaceRevision` 标识 definition 版本；`WorkspaceSnapshot` 是一次解析后的不可变有效快照；`WorkspaceFingerprint` 标识该快照的有效 identity（用于 cache/授权敏感 key，不以 canonical root 作为授权 key）。
- **root、cwd 由 Workspace 模块统一规范化和校验**：唯一 primary root 加显式 additional roots；先 canonicalize 后校验；canonical duplicate 与 overlap fail closed；cwd 必须且只能位于一个明确 root。
- **trust、filesystem capability、source authorization 三者分离**：trust 是 policy 输入不等于文件权限；文件可读不等于允许作为 Prompt 或 Skill source；Prompt source 与 Skill source 相互独立；additional root 默认仅扩展文件访问。
- **Prompt/Tool/Skill 只消费窄只读 view**：`WorkspacePromptContext`/`WorkspaceSkillContext`/`WorkspaceToolContext`/`WorkspaceAccessView` 由同一 `WorkspaceSnapshot` 原子投影；三个 Service 不自行 canonicalize roots、不查询 trust、不据文件可读性自行启用 source。
- **Turn pin 快照，restrictive update 可撤销并中断执行**：active Turn admission 时 pin 一个 `Arc<WorkspaceSnapshot>`；ordinary reload/permissive update 只影响 future Turn，不扩大 active Turn 权限；security-restricting update 通过 `WorkspaceAuthorizationLease` 撤销受影响 lease 并中断使用它的 active Turn；Tool approval 不能扩大 `WorkspaceAccessView`。
- **同根多 Session 隔离**：即使 primary root 相同，各 Session 的 cwd、additional roots、requested access、source policy、WorkspaceRevision、authorization lease 与 Snapshot 均不共享；仅共享不可变实现 cache。

## 后果

- Workspace 的价值来自统一上述不变量，而非新增全局 object registry；deletion test 成立——删除该模块会让复杂性重新散落到 Session execution、Prompt、Skill、Tool policy 与 Sandbox。
- 授权硬边界集中在 `WorkspaceAccessView`/source grant，安全属性（source injection 最小授权、restrictive update 立即撤销、跨 Session 隔离）可在单一模块内验证。
- 代价是引入 revocation lease 与 per-Turn pinning 的排序复杂度，以及“定义 request 与 effective capability 分离”带来的额外类型层次。
- worktree、remote execution 等未来能力应建立各自深模块并由 Session Workspace 引用其输出，不得塞回 Workspace；仅当出现 shared mutable aggregate、独立 lifecycle 或 remote backend identity 等真实需求时，才重新评估 `WorkspaceId`。

## 历史

本 ADR 属 V2 决策集，取代 V1 的：

- ADR 0022（Workspace 单实例薄边界）——V1 将 Workspace 建模为单实例薄边界，V2 改为 Session-owned definition 且明确不定义 WorkspaceId。
- ADR 0010（per-cwd 资源快照）——已随通用 ResourceManager 一并废弃，Workspace 不再恢复通用 ResourceManager。

两份 V1 原文见 [`../archive/v1/adr/`](../archive/v1/adr/)。
