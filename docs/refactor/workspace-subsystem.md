# Workspace 子系统架构设计

状态：目标架构已确定；实现细节待补充
日期：2026-07-16

## 目的

本文从零定义 MiniCore 的 Workspace 子系统，回答以下问题：

- MiniCore 后端是否需要 Workspace；
- Workspace 是独立 entity、Runtime-global Service，还是 Session-owned definition；
- primary root、additional roots 和 cwd 如何建模；
- trust、文件访问能力和 Prompt/Skill source authorization 如何分离；
- PromptService、ToolService 和 SkillService 消费哪些窄 view；
- Workspace 更新如何影响 active Turn 和 future Turn；
- 多个 Session 指向同一目录时是否共享 Workspace 状态；
- `WorkspaceId` 是否必要。

本文定义的是独立目标模型，不以旧 `OpenWorkspace` protocol、旧 `ResourceManager`、旧 Workspace ADR 或 IDE Workspace 形态为约束。

## 决策摘要

MiniCore 需要 Workspace **模块**，但不需要 Runtime-global `WorkspaceService`、Workspace registry 或独立 Workspace lifecycle aggregate。

目标关系为：

```text
Session
└─ Workspace                 // Session-owned persisted definition

Session execution state
└─ Arc<WorkspaceSnapshot>    // 当前已解析的不可变有效快照

Turn execution context
└─ Arc<WorkspaceSnapshot>    // 本 Turn pin 的同一快照
   ├─ WorkspacePromptContext → PromptService
   ├─ WorkspaceSkillContext  → SkillService
   ├─ WorkspaceToolContext   → ToolService
   └─ WorkspaceAccessView    → Tool policy / Sandbox
```

已经确定：

- `Workspace` 属于 Session；
- `Workspace` 是 Session-owned definition，不是独立 entity；
- 当前不定义 `WorkspaceId`；
- `SessionId` 是 Workspace definition 的 owner identity；
- `WorkspaceRevision` 标识 Session Workspace definition 的版本；
- `WorkspaceFingerprint` 标识一次解析后的有效 Workspace 快照；
- primary root、additional roots 和 cwd 由 Workspace 模块统一规范化和校验；
- trust 是 policy 输入，不等于文件权限；
- 文件可读不等于允许作为 Prompt 或 Skill source；
- Prompt source authorization 与 Skill source authorization 相互独立；
- additional root 默认只扩展文件访问，不自动成为 Prompt 或 Skill source；
- Prompt、Tool、Skill 不自行 canonicalize roots，也不自行查询 trust；
- Turn 开始时 pin 一个不可变 `Arc<WorkspaceSnapshot>`；
- ordinary reload 只影响 future Turn；
- security-restricting update 撤销 active lease，并要求中断受影响 Turn；
- 多个 Session 即使使用相同目录，也不共享 mutable Workspace state；
- Workspace 不拥有 Prompt/Skill discovery、Tool registry、Sandbox、conversation、Model、provider、VCS、indexing、UI window 或 remote workspace server。

## 同类项目结论

“Workspace”在同类产品中至少表示五种不同概念，不能因为名称相同就直接照搬：

| 项目 | 实际核心形态 | 对 MiniCore 的启示 |
| --- | --- | --- |
| Codex | Thread/Turn 持有 cwd、workspace roots 和 environment snapshot；核心没有 IDE 式 Workspace aggregate | 把执行目录和授权 roots 固定在 Turn 边界即可，不需要大型 Workspace owner |
| pi | Session、ResourceLoader、SettingsManager 和工具围绕固定 cwd 工作 | 小型嵌入式 Agent runtime 可以直接以 Session cwd 为中心 |
| Claude Code | working directory 与 additional directories 定义文件访问域；additional directory 通常不自动成为完整配置根 | 文件访问和 instruction/config source 必须分开授权 |
| VS Code / Cursor | Workspace 还是窗口、设置、索引、任务、扩展和多根项目的 UI 容器 | 这些前端职责不应进入 MiniCore 后端 |
| Grok Build | Workspace crate 同时承担远程 server、FS/VCS RPC、session、toolset rebuild、上传恢复和 graceful drain | 其复杂度来自远程部署；MiniCore 只借鉴统一 cwd/trust/tool access snapshot，不复制容器规模 |

跨项目稳定共识：

1. cwd 和文件访问 roots 必须由同一模块解释；
2. additional roots 必须进入真实文件访问限制，不能只影响资源发现；
3. 可读目录不应自动成为隐藏 Prompt、Skill 或 executable config 来源；
4. active execution 应捕获稳定快照；
5. IDE window、indexing、VCS 和 extension host 不是通用 Agent backend 的 Workspace 职责。

## 为什么保留 Workspace 概念

如果只在 Session 上放置裸 `cwd: PathBuf` 和 `roots: Vec<PathBuf>`，以下复杂性会重新散落到多个调用方：

- path canonicalization；
- symlink、junction、UNC 和大小写语义；
- cwd containment；
- additional root 去重和重叠判断；
- per-root trust；
- effective read/write ceiling；
- Prompt/Skill source authorization；
- Tool sandbox root projection；
- Turn snapshot fingerprint；
- reload 与 security revocation。

因此 Workspace 模块通过 deletion test：删除它后，复杂性会重新出现在 Session execution、Prompt source、Skill source、Tool policy 和 Sandbox 中。

Workspace 模块的价值来自统一这些不变量，而不是拥有一个新的全局 object registry。

## 为什么不建立 WorkspaceService

当前目标模型没有以下真实需求：

