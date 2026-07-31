# Workspace 子系统架构设计

状态：当前权威架构（ADR 0127后，生产实现待启动）
日期：2026-07-31

## 目的

本文从零定义 MiniCore 的 Workspace 子系统，回答以下问题：

- MiniCore 后端是否需要 Workspace；
- Workspace 是独立 entity、Runtime-global Service，还是 Session-owned definition；
- primary root、additional roots 和 cwd 如何建模；
- trust、文件访问能力和 Prompt/Skill source authorization 如何分离；
- PromptService、ToolService 和 SkillService 消费哪些窄 view；
- Workspace definition如何在Turn之间更新，authority hard restriction如何中断active Turn；
- 多个 Session 指向同一目录时是否共享 Workspace 状态；
- `WorkspaceId` 是否必要。

本文独立定义 Workspace 模型，不采用 IDE Workspace 形态。

## 决策摘要

MiniCore 需要 Workspace **模块**，但不需要 Runtime-global `WorkspaceService`、Workspace registry 或独立 Workspace lifecycle aggregate。

关系为：

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

核心设计决策：

- `Workspace` 属于 Session；
- `Workspace` 是 Session-owned definition，不是独立 entity；
- 当前不定义 `WorkspaceId`；
- `SessionId` 是 Workspace definition 的 owner identity；
- `WorkspaceRevision` 标识 Session Workspace definition 的版本；
- 当前有效 Workspace 状态由不可变 `Arc<WorkspaceSnapshot>` 本身及其私有投影承载，不定义解析标识或代际值；
- primary root、additional roots 和 cwd 由 Workspace 模块统一规范化和校验；
- trust 是 policy 输入，不等于文件权限；
- 文件可读不等于允许作为 Prompt 或 Skill source；
- Prompt source authorization 与 Skill source authorization 相互独立；
- additional root 默认只扩展文件访问，不自动成为 Prompt 或 Skill source；
- Prompt、Tool、Skill 不自行 canonicalize roots，也不自行查询 trust；
- Turn 开始时 pin 一个不可变 `Arc<WorkspaceSnapshot>`；
- Workspace definition update只在Session Idle时接受，非Idle返回SessionBusy；
- authority hard restriction通过sticky SecurityRevoked中断active Turn，不动态撤销WorkspaceSnapshot或open handle；
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
- Turn-pinned immutable Snapshot 及其私有窄 view；
- Idle-only update、authority resolution与security interruption。

因此 Workspace 模块通过 deletion test：删除它后，复杂性会重新出现在 Session execution、Prompt source、Skill source、Tool policy 和 Sandbox 中。

Workspace 模块的价值来自统一这些不变量，而不是拥有一个新的全局 object registry。

## 为什么不建立 WorkspaceService

当前模型没有以下真实需求：

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

后者通过显式 `/reload` 或 Idle-only workspace re-resolve 后发布新的不可变对象表达；cache 可直接清空，不能成为正确性依据。

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
∩ per-call approval
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
    ) -> Result<WorkspaceSnapshotCandidate, WorkspaceResolveError>;
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
→ current authority decision/provenance
→ WorkspaceSnapshotCandidate
```

Resolver不读取Prompt/Skill正文。Session lifecycle随后让PromptService和SkillService只在candidate授权的source roots内capture immutable values，最后调用candidate private `finish(...)`创建WorkspaceSnapshot；因此任一source capture失败都不会发布partial Snapshot。

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

reload由Session lifecycle在Idle时使用current exact Workspace definition再次调用`resolve()`；active Turn期间不reload。

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
    prompt_sources: Arc<[CapturedWorkspacePromptSource]>,
    skill_sources: Arc<[CapturedWorkspaceSkillSource]>,
    diagnostics: Arc<[WorkspaceDiagnostic]>,
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

Snapshot不原地修改。Workspace definition update只在Idle时产生新的Snapshot；authority hard restriction中断active Turn，terminal后按current authority重新resolve新Snapshot。

```rust
impl WorkspaceSnapshot {
    pub fn prompt_context(&self) -> WorkspacePromptContext;
    pub fn skill_context(&self) -> WorkspaceSkillContext;
    pub fn tool_context(&self) -> WorkspaceToolContext;
    pub fn access_view(&self) -> WorkspaceAccessView;
    pub fn summary(&self) -> WorkspaceSummary;
}
```

这些投影的构造器是私有的，调用方不能用任意 paths 自行构造“已授权 Workspace context”。

## 窄只读 View

Workspace candidate在publication前提供两个互不扩权的capture context；字段和constructor均为crate-private：

```rust
pub struct AuthorizedPromptSourceRoot {
    key: WorkspaceRootKey,
    canonical_path: CanonicalWorkspacePath,
    trust: WorkspaceRootTrust,
}

