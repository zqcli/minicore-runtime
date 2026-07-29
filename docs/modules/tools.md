# Tool 子系统架构设计

状态：当前权威架构（ADR 0124后，生产实现待启动）
日期：2026-07-29

## 目的

本文定义ToolService/ToolSet的注册、披露、schema validation、Hook、policy、Interaction approval、Sandbox、Session-local file mutation queue和executor流程。

ADR 0124后的关键变化：

- Tool side-effect start由SessionExecutor current-Runtime状态管理；
- Session ledger不保存`ToolExecutionStarted`；
- ToolResult不引用execution-start entry；
- Session ledger不保存`ToolRoundCompleted`；
- matching ToolResult集合完整后自动形成model-visible Tool exchange；
- restart不自动重放incomplete ToolCall。

## `for_turn` 的 Turn 边界

```text
Turn admission
→ capture exact Agent/Session/Workspace/shared roots/model capability
→ ToolService::for_turn
→ Arc<ToolSet>
```

ToolSet只服务一个Turn。shared resource reload不改变active ToolSet；future Turn捕获new ToolResourceView。

## 决策摘要

- ToolService是Runtime-owned shared deep module；
- ToolResourceView在initialize或shared `/reload`时原子替换；
- 每Turn构造独立immutable ToolSet；
- ToolPromptView只能由parent ToolSet投影；
- Tool schema validation、Hook、policy、approval、Sandbox和executor统一通过ToolSet；
- ToolCallId由ModelGateway adapter从provider/tool protocol归一化；协议无原生ID时生成response-local opaque ID；ItemId来自MiniCore；
- ToolInvocation Item贯穿call、Interaction、execution、result和recovery；
- Tool execution outcome为Completed(exact ToolResult)或Abandoned；
- pre-execution deny/failure产生truthful ToolResult；
- side-effect start不持久化；
- SessionExecutor owner-local start reservation与EmergencyControl排序；
- ToolResult append后更新Item；同一assistant所有calls有matching result后exchange自动进入conversation；
- ToolSet不直接写SessionStorage或推进AgentLoop；
- 每loaded Session拥有独立file mutation queue；
- 同Session同canonical file key FIFO，不同key可并发；
- 多文件/open-world/Serial Tool使batch按call order串行；
- 跨Session共享Workspace不协调；
- MVP不启用通用bash；
- production Sandbox adapter开始前必须关闭O1 capability enforcement门禁。

## 对象关系

```text
MiniCoreRuntime
├─ ToolService
│  ├─ ToolSourceAdapter*
│  ├─ current Arc<ToolResourceView>
│  └─ cache/registry internals
└─ loaded Session
   └─ TurnExecutionContext
      └─ Arc<ToolSet>
         ├─ ToolPromptView
         ├─ Tool route/requirements
         ├─ ToolExecutionControl
         └─ shared executor implementations
```

## 最小外部 Interface

```rust
pub struct ToolService {
    // private source/cache/current-build state
}

impl ToolService {
    pub async fn build_candidate(
        &self,
        source: ToolSourceSnapshot,
    ) -> Result<Arc<ToolResourceView>, ToolReloadError>;

    pub fn for_turn(
        &self,
        resources: Arc<ToolResourceView>,
        context: ToolTurnContext,
    ) -> Result<Arc<ToolSet>, ToolSetError>;
}
```

```rust
pub struct ToolSet {
    // private immutable routes/policy/views
}

impl ToolSet {
    pub fn prompt_view(&self) -> ToolPromptView<'_>;

    pub async fn execute(
        &self,
        request: ToolExecutionRequest<'_>,
        sink: &dyn ToolUpdateSink,
    ) -> ToolExecutionOutcome;
}
```

ToolService/ToolSet不暴露generic registry mutation、raw executor、SessionWriter或Workspace absolute path。

## Tool 注册

```rust
pub struct ToolDefinition {
    pub name: ToolName,
    pub description: Arc<str>,
    pub input_schema: ToolInputSchema,
    pub annotations: ToolAnnotations,
    pub requirements: ToolRequirements,
    executor: Arc<dyn ToolExecutor>,
}
```

ToolName是stable selection/protocol key。同一个ToolResourceView内duplicate ToolName拒绝candidate publication。