```text
Workspace 独立 create/open/close
Workspace 独立 CRUD 和 lookup
多个 Session 共享同一个 mutable Workspace aggregate
单 Runtime 内维护 current Workspace registry
remote Workspace reconnect
Workspace-level warm eviction
Workspace 自己拥有 conversation、resource 或 service lifecycle
```

在这些需求不存在时，`WorkspaceService` 只会成为：

```text
Session owner
→ WorkspaceService
→ Session-owned Workspace state
```

这是一层浅转发，并会引入不必要的共享可变状态。

MiniCore 只保留一个小型 `WorkspaceResolver` 模块。它隐藏解析复杂性，但不拥有 registry、current Workspace 或独立 lifecycle。

## 领域定义

### Workspace

`Workspace` 是 Session 持久化的工作目录与授权请求定义：

```rust
pub struct Workspace {
    pub revision: WorkspaceRevision,
    pub primary_root: WorkspaceRootSpec,
    pub additional_roots: Vec<WorkspaceRootSpec>,
    pub cwd: WorkspaceCwdSpec,
}
```

它表达 Session **请求使用什么目录和 source**，不直接授予权限。

```rust
pub struct WorkspaceRootSpec {
    pub key: WorkspaceRootKey,
    pub path: PathBuf,
    pub requested_access: RequestedFilesystemAccess,
    pub sources: WorkspaceSourcePolicy,
}

pub enum RequestedFilesystemAccess {
    ReadOnly,
    ReadWrite,
}

pub struct WorkspaceSourcePolicy {
    pub prompt: bool,
    pub skill: bool,
}
```

`WorkspaceRootKey` 只在一个 Session Workspace definition 内稳定，用于 cwd 引用、diagnostics 和 definition update。它不是全局项目 identity，也不是授权 token。

cwd 使用 root-relative 表达，避免使用进程 ambient cwd：

```rust
pub struct WorkspaceCwdSpec {
    pub root: WorkspaceRootKey,
    pub relative_path: WorkspaceRelativePath,
}
```

`WorkspaceRelativePath` 必须满足：

- 不是绝对路径；
- 不含平台 path prefix；
- 规范化后不含 `..` 逃逸；
- 空路径表示 root 本身。

默认 Workspace 可以通过 helper 创建：

```rust
Workspace::local(primary_root)
```

建议默认请求：

```text
primary root
→ cwd = root
→ requested access = ReadWrite
→ requested Prompt source = true
→ requested Skill source = true

additional root
→ requested access 由调用方明确指定
→ Prompt/Skill source 默认 false
```

这些只是 request。最终 capability 必须经过 Workspace authority 收紧。

### WorkspaceRevision

`WorkspaceRevision` 标识 Session-owned definition 的版本：

```text
修改 primary root
修改 additional roots
修改 cwd
修改 requested access
修改 Prompt/Skill source request
→ WorkspaceRevision 改变
```

以下变化不修改 WorkspaceRevision：

```text
trust decision 变化
managed policy 变化
目录暂时 unavailable
Prompt/Skill 文件内容变化
cache 或 load state 变化
```

后者通过新的 WorkspaceSnapshot、authority revision 或对应子系统自己的 definition/content identity 表达。

## Root 与 cwd 规则

### Primary Root

每个 Workspace 恰好有一个 primary root。

primary root 的语义仅包括：

- 默认 Workspace 创建锚点；
- 默认 cwd；
- UI 展示和 Session catalog 分组锚点；
- project-local Prompt/Skill source 的默认候选 root。

primary root 不自动表示：

- trusted；
- read/write 已授权；
- Tool 可以执行任意命令；
- Prompt/Skill source 已授权；
- Workspace 有独立 entity identity。

### Additional Roots

additional roots 用于扩展当前 Session 的文件访问域。

基础规则：

- 必须显式声明；
- 默认只请求文件访问；
- 不自动成为 Prompt source；
- 不自动成为 Skill source；
- 不改变 primary root；
- 不形成新的 Workspace；
- future Turn 使用更新后的 root set；
- Tool sandbox 必须看到与 Workspace 模块相同的 effective root grants。

Claude Code 的 additional directories 证明“扩展文件访问”和“新增配置根”是不同语义。MiniCore 把这种区别直接编码进类型，而不是依赖约定。

### Canonicalization

WorkspaceResolver 对所有 root 执行平台正确的 canonicalization：

```text
输入 path
→ 验证存在且为目录
→ 平台 canonicalization
→ symlink / junction 解析
→ canonical root
→ duplicate / overlap 检查
→ cwd containment 检查
```

基础约束：

- containment 不使用字符串前缀；
- canonical duplicate roots 拒绝；
- 当前版本拒绝任意 root overlap；
- symlink 指向另一个 declared root 也按 canonical duplicate/overlap 处理；
- cwd 必须位于且只位于一个 root 内；
- cwd 的 `WorkspaceRootKey` 必须与实际 containing root 一致。

拒绝 overlapping roots 是有意的保守选择。它避免同一路径同时命中不同 trust、read/write 和 source policy。未来只有在真实需求出现时，才考虑“最具体 root 优先”等更复杂规则。

### 不存在的写入目标

一次 Workspace resolve 不能消除所有 TOCTOU 风险，也不能 canonicalize 尚不存在的最终写入文件。

因此：

- WorkspaceSnapshot 表达 Turn 的授权上限；
- Tool requirements 在具体调用时规范化目标路径；
- WorkspaceAccessView 对 nearest existing ancestor 和目标相对路径做检查；
- ToolSandbox 在真实 open/create/rename 时继续执行强制限制；
- approval 不能扩大 WorkspaceAccessView 的上限。

