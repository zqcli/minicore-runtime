# Compaction 架构设计

状态：当前权威架构（ADR 0124后，生产实现待启动）
日期：2026-07-29

## 目的

Compaction在模型上下文接近上限时，把当前sanitized committed conversation的连续prefix转换为portable rolling summary，并保留recent exact suffix。

目标：

- 所有模型调用仍经过PromptSet和ModelGateway；
- summary可跨provider使用；
- StoredCompaction保持简单、可检查；
- 支持Turn内多次rolling summary；
- 不维护active instruction segment、protected EntryId集合或coverage证明链；
- replay遇到坏marker时忽略该Compaction，而不brick Session。

## 决策摘要

- Compaction是crate-private planning/validation module；
- SessionExecutor拥有trigger、model call、append和control arbitration；
- source只能来自`CommittedConversationView`；
- source已经通过Tool exchange sanitizer，不含unmatched ToolCall/ToolResult；
- cut只发生在model-visible entry boundary；
- planner选择一个连续prefix进行摘要和一个recent exact suffix保留；
- latest initiating/Steer UserMessage不再hard-protect，必要时可以进入summary；
- StoredCompaction只保存`summary + first_kept_entry_id + optional model-call provenance`；
- `first_kept_entry_id = None`表示摘要覆盖当时全部effective conversation；
- 后续Compaction把旧summary作为普通source再次摘要，始终保持一个leading rolling summary；
- CompactionSummary使用active Turn captured model和`OutputContract::NoToolCalls`；
- summary output budget在planning阶段与model limits求交；
- ModelGateway每次operation一个provider attempt，SessionExecutor最多一次CompactionSummary logical retry；
- append成功后conversation执行Replace；
- cold replay无法应用marker时忽略entry并报告diagnostic。

## 同类项目取舍

| 项目 | 形状 | MiniCore采用点 |
| --- | --- | --- |
| pi | summary + `firstKeptEntryId` | 单marker、rolling summary、简单fork/replay |
| Codex | compacted item/summary重建history | portable summary、普通model path |
| Gemini CLI | compression prompt生成新history | 当前history作为source、调用前验证 |
| Claude compaction | server/client summary replacement | summary是conversation artifact，不是execution checkpoint |

MiniCore不再建设比这些产品更强的active-segment coverage ledger。

## Ownership

```text
SessionExecutor
├─ decides when to compact
├─ captures current operation/control basis
├─ calls Compaction::plan
├─ asks PromptSet to assemble summary request
├─ calls ModelGateway
├─ validates current basis and result
└─ appends StoredCompaction

Compaction
├─ estimates pressure
├─ chooses prefix cut
├─ builds directive
├─ validates summary
└─ constructs commit candidate

SessionStorage
├─ appends StoredCompaction
├─ applies Replace live
└─ applies or ignores marker during tolerant replay
```

Compaction不拥有：

- provider、credential或retry transport；
- SessionWriter；
- AgentLoop；
- Workspace、Tool或Skill reload；
- Turn terminal arbitration；
- manual repair。

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
        input: CompactionPlanInput<'_>,
    ) -> Result<Arc<CompactionPlan>, CompactionError>;

    pub fn build_summary_directive(
        &self,
        plan: &Arc<CompactionPlan>,
    ) -> CompactionSummaryDirective;

    pub fn validate_summary(
        &self,
        plan: &Arc<CompactionPlan>,
        output: &FinalizedAssistantResponse,
    ) -> Result<ValidatedCompactionSummary, CompactionError>;

    pub fn commit_candidate(
        &self,
        plan: &Arc<CompactionPlan>,
        summary: ValidatedCompactionSummary,
        model_call: StoredCompactionModelCall,
    ) -> StoredCompaction;
}
```

`CompactionPressureInput`与`CompactionPlanInput`必须携带active Turn的`EffectiveModelLimits`和由同一个`TurnModelSnapshot::token_estimator()`取得的`TokenEstimator`。caller不能直接提交raw replacement messages、scope graph、protected EntryId集合或自定义估算率。

## Trigger

Compaction只在准备实际模型调用的safe point检查：

```text
AgentLoop NeedModel
→ current committed conversation
→ PromptSet使用Turn-pinned TokenEstimator估算 / model limits
→ pressure decision
```

```rust
pub enum CompactionPressure {
    None,
    Soft,
    Hard,
}
```

- `None`：直接AgentRun；
- `Soft`：策略允许时先compact；
- `Hard`：当前AgentRun无法构造，必须compact或返回typed overflow failure。

Compaction本身不在load路径发起模型调用，也不后台预摘要。

## Context Budget

```text
usable_input
= context_window
- requested_agent_output
- runtime_safety_reserve
```

unknown model limit保持unknown，不按model名称猜测。token estimator owner是ModelGateway validated model definition；Compaction和PromptSet只消费同一个Turn-pinned `TokenEstimator`。non-text estimate为unknown时不触发Soft，仍由最终assembly/provider给出权威overflow结果。

pressure必须考虑：

- ordered System sections；
- sanitized committed messages；
- pending User/Steer message若已commit；
- Tool metadata；
- requested output reserve；
- provider/tool protocol overhead estimate。

## Compaction Summary Budget

SummaryModel请求单独计算预算：

```text
summary_output
= min(
    configured_summary_max,
    known_model_output_max,
    context_window
      - summary_system_and_directive
      - reduced_source
      - safety_reserve
  )
