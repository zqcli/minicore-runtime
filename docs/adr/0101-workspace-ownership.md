# ADR 0101: Workspace 属于 Session

状态：Accepted
日期：2026-07-24

## 背景

MiniCore 后端需要统一解释工作目录、文件访问域、trust 与 source authorization，但“Workspace”一词在同类产品中含义分裂（Turn-bound roots、Session cwd、IDE window、远程 workspace server 等）。若只在 Session 上放裸 `cwd` 与 `roots`，canonicalization、containment、per-root trust、read/write ceiling、Prompt/Skill source authorization、Sandbox root projection与Turn snapshot会重新散落到多个调用方；若建立 Runtime-global `WorkspaceService`，则在没有 shared mutable aggregate、独立 lifecycle 或 remote backend 的前提下只会引入浅转发与不必要的共享可变状态。需要一个决策明确 Workspace 的归属、边界与执行语义。

权威模型与全部字段、类型、Test Matrix 见 [`../modules/workspace.md`](../modules/workspace.md)。

## 决策

- **Workspace 是 Session-owned definition**：属于 immutable `SessionDefinition.workspace`，不是独立 entity、Runtime-global service、registry 或 lifecycle aggregate；`SessionId` 即 owner identity，当前不定义 `WorkspaceId`。
- **三种精确 identity 分工**：`WorkspaceRevision`标识definition版本；`WorkspaceSnapshot`是一次解析后的不可变有效快照；`WorkspaceFingerprint`标识该快照在当前Runtime内的有效identity（用于cache/授权敏感key，不以canonical root作为授权key，不跨Runtime恢复）。
- **root、cwd 由 Workspace 模块统一规范化和校验**：唯一 primary root 加显式 additional roots；先 canonicalize 后校验；canonical duplicate 与 overlap fail closed；cwd 必须且只能位于一个明确 root。
- **trust、filesystem capability、source authorization 三者分离**：trust 是 policy 输入不等于文件权限；文件可读不等于允许作为 Prompt 或 Skill source；Prompt source 与 Skill source 相互独立；additional root 默认仅扩展文件访问。
- **Prompt/Tool/Skill 只消费窄只读 view**：`WorkspacePromptContext`/`WorkspaceSkillContext`/`WorkspaceToolContext`/`WorkspaceAccessView` 由同一 `WorkspaceSnapshot` 原子投影；三个 Service 不自行 canonicalize roots、不查询 trust、不据文件可读性自行启用 source。
- **Turn pin快照，Workspace update只在Idle**：active Turn admission时pin一个`Arc<WorkspaceSnapshot>`并保持到terminal；loaded Session的Workspace definition patch在Starting/Running/Finishing时返回`SessionBusy`。authority hard restriction通过Turn-targeted`SecurityRevoked`中断执行，terminal后重新resolve，不动态撤销Snapshot或open handle；Tool approval不能扩大`WorkspaceAccessView`。完整规则见ADR 0121。
- **同根多Session隔离**：即使primary root相同，各Session的cwd、additional roots、requested access、source policy、WorkspaceRevision、Snapshot与security target generation均不共享；仅共享不可变实现cache。

## 后果

- Workspace 的价值来自统一上述不变量，而非新增全局 object registry；deletion test 成立——删除该模块会让复杂性重新散落到 Session execution、Prompt、Skill、Tool policy 与 Sandbox。
- 授权硬边界集中在`WorkspaceAccessView`/source grant，安全属性（source injection最小授权、Turn-pinned capability、跨Session隔离）可在单一模块内验证。
- 代价是Workspace definition不能在active Turn中热更新；Host需要显式`Cancel → wait settled → UpdateDefinition`。收益是删除revocation lease、open-handle动态撤权和append/revoke排序复杂度。
- worktree、remote execution 等未来能力应建立各自深模块并由 Session Workspace 引用其输出，不得塞回 Workspace；仅当出现 shared mutable aggregate、独立 lifecycle 或 remote backend identity 等真实需求时，才重新评估 `WorkspaceId`。

## 历史

本 ADR 属 V2 决策集，取代 V1 的：

- ADR 0022（Workspace 单实例薄边界）——V1 将 Workspace 建模为单实例薄边界，V2 改为 Session-owned definition 且明确不定义 WorkspaceId。
- ADR 0010（per-cwd 资源快照）——已随通用 ResourceManager 一并废弃，Workspace 不再恢复通用 ResourceManager。

两份 V1 原文见 [`../archive/v1/adr/`](../archive/v1/adr/)。

2026-07-27：[ADR 0121](0121-workspace-updates-require-idle.md)将Workspace definition update收窄为Idle-only，并以SecurityRevoked Turn interruption取代active lease revocation；本ADR的Session ownership、窄view与Turn pinning保持有效。
2026-07-28：[ADR 0122](0122-workspace-fingerprints-are-runtime-local.md)将Workspace及其view fingerprint收窄为Runtime-instance-local opaque identity；restart重新resolve，不恢复旧Snapshot、grant或fingerprint family。
