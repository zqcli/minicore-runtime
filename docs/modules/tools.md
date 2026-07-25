# Tool 子系统架构设计

状态：当前权威架构（设计已冻结，生产实现待启动）
日期：2026-07-25

## 目的

本文定义 MiniCore Tool 子系统的基础对象、所有权、模型披露、调用路由、参数准备、Hook、权限审批、Sandbox、并发执行和领域投影。

本文以以下关系为基础：

```text
MiniCoreRuntime 初始化一个 Arc<ToolService>
Agent、Session 和 Turn 领域对象不持有 Tool 属性
Session execution 在 candidate Turn admission 期间、第一次模型调用前调用 ToolService::for_turn(...)
ToolService::for_turn(...) 返回本 Turn 使用的不可变 ToolSet
同一 Turn 内的全部 LLM → Tool → LLM 循环复用同一个 ToolSet
ToolCall 和 ToolResult 表达为同一个 ToolInvocation Item
Tool approval 表达为该 Item-owned durable Interaction
```

以下内容不在本设计范围内：

- Tool 注册配置的持久化格式；
- 跨 Runtime 的 Tool 分发；
- Turn cold recovery 时如何证明并重建完全相同的 Tool executor implementation；
- ToolResult large-content reference 和 executor identity 的 exact storage payload；
- 具体操作系统 Sandbox 实现；
- MCP transport、插件协议和 provider tool-search adapter 的实现细节。

## `for_turn` 的 Turn 边界

接口命名为 `ToolService::for_turn` 而非 `begin_turn`，因为后者会暗示 Tool 子系统负责创建 Turn 或改变 Turn 状态：

```rust
ToolService::for_turn(context) -> ToolSet
```

准确语义是：

1. Session execution 已预留 admission slot 和 candidate TurnId，但领域 Turn 尚未发布；
2. Session execution 在第一次模型调用前构造 `ToolTurnContext`；
3. Session execution 调用 `ToolService::for_turn(context)`；
4. ToolService 原子捕获当前可用 Tool，并返回不可变 `ToolSet`；
5. ToolSet 的 `prompt_view()` 被同一个 candidate Context 的 PromptSet 固定；
6. initiating UserMessage append 成功后，同一 Turn 的所有模型调用和 ToolCall 执行复用该 ToolSet；
7. admission 失败或 Turn 到达 terminal status 后释放 ToolSet。

因此：

- `for_turn` 从 Turn 执行边界开始；
- `for_turn` 是 `ToolService` 的方法；
- `for_turn` 不是 `Turn` 领域对象的方法；
- ToolService 不创建 Turn，也不修改 TurnStatus；
- ToolSet 是执行期对象，不写入 Turn 领域对象。

ToolSet 的“不可变”指 ToolSpec、Exposure、route 和 Deferred index 在该 Turn 内不改变。每次ToolCall的进度、terminal state、approval waiter、cancellation和资源锁属于执行状态，不改变ToolSet的有效工具快照。完整admission、AgentLoop、append/apply conversation projection和logical model-call关系见 [Turn 执行模块与执行上下文架构设计](turn-execution-context.md)。

## 决策摘要

本设计确立以下决策：

- 一个 MiniCoreRuntime 初始化并拥有一个 `Arc<ToolService>`；
- Agent、Session 和 Turn 不保存 Tool definitions、Tool overrides、active tools、executor 或 ToolSet；
- Tool 注册时原子绑定模型定义和真实 executor；
- `ToolService` 是长生命周期深模块，隐藏注册表、策略、审批、Sandbox、Hook、锁和 Deferred index；
- `ToolSet` 是某个 Turn 的不可变有效工具快照；
- `ToolSet::prompt_view()` 提供给 PromptSet 的模型安全工具投影；
- `ToolSet::fingerprint()` 标识 ToolSpec、Exposure、route、Workspace、policy 和 provider projection 的有效组合；
- Session execution 为模型 ToolCall 分配 ItemId，`ToolSet::execute()` 接收 ToolExecutionRequest 并返回一一对应的 ToolExecutionOutcome；
- 注册全集、模型披露集和已授权执行集是三个不同集合；
- Tool exposure 使用 `Direct / Deferred / Hidden`；
- 静态 Tool annotations 只用于模型提示和调度，不作为安全授权依据；
- 权限判断基于规范化参数生成的动态 `ToolRequirements`；
- PreToolUse 修改参数后必须重新校验并重新计算 ToolRequirements；
- 参数在权限判断和审批前冻结，审批后不允许修改；
- 所有直接、Deferred 和内部调用共享唯一执行链；
- approval 只授予执行资格，Sandbox 继续执行强制限制；
- 同批调用串行完成 preflight，再根据并发模式和资源锁执行；
- 首版ask-user route在同批副作用ToolCall前独占等待，不持有资源锁，回答后其余调用才进入普通调度；
- ToolExecutionOutcome 按原始 ToolExecutionRequest 顺序返回；
- 正常、错误、拒绝和 confirmed cancellation 产生 truthful ToolResult；executor/side-effect outcome unknown 返回 Abandoned，不生成 ToolResult。

## 对象关系

```text
MiniCoreRuntime
└─ Arc<ToolService>
   ├─ ToolRegistry
   ├─ ToolHooks
   ├─ ToolAuthorization
   │  ├─ ToolPolicy
   │  ├─ ToolGrantStore
   │  └─ ToolSandbox
   ├─ ToolResourceLocks
   └─ DeferredToolIndex

candidate Turn admission
└─ ToolService::for_turn(ToolTurnContext)
   └─ ToolSet
      ├─ Turn-scoped ToolExecutionControl
      ├─ ToolPromptView
      ├─ ToolSetFingerprint
      ├─ immutable ToolName → Arc<dyn Tool> routes
      ├─ DeferredToolIndex snapshot
      └─ execute(ToolExecutionRequest[]) → ToolExecutionOutcome[]
```

## 最小外部 interface