pub struct AuthorizedSkillSourceRoot {
    key: WorkspaceRootKey,
    canonical_path: CanonicalWorkspacePath,
    trust: WorkspaceRootTrust,
}

pub(crate) struct WorkspacePromptCaptureContext {
    session_id: SessionId,
    cwd: CanonicalWorkspacePath,
    roots: Arc<[AuthorizedPromptSourceRoot]>,
}

pub(crate) struct WorkspaceSkillCaptureContext {
    session_id: SessionId,
    cwd: CanonicalWorkspacePath,
    roots: Arc<[AuthorizedSkillSourceRoot]>,
}

pub struct CapturedWorkspacePromptSource {
    relative_location: WorkspaceRelativePath,
    content: Arc<str>,
    authorization: WorkspaceSourceAuthorization,
}

pub struct CapturedWorkspaceSkillSource {
    relative_location: WorkspaceRelativePath,
    bytes: Arc<[u8]>,
    authorization: WorkspaceSourceAuthorization,
}

pub struct WorkspaceSourceAuthorization {
    root_key: WorkspaceRootKey,
    canonical_root: CanonicalWorkspacePath,
    trust: WorkspaceRootTrust,
}

pub enum WorkspaceSourceCaptureError {
    InvalidRelativeLocation,
    SourceOutsideAuthorizedRoot,
    SourceKindNotAuthorized,
}

impl WorkspacePromptCaptureContext {
    pub(crate) fn session_id(&self) -> SessionId;
    pub(crate) fn cwd(&self) -> &CanonicalWorkspacePath;
    pub(crate) fn roots(&self) -> &[AuthorizedPromptSourceRoot];
}

impl WorkspaceSkillCaptureContext {
    pub(crate) fn session_id(&self) -> SessionId;
    pub(crate) fn cwd(&self) -> &CanonicalWorkspacePath;
    pub(crate) fn roots(&self) -> &[AuthorizedSkillSourceRoot];
}

impl AuthorizedPromptSourceRoot {
    pub(crate) fn key(&self) -> &WorkspaceRootKey;
    pub(crate) fn canonical_path(&self) -> &CanonicalWorkspacePath;
    pub(crate) fn trust(&self) -> WorkspaceRootTrust;
}

impl AuthorizedSkillSourceRoot {
    pub(crate) fn key(&self) -> &WorkspaceRootKey;
    pub(crate) fn canonical_path(&self) -> &CanonicalWorkspacePath;
    pub(crate) fn trust(&self) -> WorkspaceRootTrust;
}

impl WorkspaceSourceAuthorization {
    pub(crate) fn root_key(&self) -> &WorkspaceRootKey;
    pub(crate) fn trust(&self) -> WorkspaceRootTrust;
}

impl WorkspacePromptCaptureContext {
    pub(crate) fn capture(
        &self,
        root_key: &WorkspaceRootKey,
        relative_location: WorkspaceRelativePath,
        content: Arc<str>,
    ) -> Result<CapturedWorkspacePromptSource, WorkspaceSourceCaptureError>;
}