## Trust、Capability 与 Source Authorization

这三个概念必须分开。

### Trust

trust 是 Workspace authority 对某个 canonical root 的判定：

```rust
pub struct WorkspaceRootTrust {
    pub level: WorkspaceTrustLevel,
    pub revision: WorkspaceTrustRevision,
}

pub enum WorkspaceTrustLevel {
    Trusted,
    Restricted,
    Untrusted,
}
```

trust 不是 capability：

```text
Trusted
≠ 自动 ReadWrite
≠ 自动 Tool execution
≠ 自动 Prompt source
≠ 自动 Skill source
```

Workspace 不拥有全局 trust history。持久化用户选择、managed policy 和 headless policy 属于 `WorkspaceAuthority` adapter 或其背后的 store。

WorkspaceSnapshot 只保存某次解析得到的 per-root trust verdict 和 policy revision。

### Filesystem Capability

最终文件访问能力是请求与 authority ceiling 的交集：

```text
requested filesystem access
∩ host / managed policy ceiling
∩ platform / sandbox enforcement capability
→ effective filesystem grant
```

最小有效类型：

```rust
pub enum WorkspaceFilesystemGrant {
    None,
    ReadOnly,
    ReadWrite,
}
```

Workspace 只定义文件访问上限，不拥有：

- network permission；
- process permission；
- environment variable permission；
- Tool approval；
- Tool-specific policy；
- Tool executor。

这些仍属于 Tool 子系统。Tool 最终权限必须满足：

```text
ToolRequirements
∩ ToolPolicy
∩ approval/grant
∩ WorkspaceAccessView
∩ ToolSandbox capability
```

### Source Authorization

项目文件可以被当作普通数据读取，并不意味着可以作为隐藏模型指令或 Skill definition 加载。

有效 source authorization 分别计算：

```text
requested Prompt source
∩ authority Prompt source ceiling
∩ readable filesystem grant
→ effective Prompt source grant

requested Skill source
∩ authority Skill source ceiling
∩ readable filesystem grant
→ effective Skill source grant
```

基础不变量：

- filesystem Read 不推出 Prompt source；
- filesystem Read 不推出 Skill source；
- Prompt source 不推出 Skill source；
- Skill source 不推出 Prompt source；
- Tool sandbox root 不推出任何 source authorization；
- additional root 默认没有 source grant；
- PromptService 和 SkillService 不查询 trust，只消费最终 source grant。

## WorkspaceResolver

WorkspaceResolver 是 Workspace 模块的唯一解析入口：

```rust
pub struct WorkspaceResolver {
    paths: Arc<dyn WorkspacePathAdapter>,
    authority: Arc<dyn WorkspaceAuthority>,
}

impl WorkspaceResolver {
    pub async fn resolve(
        &self,
        session_id: SessionId,
        workspace: &Workspace,
    ) -> Result<ResolvedWorkspace, WorkspaceResolveError>;
}
```

WorkspaceResolver 完成：

```text
Workspace definition
→ root/cwd 规范化
→ duplicate/overlap/containment 校验
→ 一致的 authority decision
→ effective filesystem grants
→ effective Prompt source grants
→ effective Skill source grants
→ authorization lease
→ view fingerprints
→ immutable WorkspaceSnapshot
```

WorkspaceResolver 不提供：

```text
open
close
register
lookup
current
attach
reload
watch
invalidate
```

reload 只是 Session execution owner 用当前 Workspace definition 再次调用 `resolve()`。

### 内部 Seams

```rust
pub trait WorkspacePathAdapter: Send + Sync {
    async fn canonicalize_directory(
        &self,
        path: &Path,
    ) -> Result<CanonicalWorkspacePath, WorkspacePathError>;
}

pub trait WorkspaceAuthority: Send + Sync {
    async fn authorize(
        &self,
        request: WorkspaceAuthorityRequest,
    ) -> Result<WorkspaceAuthorityDecision, WorkspaceAuthorityError>;
}
```

真实 adapter 至少包括：

```text
WorkspacePathAdapter
├─ OS filesystem adapter
└─ deterministic test filesystem adapter

WorkspaceAuthority
├─ interactive desktop host policy adapter
├─ managed/headless policy adapter
└─ deterministic test adapter
```

当前不引入通用 remote VFS interface。只有出现第二个真实 production backend 时，才把 local path locator 抽象为 local/remote Workspace backend seam。

## WorkspaceSnapshot

WorkspaceSnapshot 是某个 Session 当前有效 Workspace 的不可变解析结果：

```rust
pub struct WorkspaceSnapshot {
    session_id: SessionId,
    definition_revision: WorkspaceRevision,
    roots: Arc<[ResolvedWorkspaceRoot]>,
    cwd: ResolvedWorkspaceCwd,
    authorization: WorkspaceAuthorizationLease,
    diagnostics: Arc<[WorkspaceDiagnostic]>,
    fingerprint: WorkspaceFingerprint,
}
```

```rust
pub struct ResolvedWorkspaceRoot {
    pub key: WorkspaceRootKey,
    pub role: WorkspaceRootRole,
    pub canonical_path: CanonicalWorkspacePath,
    pub trust: WorkspaceRootTrust,
    pub filesystem: WorkspaceFilesystemGrant,
    pub prompt_source: bool,
    pub skill_source: bool,
}

pub enum WorkspaceRootRole {
    Primary,
    Additional,
}
```

Snapshot 的数据部分不原地修改。authority 更新、reload 或 root 变化产生新的 Snapshot。