Tool 子系统只向调用方暴露三个主要对象：`Tool`、`ToolService` 和 `ToolSet`。

```rust
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;

    fn requirements(
        &self,
        arguments: &Value,
        context: &ToolCallContext,
    ) -> Result<ToolRequirements, ToolError>;

    async fn execute(
        &self,
        invocation: ToolInvocation<'_>,
    ) -> ToolResult;
}

pub struct ToolService {
    registry: ToolRegistry,
    hooks: Arc<dyn ToolHooks>,
    authorization: ToolAuthorization,
    locks: ToolResourceLocks,
}

impl ToolService {
    pub fn new(config: ToolServiceConfig) -> Self;

    pub fn register(
        &self,
        tool: Arc<dyn Tool>,
    ) -> Result<(), ToolRegistrationError>;

    pub fn for_turn(
        &self,
        context: ToolTurnContext,
    ) -> ToolSet;
}

pub struct ToolExecutionRequest {
    pub item_id: ItemId,
    pub call: ToolCall,
}

pub enum ToolExecutionOutcome {
    Completed {
        item_id: ItemId,
        source: ToolOutcomeSource,
        result: ToolResult,
    },
    Abandoned {
        item_id: ItemId,
        reason: ToolAbandonReason,
    },
}

pub struct ToolSet;

impl ToolSet {
    pub fn prompt_view(&self) -> ToolPromptView;

    pub fn fingerprint(&self) -> &ToolSetFingerprint;

    pub async fn execute(
        &self,
        requests: Vec<ToolExecutionRequest>,
    ) -> Vec<ToolExecutionOutcome>;
}
```

`ToolExecutionOutcome::Completed` 是truthful result candidate。Session execution把它append为matching `role = tool` message后，对应ToolInvocation进入Completed operational state；只有后续`tool_round_completed`event成功append后，该结果才进入模型conversation。`Abandoned`没有Tool message，也不进入ToolRound。

ToolService 在 Runtime 初始化时一次性注入真实 seam：

```rust
pub struct ToolServiceConfig {
    pub hooks: Arc<dyn ToolHooks>,
    pub sandbox: Arc<dyn ToolSandbox>,
    pub policy: ToolPolicyConfig,
    pub deferred_result_limit: usize,
}
```

Approval 和 UserQuestion delivery 都不是 Runtime-global ToolService dependency。Session execution 在 Turn capture 时提供一个 Turn-scoped internal `ToolExecutionControl`；它是 Tool 到 SessionExecutor 的 crate-internal producer seam。无交互环境由该 interface durable resolve 为 fail-closed decision，不使用 `None` 表示隐式允许。TUI、Web、GUI 和 RPC 只是接收 UI-safe Interaction view 的 Adapter，不直接持有 waiter 或 writer。

调用方不需要直接了解 ToolRegistry、ToolPolicy、Interaction persistence、Sandbox、Hook 顺序、资源锁或 Deferred lookup。

## Tool 注册

Tool 是定义和 executor 的原子注册单元：

```text
Arc<dyn Tool>
├─ ToolSpec
├─ ToolRequirements derivation
└─ execute
```

ToolRegistry 使用 `ToolName` 作为 Runtime 内的规范 key：

```rust
pub struct ToolRegistry {
    tools: RwLock<HashMap<ToolName, Arc<dyn Tool>>>,
}
```

基础规则：

- 同一个 ToolName 在一个 Registry snapshot 内只能绑定一个 Tool；
- 重复注册返回 ToolRegistrationError，不能静默覆盖已有 executor；
- ToolSpec 和 executor 不能分别注册；
- `search_tools` 和 `invoke_tool` 是保留名称，普通 Tool 不能注册或覆盖；
- ToolSet 创建时原子捕获 ToolSpec 和对应的 `Arc<dyn Tool>`；
- ToolSet 创建后的新注册不影响该 ToolSet；
- 后续 Turn 创建的新 ToolSet 可以看到新的 Registry 状态。

Tool replace 和 unregister 留待插件生命周期确有需求时再增加专用 interface，不复用 register 的语义。

当前使用 ToolName 作为运行时注册和调用 identity。持久化 Tool definition identity 或 versioning 留待确有 replay、migration 或审计需求时再定义。

## ToolSpec 和 Exposure

```rust
pub struct ToolSpec {
    pub name: ToolName,
    pub description: String,
    pub input_schema: JsonSchema,
    pub output_schema: Option<JsonSchema>,
    pub exposure: ToolExposure,
    pub annotations: ToolAnnotations,
}

pub enum ToolExposure {
    Direct,
    Deferred,
    Hidden,
}
```

Exposure 语义：

| Exposure | 模型披露 | 允许的调用来源 |
| --- | --- | --- |
| `Direct` | 进入 `ToolSet::prompt_view()` 的 ToolSpec | 模型直接 ToolCall |
| `Deferred` | 只通过 Tool search 披露 | Deferred lookup 后的调用 |
| `Hidden` | 不向模型披露 | Runtime 内部调用 |

模型产生一个未披露 ToolName 不代表该调用有效。ToolSet 必须结合内部 `ToolCallSource` 校验 Exposure：

```rust
pub enum ToolCallSource {
    ModelDirect,
    Deferred,
    Internal,
}
```

`ToolCallSource` 是 ToolSet 内部路由状态，不从模型 arguments 或 function-call payload 反序列化。普通模型 ToolCall 标记为 `ModelDirect`；`invoke_tool` 展开后标记为 `Deferred`；Runtime 内部入口标记为 `Internal`。

公开的`ToolSet::execute(Vec<ToolExecutionRequest>)`将request.call视为ModelDirect，并识别保留的`invoke_tool` request。provider原生deferred loading由provider adapter规范化为同一个`invoke_tool` request，再进入ToolSet；Deferred和Internal来源不能由普通调用方直接指定。

基础约束：