MVP Tool来源：

- Runtime builtin；
- explicit configured extension/tool source；
- deferred Tool descriptor。

不从Workspace任意可执行文件自动注册Tool。

## ToolSpec 和 Exposure

```rust
pub struct ToolSpec {
    pub name: ToolName,
    pub description: Arc<str>,
    pub input_schema: ToolInputSchema,
}

pub struct ToolPromptView<'a> {
    specs: &'a [ToolSpec],
}
```

MVP不增加独立guidelines字段。Prompt只能看到ToolSet允许披露的spec；executor route、permissions和Sandbox internals不进入模型上下文。

Tool disclosure与execution route来自同一个ToolSet，caller不能伪造只披露不执行或执行未披露Tool的组合。

## ToolAnnotations 和 Requirements

```rust
pub struct ToolAnnotations {
    pub read_only: bool,
    pub destructive: bool,
    pub open_world: bool,
    pub parallelism: ToolParallelism,
}

pub enum ToolParallelism {
    Parallel,
    Serial,
}

pub struct ToolRequirements {
    pub filesystem: FilesystemRequirement,
    pub network: NetworkRequirement,
    pub process: ProcessRequirement,
}
```

annotations用于UI/policy/scheduling提示，requirements用于实际authorization/Sandbox决策。二者不由模型控制。

## ToolCall、Invocation 和 Result

```rust
pub struct ToolCall {
    pub id: ToolCallId,
    pub name: ToolName,
    pub arguments: Value,
    pub index: u32,
}

pub struct ToolExecutionRequest<'a> {
    pub item_id: ItemId,
    pub call: &'a ToolCall,
}
```

```rust
pub enum ToolExecutionOutcome {
    Completed {
        item_id: ItemId,
        call_id: ToolCallId,
        source: ToolOutcomeSource,
        result: ToolResult,
    },
    Abandoned {
        item_id: ItemId,
        call_id: ToolCallId,
        reason: ToolAbandonReason,
    },
}

pub enum ToolOutcomeSource {
    PreExecution,
    Executed,
}
```

ToolResult：

```rust
pub struct ToolResult {
    pub call_id: ToolCallId,
    pub disposition: ToolResultDisposition,
    pub content: ToolResultContent,
    pub details: Option<Value>,
}
```

不包含：

```text
execution_started_entry_id
ToolRoundId
SessionWriter receipt
provider-visible message wrapper
```

## ToolSet 构建

```rust
pub struct ToolTurnContext {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub agent: AgentRevisionRef,
    pub session_revision: SessionDefinitionRevision,
    pub workspace: WorkspaceToolContext,
    pub tool_calling: ToolCallingCapabilities,
    execution_control: Arc<dyn ToolExecutionControl>,
}
```

private execution_control只能由Session execution注入。

`for_turn`：

1. 从captured ToolResourceView解析selection；
2. 应用Agent/Session Tool policy；
3. 检查model tool-calling capability；
4. 结合WorkspaceToolContext计算route ceiling；
5. 构建immutableToolSet和ToolPromptView；
6. 不执行Tool、不发起Interaction、不写storage。

## Deferred Tool

Deferred Tool只有在模型选择后才加载完整definition/executor，但descriptor必须在ToolSet capture时固定：

```rust
pub struct DeferredToolDescriptor {
    pub name: ToolName,
    pub description: Arc<str>,
    pub input_schema: ToolInputSchema,
    pub source: ToolSourceRef,
}
```

lazy load只能读取captured source bytes/object。shared reload不改变active ToolSet。

load失败产生PreExecution ToolResult或typed Tool unavailable，不能静默执行另一个同名Tool。

## 参数准备和 Hook

```text
provider ToolCall
→ lookup disclosed route
→ schema validation
→ normalize typed arguments
→ PreToolUse hooks
→ policy/requirements
→ optional Interaction
→ Sandbox admission
→ owner-local start reservation
→ executor
→ PostToolUse hooks
→ ToolExecutionOutcome
```

Hook规则：

- hook不能修改ToolCallId；
- hook可以拒绝或重写arguments；
- rewrite后重新schema/policy/requirements验证；
- PreToolUse failure默认deny/fail closed；
- PostToolUse可以redact/normalize result，不能把unknown outcome伪造成success；
- hook不直接写SessionStorage、发布Runtime event或持有waiter。