```rust
impl WorkspaceSnapshot {
    pub fn prompt_context(&self) -> WorkspacePromptContext;
    pub fn skill_context(&self) -> WorkspaceSkillContext;
    pub fn tool_context(&self) -> WorkspaceToolContext;
    pub fn access_view(&self) -> WorkspaceAccessView;
    pub fn summary(&self) -> WorkspaceSummary;
    pub fn fingerprint(&self) -> &WorkspaceFingerprint;
}
```

这些投影的构造器是私有的，调用方不能用任意 paths 自行构造“已授权 Workspace context”。

## 窄只读 View

### WorkspacePromptContext

```rust
pub struct WorkspacePromptContext {
    session_id: SessionId,
    cwd: CanonicalWorkspacePath,
    primary_root: CanonicalWorkspacePath,
    source_roots: Arc<[AuthorizedPromptSourceRoot]>,
    authorization: WorkspaceAuthorizationLease,
    fingerprint: WorkspacePromptFingerprint,
}

impl WorkspacePromptContext {
    pub fn cwd(&self) -> &CanonicalWorkspacePath;
    pub fn primary_root(&self) -> &CanonicalWorkspacePath;
    pub fn source_roots(&self) -> &[AuthorizedPromptSourceRoot];
    pub fn check_authorization(&self) -> Result<(), WorkspaceAuthorizationRevoked>;
    pub fn fingerprint(&self) -> &WorkspacePromptFingerprint;
}
```

PromptService 可以：

- 在 `source_roots` 内执行 Prompt-specific discovery；
- 使用 cwd 选择路径相关 instructions；
- 记录 source provenance；
- 以 fingerprint 作为 Workspace source cache 输入。

PromptService 不可以：

- 扩大 source root；
- 根据文件可读性自行启用 source；
- 查询 trust store；
- 获得 write capability；
- 把 additional root 自动当作 Prompt root。

### WorkspaceSkillContext

```rust
pub struct WorkspaceSkillContext {
    session_id: SessionId,
    cwd: CanonicalWorkspacePath,
    source_roots: Arc<[AuthorizedSkillSourceRoot]>,
    authorization: WorkspaceAuthorizationLease,
    fingerprint: WorkspaceSkillFingerprint,
}

impl WorkspaceSkillContext {
    pub fn cwd(&self) -> &CanonicalWorkspacePath;
    pub fn source_roots(&self) -> &[AuthorizedSkillSourceRoot];
    pub fn check_authorization(&self) -> Result<(), WorkspaceAuthorizationRevoked>;
    pub fn fingerprint(&self) -> &WorkspaceSkillFingerprint;
}
```

SkillService 只能在这些 source roots 内发现和读取 Workspace Skill。

Catalog entry 必须记录精确 source identity 和 content identity。后续 lazy load 必须使用 pinned Catalog entry 与本 Turn 的 WorkspaceSkillContext，不能通过 `skill_id + current Session Workspace` 重新解析新 source。

### WorkspaceAccessView

```rust
pub struct WorkspaceAccessView {
    session_id: SessionId,
    cwd: CanonicalWorkspacePath,
    roots: Arc<[WorkspaceAccessRoot]>,
    authorization: WorkspaceAuthorizationLease,
    fingerprint: WorkspaceAccessFingerprint,
}
```

WorkspaceAccessView 隐藏 path containment 和 capability 检查：

```rust
impl WorkspaceAccessView {
    pub fn session_id(&self) -> SessionId;
    pub fn cwd(&self) -> &CanonicalWorkspacePath;
    pub fn fingerprint(&self) -> &WorkspaceAccessFingerprint;

    pub async fn authorize(
        &self,
        input: WorkspacePathInput<'_>,
        mode: WorkspaceFileMode,
    ) -> Result<AuthorizedWorkspacePath, WorkspaceAccessError>;
}

pub enum WorkspacePathInput<'a> {
    CwdRelative(&'a WorkspaceRelativePath),
    Absolute(&'a Path),
}
```

```rust
pub enum WorkspaceFileMode {
    Read,
    Write,
}
```

Tool policy、Tool requirements 和 Sandbox 使用该 view 作为文件权限硬上限。普通 Tool 不能直接使用 roots 自行实现包含判断。

`CwdRelative` 只能相对 Snapshot 中的 canonical cwd 解析；`Absolute` 必须重新执行 root containment。任何其他相对 `Path`、平台 prefix、`..` 逃逸或 ambient process cwd 解释都必须拒绝。`AuthorizedWorkspacePath` 是文件类 ToolRequirement、资源锁和 Sandbox 可以消费的唯一已授权 path 值；raw model path 不能越过该类型直接进入 executor。

### WorkspaceToolContext

```rust
pub struct WorkspaceToolContext {
    access: WorkspaceAccessView,
    fingerprint: WorkspaceToolFingerprint,
}

impl WorkspaceToolContext {
    pub fn access(&self) -> &WorkspaceAccessView;
    pub fn fingerprint(&self) -> &WorkspaceToolFingerprint;
}
```

ToolService 通过该 context 获得：

- canonical cwd；
- read/write root ceiling；
- authorization lease；
- stable access fingerprint。

它不包含 Prompt source、Skill source、Tool registry、approval 或 provider 信息。

## WorkspaceAuthorizationLease

不可变 Snapshot 不能被原地收紧，但 security revocation 不能等待下一个 Turn。

因此 Snapshot 携带一个不授予额外能力、只允许被撤销的 lease：