| ToolCallSource | Direct | Deferred | Hidden |
| --- | --- | --- | --- |
| `ModelDirect` | 允许 | 拒绝 | 拒绝 |
| `Deferred` | 拒绝 | 允许 | 拒绝 |
| `Internal` | 允许 | 允许 | 允许 |

Internal 来源只能通过 ToolService 私有入口创建，仍然经过参数校验、Hook、policy、approval 和 Sandbox。模型和普通外部调用方不能指定 Internal 来源。

## ToolAnnotations 和 ToolRequirements

ToolAnnotations 是静态提示：

```rust
pub struct ToolAnnotations {
    pub read_only_hint: bool,
    pub destructive_hint: bool,
    pub idempotent_hint: bool,
    pub open_world_hint: bool,
    pub concurrency: ToolConcurrency,
}

pub enum ToolConcurrency {
    Parallel,
    Serial,
}
```

这些字段帮助模型理解工具，并帮助 ToolSet 选择默认调度方式。它们不能单独授予文件、进程或网络权限。

ToolRequirements 根据规范化后的具体参数动态生成：

```rust
pub struct ToolRequirements {
    pub permissions: PermissionSet,
    pub resources: Vec<ToolResourceAccess>,
}

pub struct ToolResourceAccess {
    pub key: ToolResourceKey,
    pub mode: ToolResourceMode,
}

pub enum ToolResourceMode {
    Read,
    Write,
    Exclusive,
}
```

PermissionSet 的最小形状：

```rust
pub struct PermissionSet {
    pub filesystem: Vec<FilePermission>,
    pub network: Vec<NetworkPermission>,
    pub processes: Vec<ProcessPermission>,
    pub environment: Vec<EnvironmentPermission>,
}
```

空集合表示不请求对应权限。具体 path、network target、process 和 environment 表达留给后续细化，但 PermissionSet 必须支持确定性规范化、交集计算和 fingerprint。

例如，同一个 Bash Tool 可以因为具体命令不同产生完全不同的 ToolRequirements。ToolPolicy、approval 和 Sandbox 都必须使用动态 requirements，不可信任静态 read-only hint。

## ToolCall、Invocation 和 Result

模型返回的原始调用：

```rust
pub struct ToolCall {
    pub call_id: ToolCallId,
    pub name: ToolName,
    pub arguments: Value,
}
```

ToolSet resolve 后生成私有路由值，携带可信调用来源和目标 executor：

```rust
struct ResolvedToolCall {
    call: ToolCall,
    source: ToolCallSource,
    tool: Arc<dyn Tool>,
}
```

每次调用使用的安全上下文：

```rust
pub struct ToolCallContext<'a> {
    pub turn: &'a ToolTurnContext,
    pub source: ToolCallSource,
    pub tool_name: &'a ToolName,
}
```

ToolCallContext 不包含可由模型覆盖的安全字段；conversation content 如需参与 Hook，只能以只读安全投影提供。

传给 executor 的调用已经完成路由、参数规范化、Workspace authorization、policy、approval 和 Sandbox 准备。executor 只能看到不含 capability handle 的窄上下文：

```rust
pub struct ToolExecutionContext<'a> {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub source: ToolCallSource,
    pub tool_name: &'a ToolName,
}

pub struct ToolInvocation<'a> {
    pub call_id: &'a ToolCallId,
    pub arguments: &'a Value,
    pub context: ToolExecutionContext<'a>,
    pub updates: &'a dyn ToolUpdateSink,
}
```

`ToolInvocation.arguments` 在 approval 后保持不可变。`ToolExecutionContext` 不包含 `ToolTurnContext`、WorkspaceToolContext、WorkspaceAccessView、approval interface、grant store 或 Sandbox handle；普通 executor 不能在 requirements 冻结后重新请求其他 path capability。完整 `ToolCallContext` 只供 Tool requirements、Hook 和 ToolAuthorization 的 preflight 使用。

最终结果：

```rust
pub struct ToolResult {
    pub call_id: ToolCallId,
    pub disposition: ToolResultDisposition,
    pub content: ToolResultContent,
    pub details: Option<Value>,
}

pub enum ToolResultDisposition {
    Succeeded,
    Failed,
    Denied,
    Cancelled,
}
```

ToolResult 既表达 executor success，也表达 unknown tool、schema error、Hook deny、policy deny、approval reject、Sandbox deny、timeout 和 confirmed cancellation。provider adapter 可以从 disposition 派生 `is_error`。side-effect outcome unknown 时不构造 ToolResult；对应 ToolInvocation Item 进入 Abandoned。

流式更新使用最小 interface：

```rust
pub trait ToolUpdateSink: Send + Sync {
    fn emit(&self, update: ToolUpdate);
}

pub enum ToolUpdate {
    Started { call_id: ToolCallId },
    Progress { call_id: ToolCallId, message: String },
    Output { call_id: ToolCallId, content: ToolResultContent },
    Completed { call_id: ToolCallId },
}
```

## ToolSet 构建

candidate Turn admission 在第一次模型调用前执行：

```rust
let tool_set = runtime
    .tools
    .for_turn(tool_turn_context);
```

ToolService 在 `for_turn()` 内完成：

```text
读取 Registry 的一致快照
→ 根据 ToolTurnContext 过滤当前 Turn 可使用的工具
→ 建立 ToolName → Arc<dyn Tool> routes
→ 提取 Direct ToolSpec
→ 建立 DeferredToolIndex snapshot
→ 根据 provider 能力生成原生 deferred 投影，或注入 fallback search_tools / invoke_tool specs
→ 计算 ToolSetFingerprint
→ 返回不可变 ToolSet
```

ToolTurnContext 至少需要表达：

```rust
pub struct ToolTurnContext {
    pub agent: AgentRevisionRef,
    pub session_id: SessionId,
    pub session_revision: SessionDefinitionRevision,
    pub turn_id: TurnId,
    pub workspace: WorkspaceToolContext,
    pub provider: ProviderCapabilities,
    execution_control: Arc<dyn ToolExecutionControl>,
    pub cancellation: CancellationToken,
    pub updates: Arc<dyn ToolUpdateSink>,
}
```

