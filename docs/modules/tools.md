# Tool 子系统架构设计

状态：当前权威架构；M8.1最小Scripted Tool round-trip与crate-private `ToolOperationSlot`完整生命周期已实现（Prepared→Running→Settling→Terminal：per-request slot-owned `ToolStartGate` first-wins start gate、typed started proof、Running cancellation pair、signal先赢→PreExecution Cancelled且不调用factory、start先赢→Settling继续await same run后truthful settle、PreExecution/Executed/Abandoned truthful settlement），crate-private scripted approval/UserQuestion控制正确性seam亦已实现；M13/V4-C0-1已由ADR 0140关闭。M14五个closed/default-off production builtins现均已实现：`ask_user`（ADR 0142）、`read_file`（ADR 0143）、`list_directory`（ADR 0144）、`write_file`（ADR 0146）与`fetch_url`（ADR 0147）。`write_file`交付了真实typed `ToolExecutionPlan::FileMutation`、owner-tracked non-mutating preparation、capability-opened physical target identity、每loaded Session一个`SessionFileMutationQueue`、exact-request-bound FIFO ticket，以及由`ToolOperationSlot`在Running/Settling持有至Terminal的mutation permit；`fetch_url`交付了exact HTTPS origin与host-pinned socket addresses的resource authority、reject-all ambient resolver、bounded response policy与owner-contained cancellation。四个ask/filesystem bool加materialized fetch authority Option形成32种closed selection，固定顺序为`ask_user → read_file → list_directory → write_file → fetch_url`。完整generic ToolService/source/schema/hooks/policy、process及其他production executor/adapter与public Tool DTO仍待实现。
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
- production Sandbox adapter必须遵守ADR 0140 capability enforcement合同并证明effective enforcement。

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
pub struct ToolName(String);

pub struct ToolInputSchema {
    schema: BoundedJsonSchema,
}

impl ToolName {
    pub fn as_str(&self) -> &str;
}

impl ToolInputSchema {
    pub fn schema(&self) -> &BoundedJsonSchema;
}

pub struct ToolDefinition {
    pub name: ToolName,
    pub description: Arc<str>,
    pub input_schema: ToolInputSchema,
    pub annotations: ToolAnnotations,
    pub requirements: ToolRequirements,
    executor: Arc<dyn ToolExecutor>,
}
```

ToolName是stable selection/protocol key。同一个ToolResourceView内duplicate ToolName拒绝candidate publication。constructor先验证[Wire Schema](wire-schema.md#stable-symbolic-keys)共同floor，再要求portable provider grammar exact `[A-Za-z0-9_-]{1,64}`；field保持private，不能用任意String绕过。该grammar是OpenAI/Anthropic首版adapter的共同可表达子集，future provider不得在adapter内改名或放宽同一wire version。

`ToolInputSchema`只能从BoundedJsonSchema构造。MVP semantic subset支持object root、type/properties/required/additionalProperties/items/enum/const、numeric/string/array bounds、description/title、allOf/anyOf/oneOf与local `$defs/$ref`；拒绝remote ref、patternProperties、dynamic/recursive ref、network lookup和owner未支持keyword。schema candidate必须同时通过bounded wire validation与Tool schema semantic validation。

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
    pub arguments: BoundedJsonObject,
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
    pub details: Option<BoundedJsonValue>,
}

pub struct ToolResultContent {
    parts: Arc<[ToolResultContentPart]>,
}

pub enum ToolResultContentPart {
    Text {
        text: Arc<str>,
    },
}

impl ToolResultContent {
    pub fn parts(&self) -> &[ToolResultContentPart];
}

pub enum ToolResultDisposition {
    Succeeded,
    Failed,
    Denied,
    Cancelled,
}

pub enum ToolAbandonReason {
    OutcomeUnknown,
    RuntimeFailure,
}
```

`ToolResultContent`是唯一model-visible/stored result body；MVP只支持1..32个safe Text parts，每part<=65,536 bytes、aggregate<=262,144 bytes。它是external Tool text carrier，payload中的CR/CRLF不做silent normalization；owner只能fail closed，或在构造前显式替换为U+FFFD并记录bounded diagnostic。structured executor payload必须由Tool owner确定性render为Text；raw JSON可在bounded `details`中供current-process trusted debug projection使用，但不自动进入模型，且[Conversation JSONL Format V1](../formats/conversation-jsonl-v1.md#tool-message)明确不记录details。

`Succeeded`要求Tool owner产生exact successful business result：ordinary Tool来自executor exact success；ask-user等built-in control Tool可以在不启动side effect/executor的情况下由合法Interaction resolution产生truthful success。`Failed`表示有truthful Tool/preflight error（例如unknown Tool、invalid arguments、Hook或exact executor failure）；`Denied`表示policy、approval、Workspace authority或Sandbox capability的pre-execution fail-closed拒绝；`Cancelled`只在能证明side effect未开始或executor返回exact cancellation result时使用。outcome unknown不能伪造成上述任一disposition，必须使用`ToolExecutionOutcome::Abandoned`，其reason只能是`OutcomeUnknown`或`RuntimeFailure`。raw internal error不能进入reason。

source/disposition valid matrix：

| Source | Allowed dispositions |
| --- | --- |
| PreExecution | Succeeded、Failed、Denied、Cancelled |
| Executed | Succeeded、Failed、Cancelled |

PreExecution Succeeded用于ask-user等无需executor side effect即可truthfully完成的built-in Tool；Executed Denied invalid。unknown outcome使用Abandoned，不伪造Completed。

这三个execution类型、`ToolResultContent`、`ToolResultDisposition`和`ToolAbandonReason`的完整shape只在Tools定义。`ToolCall`不保存ItemId；ActiveTurnTask在assistant response live apply前按同一candidate分配ItemId并构造`ToolExecutionRequest`，随后把Assistant、Started Items和expected set作为一个owner-local no-await mutation应用。`call_index`是validated assistant response中ToolCall的zero-based稳定顺序，由Session Execution按finalized content order规范化；provider adapter用于stream/final关联的内部index不作为mutation queue顺序来源。Turn/Item和storage可以投影`ItemId + ToolCallId`，但不得定义第二个execution input/outcome。

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
    execution_control: Arc<ToolExecutionControl>,
    mutation_queue: Arc<SessionFileMutationQueue>,
}
```

`execution_control`与`mutation_queue`均为crate-private capture输入。SessionExecutor在reserve candidate TurnId/control_generation后创建绑定该candidate与EmergencyControl的Turn-scoped concrete control handle（crate-private `ToolExecutionControl`，Session Execution私有类型，不冻结public trait），并克隆本loaded Session拥有的mutation queue；两者在ActiveTurnTask spawn前进入`ToolTurnContext`。同一个control handle随后交给ActiveTurnTask使用，禁止task spawn后替换或二次注入。

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

pub enum ToolApprovalResolution {
    Allowed {
        option_index: u32,
        kind: ToolApprovalOptionKindView,
    },
    Denied,
}

pub(crate) enum ToolApprovalDecision {
    AllowOnce,
    AllowWith(ToolPermissionSet),
    Deny,
}

pub(crate) enum ToolCapabilityClass {
    FilesystemRead,
    FilesystemWrite,
    Network,
    Process,
}

pub(crate) struct ToolPermissionSet {
    // exact final class-level permissions; raw set representation remains private
}

pub(crate) enum ToolSandboxContract {
    Available(ToolCapabilitySet),
    Unavailable,
}
```

