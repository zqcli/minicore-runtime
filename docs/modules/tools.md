# Tool 子系统架构设计

状态：当前权威架构（ADR 0133后，生产实现待启动）
日期：2026-07-31

## 目的

本文定义ToolService/ToolSet的注册、披露、schema validation、Hook、policy、Interaction approval、Sandbox、Session-local file mutation queue和executor流程。

ADR 0124后的关键变化：

- Tool side-effect start由ActiveTurnTask current-Runtime状态管理；
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
- Tools唯一拥有`ToolCall`、`ToolExecutionRequest`和`ToolExecutionOutcome`；
- ToolCallId由ModelGateway adapter从provider/tool protocol归一化；协议无原生ID时生成response-local opaque ID；ItemId来自MiniCore；
- ToolInvocation Item贯穿call、Interaction、execution、result和recovery；
- Tool execution outcome为Completed(exact ToolResult)或Abandoned；
- pre-execution deny/failure产生truthful ToolResult；
- side-effect start不持久化；
- ActiveTurnTask owner-local start reservation与EmergencyControl排序；
- ToolResult live apply后更新Item；同一assistant所有calls有matching result后exchange自动进入conversation；
- ToolSet不直接修改LiveSessionState、写SessionRecorder或开始下一次Model；
- 每loaded Session拥有独立file mutation queue；
- SessionExecutor在Turn capture前创建Turn-scoped execution control handle，并把该handle与Session-local mutation queue注入ToolSet；
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
   ├─ SessionExecutor
   │  └─ Arc<SessionFileMutationQueue>
   └─ TurnExecutionContext
      └─ Arc<ToolSet>
         ├─ ToolPromptView
         ├─ Tool route/requirements
         ├─ Turn-scoped ToolExecutionControl handle
         ├─ Arc<SessionFileMutationQueue>
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
        request: ToolExecutionRequest,
        sink: &dyn ToolUpdateSink,
    ) -> ToolExecutionOutcome;
}
```

ToolService/ToolSet不暴露generic registry mutation、raw executor、SessionRecorder或Workspace absolute path。

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
    pub tool_call_id: ToolCallId,
    pub name: ToolName,
    pub arguments: Value,
    pub call_index: u32,
}

#[derive(Clone)]
pub struct ToolExecutionRequest {
    pub item_id: ItemId,
    pub call: Arc<ToolCall>,
}
```