ToolTurnContext 是 crate-internal execution input；private `execution_control` 字段只能由 Session execution 注入。它不进入 Agent、Session 或 Turn 的持久领域字段。它必须来自同一个 captured SessionDefinitionRevision，不能按 AgentId/SessionId 回查 mutable current heads；完整规则见 [Agent 与 Session 生命周期架构设计](agent-session-lifecycle.md)。

`WorkspaceToolContext` 由本 Turn pin 的 `WorkspaceSnapshot` 投影，包含 canonical cwd、`WorkspaceAccessView`、authorization lease 和 stable fingerprint。它是 filesystem capability ceiling：ToolRequirements、ToolPolicy、approval 和 grant 只能进一步收紧，不能扩大该 view。ToolService 不自行 canonicalize Workspace roots，也不从 trust 推断权限。完整规则见 [Workspace 子系统架构设计](workspace.md)。

## ToolSet Fingerprint

`ToolSetFingerprint` 至少覆盖：

```text
稳定排序后的规范化 ToolSpec
ToolExposure 和 Deferred projection
ToolName → executor route identity
WorkspaceToolFingerprint / WorkspaceAccessFingerprint
Tool policy revision
provider capability projection
ToolSet capture algorithm version
```

锁状态、approval waiter、随机指针、cancellation 和 ToolUpdate 不进入 fingerprint。

ToolSetFingerprint 首先保证同一进程内模型披露与 executor route 的一致性。exact cold recovery 还要求 Tool 注册提供可持久重建的 implementation identity/version；如果某个 route 只有进程内 identity，manifest 可以记录为不可恢复，host restart 后必须 fail closed，不能用同名 current Tool 静默替代。

`ToolPromptView.tool_set_fingerprint` 携带 parent ToolSetFingerprint，必须等于来源 ToolSet 的 fingerprint。该字段不进入模型 payload，只用于 PromptSet/ToolSet cross-binding。

## Tool 组装给模型

同一个 ToolSet 同时保存模型披露和执行路由，因此模型看到的 schema 与 ToolCall 实际解析到的 executor 来自同一快照：

```rust
let tool_set = runtime.tools.for_turn(context);
let tool_view = tool_set.prompt_view();
let prompt_set = runtime.prompt_service.for_turn(PromptTurnContext {
    tools: tool_view,
    // ...
}).await?;
```

后续模型调用只使用 PromptSet 已固定的工具投影：

```text
ToolSet.prompt_view().ToolSpec
→ PromptSet
→ AssembledModelContext.tools
↔ 同一个 ToolSet 内的 executor route
```

assembly 不再接收另一个任意 ToolPromptView。

## Deferred Tool

大工具集使用渐进披露。provider 原生支持 deferred loading 时，provider adapter 可以将 Deferred ToolSpec 转换成原生 tool search 机制。

provider 不支持时，ToolSet 向模型添加两个保留工具名：

```text
search_tools
invoke_tool
```

命名规则：

- 模型可见名称使用 `search_tools` 和 `invoke_tool`；
- 不使用 `CallToolTool`、`InvokeToolTool` 等重复名称；
- `search_tools` 查询当前 ToolSet 的 DeferredToolIndex；
- `invoke_tool` 是 ToolSet 的保留路由调用，不作为普通目标 executor 嵌套执行；
- `search_tools` 返回最多 N 个当前 ToolSet 内的完整 Deferred ToolSpec，N 由 ToolService 配置确定。

Fallback 流程：

```text
模型调用 search_tools(query)
→ 只搜索当前 ToolSet 中允许的 Deferred tools
→ 返回少量 ToolName、description 和 input schema

模型调用 invoke_tool(name, arguments)
→ ToolSet校验invoke_tool request
→ 将调用展开为 source = Deferred 的真实 ToolCall
→ 校验目标 Exposure == Deferred
→ 对真实目标执行一次完整 pipeline
```

`invoke_tool` 必须在参数校验、Hook、policy、approval 和 Sandbox 之前展开。这样 executor、approval UI、audit 和 PostToolUse 看到的都是实际目标 Tool，且不会产生双重审批、双重 Hook 或嵌套调度。

`invoke_tool` 不能调用 Direct 或 Hidden Tool，防止模型借助代理入口绕过直接披露和内部可见性规则。

目标不存在、目标不是Deferred、invoke_tool request schema无效时，ToolSet 返回 `ToolExecutionOutcome::Completed`，其中包含统一的 failed ToolResult，并记录精确内部 audit 原因。模型可见错误不泄漏 Hidden Tool 是否存在。

## 参数准备和 Hook

ToolCall 参数来自模型，必须视为不可信输入：

```text
原始 JSON
→ route 和 Exposure 校验
→ schema 校验
→ 默认值和兼容字段处理
→ 路径、枚举等确定性规范化
→ PreToolUse
→ 若参数改变则重新校验和规范化
→ 参数冻结
```

Hook 使用固定锚点，不引入通用 middleware `next()` 链：

```rust
pub trait ToolHooks: Send + Sync {
    async fn before(
        &self,
        call: &ToolCall,
        context: &ToolCallContext,
    ) -> BeforeToolUseDecision;

    async fn after(
        &self,
        call: &ToolCall,
        result: &mut ToolResult,
        context: &ToolCallContext,
    );
}

pub enum BeforeToolUseDecision {
    Continue,
    Rewrite(Value),
    Deny(String),
}
```

ToolService 接收一个 ToolHooks adapter。需要多个 Hook 时由该 adapter 在内部按固定顺序组合，ToolSet 不暴露 middleware chain 或 `next()` 语义。

基础规则：