```rust
pub struct WorkspaceAuthorizationLease;
pub struct WorkspaceAuthorizationControl;

impl WorkspaceAuthorizationLease {
    pub fn check(&self) -> Result<(), WorkspaceAuthorizationRevoked>;
}

impl WorkspaceAuthorizationControl {
    pub fn revoke(&self, reason: WorkspaceRevocationReason);
}
```

WorkspaceResolver 返回 Snapshot 与 revocation control 的 resolved pair：

```rust
pub struct ResolvedWorkspace {
    pub snapshot: Arc<WorkspaceSnapshot>,
    control: WorkspaceAuthorizationControl,
}
```

`WorkspaceAuthorizationControl` 只交给 Session execution owner，不进入 Prompt、Tool、Skill 或领域 Session。Session execution owner 为每个 active Turn 跟踪其 control/cancellation 关联；ordinary Snapshot replacement 不撤销旧 control，security-restricting update 则先调用 `revoke()`，再中断所有使用该 lease 的 active Turn。这样不需要 Runtime-global Workspace registry，也能明确找到受影响执行。

语义：

```text
Snapshot data
→ 固定本 Turn 的 capability ceiling

Authorization lease
→ 只回答该 ceiling 是否仍可继续使用
```

lease 不能扩大 Snapshot 中不存在的 capability。

检查点至少包括：

- project Prompt source discover/read 前；
- Workspace Skill discover/read/lazy load 前；
- 每批 ToolCall preflight 前；
- ToolSandbox 文件操作前；
- 每次新的模型调用前。

## Session Ownership 与 Lifecycle

### Session-owned Definition

Workspace definition 属于 immutable SessionDefinition：

```rust
pub struct SessionDefinition {
    pub session_id: SessionId,
    pub revision: SessionDefinitionRevision,
    pub agent: AgentRevisionRef,
    pub workspace: Workspace,
    // model / prompts ...
}
```

Workspace definition update 同时产生新的 WorkspaceRevision 和 SessionDefinitionRevision。active Turn 继续 pin 旧 Snapshot，loaded Session execution 只更新 future admission 的 current definition projection。完整生命周期和线性化规则见 [Agent 与 Session 生命周期架构设计](agent-session-lifecycle.md)。

loaded Session execution state 保存当前解析状态：

```rust
pub enum SessionWorkspaceState {
    Resolving,
    Ready(ResolvedWorkspace),
    Unavailable(WorkspaceUnavailable),
}
```

`SessionWorkspaceState` 是执行状态，不进入 Workspace definition，也不是第二个 durable source of truth。

### 创建与加载

```text
创建或加载 Session
→ capture exact SessionDefinitionRevision / WorkspaceRevision
→ WorkspaceResolver::resolve(exact definition.workspace)
→ publication 前 CAS SessionLifecycle 仍为 Open且 current revision 未变化
→ Ready(ResolvedWorkspace { snapshot, control })
   或 Unavailable(error)

CAS stale 时丢弃旧 resolve 结果并按新 SessionDefinitionRevision 重试，不能发布旧 Workspace projection
```

Workspace unavailable 时：

- conversation/history query 仍可工作；
- 不允许开始新 Turn；
- 可以显式 retry/reload；
- 不使用 last-good Snapshot 静默开始 future Turn；
- loaded Session readiness 进入 `SessionReadiness::Unavailable(SessionUnavailable::WorkspaceUnavailable)`，不修改 durable SessionLifecycle。

### 更新

用户发起 definition update 时：

```text
expected SessionLifecycle = Open
+ expected SessionDefinitionRevision + WorkspaceRevision
→ 在 per-session lifecycle gate 内构造并 resolve complete candidate

ordinary/permissive update
→ 原子提交新 SessionDefinitionRevision
→ 发布 future Snapshot

security-restricting definition update
→ 先 revoke 受影响的旧 lease
→ durable commit 新 SessionDefinitionRevision
→ terminalize 受影响 active Turn
→ 发布 future Snapshot
→ 最后确认 update success
```

这样不会出现：

```text
Session 已保存新 roots
但 Tool/Prompt/Skill 仍使用旧授权
```

### Reload

reload 不属于 WorkspaceService method：

```text
Session execution owner 读取 current exact SessionDefinition.workspace
→ 再次 WorkspaceResolver::resolve(...)
→ success：原子发布新 Snapshot
→ unavailable：future Turn admission fail closed
```

reload 只重新解析 Workspace facts。Prompt、Skill 和 Tool 的 source/cache/registry reload 仍由各自子系统负责。

### Unload

Workspace 没有独立 unload lifecycle。

```text
SessionExecutionState = Idle
→ UnloadSession
→ 释放 SessionWorkspaceState
```

Session active 时 unload 返回 Busy；调用方先显式 cancel 并等待 Turn terminal。

没有 Workspace registry entry、shared aggregate 或 backend connection 需要单独关闭。

## Turn Pinning

Turn admission 时，Session execution owner 原子捕获当前 WorkspaceSnapshot：

```rust
let workspace = session
    .workspace_state()
    .require_ready()?
    .snapshot
    .clone();
```

推荐 capture 依赖图：

```text
exact SessionDefinitionRevision + AgentRevisionRef + candidate TurnId
+ AgentPrompts + SessionPrompts + TurnModelSnapshot
+ Arc<WorkspaceSnapshot>
├─ SkillService::catalog(SkillCatalogContext {
│    agent, session_id, session_revision, workspace: workspace.skill_context()
│  }) → SkillCatalog
└─ ToolService::for_turn(ToolTurnContext {
     agent, session_id, session_revision, turn_id,
     workspace: workspace.tool_context(),
     provider: model.capabilities(), execution_mode, turn_port, cancellation, updates
   }) → ToolSet

SkillCatalog.prompt_view()
+ ToolSet.prompt_view()
+ workspace.prompt_context()
+ exact AgentPrompts / SessionPrompts
+ TurnModelSnapshot
→ PromptService::for_turn(...)
→ PromptSet
→ TurnExecutionContext
```