impl WorkspaceSkillCaptureContext {
    pub(crate) fn capture(
        &self,
        root_key: &WorkspaceRootKey,
        relative_location: WorkspaceRelativePath,
        bytes: Arc<[u8]>,
    ) -> Result<CapturedWorkspaceSkillSource, WorkspaceSourceCaptureError>;
}

impl CapturedWorkspacePromptSource {
    pub(crate) fn relative_location(&self) -> &WorkspaceRelativePath;
    pub(crate) fn content(&self) -> &str;
    pub(crate) fn authorization(&self) -> &WorkspaceSourceAuthorization;
}

impl CapturedWorkspaceSkillSource {
    pub(crate) fn relative_location(&self) -> &WorkspaceRelativePath;
    pub(crate) fn bytes(&self) -> &Arc<[u8]>;
    pub(crate) fn authorization(&self) -> &WorkspaceSourceAuthorization;
}
```

PromptService只能接收Prompt capture context，SkillService只能接收Skill capture context；二者不能读取另一类source roots，也不能自行扩大candidate授权。adapter通过`roots()`取得root key/canonical path用于discover/read，读取文件后必须调用`capture(root_key, relative_location, ...)`构造结果；该方法重新验证root仍在authorized set、relative location位于该root并填充authorization/provenance，避免sibling module伪造captured source。exact authorization只服务candidate/composition校验；CanonicalUserMessage最多投影`WorkspaceRootKey + WorkspaceRelativePath`safe origin，不能保存canonical root、trust或authorization。该投影边界消费[INV-202](../architecture.md#跨模块不变量索引)。

### WorkspacePromptContext

```rust
pub struct WorkspacePromptContext {
    session_id: SessionId,
    cwd: CanonicalWorkspacePath,
    primary_root: CanonicalWorkspacePath,
    sources: Arc<[CapturedWorkspacePromptSource]>,
}

impl WorkspacePromptContext {
    pub fn cwd(&self) -> &CanonicalWorkspacePath;
    pub fn primary_root(&self) -> &CanonicalWorkspacePath;
    pub fn sources(&self) -> &[CapturedWorkspacePromptSource];
}
```

PromptService 可以：

- 解析和选择已经capture的Workspace Prompt source；
- 使用 cwd 选择路径相关 instructions；
- 记录 source provenance；
- 可以使用captured source values作为内部cache输入，或在重新resolve时直接清空cache。

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
    sources: Arc<[CapturedWorkspaceSkillSource]>,
}

impl WorkspaceSkillContext {
    pub fn cwd(&self) -> &CanonicalWorkspacePath;
    pub fn sources(&self) -> &[CapturedWorkspaceSkillSource];
}
```

SkillService只能从这些captured sources构建Workspace Skill entries，不在Turn内重新discover或读取filesystem。

`CapturedWorkspacePromptSource`保存model-safe relative location、exact source authorization/provenance与candidate阶段已经读取、解析和规范化的immutable text `Arc`；relative location不承担正文resolver。PromptService从该值构造或复用`PromptContent`时不再读取source。`CapturedWorkspaceSkillSource`保存对应provenance与immutable captured bytes。两者字段和constructor保持crate-private，只能由PromptService/SkillService在对应capture context授权roots内产生，再由Session lifecycle candidate组装进WorkspaceSnapshot。SkillEntry后续lazy parse只解析本Turn captured SkillView entry中的captured bytes，不能通过`skill_id + current Session Workspace`重新解析future view，也不能在Turn内按path重新读取current file。SecurityRevoked会取消active operation并使迟到结果因current operation/control basis失效而被丢弃。

### WorkspaceAccessView

```rust
pub struct WorkspaceAccessView {
    session_id: SessionId,
    cwd: CanonicalWorkspacePath,
    roots: Arc<[WorkspaceAccessRoot]>,
}
```

WorkspaceAccessView 隐藏 path containment 和 capability 检查：