- PreToolUse Hook failure 默认 fail closed；
- Hook 只能拒绝调用或重写 arguments，不能修改 call_id、ToolName 或 ToolCallSource；
- Rewrite 后必须重新执行 schema 校验和规范化；
- Rewrite 后必须重新生成 ToolRequirements；
- policy 和 approval 只能看到最终冻结参数；
- PostToolUse 可以脱敏、截断和格式化模型可见结果；
- PostToolUse 发生时 executor 副作用已经发生，不能把它当成安全阻断点；
- raw execution outcome 应先进入 audit/telemetry，再允许 PostToolUse 修改模型可见结果；
- PostToolUse failure 不会重新执行 Tool；原始 outcome 保留在 audit，模型获得明确的 post-processing error；
- ToolSet 内部按 call_id 保存局部 terminal guard；每个 ToolCall 的 PostToolUse 和 terminal lifecycle 只能执行一次。

## Policy、Approval 和 Sandbox

`ToolAuthorization` 是内部深模块，对 ToolSet 隐藏权限决策、grant、approval 和 Sandbox 组合顺序：

```text
Frozen ToolCall + ToolRequirements + WorkspaceAccessView
→ hard deny rules
→ WorkspaceAccessView.authorize(file requirements)
→ ToolPolicy
→ existing ToolGrant lookup（绑定 WorkspaceAccessFingerprint）
→ 可选 ToolApprovalRequest
→ ToolApprovalDecision
→ Sandbox permissions
→ narrow ToolInvocation
→ execute
```

### Tool Execution Control

权威 execution seam：

```rust
pub struct ToolExecutionIntentStamp {
    pub call_id: ToolCallId,
    pub resolved_tool: ToolName,
    pub invocation_fingerprint: ContentHash,
    pub requirements_fingerprint: ContentHash,
    pub authorization_fingerprint: ContentHash,
}

pub enum ToolOutcomeSource {
    PreExecution,
    Executed {
        execution_started_entry_id: EntryId,
    },
}

pub(crate) trait ToolExecutionControl: Send + Sync {
    async fn request_approval(
        &self,
        item_id: ItemId,
        request: ToolApprovalRequest,
    ) -> Result<ToolApprovalDecision, ToolExecutionControlError>;

    async fn request_user_question(
        &self,
        item_id: ItemId,
        request: UserQuestionRequest,
    ) -> Result<UserQuestionAnswer, ToolExecutionControlError>;

    async fn record_execution_start(
        &self,
        item_id: ItemId,
        intent: ToolExecutionIntentStamp,
    ) -> Result<EntryId, ToolExecutionControlError>;
}

pub trait ToolSandbox: Send + Sync {
    async fn execute(
        &self,
        permissions: &PermissionSet,
        tool: &dyn Tool,
        invocation: ToolInvocation<'_>,
    ) -> ToolResult;
}
```

ToolExecutionControl由SessionExecutor提供；请求进入per-session bounded `ToolControlQueue`，但所有durable mutation仍由同一个SessionExecutor/SessionWriter执行：

```text
InteractionRequested append → host delivery
InteractionResolved append → waiter wake
ToolExecutionStarted append → side effect
```

`ToolControlQueue`不与普通Submit/Steer形成全局FIFO。SessionExecutor处理`record_execution_start`前必须观察最新`EmergencyControl` epoch并重新验证cancellation与Workspace lease。Cancel/revocation先被观察则拒绝start；`ToolExecutionStarted`先append/apply则side effect可以开始，之后必须保存truthful outcome。

`request_user_question`只允许在pre-execution ask-user route中使用：它必须发生在`ToolExecutionStarted`、`ToolResourceLocks`和外部副作用之前。等待期间不取得或持有资源锁、`WorkspaceCommitAuthorization`或其他跨Session资源；该调用的Tool future等待typed answer，但SessionExecutor本身返回主循环继续处理control、deadline和Snapshot。首版ask-user route在一个ToolRound中独占等待：ToolSet先完成该route，再允许同一assistant step的其他ToolCall进入普通调度。

TUI、RPC、Web和GUI通过[Runtime Interface](runtime-interface.md)接收UI-safe Interaction StateEvent并提交resolution；它们是Presentation Adapter，负责展示、表单和本地交互，不是ToolService Adapter，也不直接持有Tool waiter。ToolSandbox仍可以有不同操作系统或容器Adapter。

ToolExecutionControl是crate-internal execution seam，不建立InteractionService、ToolLedgerService或第二writer。其实现只能通过当前SessionWriter append已定义event entry，并在返回前完成storage-owned `apply_committed`；`request_user_question`完成`InteractionRequested → host delivery → InteractionResolved`，`record_execution_start`成功返回exact `EntryId`后才允许side effect。ToolSet把该ID带入`ToolOutcomeSource::Executed`，Session execution随后用它构造可验证的role=tool message。

审批请求至少包含：

```rust
pub struct ToolApprovalRequest {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub arguments: Value,
    pub requirements: ToolRequirements,
    pub reason: String,
    pub grant_suggestions: Vec<ToolGrantSuggestion>,
}
```

审批决策：

```rust
pub enum ToolApprovalDecision {
    AllowOnce,
    AllowForTurn,
    AllowForSession,
    AllowWith {
        permissions: PermissionSet,
    },
    Deny {
        reason: String,
    },
}
```

基础不变量：

- approval request 保存冻结参数和 ToolRequirements fingerprint；
- ToolExecutionControl 返回的 approval decision 不可扩大 ToolPolicy 已经限制的权限；
- WorkspaceAccessView 必须在 grant lookup 和 approval 前把所有文件 requirements 转换为 `AuthorizedWorkspacePath`；
- 任一文件 requirement 不在 WorkspaceAccessView ceiling 内时，在 approval 和 executor 前拒绝；
- 最终 PermissionSet 是 ToolRequirements、WorkspaceAccessView、ToolPolicy 上限、已有 grant 和 approval decision 的交集；
- `AllowWith` 以 ToolPolicy 允许的 ToolRequirements 为上限，只能进一步收紧；交集无法满足执行要求时在 executor 前拒绝；
- grant 使用明确 key 保存，不从一条 resolved Interaction 隐式推断；
- ToolExecutionControl unavailable或durable append已确定NotCommitted时，Ask决策fail closed且不执行Tool；SessionWrite OutcomeUnknown时poison writer并保守终结、不在本run重试该append，未终结前既不通知host也不执行Tool，恢复靠下次load的committed prefix；validation/policy/approval deny返回PreExecution outcome，不伪造ToolExecutionStarted；
- approval 通过后仍然执行 Sandbox；
- Sandbox 执行失败不能静默回退到无 Sandbox 执行；
- 是否允许经过新审批后进行 Sandbox escalation，留待具体工具策略决定。

