# Compaction 架构设计

状态：当前权威架构（ADR 0132后，生产实现待启动）
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
- ActiveTurnTask拥有trigger、model call、logical retry和live Replace；
- ordinary Prompt继续消费`LiveConversationView`；Compaction额外消费reducer-owned `LiveCompactionSourceView`；
- source按provider-valid stable unit携带exact EntryId origin；
- plan持有source + cut，marker只能从cut派生；
- token estimate不进入reducer source，由Compaction使用Turn-pinned estimator计算；
- Runtime-global settings在Turn admission时capture immutable snapshot；
- CompactionSummary使用active Turn captured model与`OutputContract::NoToolCalls`；
- automatic StoredCompaction始终保存完整safe model-call provenance；
- summary成功后先Replace live conversation，再inline attempt record marker；
- cold replay无法应用marker时忽略并报告diagnostic。

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
└─ StoredCompaction payload/provenance candidate

ActiveTurnTask
├─ current plan/request ownership
├─ ModelGateway call and logical retry
├─ stale/control arbitration
├─ live Replace request
└─ inline SessionRecorder attempt

Cold replay projector
└─ applies or ignores recorded marker
```

Compaction不拥有ModelGateway、SessionRecorder、LiveSessionState、retry policy、Workspace、EntryId generator或Turn terminal arbitration。

## Stable-Unit Source

`LiveConversationView`保持ordinary Prompt所需的最小shape：

```rust
pub(crate) struct LiveConversationView {
    revision: ConversationRevision,
    messages: Arc<[ModelMessage]>,
}
```

Live reducer额外提供Compaction-specific immutable projection：

```rust
pub(crate) struct LiveCompactionSourceView {
    session_id: SessionId,
    revision: ConversationRevision,
    units: Arc<[LiveCompactionUnit]>,
}

pub(crate) struct LiveCompactionUnit {
    first_entry_id: EntryId,
    kind: CompactionUnitKind,
    messages: Arc<[ModelMessage]>,
}

pub(crate) enum CompactionUnitKind {
    RollingSummary,
    UserMessage,
    AssistantMessage,
    ToolExchange,
}
```

fields与constructors保持private。`LiveConversation`/`LiveSessionState`是唯一producer，Compaction只读消费。

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

pressure不读取current Runtime config、filesystem、SessionStorage或provider。

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
truthful outcome kind
head/tail content
original byte count
omitted byte count
```

reduction由versioned `CompactionSummaryFormatVersion`固定，输入相同则输出相同。它只修改`CompactionSummarySourceView`，不改写live/durable ToolResult，也不拆开Tool exchange。首版不把无摘要的truncation直接安装为conversation result。

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

`validate_summary()`要求：

- result model exact匹配plan的TurnModelRef；
- finish reason只能是`Stop | Unknown`；
- finalized response在adapter normalization后包含exact一个non-empty Text block；
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

首版automatic SummaryModel Compaction始终`model_call = Some`。`None`仅为future显式设计的deterministic maintenance/import路径保留，不是automatic overflow fallback。wire casing、字段长度和format-v1 encoding进入V4-P1-2，不改变这里冻结的semantic fields。

## Live Replace与Recording

**Canonical cross-module invariant: INV-005.**

```text
validate current TurnId/control_generation/EmergencyControl
→ validate exact current Arc<CompactionPlan> + Arc<ModelCallRequest>
→ validate plan.session_id == loaded SessionId
→ validate current ConversationRevision == plan.source.revision
→ validate plan-derived marker仍是current stable-unit boundary
→ LiveSessionState.EntryIdGenerator allocates Compaction EntryId + binds parent_id
→ apply live Replace:
     RollingSummary(origin = new Compaction EntryId)
     + exact current retained units
→ increment ConversationRevision once
→ await SessionRecorder.record(the same StoredSessionEntry)
→ publish usage/conversation update and continue Turn
```

validation和EntryId allocation发生在同一个无await live-owner operation中；失败不消耗可观察EntryId或apply部分Replace。live reducer不接受raw replacement messages。

queued Steer不改变revision，不使in-flight Compaction失效。只有safe point实际apply的Steer、Assistant、complete Tool exchange、Input或另一Compaction Replace递增revision。Cancel/SecurityRevoked/Lifecycle可以在revision未变时通过control arbitration拒绝result。

record outcome不参与Replace validity。Recorder failure时：

- current process继续使用summary；
- new rolling summary仍有稳定EntryId origin；
- Snapshot可以显示summary和Degraded recording；
- restart只恢复recorded旧conversation；
- 不重新调用summary model来补偿recording failure。

## Cold Replay

遇到StoredCompaction时，projector先从当前selected recorded path构造effective provider-valid stable units，再解释marker：

- `first_kept_entry_id = None`：当前effective units非空时，用summary替换全部units；
- `Some(id)`：只在`id`exact匹配index大于0的unit `first_entry_id`时生效；
- marker指向ToolExchange内部ToolResult、missing/orphan/ignored entry、已经被旧Compaction覆盖的unit、非selected path或第一个unit时忽略；
- ignored entry产生bounded redacted diagnostic，但不brick history；
- valid summary形成新的leading `RollingSummary` unit，其origin是当前StoredCompaction外层EntryId；
- 后续valid Compaction可以在先前invalid/ignored marker之后继续生效。

Replay不恢复plan、settings、TokenEstimator、source revision、ActiveTurnTask或ModelCallRequest，也不重新验证当时summary预算。

## Async Execution

```text
exact next AgentRun pressure/ContextOverflow
→ capture LiveCompactionSourceView + Turn bases
→ Compaction::pressure
→ Compaction::plan
→ assemble CompactionSummary + immutable ModelCallRequest
→ atomically install exact plan/request as current task-local operation
   and increment per-Turn count
→ await ModelGateway
→ optional one logical retry with same request
→ validate same Turn/control/session/revision/plan/request
→ Compaction::validate_summary
→ live Replace + revision increment
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

### Request、Apply与Replay

- plan budget等于Prompt proof与ModelCallRequest max output；
- summary retry复用same plan/request；
- automatic provenance总是Some且字段完整；
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
- [ ] V4-P1-2冻结StoredCompaction wire casing、limits与golden vectors。
- [ ] 实现ScriptedProviderAdapter Compaction vertical slice。
- [ ] 实现Rig provider CompactionSummary mapping。