```rust
pub enum ToolExecutionOutcome {
    Completed {
        item_id: ItemId,
        source: ToolOutcomeSource,
        result: ToolResult,
    },
    Abandoned {
        item_id: ItemId,
        tool_call_id: ToolCallId,
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
    pub tool_call_id: ToolCallId,
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

`Succeeded`要求executor产生exact successful business result；`Failed`表示有truthful Tool/preflight error（例如unknown Tool、invalid arguments、Hook或exact executor failure）；`Denied`表示policy、approval、Workspace authority或Sandbox capability的pre-execution fail-closed拒绝；`Cancelled`只在能证明side effect未开始或executor返回exact cancellation result时使用。outcome unknown不能伪造成上述任一disposition，必须使用`ToolExecutionOutcome::Abandoned`。

这三个execution类型及`ToolResultDisposition`的完整shape只在Tools定义。`ToolCall`不保存ItemId；ActiveTurnTask在assistant response live apply前按同一candidate分配ItemId并构造`ToolExecutionRequest`，随后把Assistant、Started Items和expected set作为一个owner-local no-await mutation应用。`call_index`是validated assistant response中ToolCall的zero-based稳定顺序，由Session Execution按finalized content order规范化；provider adapter用于stream/final关联的内部index不作为mutation queue顺序来源。Turn/Item和storage可以投影`ItemId + ToolCallId`，但不得定义第二个execution input/outcome。

Raw ToolExecutor只返回业务payload、disposition或typed internal failure，不能选择ItemId/ToolCallId，也不能直接构造public outcome。ToolSet private outcome constructor从`ToolExecutionRequest`复制identity，并保证Completed中的`result.tool_call_id`与`request.call.tool_call_id`一致；mismatch属于internal invariant failure，不进入live reducer。

不包含：

```text
execution_started_entry_id
ToolRoundId
Session recording receipt
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
    mutation_queue: Arc<SessionFileMutationQueue>,
}
```

`execution_control`与`mutation_queue`均为crate-private capture输入。SessionExecutor在reserve candidate TurnId/control_generation后创建一个绑定该candidate与EmergencyControl的Turn-scoped control handle，并克隆本loaded Session拥有的mutation queue；两者在ActiveTurnTask spawn前进入`ToolTurnContext`。同一个control handle随后交给ActiveTurnTask使用，禁止task spawn后替换或二次注入。

`for_turn`：

1. 从captured ToolResourceView解析selection；
2. 应用Agent/Session Tool policy；
3. 检查model tool-calling capability；
4. 结合WorkspaceToolContext计算route ceiling；
5. 绑定Turn-scoped execution control与Session-local mutation queue，构建immutable ToolSet和ToolPromptView；
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
- hook不直接修改LiveSessionState、写SessionRecorder、发布Runtime event或持有waiter。

## Policy、Approval 和 Sandbox

```text
ToolRequirements
+ WorkspaceAccessView
+ ToolPolicy
+ caller/runtime safety policy
→ ToolAuthorization
```

```rust
pub(crate) enum ToolAuthorization {
    Allow,
    Deny { reason: ToolDenyReason },
    Ask { request: ToolApprovalRequest },
}
```

MVP approval只支持per-call `AllowOnce`和进一步收紧权限的`AllowWith`，不保存Turn/Session grant。public host不直接构造PermissionSet，而从exact request提供的allow options中选择。

### Approval And Question Types

```rust
pub(crate) struct ToolApprovalRequest {
    tool_name: ToolName,
    arguments_summary: String,
    reason: String,
    requirements: ToolRequirementSummaryView,
    options: Arc<[ToolApprovalOption]>,
}

pub(crate) struct ToolApprovalOption {
    view: ToolApprovalOptionView,
    decision: ToolApprovalDecision,
}

pub struct ToolApprovalRequestView {
    pub tool_name: ToolName,
    pub arguments_summary: String,
    pub reason: String,
    pub requirements: ToolRequirementSummaryView,
    pub options: Arc<[ToolApprovalOptionView]>,
}

pub struct ToolApprovalOptionView {
    pub option_index: u32,
    pub kind: ToolApprovalOptionKindView,
    pub label: String,
    pub effective_requirements: ToolRequirementSummaryView,
}

pub enum ToolApprovalOptionKindView {
    AsRequested,
    Restricted,
}

pub struct ToolRequirementSummaryView {
    pub filesystem: Option<String>,
    pub network: Option<String>,
    pub process: Option<String>,
}

pub enum ToolApprovalDecisionInput {
    Allow { option_index: u32 },
    Deny,
}

pub(crate) enum ToolApprovalDecision {
    AllowOnce,
    AllowWith(ToolPermissionSet),
    Deny,
}

pub(crate) struct ToolPermissionSet {
    // exact private capability ceiling; shape closes with production Sandbox adapter
}
```

approval invariants：

- `arguments_summary`、reason和requirements summary由trusted Tool-owned projection产生并bounded/redacted；不能直接serialize model arguments；projection失败在Interaction apply前fail closed；
- option indices在一个request内从0连续分配并唯一；request至少一个allow option，Deny始终由resolution enum提供；
- `AsRequested`映射`AllowOnce`；`Restricted`映射已证明不宽于requested/effective ceiling的`AllowWith`；
- public resolution只回传option index，不能提交path/host/process或任意permission object；unknown/cross-request index返回InteractionFamilyMismatch/InvalidArgument；
- resolution后再次执行Workspace/policy/Sandbox enforceability与ToolStartGate revalidation，approval从不替代enforcement；
- exact private option map只存在于Pending Interaction/live waiter，不进入普通Snapshot、event或diagnostic；recorded history只保存safe option view与selected decision kind。

UserQuestion首版是显式non-secret、recordable、model-visible的结构化表单：

```rust
pub struct UserQuestionRequest {
    pub title: Option<String>,
    pub questions: Arc<[UserQuestionField]>,
}

