# Compaction 架构设计

状态：当前权威架构（M10 planning、CompactionSummary request/validation与production replacement已实现；async Replace/record orchestration待实现）
日期：2026-07-31

## 目的

Compaction在模型上下文接近上限时，把sanitized live conversation的连续stable-unit prefix转换为portable rolling summary，并保留最大的可行exact suffix。

目标：

- 所有summary调用继续经过PromptSet与ModelGateway；
- source boundary由live conversation reducer提供，不从message value反查EntryId；
- complete Tool exchange永远不可拆；
- summary可跨provider使用；
- pressure、cut和summary budget使用active Turn exact model/estimator/settings；
- live Replace先发生，record outcome不决定Replace是否保留；
- recorded marker丢失或损坏时cold replay安全退回旧conversation；
- 不维护active execution checkpoint、protected-entry set或coverage proof chain。

## 决策摘要

- Compaction是crate-private planning/validation deep module；
- M10 ActiveTurnTask拥有trigger、model call、logical retry、control arbitration与record/publication；LiveSessionState reducer拥有atomic live Replace；
- ordinary Prompt继续消费`LiveConversationView`；Compaction额外消费reducer-owned `LiveCompactionSourceView`；
- source按provider-valid stable unit携带exact EntryId origin；
- plan持有source + cut，marker只能从cut派生；
- token estimate不进入reducer source，由Compaction使用Turn-pinned estimator计算；
- Runtime-global settings在Turn admission时capture immutable snapshot；
- CompactionSummary使用active Turn captured model与`OutputContract::NoToolCalls`；
- automatic StoredCompaction始终保存完整safe model-call provenance；
- summary成功后先Replace live conversation，再inline attempt record marker；
- cold replay无法应用marker时忽略并报告diagnostic。

## M4、M5 与 M10 闭合边界

**INV-005是整体不变量，完整闭合属于M10。** M4只闭合其中synchronous/no-await live-reducer subset，且不启动Compaction orchestration：

- reducer apply接受immutable `LiveCompactionSourceView`、nonzero且in-range的cut、Compaction-owned opaque `CompactionReplacement` proof，以及owning Session/Turn orchestration提供的`TurnId + Timestamp`；它从`source + cut`自行派生唯一marker，并只比较replacement内exact `StoredCompaction` marker，绝不接受raw replacement messages、raw retained suffix、planner output或模型调用输入；
- apply前在同一个无await operation创建fresh current source，调用`source.has_same_stable_identity(&fresh_current_source)`验证其`SessionId`、revision和stable-unit identity完全匹配，确认没有pending exchange并验证replacement marker等于derived marker；cross-session、stale、mismatched source/marker和pending-exchange一律拒绝；
- reducer先consume `replacement.into_parts()`，并以exact rolling-summary `ModelMessage`完成`PreparedLiveCompactionUnit` preparation；随后可将prebuilt immutable summary clone到leading unit和flattened `LiveConversationView`。所有上述验证（包括nonzero/in-range cut）、replacement/source projection、prepared unit与`ConversationRevision::checked_next()`都在EntryId allocation之前完成。成功时新Compaction EntryId是rolling-summary origin：reducer先construct exact `Arc<StoredSessionEntry>`、infallibly bind prepared summary unit、commit prepared Replace state、append同一Arc到full selected path并install preflighted revision；它只clone fresh current units中的**exact suffix**。整个M4 operation无await、无I/O且不调用Model/Recorder；
- M4不实现pressure/planner/token/budget/summary-model call、`Arc<CompactionPlan>`、`Arc<ModelCallRequest>`、retry、recorder ordering或publication。它只证明INV-003和INV-005的reducer-owned subset。

M5拥有recorded marker的tolerant replay：无效、过期或无法定位的recorded marker只ignore并diagnose，不能由M4 live reducer吞掉或解释。M10才完成INV-005剩余的exact Turn/control arbitration、`Arc<CompactionPlan>`与`Arc<ModelCallRequest>` identity、summary-model validation、orchestration/retry、inline recorder ordering和publication。下文的planning、summary call与recording flow均描述该M10完整合同，除非明确标为M4。

## Ownership

```text
LiveConversation reducer
├─ provider-valid message order
├─ complete Tool exchange grouping
├─ stable-unit EntryId origin
├─ rolling-summary origin
└─ ConversationRevision

PromptSet
├─ AgentRun fixed assembly basis
├─ CompactionSummary fixed assembly basis
└─ final model context assembly/proof

Compaction
├─ pressure classification
├─ Turn-pinned token estimates
├─ deterministic summary-source reduction
├─ prefix cut and budget
├─ summary validation
└─ CompactionReplacement proof around StoredCompaction payload

ActiveTurnTask
├─ current plan/request ownership
├─ ModelGateway call and logical retry
├─ stale/control arbitration
├─ live Replace request
└─ inline SessionRecorder attempt

M5 cold replay projector
└─ applies or ignores recorded marker
```