```rust
impl WorkspaceAccessView {
    pub fn session_id(&self) -> SessionId;
    pub fn cwd(&self) -> &CanonicalWorkspacePath;
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

`CwdRelative` 只能相对 Snapshot 中的 canonical cwd 解析；`Absolute` 必须重新执行 root containment。任何其他相对 `Path`、平台 prefix、`..` 逃逸或 ambient process cwd 解释都必须拒绝。`AuthorizedWorkspacePath` 是文件类 ToolRequirement、Session-local `FileMutationKey`推导和Sandbox可以消费的唯一已授权path值；raw model path不能越过该类型直接进入executor。

### WorkspaceToolContext

```rust
pub struct WorkspaceToolContext {
    access: WorkspaceAccessView,
}

impl WorkspaceToolContext {
    pub fn access(&self) -> &WorkspaceAccessView;
}
```

ToolService 通过该 context 获得：

- canonical cwd；
- read/write root ceiling；
- authority revision-bound effective grants；
- exact normalized access roots、cwd 和 effective filesystem grants。

它不包含 Prompt source、Skill source、Tool registry、approval 或 provider 信息。

## Immutable Snapshot And Security Events

WorkspaceSnapshot在Turn内没有可撤销lease。Workspace definition update只在Session Idle时执行，因此active Turn不会与definition update竞争，也不需要`WorkspaceAuthorizationControl`或`WorkspaceCommitAuthorization`。

```rust
pub struct ResolvedWorkspace {
    pub snapshot: Arc<WorkspaceSnapshot>,
}

pub(crate) struct WorkspaceSnapshotCandidate {
    // resolved roots, cwd, effective access and diagnostics; not yet published
}

impl WorkspaceSnapshotCandidate {
    pub(crate) fn prompt_capture_context(&self) -> WorkspacePromptCaptureContext;
    pub(crate) fn skill_capture_context(&self) -> WorkspaceSkillCaptureContext;

    pub(crate) fn finish(
        self,
        prompt_sources: Arc<[CapturedWorkspacePromptSource]>,
        skill_sources: Arc<[CapturedWorkspaceSkillSource]>,
    ) -> Arc<WorkspaceSnapshot>;
}
```

`ResolvedWorkspace`是`SessionWorkspaceState::Ready`保存的published wrapper，不是WorkspaceResolver的直接返回值；只有candidate `finish(...)`完成Prompt/Skill source capture后才能创建。

Authority hard restriction由WorkspaceAuthority或host作为独立security event发布，不伪装成Workspace definition update：

```text
current authority/policy fact published
→ route handle-scoped SecurityRevoked to affected current loaded SessionExecutionHandle
→ close admission；bind current active target when present
→ EmergencyControl sticky signal
→ Idle直接resolve；Starting取消candidate；active Turn Finishing + truthful settlement
→ active Turn时TurnInterrupted(SecurityRevoked)
→ cleanup/terminal后重新resolve current durable Workspace definition
```

SecurityRevoked只保证signal first-wins后不启动新的MiniCore-sanctioned Model、Tool、source read或workspace-dependent append。已经进入provider、kernel、子进程或远端系统的operation不能回滚，按Cancel规则保存exact outcome或`ToolAbandoned`。MiniCore不提供open-handle动态revocation，也不建立Runtime-global handle registry。

active Turn仍在既有安全点观察`EmergencyControl`：启动新Model、消费新的source/lazy load结果、发放owner-local ToolStartPermit、提交workspace-dependent conversation entry和开始下一次Tool operation前。operation先于signal完成时按现有basis/owner状态结算；signal先赢时结果被拒绝或丢弃。

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

Workspace definition update同时产生新的WorkspaceRevision和SessionDefinitionRevision。loaded Session只在execution state为Idle时接受Workspace patch；active Turn与Workspace definition update不存在并发语义。完整生命周期和线性化规则见[Agent与Session生命周期架构设计](agent-session-lifecycle.md)。

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
→ WorkspaceResolver::resolve(exact definition.workspace) → WorkspaceSnapshotCandidate
→ PromptService.capture_workspace_sources(candidate.prompt_capture_context())
→ SkillService.capture_workspace_sources(candidate.skill_capture_context())
→ candidate.finish(prompt_sources, skill_sources) → Arc<WorkspaceSnapshot>
→ publication 前 CAS SessionLifecycle 仍为 Open且 current revision 未变化
→ Ready(ResolvedWorkspace { snapshot })
   或 Unavailable(error)

任一resolve/source capture失败时不发布partial Snapshot。CAS stale时丢弃旧candidate并按新SessionDefinitionRevision重试，不能发布旧Workspace projection或旧source capture。
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
→ loaded Session要求SessionExecutionState = Idle；否则SessionBusy
→ 在per-session lifecycle serialization内构造并resolve Workspace candidate
→ PromptService/SkillService捕获candidate授权的Workspace-bound sources
→ candidate.finish得到complete immutable WorkspaceSnapshot
→ durable commit新SessionDefinitionRevision与WorkspaceRevision
→ publish Ready(new WorkspaceSnapshot)
→ 最后确认update success
```