pub struct UserQuestionField {
    pub question_index: u32,
    pub prompt: String,
    pub required: bool,
    pub input: UserQuestionInput,
}

pub enum UserQuestionInput {
    Text { multiline: bool },
    SingleChoice { options: Arc<[UserQuestionChoice]> },
}

pub struct UserQuestionChoice {
    pub option_index: u32,
    pub label: String,
}

pub struct UserQuestionAnswer {
    pub answers: Arc<[UserQuestionFieldAnswer]>,
}

pub struct UserQuestionFieldAnswer {
    pub question_index: u32,
    pub value: UserQuestionAnswerValue,
}

pub enum UserQuestionAnswerValue {
    Text(String),
    Choice { option_index: u32 },
}
```

questions/options按index严格递增且唯一。answer必须无duplicate/unknown question，required字段齐全，value family和choice index匹配。Text/labels/count/total answer limits由V4-P1-2冻结。

MVP没有`secret`、password、credential、file upload或arbitrary JSON answer variant。producer和Presentation Adapter必须明确告知用户：answer会进入live state、conversation JSONL、event/history和ask-user ToolResult，并可能发送给模型，不能输入API key、password、token或其他secret。`NonSecret`是协议约束，不是Runtime对自然语言的secret classifier。需要credential时必须未来新增独立secure host capability/one-time secret reference，raw value不得经过Interaction/ToolResult/model context。

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
        tool_call_id: ToolCallId,
    ) -> Result<ToolStartPermit, ToolExecutionControlError>;
}
```

`ToolStartPermit`是current-Runtime typed permit，不持久化：

- Session Execution拥有唯一`ToolOperationSlot`类型；Turn-scoped control handle验证current Turn/Item/call；
- 观察latest EmergencyControl；
- 原子把ToolOperationSlot从Prepared变为Running；
- permit只允许对应executor开始一次；
- drop未使用permit回滚到non-started terminal/cancel路径；
- process crash后permit和slot消失，不用于recovery。

`request_user_question`必须发生在file mutation ticket reservation和start permit前。

## 批量调度和 Session-local 文件 Mutation Queue

一个assistant response可以包含多个ToolCall。

调度规则：

- call按canonical `call_index`规范化；
- preflight/schema/policy按call order；
- ask-user route独占并先完成；
- `Serial`、multi-file或open-world Tool使batch按call order串行；
- 普通single-file Tool按canonical file key取得Session-local FIFO ticket；
- 不同key可以并发；
- result允许逆序完成；
- model-visible output始终按assistant call order。