SkillCatalog 与 ToolSet 没有直接依赖，可以并行捕获；PromptSet 必须最后创建并绑定二者的精确 fingerprint。

```rust
pub(crate) struct TurnExecutionContext {
    session_id: SessionId,
    session_revision: SessionDefinitionRevision,
    agent: AgentRevisionRef,
    model: TurnModelSnapshot,
    workspace: Arc<WorkspaceSnapshot>,
    skill_service: Arc<SkillService>,
    skill_context: SkillCatalogContext,
    skill_catalog: Arc<SkillCatalog>,
    tool_set: ToolSet,
    prompt_set: PromptSet,
    fingerprint: ExecutionContextFingerprint,
    diagnostics: Arc<[TurnContextDiagnostic]>,
}
```

字段保持私有，避免不同 Turn 的 Workspace、Catalog、ToolSet 和 PromptSet 被交叉组合。完整 capture、逻辑模型调用、AgentLoop 和 recovery 规则见 [Turn 执行模块与执行上下文架构设计](turn-execution-context.md)。

Turn 领域对象仍不持有 Workspace。只有 Turn execution context pin Snapshot。

同一 Turn 内：

- 不再次读取 Session current Workspace；
- 不再次调用 WorkspaceResolver；
- 不切换 cwd；
- 不增加 additional roots；
- 不获得 reload 后的新 capability；
- Skill lazy load 继续使用 pinned WorkspaceSkillContext；
- ToolSet 继续使用 pinned WorkspaceToolContext；
- PromptSet 继续使用 pinned WorkspacePromptContext。

## Update 对 Active/Future Turn 的影响

Workspace 变化分为两类。

### Ordinary Update

包括：

```text
cwd 变化
新增 root
新增 capability
新增 Prompt/Skill source grant
```

语义：

- active Turn 继续使用旧 Snapshot 和由其派生的同一个 TurnExecutionContext；
- future Turn admission 捕获新 Snapshot 并创建新 Context；
- permissive update 不扩大 active Turn 权限；
- active Turn 不原地重建 PromptSet、ToolSet、SkillCatalog 或模型调用 baseline。

### Security-Restricting Update

包括：

```text
trust 降级
移除 root
收紧 read/write grant
撤销 Prompt/Skill source authorization
managed policy hard deny
```

Definition-changing restriction，例如移除 root 或修改 Workspace grants：

1. 在 per-session lifecycle gate 内 resolve/validate candidate definition；
2. 撤销受影响 WorkspaceAuthorizationLease；
3. 阻止新的 source read、Skill load、input composition、Tool execution 和模型调用；
4. durable commit 新 SessionDefinitionRevision；
5. 通知 Session execution owner并中断使用该 lease 的 active Turn；
6. 发布 future Turn 使用的新 Snapshot；
7. 最后向调用方确认 update success；
8. 不把 last-good Snapshot 当作 fallback。

若 revoke 后 durable commit 失败，不能恢复旧 lease；旧 definition 仍是 durable current，SessionReadiness 进入 Unavailable，等待从 durable truth 重新 resolve/repair。

Authority-only restriction，例如 trust/policy store 降级、managed hard deny 或外部撤销 source authorization，不创建假的 WorkspaceRevision 或 SessionDefinitionRevision：

1. WorkspaceAuthority 在自己的 publication gate 内原子发布新 authority revision/policy，并 revoke 受影响旧 lease；
2. 通知 Session execution owner并中断受影响 active Turn；
3. 使用 current exact SessionDefinition.workspace 重新 resolve；
4. success 时发布受限的新 Snapshot，unavailable 时 future admission fail closed；
5. SessionDefinitionRevision 保持不变。

如果撤权发生在 provider request 已发送之后，MiniCore 无法撤回 provider 已看到的内容。它能保证的是：

- best-effort cancel in-flight request；
- 基于已撤销上下文产生但尚未append的UserMessage、Steer contribution或模型结果直接丢弃；已append但conversation-hidden的assistant/tool entries不得再追加`tool_round_completed`，随后中断active Turn；
- 不开始下一次模型调用；
- 已完成的外部副作用不伪装成已回滚。

## 多 Session 语义

每个 Session 独立拥有 Workspace definition 和 current Snapshot。

```text
Session A ── Workspace A definition ── Snapshot A
Session B ── Workspace B definition ── Snapshot B
```

即使二者 primary root 相同：

- cwd 可以不同；
- additional roots 可以不同；
- requested access 可以不同；
- source policy 可以不同；
- WorkspaceRevision 不同；
- authorization lease 不共享；
- Session A reload 不替换 Session B Snapshot；
- Session-scoped Tool grant 不跨 Session 复用。

允许共享的只是不可变实现 cache：

```text
canonical path metadata
content-addressed Prompt/Skill body
只读文件 metadata
```

不能仅以 canonical root 作为授权 cache key。任何 authorization-sensitive cache 至少必须包含对应 Workspace view fingerprint。

## Workspace Identity

### 当前不定义 WorkspaceId

当前 Workspace 没有独立于 Session 的：

- lifecycle；
- lookup；
- registry；
- mutation owner；
- durable aggregate state；
- shared reference semantics。