`ToolPermissionSet::restricted_candidate(...)`只接受等于或窄于current ceiling的class set；新增任何class返回closed typed error。`ToolSandboxContract::admit(final_permissions)`只在Sandbox available且`final ⊆ enforceable`时返回move-only proof；否则返回`Unavailable`或携带exact `final − enforceable`的`CapabilityGap`。两类失败都只转换为固定、bounded、non-secret的`PreExecution + Denied`文本，missing classes不进入model-visible content。M13.3后per-request planner只返回一个plan；`Execute.permissions`与`Approval.permissions`是final class-level事实唯一owner。Direct Execute在plan离开Tools owner前admit；Approval先present host，`AllowOnce`重新admit同一ceiling，`AllowWith(candidate)`先在release code证明candidate不宽于ceiling再admit candidate。只有成功才进入ToolStartGate reservation；失败直接settle PreExecution Denied。该合同不代表resource-level path/host/process grants或production Sandbox已实现。

approval invariants：

- `arguments_summary`、reason和requirements summary由trusted Tool-owned projection产生并bounded/redacted；不能直接serialize model arguments；projection失败在Interaction apply前fail closed；
- option indices在一个request内从0连续分配并唯一；request至少一个allow option，Deny始终由resolution enum提供；
- `AsRequested`映射`AllowOnce`；`Restricted`映射已证明不宽于requested/effective ceiling的`AllowWith`；
- public resolution只回传option index，不能提交path/host/process或任意permission object；unknown/cross-request index返回InteractionFamilyMismatch/InvalidArgument；
- public/storage terminal projection使用同一个ToolApprovalResolution；Allowed必须保存exact selected option index与kind，Denied无private reason/PermissionSet；
- resolution后再次执行Workspace/policy/Sandbox enforceability与ToolStartGate revalidation，approval从不替代enforcement；
- exact private option map只存在于Pending Interaction/live waiter，不进入普通Snapshot、event或diagnostic；recorded history只保存safe option view与selected decision kind。

Implementation staging：M1.5的safe view/replay carrier可以表示`Restricted`；M13.3已实现`Restricted → AllowWith(ToolPermissionSet)` private exact option map、recorded safe kind/index validation与Session resume前class-level ceiling/Sandbox revalidation，因此adapter-independent conformance可以执行Restricted。production Tool source仍必须从真实policy/workspace facts构造resource-level candidate，host仍只能选择exact option index；不得从safe view或任意public input发明PermissionSet。production Tool/Sandbox adapter必须遵守ADR 0140并用自己的contract suite证明effective enforcement。

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