## Policy、Approval 和 Sandbox

```text
ToolRequirements
+ WorkspaceAccessView
+ ToolPolicy
+ caller/runtime safety policy
→ ToolAuthorization
```

```rust
pub enum ToolAuthorization {
    Allow,
    Deny { reason: ToolDenyReason },
    Ask { request: ToolApprovalRequest },
}
```

MVP approval只支持per-call `AllowOnce`和进一步收紧权限的`AllowWith`，不保存Turn/Session grant。

approval不替代Sandbox。无法强制required capability时production executor必须在side effect前拒绝；该能力声明和差集算法由O1在production Tool/Sandbox adapter开始前冻结。

### Tool Execution Control

```rust
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

    async fn reserve_execution_start(
        &self,
        item_id: ItemId,
        call_id: ToolCallId,
    ) -> Result<ToolStartPermit, ToolExecutionControlError>;
}
```

`ToolStartPermit`是current-Runtime typed permit，不持久化：

- SessionExecutor验证current Turn/Item/call；
- 观察latest EmergencyControl；
- 原子把ToolOperationSlot从Prepared变为Running；
- permit只允许对应executor开始一次；
- drop未使用permit回滚到non-started terminal/cancel路径；
- process crash后permit和slot消失，不用于recovery。

`request_user_question`必须发生在file mutation ticket reservation和start permit前。

## 批量调度和 Session-local 文件 Mutation Queue

一个assistant response可以包含多个ToolCall。

调度规则：

- call按assistant index规范化；
- preflight/schema/policy按call order；
- ask-user route独占并先完成；
- `Serial`、multi-file或open-world Tool使batch按call order串行；
- 普通single-file Tool按canonical file key取得Session-local FIFO ticket；
- 不同key可以并发；
- result允许逆序完成；
- model-visible output始终按assistant call order。

```rust
pub struct SessionFileMutationQueue {
    // private per-loaded-session queues
}
```

mutation permit：

- 基于fully canonical target；
- create target使用nearest existing ancestor + normalized suffix；
- underlying I/O settle前不释放；
- Cancel可以忽略业务结果，但不能提前释放permit；
- 不跨Session/Runtime/process协调。

## Tool 执行流程

### PreExecution Result

以下路径不开始side effect：

```text
unknown tool
schema invalid
hook deny/failure
policy deny
approval deny
Sandbox unavailable
EmergencyControl wins before start
```

返回：

```text
Completed {
  source = PreExecution,
  result.disposition = Failed | Denied | Cancelled
}
```

Session executionappend role=tool message。

### Executed Result

```text
all validation passes
→ optional mutation ticket
→ reserve_execution_start
→ executor side effect
→ exact result
→ PostToolUse
→ release/settle resources
→ Completed { source = Executed }
```

ToolResult append失败不能重新执行Tool。

### Abandoned

```text
executor future/local process outcome unknown
remote operation outcome unknown
host restart loses exact result
```

返回/投影Abandoned，不构造ToolResult，不自动retry。

## LLM 循环

SessionExecutor流程：

```text
append Assistant(intermediate with ToolCalls)
→ ToolSet.execute calls
→ append each exact ToolResult as role=tool
→ SessionStorage matches all calls/results
→ final matching result produces CommittedToolExchangeDelta
→ AgentLoop.accept_committed_tool_results
→ optional Steer
→ next Model
```

ToolSet只返回outcome。它不能：

- append Assistant/Tool/terminal；
- 决定exchange complete；
- 调用AgentLoop；
- 开始下一次模型调用；
- 消费Steer/FollowUp。

## Domain 投影

assistant ToolCall append：

```text
ToolInvocation Item → Started
```

matching Tool message append：

```text
ToolInvocation Item → Completed(result)
```

ToolAbandoned：

```text
ToolInvocation Item → Abandoned
```