因此独立 `WorkspaceId` 会与 `SessionId` 重复，并诱导错误共享。

MiniCore 使用四种精确 identity：

```text
Workspace definition owner
→ SessionId

Definition concurrency/version
→ WorkspaceRevision

Canonical root grouping/cache input
→ WorkspaceRootFingerprint

Effective Turn-visible Workspace state
→ WorkspaceFingerprint / view fingerprint
```

primary root fingerprint 可以用于 UI 和 Session catalog 的项目分组，但它不表示一个 Workspace entity，也不能用于授权。

### 何时重新引入 WorkspaceId

只有出现以下真实需求之一时，才重新评估：

- 多个 Session 引用同一个独立 mutable Workspace aggregate；
- Workspace 有独立 create/open/close/reconfigure lifecycle；
- Runtime 或 supervisor 需要按 Workspace 路由多个同时存在的 backend；
- remote Workspace 有独立于 mount path 的稳定 provider identity；
- Workspace 拥有不能归属于 Session 的 durable state。

即使未来引入：

```text
WorkspaceId
≠ canonical path
≠ WorkspaceFingerprint
≠ authorization token
```

## Fingerprint

Workspace 至少需要：

```text
WorkspaceFingerprint
WorkspacePromptFingerprint
WorkspaceSkillFingerprint
WorkspaceAccessFingerprint
WorkspaceToolFingerprint
```

WorkspaceFingerprint 覆盖：

- SessionId；
- WorkspaceRevision；
- canonical roots 和稳定顺序；
- resolved cwd；
- per-root trust verdict/revision；
- effective filesystem grants；
- effective Prompt source grants；
- effective Skill source grants；
- authority policy revision；
- canonicalization algorithm version。

不覆盖：

- Prompt/Skill 文件内容；
- diagnostics 文本；
- cache/load state；
- 随机 lease token；
- display name；
- filesystem watcher 状态。

各窄 view 使用自己的 fingerprint，避免无关变化造成跨子系统 cache invalidation。例如只收紧 write grant，不应改变 WorkspacePromptFingerprint。

## Error 与 Diagnostics

基础错误：

```rust
pub enum WorkspaceErrorKind {
    InvalidDefinition,
    RootUnavailable,
    RootNotDirectory,
    CanonicalizationFailed,
    DuplicateRoot,
    OverlappingRoots,
    InvalidRelativePath,
    CwdOutsideRoots,
    CwdRootMismatch,
    AuthorityUnavailable,
    AuthorityDenied,
    RequiredCapabilityUnavailable,
    AuthorizationRevoked,
}
```

错误必须区分：

```text
Rejected
→ definition 确定非法，不应自动 retry

Unavailable
→ definition 可能有效，但当前目录或 authority 暂不可用

Revoked
→ 先前 Snapshot 的安全资格已失效
```

Diagnostics 至少记录：

- root key 和 role；
- source provenance；
- trust/policy revision；
- requested 与 effective grant 差异；
- source grant 被拒绝原因；
- canonical duplicate/overlap；
- unavailable/retryable；
- fingerprint。

绝对路径默认不进入模型可见 metadata。公开 diagnostics 和 UI projection 应支持 path redaction 或 display-relative path。

## 与三个 Service 的关系

### PromptService

PromptService 消费 `WorkspacePromptContext`，负责：

- Prompt-specific discovery；
- source parsing；
- scope/role/merge；
- content cache；
- PromptSet 创建。

Workspace 只授权 source，不解析 Prompt。

### SkillService

SkillService 消费 `WorkspaceSkillContext`，负责：

- Skill-specific discovery；
- Catalog；
- metadata conflict；
- exact content identity；
- lazy load；
- content cache。

Workspace 只授权 source，不解析 Skill。

### ToolService

ToolService 消费 `WorkspaceToolContext`，负责：

- Tool filtering；
- dynamic ToolRequirements；
- ToolPolicy；
- approval/grant；
- Sandbox；
- Tool execution。

WorkspaceAccessView 只是 filesystem capability ceiling。它不决定某个 Tool 是否可用，也不替代 ToolPolicy 或 Sandbox。

## 明确不属于 Workspace

```text
Agent definition
Session conversation
Turn / Item / Interaction
PromptDefinition / PromptSet
ToolRegistry / ToolSet / Tool executor
SkillCatalog / LoadedSkill
Model / provider / credentials
network / process / environment policy
Tool approval / Tool grant
Sandbox implementation
VCS / worktree
code index
file watcher
IDE window / tabs / layout
extension host
remote workspace server
upload / telemetry
```

未来若需要 worktree 或 remote execution，应建立各自深模块，并让 Session Workspace 引用其输出；不能把所有项目相关能力重新塞回 Workspace。

## 被否决的方案

### 只保留 cwd 与 roots DTO

否决原因：canonicalization、trust、source authorization 和 sandbox projection 会散落到三个 Service 和 Session execution。

### Runtime-global WorkspaceService

否决原因：当前没有 shared mutable Workspace aggregate、独立 lifecycle 或 remote backend。它会制造 registry、attachment、close、fan-out 和跨 Session mutation ordering。

### path-derived WorkspaceId

否决原因：同一 root 下不同 Session 的 cwd、additional roots 和授权不同；path key 适合分组，不等于 entity identity。

### IDE Workspace aggregate

否决原因：window、indexing、tasks、extensions、VCS 和 editor state 属于宿主产品，不属于可嵌入 MiniCore runtime。

### additional roots 自动加入 Prompt/Skill discovery