```rust
pub(crate) struct SessionFileMutationQueue {
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

`CancelledBeforeStart`不是独立terminal outcome variant。只要start reservation尚未获胜且可以证明没有side effect，Cancel/SecurityRevoked也返回`Completed { source = PreExecution, result.disposition = Cancelled }`。ActiveTurnTask把truthful outcome apply为live role=tool message，并完成inline record attempt。

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

ToolResult recording失败不能重新执行Tool。

### Abandoned

```text
executor future/local process outcome unknown
remote operation outcome unknown
host restart loses exact result
```

返回/投影Abandoned，不构造ToolResult，不自动retry。

## LLM 循环

ActiveTurnTask流程：

```text
apply Assistant(intermediate with ToolCalls) live
→ await SessionRecorder.record
→ ToolSet.execute calls
→ apply each exact ToolResult live + await inline record attempt
→ LiveConversation reducer matches all calls/results
→ complete exchange becomes model-visible
→ optional Steer
→ next Model
```

ToolSet只返回outcome。它不能：

- apply Assistant/Tool/terminal live mutation；
- 写SessionRecorder；
- 决定exchange complete；
- 开始下一次模型调用；
- 消费Steer/FollowUp。

## Domain 投影

assistant ToolCall live apply：

```text
ToolInvocation Item → Started
```

matching Tool message live apply：

```text
ToolInvocation Item → Completed(result)
```

ToolAbandoned：

```text
ToolInvocation Item → Abandoned
```

ToolSet只产出typed terminal outcome，不拥有conversation reducer。live complete exchange和first-terminal规则由[Turn / Item / Interaction](turn-item-interaction.md#complete-tool-exchange)拥有；cold duplicate/orphan/incomplete隔离由Conversation Recording replay projector实现。Tools module不维护第二份算法。

## Cancellation 与 SecurityRevoked

- Prepared：Emergency先赢，不发permit，产生PreExecution Cancelled ToolResult；
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
pub(crate) enum ToolExecutionError {
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

`ToolExecutionError`只在Tools implementation内部分类，不与`ToolExecutionOutcome`并列越过module seam。`ToolSet::execute`按实际阶段统一映射：

- start前的Tool unavailable、invalid arguments、hook/policy/approval/Sandbox failure或Cancel → `Completed { source = PreExecution, result }`；
- start后的exact executor failure/result → `Completed { source = Executed, result }`；
- side effect可能已经发生且exact outcome未知 → `Abandoned`。

无法保持Item/ToolCall correlation的internal invariant error不得伪造ToolResult；它投影为Abandoned并由Session Execution按typed diagnostic决定Turn failure。caller只match唯一terminal outcome enum。

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
- Turn-scoped ToolExecutionControl只在ActiveTurnTask current scope内发放start permit；
- permit不持久化；
- Running Tooltruthful settle；
- ToolResult recording失败不重放Tool；
- complete call/result集合才进入model conversation；
- ToolSet不修改LiveSessionState、不写SessionRecorder、不推进async loop；
- ask-user不持mutation permit；
- 同Session同file mutation FIFO；
- 跨Session不协调；
- restart不自动重放Tool；
- production Sandbox capability不足时必须pre-execution拒绝。

## Test Matrix

至少覆盖：

- duplicate ToolName candidate失败；
- disclosed route与executor同源；
- executor不能伪造ItemId/ToolCallId，outcome identity与request exact match；
- schema invalid/unknown Tool PreExecution result；
- ToolResultDisposition closed mapping：Succeeded/Failed/Denied/Cancelled，unknown outcome只能Abandoned；
- approval deny、Sandbox unavailable和cancel-before-start均返回matching PreExecution ToolResult；
- Hook rewrite后重新验证；
- approval allow/deny/family；
- approval public view redaction，request-scoped option index映射AllowOnce/AllowWith且restricted不能扩大权限；
- unknown/cross-request option拒绝，resolution后重新执行Sandbox enforceability与ToolStartGate；
- UserQuestion Text/SingleChoice index/required/family validation且protocol无secret variant；
- UserQuestion在mutation/start前；
- ToolSet在task spawn前由captured control handle与Session-local queue完整构造；
- ToolStartPermit只使用一次；
- start permit vs Cancel/Security双向race；
- Running Tool cancellation settlement；
- exact result recording失败不重新执行；
- outcome unknown Abandoned；
- multi-call inverse completion；
- 一个pre-execution deny与一个executed success仍按call_index闭合完整exchange；
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
4. provider-specific ToolResult error lowering。

ToolStartPermit使用actor message、owner-local CAS或等价private实现属于Session Execution内部选择；只要保持本节冻结的Turn-scoped handle、single-use permit和EmergencyControl first-wins interface，就不再是开放架构问题。

## 设计进度

- [x] ToolService/ToolSet ownership；
- [x] shared reload和per-Turn capture；
- [x] ToolSpec-only prompt view；
- [x] ToolCall/Item identity；
- [x] policy/approval/UserQuestion/Sandbox顺序；
- [x] Session-local file mutation queue；
- [x] 删除durable ToolExecutionStarted和ToolRoundCompleted；
- [x] 统一ToolCall/ToolExecutionRequest/ToolExecutionOutcome canonical owner与pre-execution result；
- [x] complete Tool exchange自动projection；
- [ ] O1 production Sandbox gate；
- [ ] production implementation/tests。