基础 grant key：

```rust
pub struct ToolGrantKey {
    pub scope: ToolGrantScope,
    pub tool_name: ToolName,
    pub arguments_hash: ContentHash,
    pub requirements_hash: ContentHash,
    pub workspace_access_fingerprint: WorkspaceAccessFingerprint,
    pub policy_revision: PolicyRevision,
}

pub enum ToolGrantScope {
    Turn(TurnId),
    Session(SessionId),
}

pub struct ToolGrantSuggestion {
    pub scope: ToolGrantScope,
    pub permissions: PermissionSet,
    pub description: String,
}

pub struct PolicyRevision;
```

更宽泛的目录、命令或域名规则必须作为显式 ToolGrant rule 表达，不能通过忽略 arguments hash 自动扩大一次审批。

## 批量调度和资源锁

ToolSet 内部完成批量调度，不暴露独立 ToolScheduler 类。

```text
ToolExecutionRequest[]
→ 全程保留 item_id + call
→ 按原始顺序串行 preflight
→ 若存在内建 ask-user route，先独占完成该 route
→ 收集允许执行的调用
→ 根据 ToolConcurrency 和 ToolResourceAccess 调度
→ 并发执行无冲突调用
→ 按原始 request/call_index 返回 ToolExecutionOutcome[]
```

调度规则：

- `Serial` Tool 默认按 ToolName 串行；跨 Tool 冲突由 ToolResourceAccess 统一处理；
- `Parallel` Tool 只有在资源访问不冲突时并发执行；
- 同一资源允许并发 Read，Write 或 Exclusive 与冲突访问串行；
- 资源 key 来自规范化参数后的 ToolRequirements；
- 参数名称猜测不能作为安全资源锁的唯一来源；
- ToolCall event 可以按实际完成顺序流式发布；
- ToolExecutionOutcome 按原始 ToolExecutionRequest 顺序返回；complete ToolRound 只消费 Completed results；
- 首版ask-user route pending时不启动同一assistant step的其他ToolCall；它形成`PreExecution` outcome后，剩余调用才进入普通调度；
- schema、Hook 或 policy 对单个调用的拒绝形成该调用的 ToolResult；
- 用户显式拒绝或 Turn cancellation 停止后续 preflight，并为未执行调用生成取消结果；
- 所有等待 approval、锁和 executor 的操作都响应 Turn cancellation。

ToolResourceLocks 属于 ToolService，用于协调同一 Runtime 内跨 Turn 的共享 workspace 和外部资源；单个 ToolSet 只持有它的共享引用。

文件类 ToolResourceKey 必须基于 canonical filesystem namespace 和 fully canonical target identity，而不是 `WorkspaceId`、root fingerprint 或裸相对路径。对于尚不存在的 create/rename target，key 使用 canonical nearest-existing ancestor 加规范化剩余 path。这样两个 Session 即使使用不同 root anchor 指向同一物理目标，也会正确竞争同一把锁。外部资源使用对应实例的稳定 namespace/key。

## Tool 执行流程

```text
ToolSet::execute(ToolExecutionRequest[])
→ 使用 request.item_id 绑定 ToolInvocation / Interaction
→ 按 ToolCallSource resolve
→ 若为 invoke_tool，展开为真实 Deferred ToolCall
→ Exposure 校验
→ 参数 schema 校验和规范化
→ PreToolUse
→ 必要时重新校验和规范化
→ 冻结参数
→ Tool::requirements
→ WorkspaceAccessView.authorize(file requirements)
→ ToolAuthorization: workspace ceiling / policy / grant / approval
→ ToolResourceLocks
→ final control/revocation validation
→ ToolExecutionControl.record_execution_start → execution_started_entry_id
→ 再校验 revocation lease / cancellation
→ ToolSandbox
→ Tool::execute
→ raw outcome audit
→ PostToolUse
→ ToolExecutionOutcome::Completed {
     source = Executed { execution_started_entry_id },
     ToolResult candidate
   }
   或 Abandoned { reason }
→ 按原始 request 顺序返回
```

内建 ask-user route 使用同一执行入口，但不进入有副作用的后半段：

```text
preflight / schema / hook / policy
→ ToolExecutionControl.request_user_question
→ WaitingForUserInput（不取资源锁、不写ToolExecutionStarted）
→ UserAnswer / Cancelled / Expired
→ PreExecution ToolResult candidate
→ Session execution append role=tool message
```

普通 Tool 在已经`ToolExecutionStarted`或持有资源锁后不得调用`request_user_question`；若未来需要该能力，必须另行定义不持锁的producer protocol。

在record_execution_start之前形成的exact validation/policy/approval/unavailable ToolResult以`source = PreExecution`返回Completed；不进入ToolSandbox，也不伪造ToolExecutionStarted。Session execution随后append role=tool message。若该append已证明NotCommitted，只重试同一entry draft；若返回SessionWrite OutcomeUnknown，poison writer并保守终结当前操作、不在本run重放该append，恢复在下次load读committed prefix后按状态处理，不重放Tool。只有Tool side effect本身outcome unknown（工具副作用未知），或crash后exact result不可恢复，才返回/持久化Abandoned；此类不构造synthetic ToolResult。

所有调用来源必须进入该唯一执行路径：