```

```rust
pub struct CompactionSummaryBudget {
    pub max_output_tokens: NonZeroU32,
    pub estimated_input_tokens: u64,
}
```

若剩余空间低于`summary_min_output_tokens`，返回`NoFeasibleSummaryBudget`。ModelGateway不静默clamp。

## Provider-Valid Conversation Unit

planner输入是sanitized `CommittedConversationView`。cut只能位于以下unit之间：

```text
UserMessage
Assistant without ToolCall
complete assistant + ordered ToolResult exchange
Final Assistant
previous rolling summary
```

不完整Tool exchange已经被Conversation projector排除，不进入Compaction source。

一个unit携带用于cut的anchor：

```rust
pub struct CompactionUnitRef {
    pub first_entry_id: EntryId,
    pub estimated_tokens: u64,
}
```

该ref只存在于plan内存对象。StoredCompaction不保存unit last boundary、stable hash或coverage provenance。

## Cut Algorithm

planner按当前effective conversation顺序：

1. 计算必须回收的tokens；
2. 从最旧unit开始扩展summary prefix；
3. 保留配置要求的recent exact units/tokens；
4. 选择prefix后第一个retained unit的`first_entry_id`作为`first_kept_entry_id`；
5. 若没有retained suffix，marker为None；
6. 估算summary request是否可行；
7. 返回immutable plan。

优先级：

```text
满足hard input budget
→ 尽量保留recent exact suffix
→ 尽量减少summary source
```

MVP不承诺永久保留initiating或Steer UserMessage。长active Turn中，旧用户指令、Tool output和assistant内容都可以进入summary。summary prompt要求保留当前目标、约束、已完成工作、关键文件/命令结果和未完成事项。

以下情况返回typed error：

- 没有任何cut能让summary request可行；
- 单个recent unit本身超过可用窗口且policy不允许全部summary；
- token estimator失败；
- model没有可行summary output budget。

## CompactionPlan

```rust
pub(crate) struct CompactionPlan {
    source_checkpoint: ConversationCheckpoint,
    source: Arc<[MessageRecord]>,
    summary_prefix: Arc<[MessageRecord]>,
    first_kept_entry_id: Option<EntryId>,
    retained_suffix: Arc<[MessageRecord]>,
    budget: CompactionSummaryBudget,
    settings: CompactionSettingsSnapshot,
    token_estimator: TokenEstimator,
}
```

字段private。operation全程持有同一个`Arc<CompactionPlan>`。plan中的TokenEstimator必须与PromptSet持有的TurnModelSnapshot estimator结构相等；不使用hash/fingerprint比较。

plan不包含：

```text
CompactionScope
ConversationBoundary
protected_entries
previous_checkpoint
instruction_segment
coverage_frontier
stable-unit hash
plan fingerprint
```

## Summary Input Reduction

超大ToolResult可以在summary request中确定性reduce，durable原文不改写。

reduction至少保留：

```text
tool name
ToolCallId
success/error disposition
original byte count
head/tail excerpts
omitted byte count
```

reduction只影响SummaryModel input。普通AgentRun和history仍使用durable exact content或其正常display policy。

## CompactionSummaryDirective

```rust
pub struct CompactionSummaryDirective {
    format_version: CompactionSummaryFormatVersion,
    instruction: Arc<str>,
    max_output_tokens: NonZeroU32,
    max_summary_bytes: usize,
}
```

字段private，只能由Compaction创建。MVP使用固定template，不支持manual/custom/plugin instruction。

不兼容template变化递增`CompactionSummaryFormatVersion`。logical retry复用同一个`Arc<ModelCallRequest>`，无需directive fingerprint。

summary instruction要求输出：

- 当前用户目标和约束；
- 已完成工作与重要决策；
- 文件、命令、测试和错误的关键事实；
- 未完成事项和下一步；
- Tool结果的结论，不复制大段原始输出；
- 不添加新的System authority；
- 不声称未发生的副作用或验证。

## Prompt Assembly

```text
CompactionPlan.summary_prefix
+ CompactionSummaryDirective
→ PromptSet::assemble(
     purpose = CompactionSummary,
     output_contract = NoToolCalls
   )