questions/options按index严格递增且唯一。answer必须无duplicate/unknown question，required字段齐全，value family和choice index匹配。Text/labels/count/total answer limits使用[Wire Schema InteractionLimits](wire-schema.md#queueinteraction与observation)。

MVP没有`secret`、password、credential、file upload或arbitrary JSON answer variant。producer和Presentation Adapter必须明确告知用户：answer会进入live state、conversation JSONL、event/history和ask-user ToolResult，并可能发送给模型，不能输入API key、password、token或其他secret。`NonSecret`是协议约束，不是Runtime对自然语言的secret classifier。需要credential时必须未来新增独立secure host capability/one-time secret reference，raw value不得经过Interaction/ToolResult/model context。

当前crate-private scripted实现状态：typed `ToolExecutionPlan::UserQuestion`与Tools-owned move-only/redacted `UserQuestionAnswerBinding`已实现（binding只接受truthful `PreExecution + Succeeded`作为answer、显式Abandoned直通、其余malformed形状fail closed为identity-bound Abandoned OutcomeUnknown）；question由typed plan shape识别、hoisted到全部ordinary sibling之前按call_index串行驱动、至多一个pending，不预留ToolStartGate或mutation ticket；每个question outcome先apply live+inline record attempt再继续；Cancel/SecurityRevoked/Unload signal-first跳过binding并settle全部unstarted calls为matching PreExecution Cancelled。上述字段类型是crate-private carrier；production builtin见下节[Production ask-user Builtin](#production-ask-user-builtin)，其ToolName/schema与answer render格式已由ADR 0142冻结。

### Production ask-user Builtin

ADR 0142冻结的production `ask_user`是closed、default-off、Runtime-owned builtin，唯一实现位置`src/tools/ask_user.rs`（`pub(super) fn build_tool_set() -> Arc<ToolSet>`），`src/tools.rs`只暴露narrow `ToolSet::ask_user_builtin()` production构造：

- **Default-off opt-in**：默认Runtime ToolSet保持`ToolSet::empty()`；host必须调用`MiniCoreRuntimeConfig::with_ask_user_tool()`（idempotent）opt-in，`open`恰好选择一次immutable ToolSet并经既有residency capture安装。不引入generic ToolService/registry、host callback/executor安装、authoring format或public Tool DTO。
- **Exact frozen surface**：ToolName恰为`ask_user`；description恰为“Ask the user one or more non-secret text or single-choice questions and return the answers. Use only when the task cannot continue without user input. Never request passwords, API keys, tokens, credentials, or other secrets.”；input schema是closed JSON object（optional nullable `title`、1..32 `questions`、strict adjacent `input`恰为`{"type":"text","data":{"multiline":bool}}`或`{"type":"single_choice","data":{"options":[...]}}`，每层`additionalProperties: false`）。schema是披露guidance，byte/count/index验证由既有semantic constructors执行；arguments从`BoundedJsonObject::canonical_json()`用private strict serde mirrors（每层`deny_unknown_fields`）解析，omitted/null `title`都映射`None`，indices严格递增但不要求从0开始或连续。
- **Plan闭合**：valid call→typed `ToolExecutionPlan::UserQuestion`（binding先经exact `UserQuestionRequest::validate_answer`验证再产生`PreExecution + Succeeded`）；任何parse/semantic failure→frozen `PreExecution + Failed`、恰一个Text part、文本恰为`tool arguments are invalid`、无Interaction。builtin零`ToolCapabilityClass` permission、使用available empty sandbox contract，绝不创建`ToolExecutionStart`、executor future、cancellation pair、start-gate reservation、approval或OS资源（这是production Tool slice，不是OS-backed Sandbox completion）。
- **Answer render**：恰一个deterministic compact JSON Text part，`{"answers":[{"questionIndex":3,"value":{"type":"text","data":"hello"}}]}`或choice `{"answers":[{"questionIndex":7,"value":{"type":"choice","data":{"optionIndex":11}}}]}`；answers保持升序，optional未答可渲染`{"answers":[]}`。escaping是canonical/deterministic（serde固定struct/enum field order，serde escaping不超过canonical escaping计数），输出受既有`ToolResultContent`约束（单part ≤65,536 bytes；projection envelope与`user_answer_encoded_len`同构，validated answer恒在界内）。binding内的answer re-validation mismatch或render失败是dynamic invariant，fail closed为identity-bound `Abandoned { RuntimeFailure }`，绝不产生malformed model-visible output。

approval不替代Sandbox。无法强制required capability时production executor必须在side effect前拒绝；该能力声明、差集算法与pre-start fail-closed顺序已由ADR 0140冻结。

### Production read_file Builtin

production `read_file`是closed、default-off、Runtime-owned builtin，唯一实现位置`src/tools/read_file.rs`（`pub(super) fn build_tool_set(workspace, task_context) -> Arc<ToolSet>`），`src/tools.rs`只暴露narrow `ToolSet::read_file_builtin(workspace, task_context)` production构造；`ProductionToolConfig`与`TurnToolResources::Production`负责production selection/composition与per-admission materialization：

- **Default-off opt-in与read-only authority ceiling**：默认Runtime ToolSet保持`ToolSet::empty()`；host必须调用`MiniCoreRuntimeConfig::with_read_file_tool()`（idempotent，与`with_ask_user_tool`相互独立）opt-in。该opt-in不只是一个Tool安装：`open`同时选择`WorkspaceResolver::new_with_read_access`，使每个declared root的authority ceiling恰为`ReadOnly` filesystem access（requested `ReadWrite`被收窄为`ReadOnly`，绝不授予`ReadWrite`），Prompt/Skill source ceilings保持false（opt-in绝不silently授权source discovery），trust为`Restricted`（filesystem grant独立于source trust）。Resolver与Runtime owner共享同一`WorkspaceReadAccessControl`：host经既有`MiniCoreRuntime::invalidate_session_workspace_authority(session_id)` seam（read_file opt-in时该方法先经该control发布permanent per-Session read revocation）revoke后，该Session每次future resolve/revalidation都授予filesystem `None`（永不`AuthorityDenied`、永不恢复`ReadOnly`），idempotent、本Runtime lifetime内无unrevoke。不引入generic ToolService/registry、host callback/executor安装或public Tool DTO。
- **Exact frozen surface**：ToolName恰为`read_file`；description恰为“Read one UTF-8 text file relative to the workspace working directory and return its full contents as a single text part. Use for reading source code, configuration, and other text files inside the workspace.”；input schema是closed JSON object（`additionalProperties: false`，恰一个required `path` string、`maxLength: 4096`）。schema是披露guidance：path由semantic `WorkspaceRelativePath` constructor验证（canonical cwd-relative、拒绝absolute/dot/dotdot/空段/trailing slash/drive prefix/backslash/control bytes，≤4,096 bytes、≤256 segments；空path是合法value但被Workspace authorization作为read target拒绝，因此schema不披露`minLength`）。arguments从`BoundedJsonObject::canonical_json()`用private strict serde mirror（`deny_unknown_fields`）解析，任何parse/semantic failure→frozen `PreExecution + Failed`、恰一个Text part、文本恰为`tool arguments are invalid`、无Interaction。definition mode为`ToolExecutionMode::Parallel`（每次调用一次有界regular-file read，不向无关操作强加Serial语义）。
- **Plan闭合与授权**：valid call→`ToolExecutionPlan::Execute`，permission恰为`ToolCapabilityClass::FilesystemRead`；授权在**任何start factory存在之前同步**经`WorkspaceAccessView::authorize_read(&path)`完成：containing root是持有canonical cwd的exact root，cwd在该root内的相对位置被prepend到requested path，结果capability-relative target必须fully normal。授权成功返回opaque `AuthorizedWorkspaceReadPath`（captured root capability `Arc<cap_std::fs::Dir>` + normalized capability-relative target；绝不暴露ambient absolute path，raw model path不能越过该类型）。每个授权错误（无readable grant、root path本身即cwd是目录不是read target、basis unavailable）collapse为同一个frozen `PreExecution + Denied`文本`workspace file access is denied`（bounded、non-secret）；absolute、dot/dotdot、平台prefix等非canonical path在更早的`WorkspaceRelativePath` semantic parse阶段归入`tool arguments are invalid`。builtin的outer sandbox contract available恰为`FilesystemRead`；Execute plan在离开Tools owner前被admit一次，拒绝即frozen Denied。composed ToolSet保持同一contract：read_file route被admit恰一次（绝不二次），ask_user route只产生UserQuestion/PreExecution shape，根本不进入Execute-only Sandbox admission。
- **Capability-relative open与有界读取**：started executor绝不使用ambient path、`std::fs::read`或`canonicalize`：经`AuthorizedWorkspaceReadPath::open_nonblocking()`（read + O_NONBLOCK，cap-std `Dir.open_with`）打开，open解析经captured root `Dir`——symlink escape在该open处fail（读为frozen unreadable）；FIFO等special entry的nonblocking open无需writer配对、立即返回，随后fstat regular-file check拒绝（目录/FIFO/socket/device等non-regular一律frozen unreadable，绝不挂起）；metadata size已超界时先拒绝、不读一个byte；随后有界读取至多65,537 bytes（超界检测无需unbounded allocation）。成功内容恰一个Text part ≤65,536 bytes：valid UTF-8 + safe-text（既有`ToolResultContent` owner contract）。frozen `Completed + Failed`文本恰为：`file could not be read`（missing、non-regular、open/read error、symlink escape、或valid UTF-8但protocol不能作为single safe Text part披露）、`file is not valid UTF-8`、`file is too large`（>65,536 bytes）；空regular file成功为恰一个empty Text part。
- **Owner-tracked blocking execution与truthful cancellation**：executor经`RuntimeTaskContext::spawn_blocking_tracked`调度一个tracked blocking job，job一旦scheduled绝不drop或detach：cancellation在job创建前已赢（或与scheduling race先赢）时，biased select证明零I/O，settle为frozen `Completed + Cancelled`文本`file read was cancelled`（truthful，绝非OutcomeUnknown）；cancellation在read运行中到达（即使nonblocking open注册期间）时保持await同一tracked job直到它settle并保留truthful result（known success/failure绝不重写）。只有`RuntimeTaskError`（owner closing、worker unavailable、operation panic）settle为`Abandoned { RuntimeFailure }`；signal-before-start情形由ToolSet gate slot拥有，在executor构造前settle自己的PreExecution Cancelled。这是production executor slice（非scripted）：application work与allocation有界（一次nonblocking capability open + 至多65,537 bytes read），owner不会detach且settlement可确认。诚实边界：special files被nonblocking-reject，但regular-file wall-clock I/O依赖OS/filesystem，runtime不fabricate timeout或wall-clock上界。
- **三十二种frozen selection与per-admission materialization**：Runtime `open`先把public `fetch_url` tool opt-in与installed origins汇合为`Option<Arc<FetchUrlResources>>`；selected但zero origins或duplicate canonical origins在构造Tool config前fail closed。`ProductionToolConfig::new(ask_user, read_file, list_directory, write_file, fetch_url_resources)`以四个bool加该Option作为closed selection事实，恰有32种shape；Option本身是内部唯一fetch selection事实源，没有第二个fetch bool可错配。definition/spec固定顺序为`ask_user → read_file → list_directory → write_file → fetch_url`，只包含enabled成员、无重复。无Workspace-bound builtin时，empty/ask/fetch/ask+fetch反复materialize复用captured base Arc；只要任一Workspace-bound builtin开启，每次admission都对exact captured Workspace snapshot materialize新ToolSet并绑定exact Runtime task context与同一Session mutation queue，immutable fetch authority Arc原样进入planner。composer只有五个frozen name route，unknown name经正常ToolSet lookup不可用；outer sandbox是enabled routes所需的exact `FilesystemRead`/`FilesystemWrite`/`Network` union，各route只经过一次admission。`ReloadSharedResources`保留该frozen config不变。

### Production list_directory Builtin

ADR 0144冻结的production `list_directory`是closed、default-off、Runtime-owned builtin，唯一实现位置`src/tools/list_directory.rs`；`ToolSet::list_directory_builtin(workspace, task_context)`只作为focused constructor，production安装走`ProductionToolConfig`：

- **Exact surface与authority**：ToolName恰为`list_directory`；description恰为“List direct entries in one directory relative to the workspace working directory and return sorted JSON names and types. Use for discovering files and subdirectories without reading file contents.”；closed schema只有required `path` string（`maxLength: 4096`、`additionalProperties: false`），empty path表示cwd。`with_list_directory_tool()` default-off/idempotent，与ask/read独立；它与read_file共享同一个`WorkspaceResolver::new_with_read_access`/`WorkspaceReadAccessControl`，不建立第二authority或revocation registry。
- **Plan与capability open**：strict serde mirror + `WorkspaceRelativePath`是semantic authority；invalid→`tool arguments are invalid`，authorization failure→`workspace directory access is denied`，均在start factory前。valid plan permission恰为`FilesystemRead`。`authorize_read_directory`与`authorize_read`共享containing-root/read-grant/cwd prepend/fully-normal实现，返回opaque `AuthorizedWorkspaceReadDirectory`；请求empty path表示cwd：cwd恰为root时authorized target为空并clone captured root Dir，cwd在root子目录时则经captured root `Dir::open_dir(cwd_in_root)`；其他target同样capability-relative open，symlink escape在open处fail closed。
- **Direct bounded enumeration**：一个owner-tracked blocking job只调用opened Dir的`entries()`，最多消费257项以证明256-entry boundary；不递归、不读文件内容、不构造ambient path。每个in-bound entry只取bare UTF-8 name与不跟随target的`DirEntry::file_type()`；成功取得第257项后立即证明overflow而不再检查其name/type，type恰为`file | directory | symlink | other`。retained names总计≤8,192 bytes，先借用检查UTF-8/budget再复制；按UTF-8 name bytes排序后render恰一个≤65,536-byte safe Text，shape`{"entries":[{"name":"...","type":"file"},...]}`。
- **Truthful outcomes/cancellation**：missing/not-directory/open/iteration error或in-bound entry type error→`directory could not be listed`；in-bound non-UTF-8 name→`directory contains an unsupported entry name`；bounds→`directory listing is too large`；job scheduling前cancel→零I/O证明的`directory listing was cancelled`。job一旦scheduled就等待同一settlement并保留known result；`RuntimeTaskError`或closed serialization invariant panic→`Abandoned { RuntimeFailure }`。filesystem open/enumeration wall-clock依赖OS/filesystem，不声称timeout。

### Production write_file Builtin

ADR 0146冻结的production `write_file`是closed、default-off、Runtime-owned builtin，唯一实现位置`src/tools/write_file.rs`；focused构造为`ToolSet::write_file_builtin(workspace, task_context, queue)`，production安装走`ProductionToolConfig`：

- **Exact surface与authority**：ToolName恰为`write_file`，mode为`Parallel`；closed schema只有required `path`（`maxLength: 4096`）与`content`（`maxLength: 16384`），`additionalProperties: false`。content是safe UTF-8、允许empty、exact bytes写入、不normalize newline。`with_write_file_tool()`选择ReadWrite authority ceiling，但requested `ReadOnly` root仍保持ReadOnly并在plan时以`workspace file write access is denied`拒绝；Prompt/Skill source ceilings保持false。
- **Typed preparation与opaque target**：valid call产生`ToolExecutionPlan::FileMutation { permissions = FilesystemWrite, prepare, queue }`。planner只同步授权`WorkspaceAccessView::authorize_write`并构造move-only preparation factory，不执行I/O。round owner按`call_index`串行调用factory；每个factory调度一个owner-tracked blocking job，prepare只打开capability handle/metadata并产生opaque `WorkspaceFileMutationKey`与move-only target，不create/truncate/write。existing target以exact opened regular-file `same_file::Handle`为key并保留同一File；create target以exact direct-parent Dir identity加normalized final name为key并保留该Dir。
- **Session-local FIFO与slot-owned permit**：preparation完整settle且exact emergency basis仍current后，round owner同步按`call_index`在同一loaded Session queue预留exact-request-bound ticket。same key FIFO、different key独立；waiting ticket drop/cancel移除自身并唤醒next，foreign request fail closed，last ticket/permit离开后删除entry。ticket取得permit后才允许ToolStartGate reservation；Running与Settling slot持有permit，只有exact outcome已绑定、Settling→Terminal时释放。不同Session/Runtime/process不协调。
- **Capability-relative full replacement**：existing target在保留的exact File上从offset zero truncate并`write_all`；create target只经保留parent Dir以final-component no-follow方式open/create/truncate。没有ambient path、`canonicalize`、mkdir、append、patch、atomic rename或fsync；write failure可能留下truncate/partial file，不retry、不声称crash durability。
- **Two tracked jobs and truthful cancellation**：preparation与write是两个独立owner-tracked blocking jobs；move-only target只经private single-consumer handoff从preparation移到write。write job创建前cancellation获胜时证明零mutation并返回`file write was cancelled`；job一旦scheduled，后到signal仍等待同一job并保留truthful success/failure。ordinary target/write failure为`file could not be written`；success为`file written`；owner/worker/panic/join uncertainty为`Abandoned { RuntimeFailure }`。application bytes/allocation有界，ordinary filesystem wall-clock不声明timeout。

### Production fetch_url Builtin

ADR 0147冻结的production `fetch_url`是closed、default-off、Runtime-owned network builtin，唯一实现位置`src/tools/fetch_url.rs`；focused构造为`ToolSet::fetch_url_builtin(resources)`，production安装由Runtime在`open`时materialize authority后传入`ProductionToolConfig`：

- **Tool selection与authority installation分离**：`MiniCoreRuntimeConfig::with_fetch_url_tool()`只选择Tool，`with_fetch_url_origin(FetchUrlOrigin)`只安装validated authority，二者均不隐式启用另一方。tool off时origins不materialize且不披露；tool on但zero origins、或selected origins存在duplicate canonical origin时`open`返回`InvalidConfiguration`；client build failure返回`RuntimeDependencyUnavailable`。authority是Runtime-wide immutable value，不随Workspace reload/invalidation变化。
- **Exact host-installed authority**：`FetchUrlOrigin::new(origin, addresses)`只接受safe、≤2,048-byte、bare HTTPS DNS origin与1..=8个port-matching fixed `SocketAddr`；拒绝IP-literal URL、userinfo/query/fragment/non-root path、unspecified/multicast/IPv4 broadcast address，duplicate addresses按first occurrence去重。public value与errors完全redacted，无hostname/address getters。每个origin一个client：shared locked-down transport builder + `https_only(true)`、zero idle pool、10秒connect/30秒whole-request timeout、exact hostname `resolve_to_addrs` override与reject-all fallback resolver；无ambient DNS、rebinding、redirect、retry、proxy或automatic compression。
- **Closed planner与single exact GET**：ToolName恰为`fetch_url`，mode `Parallel`；closed schema只有required `url` string（`maxLength: 4096`、`additionalProperties: false`）。raw hierarchical URL gate先拒绝WHATWG recovery与empty-userinfo erasure，再按canonical scheme/host/effective port匹配exact installed origin；invalid URL→`tool arguments are invalid`，foreign origin→`network URL access is denied`，都在start factory前。authorized plan只携带`Network` permission与move-only `AuthorizedFetchUrl`；target唯一consuming operation `send(self)`发送一个empty-body GET，固定`Accept: text/plain, application/json`、`Accept-Encoding: identity`、`Connection: close`，caller不能取得或改写raw Client/Url。
- **Bounded response与truthful cancellation**：只有2xx读取body；non-2xx不读取或披露body/status/header。Content-Type必须恰一个`text/plain`或`application/json`，Content-Encoding只允许absent或single `identity`；known Content-Length >65,536提前拒绝，否则stream最多65,537 bytes。success必须valid UTF-8 safe Text，exact bytes作为一个Text part返回，不trim/newline-normalize/parse JSON。send、headers与body在同一个slot-owned executor future中poll，不spawn child task；pre-cancel证明zero GET，in-flight cancel drop exact operation state并返回`URL fetch was cancelled`，natural result不被late cancellation改写。transport/non-2xx→`URL could not be fetched`，response media/encoding→`URL response type is unsupported`，oversize→`URL response is too large`，invalid text→`URL response is not valid text`；owner invariant→`Abandoned { RuntimeFailure }`。

### Tool Execution Control

approval与UserQuestion控制现由Session-private concrete `ToolExecutionControl`实现（crate-private、`Clone`的Turn-scoped handle，携带exact TurnId与既有`InteractionRequested` completion lane sender），复用既有Interaction actor/wire/storage owner完成apply+record+notify与resolution-before-resume，不冻结public interface/trait（完整trait不再作为future target承诺）。start/cancel部分继续由Session Execution/Tools组合的concrete seam实现。Session Execution在assistant ToolCall live apply后、record await前为每个exact `ToolExecutionRequest` capture构造`ToolOperationSlot::Prepared`（绑定exact `EmergencyControlHandle` + observation），每个request对应slot自己的`ToolStartGate` lock-free atomic slot（`Prepared → Reserved → Started | Cancelled`，无mutex、无poison、与Emergency owner mutex无锁序）；reservation（Prepared→Reserved CAS）在EmergencyControl owner mutex内对exact unsignaled target/epoch执行，与`signal`在同一mutex上线性化（first-wins）；move-only `ToolStartPermit`经`start()`（Reserved→Started CAS）产生typed `ToolStartedExecution` proof，`ToolSet::run_started_execution`先复验proof的exact capture再调用move-only `ToolExecutionStart` factory构造future（foreign proof fail-closed为identity-bound Abandoned），executor future只在proof存在后poll，drop未用permit回滚reservation；signal/stale先赢→不调用factory、matching PreExecution Cancelled ToolResult；reservation/start先赢后slot进入Running（持有operation自己的`ToolCancellationHandle`），signal只触发cancellation observer、slot经Settling继续await same run至executor cooperative cleanup/result后truthful settle（started run不因signal drop）。approval/UserQuestion控制seam已完成（crate-private scripted）：

- Emergency observation携带opaque owner identity（`Arc<EmergencyControlOwner>`，`Arc::ptr_eq`验证，foreign observation无法通过）；
- interaction presentation、host resolution、UserQuestion answer binding与unstarted settlement均为move-only typed permit，每个permit绑定owner+target/epoch+同一`ToolExecutionRequest` capture，在Emergency owner mutex内与`signal`/close first-wins线性化（signal-first→不present/bind/settle固定结果，permit-first→signal随后到达仍授权该精确步骤并truthful settle）；
- Submit→Turn signal迁移（`migrate_target`）在同一owner mutex内原子进行，旧basis的signal保留到new basis；
- typed `ToolExecutionPlan`四路结构（旧generic Interaction plan删除）：

```rust
pub(crate) enum ToolExecutionPlan {
    Execute {
        permissions: ToolPermissionSet,
        start: ToolExecutionStart,
    },
    Approval {
        permissions: ToolPermissionSet,
        request: ToolApprovalRequest,
        allowed: ToolExecutionStart,
        denied: ToolExecutionResult,
    },
    UserQuestion {
        request: UserQuestionRequest,
        answer: UserQuestionAnswerBinding,
    },
    FileMutation {
        permissions: ToolPermissionSet,
        prepare: ToolExecutionPreparation,
        queue: Arc<SessionFileMutationQueue>,
    },
    PreparedFileMutation {
        ticket: SessionFileMutationTicket,
        start: ToolExecutionStart,
    },
    PreExecution(ToolExecutionResult),
}
```

- UserQuestion由typed plan shape识别：hoisted到全部ordinary sibling之前、按call_index串行驱动、至多一个pending；每个question outcome（answer或owner cancellation）先apply live+inline record attempt再呈现下一个question或启动ordinary sibling；question绝不预留`ToolStartGate`或mutation ticket、绝不构造start factory；
- valid answer经move-only `UserQuestionAnswerBinding::bind`产生identity-bound `PreExecution + Succeeded` ToolResult；Cancel/SecurityRevoked/Unload signal-first跳过binding并settle全部unstarted calls为matching PreExecution Cancelled；presentation/resolution/binding permit-first各阶段settle truthfully；abandoned question对remaining无副作用（known preflight保留、其余unstarted settle为PreExecution Failed）。

`ToolStartPermit`是current-Runtime typed permit，不持久化：

- Session Execution拥有唯一`ToolOperationSlot`类型（Prepared→Running→Settling→Terminal完整生命周期已实现）；Turn-scoped control handle验证current Turn/Item/call；
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
- UserQuestion/ask-user由typed plan shape识别并独占先完成（hoisted到全部ordinary sibling之前、按call_index串行、至多一个pending；每个question outcome先apply live+inline record attempt再继续；question不reserve/start ToolStartGate，也不涉及mutation ticket）；
- `Serial`、multi-file或open-world Tool使batch按call order串行；
- typed single-file mutation plan按opaque physical key取得Session-local FIFO ticket；
- 不同key可以并发；
- result允许逆序完成；
- model-visible output始终按assistant call order。

```rust
pub(crate) struct SessionFileMutationQueue {
    // private per-loaded-session queues
}
```

mutation permit：

- existing target基于exact capability-opened regular-file identity；
- create target使用exact direct-parent identity + normalized final filename；
- underlying I/O settle前不释放；
- Cancel可以忽略业务结果，但不能提前释放permit；
- 不跨Session/Runtime/process协调。

实现状态：上述queue、ticket、permit与slot attachment均已实现。round先完整settle全部UserQuestion，再按`call_index`串行prepare并reserve mutation tickets；signal在preparation前获胜时factory不调用，scheduled preparation后获胜时仍等待同一preparation settle、随后不reserve ticket；waiting signal移除ticket并阻止start；started mutation的permit由Running移入Settling，只有Settling→Terminal后释放。

## Tool 执行流程

### PreExecution Result

以下路径不开始side effect：

```text
unknown tool
schema invalid
hook deny/failure
policy deny
approval deny
ask-user合法answer完成
Sandbox unavailable
EmergencyControl wins before start
```

返回：

```text
Completed {
  source = PreExecution,
  result.disposition = Succeeded | Failed | Denied | Cancelled
}
```

`PreExecution + Succeeded`只用于ask-user等built-in control route已经truthfully完成且没有executor side effect的结果；普通Tool不得用它绕过executor。UserQuestion的valid answer经move-only `UserQuestionAnswerBinding::bind`产生identity-bound `PreExecution + Succeeded`（malformed/panic fail closed为identity-bound Abandoned OutcomeUnknown）。`CancelledBeforeStart`不是独立terminal outcome variant。只要start reservation尚未获胜且可以证明没有side effect，Cancel/SecurityRevoked也返回`Completed { source = PreExecution, result.disposition = Cancelled }`；signal-first对全部unstarted calls（含pending question）跳过binding并settle matching Cancelled，abandoned question下known preflight保留、其余unstarted settle为`PreExecution Failed`。ActiveTurnTask把truthful outcome apply为live role=tool message，并完成inline record attempt。

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

- Prepared：Emergency先赢，不发permit，产生PreExecution Cancelled ToolResult（已实现；含pending UserQuestion：signal-first跳过binding，全部unstarted calls settle matching PreExecution Cancelled）；
- Running：发送best-effort cancellation，等待executor teardown/result（已实现：Running slot持有operation自己的`ToolCancellationHandle`；production `read_file`/`list_directory`/`write_file`都保持await同一owner-tracked job，write mutation还持有`SessionFileMutationPermit`；`fetch_url`在同一slot-owned future中drop exact send/body state并settle，不spawn adapter-owned task）；
- Settling：继续等待same run与outcome capture；started mutation permit从Running移动到Settling，只在exact outcome绑定并转入Terminal时释放；
- UserQuestion/ask-user（已实现）：presentation/resolution/binding各阶段经move-only permit与signal first-wins，signal-first跳过binding并settle全部unstarted为PreExecution Cancelled，abandoned question对remaining无副作用（known preflight保留、其余unstarted为PreExecution Failed）；question不持mutation permit或ToolStartPermit，也不reserve/start ToolStartGate；
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

Conversation Storage replay：

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

`ToolExecutionError`只在Tools implementation内部分类，不与`ToolExecutionOutcome`并列越过module seam。ToolSet的`plan`/`run_started_execution`/`bind_preexecution_result`（无独立`execute`入口，执行ownership在Session Execution的per-round gate）按实际阶段统一映射：

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
ToolExecutionControl（Session-private concrete，非public trait）
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
- StoredToolOutcome format-v1 Completed/Abandoned round-trip且不含details/raw error；
- ToolResult recording失败不重放Tool；
- complete call/result集合才进入model conversation；
- ToolSet不修改LiveSessionState、不写SessionRecorder、不推进async loop；
- ask-user不持mutation permit、不预留ToolStartGate（hoisted exclusive question调度）；
- 同Session同file mutation FIFO；
- 跨Session不协调；
- restart不自动重放Tool；
- production Sandbox capability不足时必须pre-execution拒绝。

## Test Matrix

至少覆盖：

- duplicate ToolName candidate失败；
- ToolName/ToolInputSchema private constructor，BoundedJsonSchema keyword/ref/depth/byte limits；
- ToolCall arguments与ToolResult details拒绝raw/unbounded JSON；
- disclosed route与executor同源；
- executor不能伪造ItemId/ToolCallId，outcome identity与request exact match；
- schema invalid/unknown Tool PreExecution result；
- ToolResultDisposition closed mapping：Succeeded/Failed/Denied/Cancelled，unknown outcome只能Abandoned；
- ToolResultContent part/aggregate exact boundary与oversized known output归约bounded Failed result；
- ToolAbandonReason只允许OutcomeUnknown/RuntimeFailure且无raw error；
- approval deny、Sandbox unavailable和cancel-before-start均返回matching PreExecution ToolResult；
- Hook rewrite后重新验证；
- approval allow/deny/family；
- approval public view redaction，request-scoped option index映射AllowOnce/AllowWith且restricted不能扩大权限；
- unknown/cross-request option拒绝，resolution后重新执行Sandbox enforceability与ToolStartGate；
- UserQuestion Text/SingleChoice index/required/family validation且protocol无secret variant；
- production `ask_user` builtin（ADR 0142）：default-off/opt-in config path、exact definition/schema disclosure、strict parsing与semantic failure（无question、count/index/byte边界）、valid text/choice/mixed/nullable title/non-contiguous indices、deterministic escaping与optional empty answer、无Execute/start seam、Session/runtime端到端question→ToolResult→next model call；
- production `read_file` builtin：default-off/opt-in config path与32种closed selection中的exact disclosure（含frozen schema字节与顺序）、strict parsing与semantic failure（absolute/dot/drive/backslash/control/byte/segment边界）、授权Denied（无grant、root path、cwd外）、capability-relative open的symlink escape拒绝、regular-file/UTF-8/65,536-byte边界（65,536成功、65,537 too large、invalid UTF-8、unsafe text、空文件、missing/目录/FIFO）、cancellation before/after scheduling与RuntimeTaskError→Abandoned、end-to-end read_file→ToolResult→next model call、per-Session read revocation后future resolve/read denied；
- production `list_directory` builtin（ADR 0144）：exact definition/schema、empty cwd与nested direct listing、sorted compact JSON/type mapping、strict path/unknown-field failures、no-grant/symlink-target denial、entry symlink与special type不跟随、non-UTF-8 name、256/257 entries与8,192 retained-name-byte边界、iteration-error precedence、cancellation before/after scheduling、RuntimeTaskError→Abandoned、Runtime end-to-end与per-Session revocation后future denial；
- production `write_file` builtin（ADR 0146）：exact definition/schema与16,384-byte content boundary、ReadOnly/no-grant denial、existing/create/no-mkdir/full-replacement、symlink/hard-link physical identity与create final no-follow、two tracked jobs/private move-only handoff、before-job zero-mutation cancellation、after-job truthful settlement、RuntimeFailure mapping、existing与initially-missing same-target sibling FIFO、32种selection/固定顺序、Runtime end-to-end与read/write joint revocation；
- production `fetch_url` builtin（ADR 0147）：origin/address constructor与redaction、raw URL recovery/userinfo rejection、same-origin/foreign-origin authorization、reject-all DNS与pinned resolver、exact GET/headers/no redirect/retry/proxy/compression、2xx/content-type/content-encoding/body bounds/UTF-8-safe-text matrix、before-send/mid-headers/mid-body/late cancellation、32种selection/固定顺序、selected-without-authority与duplicate-origin open failure，以及Runtime loopback ToolResult→next model request end-to-end；
- UserQuestion在mutation/start前；typed plan hoisting/exclusive scheduling（call_index串行、至多一个pending）、answer binding与signal first-wins、signal-first全部unstarted settle为PreExecution Cancelled；
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
- Sandbox unenforceable capability gate（ADR 0140已关闭；production adapter继续证明effective enforcement）。

## 开放问题

1. generic production policy/resource-level permission producer与future adapter的effective-enforcement conformance；
2. v0.1 production builtin Tool集合已冻结为`ask_user`/`read_file`/`list_directory`/`write_file`/`fetch_url`（ADR 0142/0143/0144/0146/0147）；新增builtin属于post-MVP独立决策；
3. `max_tool_result_bytes`和future blob handling；
4. provider-specific ToolResult error lowering。

ToolStartPermit使用actor message、owner-local CAS或等价private实现属于Session Execution内部选择；只要保持本节冻结的Turn-scoped handle、single-use permit和EmergencyControl first-wins interface，就不再是开放架构问题。

## 设计进度

- [x] ToolService/ToolSet ownership；
- [x] shared reload和per-Turn capture；
- [x] ToolSpec-only prompt view；
- [x] ToolCall/Item identity；
- [x] policy/approval/UserQuestion/Sandbox顺序；
- [x] Session-local file mutation queue、exact-request-bound ticket与mutation permit attachment through Running/Settling；
- [x] 删除durable ToolExecutionStarted和ToolRoundCompleted；
- [x] 统一ToolCall/ToolExecutionRequest/ToolExecutionOutcome canonical owner与pre-execution result；
- [x] complete Tool exchange自动projection；
- [x] ToolStartGate first-wins start gate与typed started proof，及完整`ToolOperationSlot`生命周期（Prepared→Running→Settling→Terminal、Running cancellation pair与truthful settle；M8.3 foundation已从round-local narrow slice升级为slot）；
- [x] UserQuestion producer与exclusive scheduler正确性（typed `ToolExecutionPlan::UserQuestion`、move-only `UserQuestionAnswerBinding`（仅truthful PreExecution+Succeeded为answer）、hoisted exclusive question调度（call_index串行、至多一个pending、不reserve/start ToolStartGate且不涉及mutation ticket）、signal-first settlement与abandoned question无副作用）；
- [x] O1/R7/V4-C0-1 production Sandbox gate（ADR 0140；class-level algebra、direct Execute admission、Restricted/AllowOnce resume revalidation及SecurityRevoked/Sandbox-unavailable/Running round conformance）；
- [x] production `ask_user` builtin（ADR 0142；closed/default-off/Runtime-owned、`MiniCoreRuntimeConfig::with_ask_user_tool()` idempotent opt-in、`ToolSet::ask_user_builtin()`、零permission+available empty sandbox、仅UserQuestion/frozen PreExecution plans、deterministic compact JSON answer与fail-closed Abandoned）；
- [x] production `read_file` builtin（closed/default-off/Runtime-owned、`MiniCoreRuntimeConfig::with_read_file_tool()` idempotent opt-in与read-only authority ceiling、`WorkspaceResolver::new_with_read_access` + owner-held `WorkspaceReadAccessControl` per-Session永久read revocation（host security invalidation先revoke再signal）、`ToolSet::read_file_builtin()`与`ProductionToolConfig` per-admission materialization、cwd-relative opaque `AuthorizedWorkspaceReadPath`授权、capability-relative nonblocking open（symlink escape/特殊文件fail closed）、regular-file/UTF-8/65,536-byte有界读取、owner-tracked blocking execution与truthful cooperative cancellation）；
- [x] production `list_directory` builtin（ADR 0144；closed/default-off、共享filesystem authority/revocation、`with_list_directory_tool()`、opaque `AuthorizedWorkspaceReadDirectory` capability open、direct nonrecursive enumeration、entry symlink不跟随、256-entry/8,192 retained-name-byte/65,536 JSON bounds、deterministic compact JSON、owner-tracked truthful settlement）；
- [x] production `write_file` builtin（ADR 0146；closed/default-off、ReadWrite authority ceiling/requested-access intersection、`authorize_write` opaque capability target、existing/create physical identity、two tracked jobs、same-Session FIFO、permit through Settling、32种closed selection与Runtime e2e/revocation）；
- [x] production `fetch_url` builtin（ADR 0147；closed/default-off、separate Tool/origin installation、exact HTTPS origin+host-pinned addresses、reject-all DNS/no redirect-retry-proxy-compression、single bounded safe-text GET、owner-contained cancellation、32种closed selection与Runtime e2e）；
- [ ] post-MVP production implementation/tests（完整generic schema/hooks/policy、generic production ToolService、process及其余production executor/adapter与public Tool DTO）。