resolve或durable commit失败时旧definition和旧Snapshot保持current。这样不会出现：

```text
旧Workspace能力已撤销
但新definition没有durable commit
```

unloaded Session可以直接提交通过结构校验/CAS的Workspace definition；下次load按current authority完整resolve。

### Reload

reload 不属于 WorkspaceService method：

```text
loaded Session要求SessionExecutionState = Idle；否则SessionBusy
→ SessionExecutor读取current exact SessionDefinition.workspace
→ WorkspaceResolver::resolve(...)得到candidate
→ PromptService/SkillService捕获candidate授权的Workspace-bound sources
→ Ready + success：原子替换new Snapshot及captured source values
→ Ready + failure：保留old Snapshot/source values并返回typed reload error
→ Unavailable + success：发布new Snapshot并恢复Ready
→ Unavailable + failure：保持Unavailable并返回typed reload error
```

`/reload workspace`重新解析Workspace facts并重新捕获Workspace-bound Prompt/Skill source；它不替换shared Prompt/Skill/Tool/Model roots。authority hard restriction后的mandatory re-resolve若失败仍进入Unavailable；该security路径不同于用户发起且允许保留old Snapshot的reload。

### Unload

Workspace 没有独立 unload lifecycle。

```text
UnloadSession
→ Session LifecycleControl完成grace/fail-closed drain
→ SessionExecutionState = Idle
→ 释放 SessionWorkspaceState
```

Session active时由Session层的PrepareForUnload停止admission并等待自然terminal；grace deadline到期后fail-closed cancel。Workspace只在ActiveTurnTask结束、Executor进入Idle后释放resolved state。SessionRecorder没有后台queue或physical flush drain。

没有 Workspace registry entry、shared aggregate 或 backend connection 需要单独关闭。

## Turn Pinning