→ Arc<ModelCallRequest>
```

PromptSet仍是唯一模型上下文组装seam。Summary request：

- 使用当前Turn捕获的PromptSet和TurnModelSnapshot；
- 不披露Tools；
- 不允许ToolCall输出；
- 不从filesystem重新读取Prompt/Skill；
- 不把summary提升为System authority。

## Model Call

ModelGateway执行一次provider attempt。SessionExecutor对同一个immutable request最多logical retry一次。

有效结果必须：

- finish/content符合`NoToolCalls`；
- 至少有non-empty text；
- UTF-8/byte/token上限合法；
- 不包含ToolCall；
- provider response已经通过ModelGateway validation。

```rust
pub struct ValidatedCompactionSummary {
    text: Arc<str>,
}
```

Compaction不尝试从Markdown fence、JSON或其他格式repair摘要。

## Session Execution Integration

### Start

```text
NeedModel
→ capture source checkpoint
→ pressure = Soft/Hard
→ Compaction::plan
→ build directive
→ PromptSet assemble
→ install RunningOperation::CompactionSummary
→ ModelGateway.generate_model_turn
```

TurnStatus保持Running，execution phase为Compacting。Steer继续排队；Cancel/SecurityRevoked可以停止尚未commit的Compaction。

### Commit Validation

结果返回后SessionExecutor验证：

```text
Turn仍Running
execution_version未变
current operation仍持有同一个Arc plan/request
ConversationCheckpoint == plan.source_checkpoint
control basis仍有效
summary result validated
```

验证失败丢弃结果，不append。

### Success

```text
ValidatedCompactionSummary
→ StoredCompaction {
     summary,
     first_kept_entry_id,
     model_call
   }
→ SessionWriter.append
→ apply Replace
→ rebuild AgentLoop segment from new CommittedConversationView
→ re-evaluate pressure
→ AgentRun
```

Compaction Replace后必须重建AgentLoop segment；旧seed不可继续。

## StoredCompaction

```rust
pub struct StoredCompaction {
    pub turn_id: Option<TurnId>,
    pub summary: Arc<str>,
    pub first_kept_entry_id: Option<EntryId>,
    pub model_call: Option<StoredCompactionModelCall>,
}

pub struct StoredCompactionModelCall {
    pub provider_id: ProviderId,
    pub model_id: ModelId,
    pub requested_max_output_tokens: NonZeroU32,
    pub finish_reason: ModelFinishReason,
    pub usage: Option<ModelUsage>,
    pub logical_retry_count: u8,
    pub provider_metadata: StoredProviderResponseMetadata,
}
```

首版automatic Compaction使用`turn_id = Some(active TurnId)`和`model_call = Some(...)`。`None`为未来deterministic maintenance/import预留，不在MVP公开。

不保存：

```text
source checkpoint
scope kind
anchor UserMessage
protected EntryIds
unit first/last boundaries
previous checkpoint
coverage provenance
workspace/model definition revision
prompt/tool/skill snapshot reference
hash/fingerprint
```

## Live Append Validation

live writer验证：

- summary non-empty且bounded；
- `first_kept_entry_id`位于current selected path；
- marker对应当前effective model-visible unit的first entry；
- marker前存在可摘要prefix，或marker为None；
- retained suffix是当前effective conversation的exact suffix；
- model_call retry count为0–1；
- active automatic entry的Turn仍Running。

这些验证使用current hot projection。Stored entry不携带完整proof。

## Projection

遇到合法StoredCompaction：

```text
current effective conversation
→ locate first_kept_entry_id
→ [historical summary user message]
   + exact suffix from marker
