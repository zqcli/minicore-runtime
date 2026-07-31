# Compaction 架构设计

状态：当前权威架构（ADR 0126后，生产实现待启动）
日期：2026-07-30

## 目的

Compaction在模型上下文接近上限时，把sanitized live conversation的连续prefix转换为portable rolling summary，并保留recent exact suffix。

目标：

- 所有summary调用继续经过PromptSet与ModelGateway；
- summary可跨provider使用；
- source只包含provider-valid complete Tool exchange；
- live Replace先发生，record outcome不决定Replace是否保留；
- recorded marker丢失或损坏时cold replay安全退回旧conversation；
- 不维护active execution checkpoint或coverage proof chain。

## 决策摘要

- Compaction是crate-private planning/validation module；
- ActiveTurnTask拥有trigger、model call、retry和live Replace；
- source来自`LiveConversationView`；
- plan捕获`ConversationRevision`；
- StoredCompaction只保存summary、`first_kept_entry_id`和optional model-call provenance；
- CompactionSummary使用active Turn captured model与`OutputContract::NoToolCalls`；
- summary成功后先Replace live conversation，再inline attempt record marker；
- marker record失败不回滚Replace；
- restart可能恢复未压缩旧conversation；
- cold replay无法应用marker时忽略并报告diagnostic。

## 同类项目基线

| 项目 | 形状 | MiniCore采用点 |
| --- | --- | --- |
| Pi | summary + firstKeptEntryId | rolling summary与单marker |
| Codex | compacted item重建history | async Turn flow内编排 |
| Gemini CLI | compression prompt生成新history | live history先替换，recording辅助resume |

## Ownership

```text
ActiveTurnTask
├─ observes context pressure/overflow
├─ captures ConversationRevision
├─ calls Compaction::plan
├─ asks PromptSet to assemble summary context
├─ awaits ModelGateway
├─ applies live Replace
└─ await SessionRecorder.record(StoredCompaction)

Compaction
├─ estimates pressure
├─ chooses provider-valid prefix cut
├─ builds directive
├─ validates summary
└─ constructs StoredCompaction candidate

Cold replay projector
└─ applies or ignores recorded marker
```