```text
Direct model call ────────┐
Deferred invoke_tool ─────┼→ ToolSet::execute pipeline
Internal runtime call ────┘
```

任何 Tool 都不能通过直接访问 executor 绕过 Hook、policy、approval 或 Sandbox。

## LLM 循环

TurnExecutionContext 只创建一次 ToolSet。Turn execution 每次从 committed conversation 组装一次逻辑模型调用：

```rust
loop {
    let assembled = turn_context.assemble_model_context(PromptAssemblyInput::AgentRun {
        conversation: committed_conversation.view(),
        output_contract: None,
    })?;

    let output = model_gateway.generate(&assembled).await?;

    if output.tool_calls.is_empty() {
        match session_executor.handle_candidate_final(output).await? {
            CandidateFinalResult::Completed => break,
            CandidateFinalResult::ContinuedBySteer => continue,
        }
    }

    let assistant_entry = append_intermediate_assistant_message(output).await?;
    session_projections.apply_committed(&assistant_entry)?;
    let requests = tool_requests_from_committed_assistant(&assistant_entry)?;

    validate_control_and_authorization()?;
    let outcomes = turn_context
        .tool_set
        .execute(requests)
        .await;

    let results = require_all_completed_or_terminalize(outcomes).await?;
    let mut tool_entry_ids = Vec::with_capacity(results.len());
    for result in results {
        let tool_entry = append_tool_message(result).await?;
        session_projections.apply_committed(&tool_entry)?;
        tool_entry_ids.push(tool_entry.entry_id());
    }

    let completion = append_tool_round_completed(
        assistant_entry.entry_id(),
        tool_entry_ids,
    ).await?;
    session_projections.apply_committed(&completion)?;
    session_executor
        .after_committed_tool_round(completion)
        .await?; // pop/apply at most one queued Steer before the next model call
    committed_conversation = session_projections.committed_conversation();
}
```

ToolSet不决定Turn何时开始、完成、失败或中断，也不直接写conversation。`tool_round_completed`成功append后，assistant/tool messages才允许进入下一次逻辑模型调用；SessionExecutor随后按FIFO最多消费一条Steer，不能从该snippet直接绕过queue进入下一次assemble。

## Domain 投影

Tool 子系统与领域对象的关系：

```text
模型返回 ToolCall
→ Session execution append assistant/intermediate message
→ assistant tool_call content分配ItemId并投影ToolInvocation = Started
→ ToolSet.execute(ToolExecutionRequest { item_id, call })

ToolAuthorization 需要外部审批
→ ToolExecutionControl 创建 durable ToolApproval Interaction
→ Interaction 归属于同一个 ToolInvocation Item

内建 ask-user route
→ ToolExecutionControl 创建 durable UserQuestion Interaction
→ Presentation Adapter 展示并通过 Runtime facade 返回 UserAnswer
→ Interaction 归属于同一个 ToolInvocation Item

Tool 执行得到 truthful ToolResult
→ Session execution append role=tool message
→ 同一个 ToolInvocation Item = Completed { result }
→ 全部calls匹配后append tool_round_completed

side-effect outcome unknown
→ ToolInvocation Item = Abandoned
→ 不生成 ToolResult
```

Tool子系统返回typed ToolExecutionOutcome；SessionExecutor负责ToolInvocation、Interaction、Turn terminal result和conversation projection。完整领域语义见 [Turn、Item 与 Interaction 架构设计](turn-item-interaction.md)。

Agent、Session 和 Turn 不持有：

```text
RuntimeTools
AgentTools
SessionTools
TurnTools
ToolSet
ToolRegistry
Tool executor
active tool names
Tool approval state
Tool grants
Tool locks
```

领域所有权：

| 对象 | Owner |
| --- | --- |
| `Arc<ToolService>` 生命周期 | MiniCoreRuntime |
| Tool registration、Hook、policy、grant、Sandbox、locks | ToolService 内部实现 |
| 本 Turn 的有效工具快照 | TurnExecutionContext 内的 ToolSet |
| 模型调用循环、Tool message和ToolRound completion | SessionExecutor |
| ToolCall + ToolResult | 同一个 ToolInvocation Item |
| Tool approval request/decision | ToolInvocation-owned Interaction |

## 对象清单

| 分类 | 对象 |
| --- | --- |
| 外部对象 | `Tool`、`ToolService`、`ToolSet` |
| 外部 seam | `ToolHooks`、`ToolSandbox`、`ToolUpdateSink` |
| crate-internal execution seam | `ToolExecutionControl` |
| 私有实现 | `ToolRegistry`、`ToolAuthorization`、`ToolPolicy`、`ToolGrantStore`、`ToolResourceLocks`、`DeferredToolIndex` |
| 调用值类型 | `ToolCall`、`ToolExecutionRequest`、`ToolExecutionOutcome`、`ResolvedToolCall`、`ToolCallSource`、`ToolInvocation`、`ToolResult`、`ToolResultDisposition`、`ToolUpdate` |
| 定义与上下文 | `ToolServiceConfig`、`ToolPolicyConfig`、`ToolName`、`ToolSpec`、`ToolExposure`、`ToolAnnotations`、`ToolConcurrency`、`ToolTurnContext`、`ToolCallContext`、`ToolExecutionContext`、`ToolSetFingerprint`、`ToolPromptView` |
| 权限与资源 | `ToolRequirements`、`PermissionSet`、`FilePermission`、`NetworkPermission`、`ProcessPermission`、`EnvironmentPermission`、`ToolResourceAccess`、`ToolResourceKey`、`ToolResourceMode` |
| 审批与授权 | `ToolApprovalRequest`、`ToolApprovalDecision`、`ToolGrantKey`、`ToolGrantScope`、`ToolGrantSuggestion`、`PolicyRevision`、`BeforeToolUseDecision` |
| 保留模型路由 | `search_tools`、`invoke_tool` |
| Domain 投影 | `ItemContent::ToolInvocation`、`InteractionRequest::ToolApproval`、`InteractionRequest::UserQuestion` |