Compaction不拥有ModelGateway、SessionRecorder、LiveSessionState、retry policy、Workspace、EntryId generator或Turn terminal arbitration。

## Stable-Unit Source

`LiveConversationView`的canonical definition/constructor属于[Conversation Storage](conversation-storage.md#live与cold-conversation)，ordinary Prompt只通过its getters读取它。Live reducer额外提供Compaction-specific immutable projection；type语义、deep stable-identity method和validated factory属于Compaction，canonical producer仍是`LiveSessionState`：

```rust
#[derive(Clone)]
pub(crate) struct LiveCompactionSourceView {
    session_id: SessionId,
    revision: ConversationRevision,
    units: Arc<[LiveCompactionUnit]>,
}

#[derive(Clone)]
pub(crate) struct LiveCompactionUnit {
    first_entry_id: EntryId,
    kind: CompactionUnitKind,
    messages: Arc<[ModelMessage]>,
}

pub(crate) struct PreparedLiveCompactionUnit {
    kind: CompactionUnitKind,
    messages: Arc<[ModelMessage]>,
}

pub(crate) struct CompactionSourceError {
    reason: CompactionSourceErrorReason,
}

pub(crate) enum CompactionSourceErrorReason {
    EmptyUnitMessages,
    DuplicateUnitOrigin,
    MisplacedRollingSummary,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum CompactionUnitKind {
    RollingSummary,
    UserMessage,
    AssistantMessage,
    ToolExchange,
}

impl PreparedLiveCompactionUnit {
    pub(crate) fn for_live_reducer(
        kind: CompactionUnitKind,
        messages: Arc<[ModelMessage]>,
    ) -> Result<Self, CompactionSourceError>;

    pub(crate) fn bind_origin(self, first_entry_id: EntryId) -> LiveCompactionUnit;
}

impl LiveCompactionUnit {
    pub(crate) fn first_entry_id(&self) -> &EntryId;
    pub(crate) fn kind(&self) -> CompactionUnitKind;
    pub(crate) fn messages(&self) -> &[ModelMessage];
}

impl LiveCompactionSourceView {
    pub(crate) fn for_live_reducer(
        session_id: SessionId,
        revision: ConversationRevision,
        units: Arc<[LiveCompactionUnit]>,
    ) -> Result<Self, CompactionSourceError>;

    pub(crate) fn session_id(&self) -> &SessionId;
    pub(crate) fn revision(&self) -> &ConversationRevision;
    pub(crate) fn units(&self) -> &[LiveCompactionUnit];
    pub(crate) fn has_same_stable_identity(&self, other: &Self) -> bool;
}
```

fields保持private。`PreparedLiveCompactionUnit::for_live_reducer(kind, messages)`是专为live reducer准备的narrow crate-private validated factory；它在尚无新EntryId时完成该unit的全部message/kind validation，至少要求`messages` non-empty，并在成功后保存exact message Arc。`bind_origin(self, EntryId)`只把already-validated prepared value绑定为`LiveCompactionUnit`，无`Result`、无重新验证且不clone message。`LiveCompactionUnit`与`LiveCompactionSourceView`是immutable `Clone` handles：clone共享其Arc-backed messages/units，并保持exact entry origin、kind、order和message semantic identity；它不重建borrowed message或重新推断unit。它不要求Rust caller sealing，但canonical producer始终是`LiveSessionState`，Compaction只读消费。Compaction estimator/reduction是Prompt授权的read-ref consumer：它只经`ModelMessageRef`/`ModelAssistantContentRef`读取unit message，绝不destructure private transcript kind，也永远看不到stamp。`CompactionSourceError`也是Compaction-owned、private-field/redacted typed error：它只冻结`EmptyUnitMessages`、`DuplicateUnitOrigin`和`MisplacedRollingSummary`三种structural reason，`Debug`不得显示entry ID、message或其他source contents。prepared factory负责unit-local nonempty message/kind validation；source factory仍负责source-wide `first_entry_id` unique及`RollingSummary`至多一个且只能位于index 0。两者都不尝试从arbitrary messages推断完整Tool exchange：User/Assistant/Tool semantic grouping仍是reducer责任，并由reducer tests覆盖。

因此new User、ordinary Assistant和rolling summary都先得到`PreparedLiveCompactionUnit`，只在new entry ID allocation成功后立即`bind_origin(new_entry_id)`；此binding不可能再失败。complete Tool exchange在当前Tool entry allocation前已经有Assistant origin：reducer先用existing Assistant entry ID bind prepared exchange unit，再allocation当前Tool entry。无论哪一种，任何Prompt/message/kind validation都不跨越新EntryId allocation。

Compaction不导入、构造或返回`LiveConversationError`。作为factory caller的`LiveSessionState`在自己的integration boundary把`CompactionSourceError`映射为其own typed/redacted live-state error；它不把Compaction error包进public/source chain，也不要求Compaction知道LiveSessionState，从而不形成conceptual module error cycle。

source invariants：

- `session_id`绑定当前loaded Session，防止Fork后相同历史EntryId/revision被跨Session误用；
- `revision`是snapshot capture时的exact `ConversationRevision`；
- units按model-visible顺序排列，`messages`均非空；
- ordinary UserMessage为一个unit；
- 无ToolCall的AssistantMessage为一个unit；
- 含ToolCall的Assistant必须等到全部matching truthful ToolResult存在，才与按assistant call顺序排列的结果形成一个`ToolExchange` unit；其`first_entry_id`是Assistant entry；
- current rolling summary若存在，只能是leading unit；其`first_entry_id`是安装该summary的外层StoredCompaction entry ID；
- incomplete、orphan、abandoned-first Tool exchange不进入source；
- Interaction、progress、stream draft、usage-only和recording health不进入source；
- 每个unit的`first_entry_id`在该source内唯一；
- identical message values不合并，也不参与marker lookup。

`LiveCompactionSourceView::has_same_stable_identity()`是唯一公开的deep identity operation；source不保存、暴露或返回identity DTO。它**exactly**比较bound `SessionId + ConversationRevision`、unit count和ordered `(first_entry_id, CompactionUnitKind)` sequence，绝不比较`ModelMessage`或ToolResult value。M4 apply创建fresh current source并调用`source.has_same_stable_identity(&fresh_current_source)`；匹配后只从**fresh current** `units()[cut..]` clone retained unit handles，作为retained suffix。它绝不从borrowed messages、caller source raw messages或caller-supplied suffix重建unit。

source不携带：

```text
token estimate
TokenEstimator identity
Compaction settings
model limit
storage ordinal
recording health/commit proof
hash/fingerprint
```

这些值属于plan，不属于conversation protocol projection。

## Runtime Settings

```rust
pub struct CompactionSettings {
    pub enabled: bool,
    pub pressure_reserve_tokens: NonZeroU32,
    pub summary_min_output_tokens: NonZeroU32,
    pub summary_max_output_tokens: NonZeroU32,
    pub minimum_reclaimed_tokens: NonZeroU32,
    pub max_compactions_per_turn: NonZeroU8,
    pub summary_safety_reserve_tokens: NonZeroU32,
}

#[derive(Clone)]
pub(crate) struct CompactionSettingsSnapshot(
    Arc<ValidatedCompactionSettings>,
);
```

首版默认值：

| Field | Default |
| --- | ---: |
| `enabled` | `true` |
| `pressure_reserve_tokens` | `4096` |
| `summary_min_output_tokens` | `512` |
| `summary_max_output_tokens` | `2048` |
| `minimum_reclaimed_tokens` | `2048` |
| `max_compactions_per_turn` | `4` |
| `summary_safety_reserve_tokens` | `512` |

validation：

- `summary_min_output_tokens <= summary_max_output_tokens`；
- NonZero类型拒绝0；
- token arithmetic统一提升到checked `u64`；
- overflow返回typed `ArithmeticOverflow`，不能wrap、silent clamp或saturate成可行值。

`CompactionSettings`属于Runtime startup config。MiniCoreRuntime初始化时验证一次并持有immutable value；Turn admission clone其snapshot进入`TurnExecutionContext`。MVP不提供：

```text
Compaction settings hot reload
per-Agent / per-Session override
Runtime command mutation
JSONL persistence
settings generation/fingerprint
```

future per-Session配置必须进入SessionDefinition并定义revision/CAS语义。

## Prompt与Model Planning Basis

PromptSet继续是唯一assembly seam，同时向Compaction暴露两个窄、crate-private纯值：

```rust
pub(crate) struct AgentRunCompactionAssemblyBasis {
    fixed_input_tokens: u64,
    rolling_summary_message_overhead_tokens: u64,
    estimator: TokenEstimator,
}

pub(crate) struct CompactionSummaryAssemblyBasis {
    fixed_prompt_tokens: u64,
    system_sections: Arc<[PromptSection]>,
    output_contract: OutputContract,
    estimator: TokenEstimator,
}
```

`AgentRunCompactionAssemblyBasis.fixed_input_tokens`覆盖exact next ordinary AgentRun中conversation之外的System、Turn-static User context、ToolSpec、output-contract/structural framing和conservative message-sequence envelope。`rolling_summary_message_overhead_tokens`覆盖future user-role historical summary message除正文之外的结构开销。两者与stable-unit estimates相加必须是同一estimator下final AgentRun input estimate的conservative upper bound。

`CompactionSummaryAssemblyBasis`只覆盖Runtime required summary System policy、`NoToolCalls`、empty ToolSpec和固定结构，不包含dynamic directive或summary source。

Model basis只能由exact TurnModelSnapshot投影：

```rust
pub(crate) struct CompactionModelBasis {
    turn_model: TurnModelRef,
    model_summary: ModelResponseSummary,
    limits: EffectiveModelLimits,
    estimator: TokenEstimator,
    agent_run_output_reserve_tokens: NonZeroU32,
}
```

`agent_run_output_reserve_tokens`是TurnModelSnapshot在ordinary `ModelCallRequest.max_output_tokens = None`时使用的effective generation reservation，不是provider-advertised context limit。Compaction验证两个Prompt basis与model basis使用相同TokenEstimator；TurnExecutionContext private constructor保证它们来自同一次capture。

## Module Interface

以下`pressure/plan/validate_summary` interface是M10 planning/validation module；M4不构造或接受它的任何input/output，只调用前述reducer-owned `CompactionReplacement` apply。

```rust
pub(crate) struct Compaction;

impl Compaction {
    pub fn pressure(
        &self,
        input: CompactionPressureInput<'_>,
    ) -> CompactionPressure;

    pub fn plan(
        &self,
        input: CompactionPlanInput,
    ) -> Result<Arc<CompactionPlan>, CompactionError>;

    pub fn validate_summary(
        &self,
        plan: Arc<CompactionPlan>,
        result: &ModelCallResult,
        logical_retry_count: u8,
    ) -> Result<ValidatedCompactionSummary, CompactionError>;
}
```

`build_summary_directive()`不再是独立caller operation；directive由`plan()`构造并封装在plan中，减少caller可制造的不一致中间状态。

### Pressure Input

```rust
pub(crate) enum CompactionTrigger {
    ProactivePressure,
    PromptContextOverflow,
    ProviderContextOverflow,
}

pub(crate) struct CompactionPressureInput<'a> {
    pub source: &'a LiveCompactionSourceView,
    pub settings: &'a CompactionSettingsSnapshot,
    pub agent_run: &'a AgentRunCompactionAssemblyBasis,
    pub model: &'a CompactionModelBasis,
    pub trigger: CompactionTrigger,
    pub compactions_started: u8,
}

pub(crate) enum CompactionPressure {
    NotNeeded,
    Recommended,
    Required,
    Impossible(CompactionImpossibleReason),
}
```

pressure不读取current Runtime config、filesystem、Conversation Storage或provider。

### Plan Input

```rust
pub(crate) struct CompactionPlanInput {
    pub source: Arc<LiveCompactionSourceView>,
    pub settings: CompactionSettingsSnapshot,
    pub agent_run: AgentRunCompactionAssemblyBasis,
    pub summary_assembly: CompactionSummaryAssemblyBasis,
    pub model: CompactionModelBasis,
    pub trigger: CompactionTrigger,
    pub compactions_started: u8,
}
```

全部字段通过private constructors或TurnExecutionContext narrow methods形成；caller不能提交raw prefix、retained suffix、marker、summary output limit或任意token rate。

### Plan

```rust
pub(crate) struct CompactionPlan {
    source: Arc<LiveCompactionSourceView>,
    settings: CompactionSettingsSnapshot,
    trigger: CompactionTrigger,
    summarized_unit_count: NonZeroUsize,
    summary_source: Arc<CompactionSummarySourceView>,
    directive: CompactionSummaryDirective,
    budget: CompactionSummaryBudget,
    model: CompactionModelBasis,
    estimated_before_tokens: u64,
    estimated_after_upper_bound_tokens: u64,
    estimated_reclaimed_tokens: u64,
}

pub(crate) struct CompactionSummarySourceView {
    source_revision: ConversationRevision,
    messages: Arc<[ModelMessage]>,
}

pub(crate) struct CompactionSummaryBudget {
    fixed_prompt_tokens: u64,
    reduced_source_tokens: u64,
    directive_tokens: u64,
    safety_reserve_tokens: NonZeroU32,
    max_output_tokens: NonZeroU32,
}
```

plan只保存一个boundary事实：`summarized_unit_count`。以下值必须由plan method派生，不能独立传入：

```text
summary prefix = source.units[0..summarized_unit_count]
retained suffix = source.units[summarized_unit_count..]
first_kept_entry_id =
  source.units.get(summarized_unit_count).map(first_entry_id)
```

`summary_source`是summary prefix的确定性reduced provider-valid representation，只用于CompactionSummary Prompt。它不包含retained suffix。plan不保存第二个caller-controlled replacement vector。

## Pressure Rules

Compaction使用model basis中的exact estimator估算每个stable unit。unit包含不可估算的non-text/provider extension时返回unknown；不按model name或content kind猜值。

```text
estimated AgentRun input
= agent_run.fixed_input_tokens
+ sum(all stable-unit estimates)

effective headroom
= max(
    settings.pressure_reserve_tokens,
    model.agent_run_output_reserve_tokens
  )
```

规则：

- `ProactivePressure`：
  - settings disabled、unknown context window或unknown unit estimate → `NotNeeded`；
  - estimated input >= context window → `Required`；
  - estimated input + effective headroom >= context window → `Recommended`；
  - 其他为`NotNeeded`；
- `PromptContextOverflow | ProviderContextOverflow`：默认`Required`；
- Required情况下若disabled、source为空、context unknown、source不可估算或per-Turn count已耗尽，返回`Impossible(reason)`；
- Recommended情况下若count耗尽或没有满足minimum reclaim的prefix，caller保留已经valid的ordinary AgentRun并发布diagnostic，不把它升级为Turn failure；
- checked add overflow视为Required；若随后无法plan则fail closed。

`max_compactions_per_turn`统计已经成功assemble immutable summary request、并把exact plan/request安装为current Compaction operation而即将启动summary logical call chain的次数。ActiveTurnTask在该原子task-local安装点递增，随后才允许第一次Gateway调用。一次CompactionSummary logical retry不增加该计数；pressure、plan、Prompt assembly或ModelCallRequest construction失败均不增加。

## Prefix Cut与Budget

planner只尝试非空连续prefix：

```text
cut = 1, 2, ... source.units.len()
```

从1开始选择第一个满足全部约束的cut，因此保留最大的exact suffix。一个`ToolExchange`永远是一个unit，不能切入Assistant/ToolResult之间；existing rolling summary位于unit 0，因此任何后续Compaction都会把它并入新summary，不形成summary链。

### Deterministic Source Reduction

summary request representation可以缩减大ToolResult，但必须保留：

```text
tool name
ToolCallId
head/tail content
original byte count
omitted byte count
```

`ModelMessageRef::Tool`故意只含`ToolCallId + ToolResultContent`，不泄漏Tools execution disposition；summary reduction不得重新索取或伪造一个“outcome kind”。reduction由versioned `CompactionSummaryFormatVersion`固定，输入相同则输出相同。它只修改`CompactionSummarySourceView`，不改写live/durable ToolResult，也不拆开Tool exchange。首版不把无摘要的truncation直接安装为conversation result。

### Summary Budget

每个candidate先构造reduced source和directive，再计算：

```text
available output
= context window
  - summary fixed prompt
  - reduced source
  - directive
  - summary safety reserve

summary max output
= min(
    configured summary maximum,
    known model output maximum when present,
    available output
  )
```

任何subtraction underflow都表示candidate不可行。`summary max output`低于`summary_min_output_tokens`时返回/继续搜索`NoFeasibleSummaryBudget`。PromptSet与ModelGateway都不能静默clamp。

### Post-Replace Feasibility

```text
estimated post-Replace input
= agent_run.fixed_input_tokens
+ agent_run.rolling_summary_message_overhead_tokens
+ summary max output
+ sum(retained stable-unit estimates)
```

candidate还必须满足：

```text
estimated post-Replace input + effective headroom <= context window
estimated before - estimated post-Replace >= minimum_reclaimed_tokens
```

所有值都使用conservative upper bound。若没有cut同时满足summary-call fit、post-Replace fit和minimum reclaim，plan返回typed error。

## Prompt与Model Call Binding

```text
Arc<CompactionPlan>
→ TurnExecutionContext::assemble_compaction(plan)
→ PromptSet.assemble(
     CompactionSummary {
       source: plan.summary_source,
       directive: plan.directive,
     }
   )
→ ModelCallRequest::new(
     purpose = CompactionSummary,
     source_revision = plan.source.revision,
     max_output_tokens = Some(plan.budget.max_output_tokens),
   )
```

PromptSet固定使用：

```text
Runtime required summary System policy
user-role portable summary directive/source
OutputContract::NoToolCalls
empty ToolSpec
```

Prompt assembly proof必须匹配exact TurnModelRef、source revision、estimator structural basis和完整`CompactionSummaryBudget`。CompactionSummary logical retry复用同一个`Arc<CompactionPlan>`与`Arc<ModelCallRequest>`；ModelGateway每次调用仍最多一个provider attempt。

## Summary Validation与Provenance

```rust
pub struct StoredCompaction {
    pub summary: String,
    pub first_kept_entry_id: Option<EntryId>,
    pub model_call: Option<StoredCompactionModelCall>,
}

pub struct StoredCompactionModelCall {
    pub model: ModelResponseSummary,
    pub response_id: Option<ProviderResponseId>,
    pub usage: Option<ModelUsage>,
    pub finish_reason: ModelFinishReason,
    pub requested_max_output_tokens: NonZeroU32,
    pub logical_retry_count: u8,
    pub metadata: ProviderResponseMetadata,
}
```

Compaction是`StoredCompactionModelCall`的唯一semantic owner；Conversation Storage只引用和serialize该type，ModelGateway只提供normalized source fields。

```rust
pub(crate) struct CompactionReplacement {
    stored: StoredCompaction,
    rolling_summary: ModelMessage,
}

pub(crate) struct CompactionReplacementError {
    reason: CompactionReplacementErrorReason,
}

pub(crate) enum CompactionReplacementErrorReason {
    InvalidRollingSummary,
}

impl CompactionReplacement {
    #[cfg(test)]
    pub(crate) fn for_m4_test(
        stored: StoredCompaction,
    ) -> Result<Self, CompactionReplacementError>;

    pub(crate) fn into_parts(self) -> (StoredCompaction, ModelMessage);
}
```

`CompactionReplacement`是M4 apply接受的唯一summary proof，fields private and `Debug` redacts the summary and all model provenance. `CompactionReplacementError` has private fields; its `Debug`、`Display` and source chain redact summary text, `ModelMessageError` detail and model provenance. **M4 interface只提供上述`#[cfg(test)] for_m4_test` construction seam。** It materializes no caller-supplied transcript: factory takes an exact test `StoredCompaction`, calls Prompt's fallible `ModelMessage::rolling_summary()` once, and maps only that constructor's reachable `ModelMessageErrorReason::{EmptyText, UnsafeText, TextTooLong}` to the one redacted M4 reason `CompactionReplacementErrorReason::InvalidRollingSummary`. Empty assistant content and duplicate ToolCallId are unreachable here and are covered by separate assistant-constructor tests. This is intentionally the complete M4 replacement-error taxonomy; it does not expose text, size, CR, model provenance or a foreign source error. `into_parts(self)` is consuming and returns the exact owned `(StoredCompaction, ModelMessage)` values. The reducer consumes them before allocation; it may then clone the prebuilt immutable rolling-summary `ModelMessage` into the leading stable unit and flattened `LiveConversationView`. That is the documented shared-value projection, not an undocumented seam or a reconstruction of either value.

M4 has no production replacement constructor and does not name or depend on `ValidatedCompactionSummary`. **M10 now adds** production construction from `ValidatedCompactionSummary`: it materializes exact `StoredCompaction`, calls the same Prompt constructor, and seals the result before live apply. That M10 addition belongs to summary validation/provenance, not to this M4 interface.

The wrapper proves summary/message construction has completed, not that M4 independently validated model provenance. M4 only validates the source/cut/marker/live-state conditions. Raw decoded or replay `StoredCompaction::reconstruct` is an M5 cold-projector operation: it invokes the same fallible Prompt rolling-summary constructor only to build replay projection, or ignore/diagnose a bad marker/text, but can never create `CompactionReplacement` or call the live reducer directly.

`validate_summary()`要求：

- result model exact匹配plan的TurnModelRef；
- finish reason只能是`Stop | Unknown`；
- finalized response在adapter normalization后包含exact一个non-empty Text block；summary按external provider text规则校验，payload中的CR/CRLF不做owner normalization并fail closed；这与JSONL physical line接受CRLF无关；
- optional Reasoning不写入portable summary；
- ToolCall、Refused、empty/reasoning-only output拒绝；
- `logical_retry_count <= 1`；
- `requested_max_output_tokens`只能来自plan budget；
- response ID和metadata已经通过ModelGateway allowlist/redaction validation。

validated value持有exact plan，并生成：

```text
summary
plan-derived first_kept_entry_id
Some(StoredCompactionModelCall)
```

首版automatic SummaryModel Compaction始终`model_call = Some`。M10 summary validation完成时才会新增`ValidatedCompactionSummary → CompactionReplacement` production construction，并只以其sealed result请求LiveSessionState apply；no raw `StoredCompaction` is a live-reducer input. `None`仅为future显式设计的deterministic maintenance/import路径保留，不是automatic overflow fallback。[Conversation JSONL Format V1](../formats/conversation-jsonl-v1.md#compaction)冻结camelCase fields、null、summary<=65,536 bytes、finish/retry/request-max/metadata和marker encoding；不改变这里的semantic owner。

## M10 Live Replace与Recording

**Canonical cross-module invariant: INV-005.**

After `validate_summary()` succeeds, M10's production construction seals `CompactionReplacement` **before** it calls `LiveSessionState::apply_compaction(...)`. ActiveTurnTask first performs its final exact Turn/control/emergency and `Arc<CompactionPlan>`/`Arc<ModelCallRequest>` arbitration; it then delegates the unchanged M4 source/cut/marker transaction to the reducer. That no-await reducer operation receives an already sealed wrapper and never constructs a summary message itself:

```text
receive prebuilt CompactionReplacement from M10 ValidatedCompactionSummary construction
→ reducer consumes replacement.into_parts(); prepares rolling-summary stable unit
→ create fresh current source; validate `source.has_same_stable_identity(&fresh_current_source)` and plan-derived marker boundary
→ validate/projection/candidate preparation, then ConversationRevision.checked_next()
→ LiveSessionState.EntryIdGenerator.allocate()? + binds parent_id
→ infallibly construct exact Arc<StoredSessionEntry>
→ infallibly bind prepared RollingSummary to new Compaction EntryId
→ commit prepared Replace state with retained unit handles cloned only from fresh current source
→ append the same Arc to full selected path and install the preflighted revision
→ await SessionRecorder.record(the same Arc<StoredSessionEntry>)
→ publish usage/conversation update and continue Turn
```

validation、projection/candidate preparation、checked revision preflight和EntryId allocation发生在同一个无await live-owner operation中；returned error never consumes an ID or applies partial Replace. After allocation, exact entry-Arc construction, prepared-unit binding, state commit, same-Arc path append and revision install are infallible (ordinary allocation panic is outside the Result contract). State is never applied before entry construction. live reducer不接受raw replacement messages。M4以同样的allocation-after-validation纪律执行其source/cut/`CompactionReplacement`/`TurnId`/`Timestamp` subset；TurnId按current/start semantics验证，Timestamp是typed orchestration fact而非ambient clock，且没有Recorder、control/request或publication步骤。

queued Steer不改变revision，不使in-flight Compaction失效。只有safe point实际apply的Steer、Assistant、complete Tool exchange、Input或另一Compaction Replace递增revision。Cancel/SecurityRevoked/Lifecycle可以在revision未变时通过control arbitration拒绝result。

record outcome不参与Replace validity。Recorder failure时：

- current process继续使用summary；
- new rolling summary仍有稳定EntryId origin；
- Snapshot可以显示summary和Degraded recording；
- restart只恢复recorded旧conversation；
- 不重新调用summary model来补偿recording failure。

## M5 Cold Replay

遇到StoredCompaction时，projector先从当前selected recorded path构造effective provider-valid stable units，再解释marker：

- `first_kept_entry_id = None`：当前effective units非空时，用summary替换全部units；
- `Some(id)`：只在`id`exact匹配index大于0的unit `first_entry_id`时生效；
- marker指向ToolExchange内部ToolResult、missing/orphan/ignored entry、已经被旧Compaction覆盖的unit、非selected path或第一个unit时忽略；
- ignored entry产生bounded redacted diagnostic，但不brick history；
- valid summary形成新的leading `RollingSummary` unit，其origin是当前StoredCompaction外层EntryId；
- 后续valid Compaction可以在先前invalid/ignored marker之后继续生效。

Replay不恢复plan、settings、TokenEstimator、source revision、ActiveTurnTask或ModelCallRequest，也不重新验证当时summary预算。

## M10 Async Execution

```text
exact next AgentRun pressure/ContextOverflow
→ capture CapturedConversationViews (including LiveCompactionSourceView) + Turn bases
→ Compaction::pressure
→ Compaction::plan
→ assemble CompactionSummary + immutable ModelCallRequest
→ atomically install exact plan/request as current task-local operation
   and increment per-Turn count
→ await ModelGateway
→ optional one logical retry with same request
→ validate same Turn/control/session/revision/plan/request
→ Compaction::validate_summary
→ M10 adds production ValidatedCompactionSummary → CompactionReplacement construction
  (typed failure before live apply)
→ final no-await ActiveTurnTask arbitration of exact Turn/control/emergency
  + Arc<CompactionPlan>/Arc<ModelCallRequest>/Session/revision immediately before reducer apply
→ live Replace with preflighted revision
→ await inline record StoredCompaction attempt
→ consume safe-point Steer or reassemble AgentRun
```

不安装`RunningOperation::Compaction`，不重建AgentLoop，也不让SessionExecutor control actor await整个Compaction operation。

## Failure

- proactive unknown limit/estimate：不触发；
- hard overflow unknown limit/estimate：typed impossible，Turn按ContextOverflow failure；
- disabled/per-Turn limit/no compressible prefix：Recommended可skip，Required fail closed；
- no feasible summary budget/post-fit/minimum reclaim：Recommended可skip，Required fail closed；
- summary provider error：按CompactionSummary logical retry policy；
- invalid/refused summary：Turn failure，不apply Replace；
- stale revision/control/session/plan/request：丢弃result，不apply；
- live Replace invariant failure：Turn invariant failure；
- encode/write failure：Replace保留，recording health Degraded，Turn继续。

## Fork

- loaded Fork从同一个LiveSnapshot复制effective path，包含capture前已经live Replace但尚未record的Compaction entry与summary origin；
- unloaded Fork从RecordedHistory投影，只包含实际recorded且marker有效的summary；
- copied historical EntryId保留，future child append分配fresh ID；
- target staging重放同一stable-unit marker规则；
- 不复制Compaction task、plan、request、settings snapshot、retry timer或estimates。

## 测试要求

### Stable Units与Marker

- duplicate identical User/Assistant message仍派生exact不同marker；
- complete Tool exchange是单一unit，parallel result完成顺序不改变call-order messages；
- cut/marker不能指向ToolResult；
- incomplete/orphan/abandoned-first exchange不进入source；
- rolling summary origin是对应StoredCompaction outer EntryId；
- rolling summary可以再次摘要且只保留一个leading summary；
- all units summarized产生`first_kept_entry_id = None`；
- cross-Session source即使revision/EntryId相同也被拒绝。
- live-reducer unit/source factories reject empty unit messages, duplicate origins and RollingSummary outside index zero; complete Tool-exchange grouping remains reducer-tested rather than factory-inferred.
- `has_same_stable_identity()` uses SessionId/revision/unit count/ordered `(first_entry_id, kind)`, not equal ModelMessage values.

### M4 Reducer Subset

- M4 creates a fresh current source and `source.has_same_stable_identity(&fresh_current_source)` verifies SessionId、revision、unit count和ordered stable-unit identity；
- nonzero/in-range cut唯一派生marker；`CompactionReplacement` consumption/prepared rolling-summary unit、marker不匹配、source stale/cross-session、`has_same_stable_identity()`不匹配或存在pending exchange都在EntryId allocation前完成或拒绝；
- apply消费exact replacement parts并在allocation前prepare unit；可clone prebuilt immutable summary进入leading unit/flattened view；以new Compaction EntryId为origin并只clone fresh current units中的exact retained suffix，先`checked_next()`再allocation、最后依序infallibly construct exact entry Arc、bind prepared unit、commit Replace、append同一Arc到full path并install preflighted revision；拒绝路径不改变EntryId/head/revision/state且不做I/O；
- reducer不接收raw replacement messages/suffix/marker或raw `StoredCompaction`，不构造plan/request、不验证provider provenance、不估算token/budget、不调用模型或Recorder；
- M4 only has `#[cfg(test)] CompactionReplacement::for_m4_test(...) -> Result<_, CompactionReplacementError>`; M10 production uses only `ValidatedCompactionSummary → CompactionReplacement` construction. raw/replay StoredCompaction reconstruction never calls the reducer;
- M4 stale tests使用stale/cross-session source与cut/marker mismatch，不使用不存在的plan/request staleness。
- all M4 rejection tests participate in the [deterministic sentinel/no-ID matrix](../development-plan.md#m4-liveconversation-reducer): source factory/stale/cross-session/identity、out-of-range nonzero cut、marker mismatch和pending exchange leave head/full path/revision/all state unchanged and leave the same first candidate for the next successful allocation; zero is separately rejected before reducer invocation, and prepared-unit failure is likewise pre-allocation.

### Settings与Planning

- 默认值和min/max validation；
- active Turn settings在Runtime config未来变化时仍不漂移；
- proactive threshold、hard Prompt/provider overflow；
- unknown context与unknown unit estimate；
- model output max与configured summary max求交；
- summary minimum、safety reserve、post-Replace headroom和minimum reclaim；
- first feasible cut保留最大exact suffix；
- existing summary与大ToolResult deterministic reduction；
- checked arithmetic overflow；
- max Compactions per Turn计operation、不计logical retry。

### M10 Request、Apply与Replay；M5 Recorded-Marker Replay

- plan budget等于Prompt proof与ModelCallRequest max output；
- summary retry复用same plan/request；
- automatic provenance总是Some且字段完整；
- StoredCompaction format-v1 golden round-trip、summary 65,536-byte boundary/+1、automatic model_call non-null；
- wrong model、Refused、multiple/empty Text或retry count > 1拒绝；
- consumed Steer使plan stale，queued Steer不使其stale；
- Cancel/SecurityRevoked在revision不变时仍拒绝result；
- EntryId allocation和live Replace先于record attempt；
- recording failure保留live summary与origin；
- restart在marker缺失时恢复旧conversation；
- marker missing/orphan/ignored/first-unit/ToolResult时忽略；
- later valid Compaction在invalid marker后仍生效；
- loaded Fork在Degraded recording下复制unrecorded summary与稳定IDs。

## 被否决的方案

### 扩大ordinary LiveConversationView

会把EntryId/kind泄漏给所有Prompt assembly，并让storage origin污染普通model input seam。

### Message value equality反查marker

重复文本或结构化内容无法唯一定位；Tool exchange跨多entry时更不成立。

### Reducer预计算estimated_tokens

会把Turn model/estimator policy引入conversation source，或者需要额外estimator identity。estimate由Compaction在exact source snapshot上计算更local。

### 独立prefix/suffix/marker字段

三份boundary事实可以不一致。source + cut index可以确定性派生全部结果。

### Retained-suffix target setting

first feasible cut已经最大化suffix；额外target增加policy和配置surface，但不提高correctness。

### 恢复active checkpoint/protected entries

ADR 0124已经删除scope、frontier与coverage chain。MVP允许旧Input/Steer进入portable summary，不恢复该复杂度。

## 完成检查

- [x] 冻结reducer-owned stable-unit source与exact origin。
- [x] 冻结Runtime-global settings、默认值和Turn capture。
- [x] 冻结Pressure/Plan input、cut、budget和bounded progress。
- [x] 冻结automatic StoredCompaction model-call provenance。
- [x] 冻结live Replace与cold replay marker规则。
- [x] ADR 0134/Format V1冻结StoredCompaction wire casing、limits与golden vector contract。
- [ ] 实现ScriptedProviderAdapter Compaction vertical slice。
- [ ] 实现Rig provider CompactionSummary mapping。