Compaction不拥有ModelGateway、SessionRecorder、LiveSessionState、retry policy、Workspace或terminal arbitration。

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
    ) -> CompactionDirective;

    pub fn validate_summary(
        &self,
        plan: &Arc<CompactionPlan>,
        result: &ModelCallResult,
    ) -> Result<ValidatedCompactionSummary, CompactionError>;
}
```

```rust
pub(crate) struct CompactionPlan {
    source_revision: ConversationRevision,
    summary_prefix: Arc<[ModelMessage]>,
    first_kept_entry_id: Option<EntryId>,
    retained_suffix: Arc<[ModelMessage]>,
    directive: CompactionDirective,
    summary_budget: u32,
}
```

plan字段private。EntryId marker用于recorded replay；source revision用于current-process stale validation。

## Source Conversation

source必须来自sanitized `LiveConversationView`：

- 不含unmatched ToolCall/ToolResult；
- complete Tool exchange作为不可拆分protocol unit；
- 当前rolling summary位于leading position；
- selected live path顺序确定；
- process-local provisional stream不进入source。

recorded file是否包含当前revision不影响planning。

## Prefix Cut

planner只在provider-valid边界切割：

```text
System/policy不属于conversation prefix
rolling summary可再次进入summary
User/Assistant普通消息可切
Assistant ToolCall + all matching ToolResults不可拆
recent exact suffix按token budget保留
```

latest initiating/Steer UserMessage不永久hard-protect；必要时可进入summary。

## Pressure与Budget

```rust
pub enum CompactionPressure {
    NotNeeded,
    Recommended,
    Required,
    Impossible,
}
```

planning使用Turn-pinned `TokenEstimator`与exact model limits。summary output budget在plan阶段闭合，并与`ModelCallRequest.max_output_tokens`一致。

PromptSet仍是唯一assembly seam：

```text
CompactionPlan
→ PromptSet.assemble(CompactionSummary)
→ ModelCallRequest::new
→ ModelGateway.generate_model_turn
```

## Async Execution

```text
AgentRun assembly pressure/ContextOverflow
→ phase Compacting
→ capture control_generation + ConversationRevision
→ Arc<CompactionPlan>
→ Arc<ModelCallRequest>
→ await ModelGateway
→ optional one logical retry with same request
→ validate exact generation/revision/plan/request
→ validate summary
→ LiveConversation.replace_with_summary(...)
→ revision increments
→ await SessionRecorder.record(StoredCompaction)
→ publish live conversation/usage update
→ reassemble AgentRun
```

Cancel/SecurityRevoked、Steer consumption或任何live model-visible mutation使旧plan失效。Steer在Compaction期间只排队。

不再安装`RunningOperation::Compaction`，也不重建AgentLoop。

## StoredCompaction

```rust
pub struct StoredCompaction {
    pub summary: String,
    pub first_kept_entry_id: Option<EntryId>,
    pub model_call: Option<StoredCompactionModelCall>,
}
```

```rust
pub struct StoredCompactionModelCall {
    pub model: ModelResponseSummary,
    pub usage: Option<ModelUsage>,
    pub logical_retry_count: u8,
}
```

不保存：

- source ConversationRevision；
- previous checkpoint；
- protected EntryId集合；
- coverage chain；
- ActiveTurnTask phase；
- ModelCallRequest或PromptSet identity；
- unrecorded live tail proof。

## Live Replace与Recording

```text
validate summary and current revision
→ apply live Replace
→ increment ConversationRevision
→ await inline record StoredCompaction attempt
→ continue Turn
```

record outcome不参与Replace validity。Recorder failure时：

- current process立即获得压缩后的conversation；
- Snapshot可以显示summary；
- restart恢复recorded旧conversation；
- 若旧conversation再次overflow，可重新compact；
- 不把缺失marker视为corruption。

## Cold Replay

遇到StoredCompaction：

- marker能在当前selected recorded path安全定位时，Replace recorded conversation；
- `first_kept_entry_id = None`表示摘要覆盖当时全部effective conversation；
- marker missing、orphan或切入Tool exchange时忽略entry并报告diagnostic；
- ignored Compaction不brick后续history；
- 后续valid Compaction可以再次生效。

Cold replay不尝试恢复当时source revision或ActiveTurnTask。

## Failure

- plan impossible：Turn按ContextOverflow failure收口；
- summary provider error：按CompactionSummary retry policy；
- invalid summary：Turn failure，不apply Replace；
- stale revision/control：丢弃result，不apply；
- live Replace invariant failure：Turn invariant failure；
- encode/write failure：Replace保留，recording health Degraded，Turn继续。

## Fork

Fork复制其source中的effective conversation：

- RecordedHistory Fork按recorded marker投影；
- loaded Fork从LiveSnapshot复制effective conversation，因此包含snapshot capture前已经live Replace但尚未record的summary；
- unloaded Fork从RecordedHistory投影，marker未record时不包含该summary；
- target staging建立独立record stream；child保持Unloaded，未来Load再初始化SessionRecorder；
- 不复制Compaction task/request/retry timer。

## 测试要求

- complete Tool exchange不可被prefix cut拆分；
- rolling summary再次压缩；
- exact source revision stale rejection；
- summary budget与`ModelCallRequest.max_output_tokens`一致；
- Cancel/Steer/Compaction竞态；
- live Replace先于recording；
- marker record失败后current process继续；
- restart恢复旧conversation；
- invalid marker被忽略；
- ordinary AgentRun和CompactionSummary共用ModelGateway spine。

## 开放问题

CompactionSettings来源和wire schema仍需freeze。loaded live fork语义已由Q6关闭；Recorder其余策略见[独立review](../review/async-loop-best-effort-recording-open-questions.md)。