ToolSet只产出typed terminal outcome，不拥有durable conflict/replay判定。ToolResult/ToolAbandoned first-terminal规则、complete exchange形成、duplicate/orphan/incomplete隔离和模型可见性全部由ConversationStorage按[INV-003](../architecture.md#跨模块不变量索引)定义；Tools module不维护第二份projection算法。

## Cancellation 与 SecurityRevoked

- Prepared：Emergency先赢，不发permit，产生Cancelled result或Abandoned；
- Running：发送best-effort cancellation，等待executor teardown/result；
- Settling：等待resource/mutation permit安全释放；
- exact outcome可得：保存ToolResult；
- outcome unknown：Abandoned；
- 不声称回滚OS/provider side effect；
- 不依赖durable start marker。

## Recovery

restart不恢复：

```text
ToolStartPermit
ToolOperationSlot
executor future/process handle
mutation ticket
approval/UserQuestion waiter
```

SessionStorage replay：

- existingToolResult保留；
- complete exchange继续model-visible；
- incompleteexchange隔离；
- Running old Turn中断；
- Tool不自动重放；
- malformed记录局部skip并diagnostic。

## Error

```rust
pub enum ToolExecutionError {
    ToolUnavailable,
    InvalidArguments,
    HookFailed,
    PolicyDenied,
    ApprovalUnavailable,
    SandboxUnavailable,
    Cancelled,
    ExecutorFailed,
    OutcomeUnknown,
    InvariantViolation,
}
```

发生事实的module负责typed/redacted分类；SessionExecutor决定Turn继续或terminal。

## 对象清单

保留：

```text
ToolService
ToolResourceView
ToolSet
ToolPromptView
ToolDefinition/ToolSpec
ToolCall/ToolResult
ToolExecutionOutcome
ToolExecutionControl
ToolStartPermit（process-local）
SessionFileMutationQueue
```

不建立：

```text
ToolExecutionStarted durable event
execution_started_entry_id
ToolRound/ToolRoundCompleted
ToolManager/ToolLedgerService
cross-Session resource lock manager
Tool grant store
```

## 基础不变量

- Tool disclosure和execution route同源；
- active ToolSet immutable；
- ToolCallId/ItemId职责分离；
- schema/hook/policy/approval/Sandbox在side effect前；
- start permit由SessionExecutor current owner发放；
- permit不持久化；
- Running Tooltruthful settle；
- ToolResult append失败不重放Tool；
- complete call/result集合才进入model conversation；
- ToolSet不写SessionStorage或推进AgentLoop；
- ask-user不持mutation permit；
- 同Session同file mutation FIFO；
- 跨Session不协调；
- restart不自动重放Tool；
- production Sandbox capability不足时必须pre-execution拒绝。

## Test Matrix

至少覆盖：

- duplicate ToolName candidate失败；
- disclosed route与executor同源；
- schema invalid/unknown Tool PreExecution result；
- Hook rewrite后重新验证；
- approval allow/deny/family；
- UserQuestion在mutation/start前；
- ToolStartPermit只使用一次；
- start permit vs Cancel/Security双向race；
- Running Tool cancellation settlement；
- exact result append失败不重新执行；
- outcome unknown Abandoned；
- multi-call inverse completion；
- final matching result形成ordered exchange；
- missing/orphan result隔离，duplicate result first valid wins；
- ToolResult与ToolAbandoned冲突first valid terminal outcome wins；
- same file FIFO/different file parallel；
- multi-file/open-world Serial；
- Cancel期间mutation permit等待I/O settle；
- reload不改变active ToolSet；
- restart不恢复permit/task；
- Sandbox unenforceable capability gate（production adapter前）。

## 开放问题

1. O1：Sandbox enforcement capability schema与pre-execution差集算法；
2. production builtin Tool最小集合；
3. `max_tool_result_bytes`和future blob handling；
4. ToolStartPermit具体owner-local实现；
5. provider-specific ToolResult error lowering。

## 设计进度

- [x] ToolService/ToolSet ownership；
- [x] shared reload和per-Turn capture；
- [x] ToolSpec-only prompt view；
- [x] ToolCall/Item identity；
- [x] policy/approval/UserQuestion/Sandbox顺序；
- [x] Session-local file mutation queue；
- [x] 删除durable ToolExecutionStarted和ToolRoundCompleted；
- [x] complete Tool exchange自动projection；
- [ ] O1 production Sandbox gate；
- [ ] production implementation/tests。