否决原因：文件访问扩展会变成隐藏 instruction injection 扩展，违反最小授权。

### active Turn 在 reload 后原地替换 Workspace view

否决原因：会造成 PromptSet、ToolSet、SkillCatalog 和 lazy-loaded Skill 来自不同 Workspace revision。

## 基础不变量

- Workspace definition 的 owner 是 Session；
- Workspace 没有 Runtime-global mutable registry；
- 当前不定义 WorkspaceId；
- 所有 root/cwd 先 canonicalize 后校验；
- canonical duplicate 和 overlap fail closed；
- cwd 必须属于一个明确 root；
- filesystem access、Prompt source 和 Skill source 是三个独立 grant；
- additional root 默认 access-only；
- Prompt、Tool、Skill 不自行查询 trust 或重新推断 roots；
- 一个 WorkspaceSnapshot 原子投影全部窄 view；
- active Turn pin 同一个 WorkspaceSnapshot；
- ordinary reload 只影响 future Turn；
- permissive update 不扩大 active Turn capability；
- restrictive update 撤销 lease 并中断 active Turn；
- Tool approval 不能扩大 WorkspaceAccessView；
- Workspace unavailable 时 future Turn fail closed；
- 同根多 Session 不共享 mutable Snapshot 或 authorization lease；
- Workspace 不恢复通用 ResourceManager。

## Test Matrix

至少覆盖：

- primary root 正常解析；
- additional root 正常解析；
- canonical duplicate root；
- overlapping roots；
- cwd 在 primary root；
- cwd 在 additional root；
- cwd 越界；
- cwd root key 与实际 root 不一致；
- symlink/junction escape；
- additional root access-only 不进入 Prompt/Skill source；
- Prompt source grant 不授权 Skill；
- Skill source grant 不授权 Prompt；
- Tool write request 被 Workspace read-only ceiling 拒绝；
- Tool approval/grant 不能扩大 WorkspaceAccessView；
- Tool executor 不能访问 WorkspaceAccessView 或为未声明 path 重新授权；
- ToolGrantKey 绑定 WorkspaceAccessFingerprint；
- 不同 Session 使用不同 root anchor 指向同一目标时仍竞争同一文件锁；
- create/rename target 使用 nearest-existing-ancestor 校验，并覆盖 symlink race；
- cwd 位于 source-denied root 时不自动获得 Prompt/Skill source grant；
- Workspace Prompt cache/fingerprint 覆盖 source stamp，revocation 后不复用旧 hidden instructions；
- Workspace Skill adapter 的 discover/read 只能使用 pinned WorkspaceSkillContext；
- SkillContributionRef 将 Catalog entry、LoadedSkill、SkillInjection 和 Prompt fingerprint 绑定为同一 identity；
- WorkspacePromptContext、WorkspaceSkillContext 和 WorkspaceToolContext 不能由调用方伪造；
- authority failure 和 root unavailable；
- candidate update 失败不修改 current definition/snapshot；
- reload success 原子替换 future Snapshot；
- active Turn 保持旧 Snapshot；
- permissive update 不扩大 active Turn；
- restrictive definition update 撤销 active lease 并提交新 SessionDefinitionRevision；
- revoke 后 definition commit 失败时旧 lease 不恢复且 readiness Unavailable；
- authority-only restriction 重新 resolve current definition且不创建 SessionDefinitionRevision；
- revoked model/tool result 不进入后续 committed conversation；
- 同 root 两个 Session 的 cwd/grant/lease/fingerprint 隔离；
- view-specific fingerprint golden vectors；
- absolute path 不泄漏到模型可见 Skill metadata 或 Prompt provenance。

## 后续问题

1. `WorkspaceRelativePath` 和 `CanonicalWorkspacePath` 的跨平台编码细节。
2. WorkspaceAuthority 的 persisted trust、managed policy 和 headless adapter 形状。
3. Workspace source adapter 和 Sandbox 在各平台如何实现 handle-relative open 以防止 TOCTOU。
4. WorkspaceAuthorizationControl 与 future Session execution owner/cancellation primitive 的实现映射。
5. Session Workspace update 的最终 command interface。
6. Workspace unavailable reason 与 SessionReadiness/公开 diagnostics 的最终映射。
7. crash recovery 是否持久化 WorkspaceFingerprint、view fingerprint 或 authority revision。
8. future remote backend 出现后，是否引入 Workspace locator/backend seam。

## 设计进度

- [x] 判断 MiniCore 需要 Workspace 模块，但不需要 WorkspaceService。
- [x] 确定 Workspace 是 SessionDefinition-owned definition，并纳入 SessionDefinitionRevision。
- [x] 确定当前不定义 WorkspaceId。
- [x] 定义 primary root、additional roots 和 cwd 合法域。
- [x] 区分 trust、filesystem capability、Prompt source 和 Skill source。
- [x] 定义 WorkspaceResolver 与 WorkspaceSnapshot。
- [x] 定义 WorkspacePromptContext、WorkspaceSkillContext、WorkspaceToolContext 和 WorkspaceAccessView。
- [x] 定义 Turn pinning、ordinary reload 和 security revocation。
- [x] 将 WorkspaceSnapshot 纳入 TurnExecutionContext capture DAG，并固定字段私有。
- [x] 定义同根多 Session 的隔离语义。
- [ ] 定义跨平台 path 类型和 authority adapters 的最终字段。
- [x] 对齐 Session lifecycle、definition revision、load/readiness/execution state 和 revocation 语义。
- [ ] 定义公开 command payload 与 recovery manifest/storage integration。