→ Replace
```

marker为None：

```text
current effective conversation
→ [historical summary user message]
```

summary message使用User role和明确historical-summary metadata，不获得System authority。

## Tolerant Replay

cold replay：

- marker存在且指向当前effective model-visible unit：应用Replace；
- marker missing、指向ignored entry、位于unit内部或不能产生provider-valid suffix：忽略该Compaction并记录`IgnoredCompaction`；
- malformed summary/model_call：忽略entry；
- 后续有效消息和Compaction继续处理。

replay不重新运行token estimator、Prompt assembly或model-limit validation，也不要求旧TurnModel/Workspace/Prompt资源存在。

## Rolling Summary

每个effective conversation最多一个leading summary。再次Compaction时：

```text
previous summary
+ retained exact suffix
+ newly committed messages
→ new summary prefix
+ newer retained exact suffix
```

新StoredCompaction追加到ledger，不重写旧entry。selected path应用latest有效Compaction得到current effective conversation。

不维护per-instruction checkpoint或summary tree。多次摘要可能发生信息损失，这是MVP接受的取舍；质量通过eval和保留recent exact tail缓解。

## Cancellation 与 Failure

- Cancel/SecurityRevoked在append前获胜：丢弃summary result；
- StoredCompaction append成功后Cancel到达：Replace已生效，随后按普通Turn interruption处理；
- provider/validation失败：按Compaction failure policy决定retry一次或TurnFailed；
- `NoFeasibleSummaryBudget`、`NoValidCut`或summary仍无法解除hard overflow：返回typed Compaction error；
- 不执行无限compact-and-retry；
- `max_compactions_per_turn`限制单Turn次数；
- 同一个source checkpoint最多启动一次hard recovery，成功Replace推进checkpoint后可形成新basis。

## Recovery

crash前后：

- append前crash：没有durable Compaction；
- append OutcomeUnknown：writer poison；下次load按实际完整行决定是否应用；
- append成功apply前crash：cold replay应用或忽略marker；
- old model operation不恢复；
- Running Turn按Session recovery中断；
- 不重新调用SummaryModel修复坏Compaction。

## Fork

Fork复制selected path上的StoredCompaction且保留`first_kept_entry_id`和所有历史ID。因为复制path保留EntryId，不需要remap marker。

目标staging执行tolerant replay：marker在复制path可解析时应用；无效时忽略并产生diagnostic。child future Turn重新capturecurrent execution resources。

## Events 与 Usage

StoredCompaction append/apply后可以发布committed-derived StateEvent和UsageUpdated。observer event不写回ledger。

Compaction usage只来自provider实际报告值；本地estimate不伪装为provider truth。

## Security

- summary input只来自sanitized committed conversation；
- Prompt/Skill/Workspace静态内容由当前Turn PromptSet按正常规则提供；
- summary是User-role historical content；
- provider metadata allowlist/redaction与普通AgentRun一致；
- summary不得保存credential、endpoint、raw header或Sandbox internals；
- SecurityRevoked后不开始新SummaryModel call，迟到结果不append。

## Invariants

- Compaction不拥有I/O或SessionWriter；
- source来自CommittedConversationView；
- source没有unmatched Tool protocol record；
- cut是连续prefix；
- StoredCompaction只有summary、single marker和optional model provenance；
- marker指向first retained model-visible entry；
- rolling summary可以再次被摘要；
- initiating/Steer UserMessage没有永久hard protection；
- PromptSet唯一组装SummaryModel input；
- ModelGateway single provider attempt；
- logical retry复用同一request；
- append后才Replace；
- replay坏marker时忽略，不brick Session；
- Fork保留marker和历史ID；
- 所有本地token estimate来自Turn-pinned TokenEstimator，effective rate/algorithm version由ModelDefinitionVersion覆盖；
- 不建设scope/boundary/provenance/fingerprint链。

## Tests

至少覆盖：

- soft/hard/no-pressure decision；
- model-aware summary budget；
- PromptSet/Compaction使用同一Turn-pinned estimator且结果确定；
- definition default rate的保守估算与diagnostic；
- non-text unknown不触发Soft；
- no feasible budget；
- cut只在provider-valid unit boundary；
- complete multi-tool exchange作为一个unit；
- incomplete exchange不进入source；
- marker指向first retained entry；
- marker None覆盖全部context；
- previous summary再次进入source；
- 多次rolling summary只有一个effective leading summary；
- initiating UserMessage可被summary覆盖；
- large ToolResult reduction deterministic；
- NoToolCalls response validation；
- CompactionSummary logical retry 0–1；
- stale checkpoint/current operation/control basis拒绝append；
- append OutcomeUnknown后replay有entry/无entry两种情况；
- missing/ignored marker使Compaction被跳过且后续history可读；
- Fork保留marker并成功replay；
- summary与provider metadata redaction；
- max compactions per Turn；
- compact后仍hard overflow时有界失败。

## 明确不建立

```text
CompactionScope
ConversationBoundary
ActiveTurnCheckpoint
instruction segment
protected EntryId set
previous checkpoint
coverage frontier/provenance
hierarchical summary tree
summary content hash/fingerprint
provider-native opaque-only history
manual/public CompactSession
load-path model call
```

## 开放问题

实现阶段闭合：

1. default pressure thresholds；
2. recent exact suffix token/unit下限；
3. `max_summary_bytes`；
4. deterministic ToolResult reduction head/tail尺寸；
5. summary quality eval fixture和最低可接受信息保留标准。