不建立以下独立对象：

```text
ToolSystem
RuntimeTools / AgentTools / SessionTools / TurnTools
RegisteredTool
ToolView
ToolRouter
ToolRunner
ToolScheduler
ToolMiddlewareChain
PreToolUsePipeline / PostToolUsePipeline
公开 PreparedToolCall / ApprovedToolCall
ToolResultBuilder
CallToolTool / InvokeToolTool
```

这些职责已经由 ToolService 和 ToolSet 两个深模块吸收，或作为局部实现状态存在。

## 基础不变量

- 一个 MiniCoreRuntime 初始化一个 ToolService；
- Agent、Session 和 Turn 领域对象不持有 Tool 属性；
- candidate admission 在领域 Turn 发布前、第一次模型调用前调用 `ToolService::for_turn`；
- 同一 Turn 的全部模型与 Tool 循环复用同一个不可变 ToolSet；
- PromptSet 在创建时固定该 ToolSet 的 ToolPromptView 和 ToolSetFingerprint；
- assembly 不能替换为另一个 ToolPromptView；
- ToolSpec 和 executor 原子注册并被同一个 ToolSet 快照捕获；
- 注册全集不等于模型披露集，模型披露集不等于已授权执行集；
- ModelDirect、Deferred 和 Internal 调用遵守各自 Exposure；
- `invoke_tool` 在完整 pipeline 前展开，并且只允许调用 Deferred Tool；
- Hook 参数重写后重新校验并重新计算 requirements；
- approval 绑定冻结参数和动态 requirements；
- WorkspaceAccessView 是文件权限硬上限，并在 grant lookup、approval 和 Sandbox 前生效；
- ToolGrantKey 绑定 WorkspaceAccessFingerprint，旧 access snapshot 的 grant 不能扩大新 snapshot；
- executor 只接收 narrow ToolExecutionContext，不能访问 WorkspaceAccessView 或重新授权未声明 path；
- approval 不替代 Sandbox；
- 每个调用最多执行一次 PostToolUse 和 terminal lifecycle；
- 正常、denied、failed 和 confirmed-cancelled ToolCall 都产生 truthful ToolResult；
- outcome-unknown ToolCall 不生成 ToolResult，对应 ToolInvocation 进入 Abandoned；
- ToolExecutionOutcome 按原始 request 顺序返回；只有全部 Completed results 才能进入 complete ToolRound；
- `tool_round_completed`前ToolResult不进入下一次逻辑模型调用；
- ToolSet 不修改 Turn 状态，也不写回 Agent、Session、Turn 或 conversation storage。

## 后续问题

1. ToolName 的 namespace 和插件命名冲突规则。
2. ToolSpec input/output schema 使用的具体 JSON Schema dialect。
3. AuthorizedWorkspacePath 如何映射到各平台 Sandbox adapter，并在 open/create/rename 时抵抗 TOCTOU。
4. ToolPolicy rule、grant key 和持久化格式。
5. ToolRequirements 的文件、网络、进程和资源表达能力。
6. ToolSandbox 的 Windows、Linux、容器和远程执行 adapter。
7. DeferredToolIndex 的搜索算法、排序和最大返回数量。
8. provider 原生 deferred loading 与 fallback `search_tools / invoke_tool` 的 adapter 规则。
9. ToolUpdate 的事件类型和流式输出背压。
10. Tool route/executor implementation fingerprint 的稳定生成和 cold recovery 规则；缺失时 recovery 必须 fail closed。
11. Tool executor implementation identity 与 large ToolResult content reference 的最终持久化细节。

## 设计进度

- [x] 确定 MiniCoreRuntime 初始化并拥有一个 `Arc<ToolService>`。
- [x] 确定 Agent、Session 和 Turn 领域对象不持有 Tool 属性。
- [x] 命名 `ToolService::for_turn`，而非会暗示 Turn 创建或状态变更的 `begin_turn`。
- [x] 确定 `for_turn` 在 candidate admission 期间、第一次模型调用前由 Session execution 调用。
- [x] 确定一个 Turn 的全部模型与 Tool 循环复用同一个不可变 ToolSet。
- [x] 确定 ToolSet 提供 ToolPromptView 和 ToolSetFingerprint，并被 PromptSet 固定。
- [x] 确定 Tool 定义和 executor 原子注册。
- [x] 确定 Direct、Deferred 和 Hidden exposure。
- [x] 确定静态 annotations 与动态 requirements 分离。
- [x] 确定参数校验、Hook、policy、approval、Sandbox 和 executor 的唯一执行链。
- [x] 确定 WorkspaceAccessView 是所有文件 ToolRequirements 的硬上限，grant/approval 不能扩大。
- [x] 确定 `search_tools / invoke_tool` 命名和 Deferred 防绕过规则。
- [x] 确定串行 preflight、并发执行、资源锁和稳定结果顺序。
- [x] 确定 ToolCall、ToolResult 合并为同一个 ToolInvocation Item。
- [x] 确定 Tool approval 是 ToolInvocation-owned durable Interaction。
- [x] 确定ToolExecutionControl由Session execution提供并遵守approval、UserQuestion和execution-start ordering；Tool outcome由Session execution append为tool message。
- [x] 确定 executor/side-effect outcome unknown 时 Abandoned，不生成 synthetic ToolResult。
- [x] 确定ToolInvocation/Interaction/ToolResult/ToolRound completion通过统一SessionWriter by-entry log持久化。
- [ ] 定义 ToolName namespace 和 schema dialect。
- [ ] 定义 ToolPolicy、grant、PermissionSet 和 ToolRequirements 的最终字段。
- [ ] 定义 ToolSandbox adapter 和 ToolUpdate event。
- [x] 定义ToolCall、approval、execution-start、tool message和complete ToolRound的by-entry持久化baseline。
- [ ] 定义 exact Tool executor identity 和 cold recovery 语义。