Turn admission时，SessionExecutor把current `Arc<WorkspaceSnapshot>`交给TurnExecutionContext的canonical capture流程。完整依赖图、private constructor和跨资源同源校验只在[Turn执行模块与执行上下文](turn-execution-context.md#context-capture)维护，并由[INV-201](../architecture.md#跨模块不变量索引)约束；Workspace module不保存该图的副本。

Workspace在该流程中只保证：candidate来自current exact SessionDefinitionRevision、Snapshot已经resolve且Ready、Prompt/Skill/Tool窄view均从同一immutable Snapshot投影。Turn领域对象仍不持有Workspace，只有Turn execution context pin Snapshot。

同一 Turn 内：

- 不再次读取 Session current Workspace；
- 不再次调用 WorkspaceResolver；
- 不切换 cwd；
- 不增加 additional roots；
- 不获得 reload 后的新 capability；
- Skill lazy load 继续使用 pinned WorkspaceSkillContext；
- ToolSet 继续使用 pinned WorkspaceToolContext；
- PromptSet 继续使用 pinned WorkspacePromptContext。

## Workspace Update And Security Restriction

### Workspace Definition Update

所有Workspace definition patch都使用同一Idle-only规则，不区分ordinary、permissive或restrictive active-update分支：

```text
Session Idle
→ validate/CAS/resolve candidate
→ capture candidate-authorized Workspace Prompt/Skill sources
→ durable commit definition revision
→ publish new WorkspaceSnapshot及captured source values

Session Starting / Running / Finishing
→ SessionBusy
```

Host希望在长Turn中修改Workspace时，显式执行`Cancel → wait session_settled → UpdateDefinition`。Runtime不提供queued Workspace update、WaitForIdle或隐式Cancel。

### Authority Hard Restriction

Tool start与SecurityRevoked的first-wins/settlement合同由[INV-401](../architecture.md#跨模块不变量索引)定义；Workspace只拥有authority fact、Snapshot invalidation和重新resolve。本节不重复ToolOperationSlot状态机。

managed hard deny、trust/policy store降级或host安全事件可以在active Turn期间触发`SecurityRevoked`，但它不是definition patch，也不创建假的WorkspaceRevision或SessionDefinitionRevision：

1. WorkspaceAuthority或host发布current authority/policy事实；
2. Runtime通过current loaded map向对应`SessionExecutionHandle`设置sticky `EmergencyControl::SecurityRevoked`，存在candidate/current Turn时同时绑定current active target；
3. SessionExecutor停止new admission；Idle直接进入resolve；Starting在Input live apply前取消candidate，apply后绑定同一Turn、阻止task spawn并进入Finishing；Running/Finishing向ActiveTurnTask发布SecurityRevoked并进入或保持Finishing；
4. 已取得ToolStartPermit并进入Running的Tool保存exact outcome或`ToolAbandoned`；Prepared Tool不再启动；
5. 有active Turn时apply live `TurnInterrupted(SecurityRevoked)`、发布terminal StateEvent并释放Turn；JSONL不记录该terminal；
6. candidate清理或Turn terminal后，使用durable current `SessionDefinition.workspace`和current authority重新resolve，并捕获new candidate授权的Workspace-bound Prompt/Skill sources；
7. success时retire signal、Ready并发布new Snapshot及captured source values，failure时Unavailable且future admission fail closed。

FollowUp可以在Finishing期间排队，但terminal和重新resolve完成前不得启动；resolve失败时明确拒绝。Security signal是process-local control fact，不跨restart恢复；Load不推断历史security cause或旧Turn terminal。

如果security event发生在provider request、kernel syscall、子进程或remote side effect开始之后，MiniCore无法撤回已经看到或发生的内容。它只保证signal获胜后不启动新的sanctioned operation，并对in-flight work执行truthful settlement。已打开OS handle不会被动态撤销，该限制不再作为Workspace feature承诺。

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
- resolved Snapshot和authority-sensitive cache不共享；
- Session A reload 不替换 Session B Snapshot；
- per-call Tool approval不跨调用或Session复用。

允许共享的只是不可变实现 cache：

```text
canonical path metadata
immutable Prompt/Skill body cache using captured content values or conservative invalidation
只读文件 metadata
```

不能仅以 canonical root 作为授权 cache key。authorization-sensitive cache在Workspace re-resolve时可以直接清空；若实现内部保留cache，也必须以exact normalized root/grant/policy values保证不会跨授权basis复用。

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

MiniCore 使用精确 durable ref 和 exact structural value：

```text
Workspace definition owner
→ SessionId

Definition concurrency/version
→ WorkspaceRevision

Canonical root grouping/cache input
→ CanonicalWorkspacePath / WorkspaceRootKey（仅definition内稳定）

Effective Turn-visible Workspace state
→ 当前Turn持有的同一个 immutable Arc<WorkspaceSnapshot> 及其私有投影
```

这些值不是替代身份机制。WorkspaceSnapshot字段私有，`WorkspacePromptContext`、`WorkspaceSkillContext`、`WorkspaceAccessView`和`WorkspaceToolContext`只能由同一个Snapshot投影创建；调用方不能用任意paths、grants或policy revision拼接一个“等价view”。

primary root canonical path 可以用于 UI 和 Session catalog 的项目分组，但它不表示一个 Workspace entity，也不能用于授权。

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
≠ authorization token
```

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
}
```

错误必须区分：

```text
Rejected
→ definition 确定非法，不应自动 retry

Unavailable
→ definition可能有效，但当前目录或authority暂不可用
```

Diagnostics 至少记录：

- root key 和 role；
- source provenance；
- trust/policy revision；
- requested 与 effective grant 差异；
- source grant 被拒绝原因；
- canonical duplicate/overlap；
- unavailable/retryable；

绝对路径默认不进入模型可见 metadata。公开 diagnostics 和 UI projection 应支持 path redaction 或 display-relative path。

## 与三个 Service 的关系

### PromptService

PromptService在candidate阶段消费`WorkspacePromptCaptureContext`，在Turn构建阶段消费`WorkspacePromptContext`，负责：

- candidate阶段的Prompt-specific discovery和source capture；
- source parsing；
- scope/role/merge；
- content cache；
- PromptSet 创建。

Workspace 只授权 source，不解析 Prompt。

### SkillService

SkillService在candidate阶段消费`WorkspaceSkillCaptureContext`，在Turn构建阶段消费`WorkspaceSkillContext`，负责：

- candidate阶段的Skill-specific discovery和source capture；
- per-Turn SkillView构建；
- metadata conflict；
- lazy load；
- content cache。

Workspace 只授权 source，不解析 Skill。

### ToolService

ToolService 消费 `WorkspaceToolContext`，负责：

- Tool filtering；
- dynamic ToolRequirements；
- ToolPolicy；
- per-call approval；
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
SkillView / LoadedSkill
Model / provider / credentials
network / process / environment policy
Tool approval
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

否决原因：会造成PromptSet、ToolSet、SkillView和lazy-loaded Skill来自不同Workspace authorization basis。

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
- active Turn在整个生命周期pin同一个immutable WorkspaceSnapshot；
- Workspace definition update和reload只在Session Idle时接受，非Idle返回SessionBusy；
- authority hard restriction通过SecurityRevoked中断Turn，terminal后重新resolve；
- 不承诺动态撤销已打开OS handle或回滚已开始副作用；
- Tool approval不能扩大WorkspaceAccessView，且MVP不保存跨调用grant；
- Workspace unavailable 时 future Turn fail closed；
- 同根多Session不共享mutable Snapshot或authority-sensitive cache；
- Workspace 不恢复通用 ResourceManager。
- WorkspaceSnapshot及其view不跨Runtime恢复；load使用current definition/current authority重新resolve；
- authorization-sensitive cache在Workspace re-resolve、Tool/Policy reload或Runtime restart时清空；MVP不保存跨调用Tool grant；

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
- Tool approval不能扩大WorkspaceAccessView；
- Tool executor 不能访问 WorkspaceAccessView 或为未声明 path 重新授权；
- 不同Session即使通过不同root anchor指向同一physical target也使用各自的SessionFileMutationQueue，不互相等待；fixture明确展示可能并发与lost update；
- create/rename target 使用 nearest-existing-ancestor 校验，并覆盖 symlink race；
- cwd 位于 source-denied root 时不自动获得 Prompt/Skill source grant；
- Workspace Prompt source在Session load、Idle definition update或`/reload workspace`时捕获immutable content；SecurityRevoked后重新resolve时不复用不匹配的新authority basis；
- Workspace Skill adapter的capture只能使用WorkspaceSkillCaptureContext，并必须通过context构造CapturedWorkspaceSkillSource；
- captured SkillView entry、LoadedSkill和SkillInjection使用同一SkillId与exact source authorization完成composition前校验；live/recorded stamp只保存SkillId或Workspace root-relative safe origin；
- WorkspacePromptContext、WorkspaceSkillContext 和 WorkspaceToolContext 不能由调用方伪造；
- authority failure 和 root unavailable；
- candidate update 失败不修改 current definition/snapshot；
- loaded Session非Idle时Workspace update/reload返回SessionBusy且不排队；
- Idle Workspace update成功durable commit revision并原子发布新Snapshot；
- candidate resolve或commit失败时旧definition/Snapshot保持current；
- SecurityRevoked触发Finishing、truthful Tool settlement和TurnInterrupted；
- Idle SecurityRevoked立即失效old Snapshot并重新resolve，不创建TurnInterrupted；
- Starting SecurityRevoked在Input live apply前取消candidate且不创建Turn；apply后绑定同一Turn、阻止task spawn，并在Input publication后发布live TurnInterrupted；
- terminal后使用current definition/current authority重新resolve，success Ready、failure Unavailable；
- security signal先赢时迟到model/tool/source结果不进入后续LiveConversation；
- crash后不恢复security cause或旧TurnStatus；
- 同root两个Session的cwd/grant和security operation basis隔离；
- WorkspacePromptContext、WorkspaceSkillContext、WorkspaceAccessView和WorkspaceToolContext只能由同一个`Arc<WorkspaceSnapshot>`私有投影，不能跨Snapshot拼接；
- Runtime restart或重新resolve后使用新解析出的不可变Snapshot，旧authorization-sensitive cache不可复用；
- absolute path 不泄漏到模型可见 Skill metadata 或 Prompt provenance。

## 后续问题

1. `WorkspaceRelativePath` 和 `CanonicalWorkspacePath` 的跨平台编码细节。
2. WorkspaceAuthority 的 persisted trust、managed policy 和 headless adapter 形状。
3. Workspace source adapter和Sandbox在各平台如何实现handle-relative open以防止TOCTOU；该问题属于O1 enforcement，不提供动态handle revocation。
4. Session Workspace Idle-only update的最终command payload。
5. Workspace unavailable reason与SessionReadiness/公开diagnostics的最终映射。
6. future remote backend出现后，是否引入Workspace locator/backend seam。

## 设计进度

- [x] 判断 MiniCore 需要 Workspace 模块，但不需要 WorkspaceService。
- [x] 确定 Workspace 是 SessionDefinition-owned definition，并纳入 SessionDefinitionRevision。
- [x] 确定当前不定义 WorkspaceId。
- [x] 定义 primary root、additional roots 和 cwd 合法域。
- [x] 区分 trust、filesystem capability、Prompt source 和 Skill source。
- [x] 定义 WorkspaceResolver 与 WorkspaceSnapshot。
- [x] 定义 WorkspacePromptContext、WorkspaceSkillContext、WorkspaceToolContext 和 WorkspaceAccessView。
- [x] 定义Turn-pinned immutable Snapshot、Idle-only update和SecurityRevoked interruption（ADR 0121）。
- [x] 将 WorkspaceSnapshot 纳入 TurnExecutionContext capture DAG，并固定字段私有。
- [x] 定义同根多 Session 的隔离语义。
- [x] 按ADR 0123删除Workspace及view命名指纹；执行一致性由不可变Snapshot、私有投影和显式reload/re-resolve保证。
- [ ] 定义跨平台 path 类型和 authority adapters 的最终字段。
- [x] 对齐Session lifecycle、definition revision、load/readiness、Idle-only update和security interruption语义。
- [x] conversation JSONL不保存Turn-start Workspace摘要；WorkspaceSnapshotRef与WorkspaceRevision execution binding均不进入recording。
