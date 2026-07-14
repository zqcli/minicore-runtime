# UsageStats

`UsageStats` 描述模型 token 消耗、会话累计统计和当前上下文占用的统一口径。它不是 UI 组件，也不是 provider SDK；它是运行时把 provider usage、本地估算和会话状态归一化为 UI 可展示数据的能力。

核心原则：

```text
Usage / Cost Tokens != Context Usage
```

- `Usage` 表示模型调用真实消耗，用于展示本次 run、会话累计和后续成本估算。
- `ContextUsage` 表示下一次模型请求会占用多少上下文窗口，用于上下文进度条、pre-run threshold gate 和压缩触发；最终 call projection validation 仍负责权威拒绝。

adapter 不应自己从消息估算 token。它只消费 `usage_updated`、`run_finished { usage }`，以及对应 `agent_runtime_protocol::RuntimeSnapshot.loaded_sessions[*].session_stats` / `context_usage`。

## Context Usage 估算

MiniCore 优先使用 provider 返回的有效 assistant usage；如果找不到有效 usage，才使用本地字符估算：

```text
if last valid assistant usage exists:
  context_tokens = provider_usage_tokens + estimate(messages after that assistant)
else:
  context_tokens = estimate(all messages)
```

有效 assistant usage 必须来自非 `aborted`、非 `error` 的 assistant message，且 token 数大于 0。本地估算使用保守字符启发式：文本约 `chars / 4`，图片按固定大块估算，tool call 使用工具名加 JSON 参数长度，tool result/bash/summary 使用内容长度估算。

provider-neutral usage 使用本文“数据结构”小节和 `AgentRuntimeProtocol` 共同定义的唯一 `TokenUsage`：所有字段为 `u64`，并保留 `cached_input_tokens`、`cache_write_tokens`、`reasoning_output_tokens`。不再维护另一份 `i64 TokenUsageInfo` shape。

Codex 还把 `token_count` 作为事件推给 UI，并用 `non_cached_input + output` 作为更适合展示的 blended total，同时把 cached input 和 reasoning output 单独展示。

## 术语边界

```text
TokenUsage
  单次模型调用或聚合后的 provider token 分项。

UsageSummary
  一次 Agent run 内所有模型调用的消耗汇总。

SessionUsageStats
  一个会话从创建以来累计的模型调用消耗统计。

ContextUsage
  当前会话投影到下一次模型请求时的上下文窗口占用。
```

`SessionStatsView.total_usage` 不会因为压缩而下降；压缩只改变 `ContextUsageView.current_tokens`。这点对 UI 很重要：会话历史消耗是账本，当前上下文占用是窗口。

## 数据结构

建议在 `AgentRuntimeProtocol` 中暴露 UI 需要的 view 类型，在 `usage.rs` / `usage_stats.rs` 中实现归一化和聚合 helper。

```rust
pub struct TokenUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub total_tokens: u64,
}

pub enum UsageSource {
    ProviderReported,
    Estimated,
    Mixed,
}

pub struct ModelCallUsage {
    pub call_id: ModelCallId,
    pub run_id: Option<RunId>,
    pub provider_id: String,
    pub model_id: String,
    pub purpose: ModelCallPurpose,
    pub usage: TokenUsage,
    pub source: UsageSource,
    pub raw_provider_usage: Option<serde_json::Value>,
}

pub struct PersistedModelCallUsage {
    pub call_id: ModelCallId,
    pub run_id: Option<RunId>,
    pub provider_id: String,
    pub model_id: String,
    pub purpose: ModelCallPurpose,
    pub usage: TokenUsage,
    pub source: UsageSource,
}

pub struct UsageSummary {
    pub model_calls: u32,
    pub total: TokenUsage,
    pub by_model: Vec<ModelUsageSummary>,
    pub source: UsageSource,
}

pub struct ModelUsageSummary {
    pub provider_id: String,
    pub model_id: String,
    pub model_calls: u32,
    pub total: TokenUsage,
}
```

`ModelCallUsage` 是当前 host 内的单次模型调用事实；`PersistedModelCallUsage` 是唯一允许进入 `SessionEntryDraft` / JSONL 的 durable shape；`UsageSummary` 是一次 run 内所有模型调用的聚合。`ModelCallUsage.purpose` 必须直接复制 `ModelCallRequest.purpose`，不能在 usage 层重新判断或转换。组装 stable batch 前，`SessionRuntime` 必须显式调用等价的 `ModelCallUsage::to_persisted()`，该转换不包含 `raw_provider_usage`；writer 也必须拒绝任何携带 raw provider payload 的 draft。`raw_provider_usage` 只允许作为内部 redacted diagnostic，默认不进入 `AgentRuntimeProtocol`、`SessionEntry`、hook context 或日志明文。

`cache_write_tokens` 保留给 Anthropic / OpenAI prompt caching 之类 provider 差异；UI MVP 可以不展示，但 stats 层不要丢。

推荐派生口径：

```rust
impl TokenUsage {
    pub fn non_cached_input_tokens(&self) -> u64 {
        self.input_tokens.saturating_sub(self.cached_input_tokens)
    }

    pub fn display_total_tokens(&self) -> u64 {
        self.non_cached_input_tokens()
            + self.cache_write_tokens
            + self.output_tokens
    }

    pub fn provider_total_tokens(&self) -> u64 {
        self.total_tokens
    }
}
```

UI 主数字建议使用 `display_total_tokens()`，并把 cached input、reasoning output 单独展示。

## ContextUsage

```rust
pub enum ContextUsageSource {
    ProviderUsagePlusTrailingEstimate,
    LocalEstimate,
    ProviderReported,
}

pub struct ContextUsageView {
    pub current_tokens: u64,
    pub context_window: Option<u64>,
    pub reserve_tokens: u64,
    pub baseline_tokens: u64,
    pub effective_window_tokens: Option<u64>,
    pub remaining_tokens: Option<u64>,
    pub remaining_percent: Option<f32>,
    pub source: ContextUsageSource,
}
```

推荐计算：

```text
1. 构建当前模型可见上下文：system prompt + projected session messages + active tool schemas / snippets。
2. 在 projected messages 中从后向前找最后一个有效 assistant usage。
3. 如果找到：current_tokens = usage.total_tokens + estimate(messages after that assistant)。
4. 如果找不到：current_tokens = estimate(full model-visible context)。
5. 如果知道 context_window：remaining_tokens = context_window - current_tokens。
6. remaining_percent 使用 baseline 归一化：
   effective_window = context_window - baseline_tokens
   used_effective = max(current_tokens - baseline_tokens, 0)
   remaining_percent = (effective_window - used_effective) / effective_window
```

`baseline_tokens` 表示系统提示词、固定工具说明和运行时框架提示词等用户难以控制的基础占用。MVP 可以配置默认值，例如 `12_000`，后续再动态估算 `system prompt + active tools + prompt materials`。

## 本地估算

MVP 使用以下保守字符启发式：

```text
text: chars / 4
image: fixed image budget，例如 4800 chars / 4
tool call: tool name + JSON args length
tool result: content chars / 4
bash: command + output length / 4
compaction summary / branch summary: summary chars / 4
```

估算只用于：

- context usage 预览。
- compaction threshold。
- provider usage 不可用时的 fallback。
- run 启动前的 context-limit 风险提示。

估算不用于：

- 精确账单。
- provider 限额承诺。
- 成本审计。

provider 返回 usage 时，它是消耗统计的权威来源。本地估算只能补尾部消息或补 provider 缺失。

## Stats Ownership

```text
ModelGateway
  normalizes raw provider usage into ModelCallUsage.

Driver
  receives model-call usage through host.call_model result / stream sink;
  accumulates current Rig drive usage but does not own session stats.

SessionRuntime
  owns CurrentRunUsage, SessionUsageStats, ContextUsageView;
  emits usage_updated and includes final usage in run_finished.

SessionHandle / SessionStorage
  persist recoverable per-model-call usage with committed assistant/compaction facts;
  do not calculate realtime stats or persist the same run aggregate on every message.

UI
  renders usage views from agent_runtime_protocol::Event / agent_runtime_protocol::RuntimeSnapshot;
  does not calculate authoritative token usage.
```

`UsageSummary` for a run is the sum of all model calls in that run, not just the last model call. If a run contains multiple model/tool/model cycles, `run_finished { usage }` must include all model calls in that run.

run-level `UsageSummary` 只聚合关联到该 run 的 `ModelCallPurpose::AgentRun` 调用。独立 compaction phase 产生的 `CompactionSummary` usage 进入 `SessionUsageStats`，但不能回填到刚结束或即将重试的 Agent run aggregate。provider fallback/retry 若在同一个逻辑 model call 内发生，由 `ModelCallAttempt` 记录；任何 provider 明确报告的消耗仍使用原始 purpose 记账。

`DriverEvent::ModelCallFinished` 应携带 `Option<ModelCallUsage>`。`run_finished { usage }` 和 `DriveResult::Completed { usage }` 才携带 run-level `UsageSummary`，避免多次模型调用重复计数或丢失 per-call provider/model/source。

## Session Persistence

MVP 把每次模型调用的 usage 跟随对应 stable fact 提交，而不是把整段 run aggregate 重复写进每条 assistant message：

```text
ToolRound.assistant.usage = PersistedModelCallUsage for that tool-call response
AssistantFinal.message.usage = PersistedModelCallUsage for the final response
Compaction.result.usage = PersistedModelCallUsage for the compaction summary call
RunFinished.usage = UsageSummary aggregated for current-host UI only
```

恢复 `SessionUsageStats` 时按唯一 `call_id` 汇总 committed facts，不能把 `RunFinished.usage` 再次计入。provider 已产生 usage 但对应 assistant/tool round 尚未 commit 时，当前 host 可以发 `usage_updated`；crash 后该 in-flight usage 不保证恢复，因此 MiniCore usage 不是 provider billing ledger。

中期可以增加专门 session entry，以更精确保存失败调用或不伴随 assistant message 的 usage：

```rust
pub enum SessionEntry {
    Usage {
        base: EntryBase,
        fact: PersistedModelCallUsage,
    },
    // ...
}
```

`ModelCallPurpose` 的权威定义在 [ModelGateway](model-gateway.md)。usage/persistence 不定义 `UsagePurpose`，也不把 retry 或客户端是否选中该 session 当成业务目的：Agent run 的 provider retry、overflow recovery 后重跑和后台 session 继续执行仍归 `AgentRun`；压缩摘要及其重试仍归 `CompactionSummary`。未来新增 branch summary、session title 等模型任务时，应增加明确 purpose 变体。

这样 UI 后续可以区分正常回答和压缩摘要等真实业务目的，同时不会因为 retry/fallback/客户端 selection 改变统计分类。

## AgentRuntimeEvents And RuntimeSnapshot

`usage_updated` 是实时 UI 更新事件，生命周期顺序以 [AgentRuntimeEvents](agent-runtime-events.md) 为准；它可以包含当前 host 尚未提交的运行中统计。需要跨进程恢复的 usage facts 必须写入相关稳定 session batch，并在正常 `run_finished` 或 `compaction_finished` 前 commit。

推荐发送时机：

```text
model call finished
  → usage_updated { run_usage, session_stats?, context_usage? }

final stable batch committed
  → usage_updated { run_usage, session_stats, context_usage }
  → run_finished { usage: final run usage }

compaction finished
  → usage_updated { context_usage after compaction }

resources/tools/system prompt changed
  → usage_updated { context_usage estimated with new prompt materials }
```

每个 `agent_runtime_protocol::RuntimeSnapshot.loaded_sessions[*]` 都应包含 `session_stats: Option<SessionStatsView>` 和 `context_usage: Option<ContextUsageView>`。完整结构定义以 [AgentRuntimeProtocol](agent-runtime-protocol.md) 为准。

adapter reducer 重建后应以 snapshot 为权威恢复全部 loaded session 的 usage view，而不是要求 runtime 重放所有历史 `usage_updated`。查看未加载 session 或按需读取详细统计时，使用 `RuntimeQuery::Usage(UsageQuery::GetSessionStats | GetContextUsage)`；query 返回完整 view，loaded session 的后续变化继续由 `usage_updated` 替换 cache。adapter 不应轮询 query 来模拟实时 usage stream。

## UI 展示建议

建议 UI 至少分三块展示：

```text
当前上下文
  73% remaining
  38k / 128k
  source: provider + estimate

本次运行
  12.4k tokens
  input 10.1k (+8.0k cached)
  output 1.9k
  reasoning 400
  3 model calls

本会话累计
  284k tokens
  input 210k (+120k cached)
  output 62k
  reasoning 12k
  compactions 2
```

上下文条颜色建议：

```text
remaining >= 40%: normal
20% <= remaining < 40%: warning
remaining < 20%: danger
current_tokens >= context_window - reserve_tokens: should compact
```

## 设计约束

- 不要把 `Usage` 和 `ContextUsage` 合并成一个数字。
- 不要让 UI 自己从 messages 估算 token。
- 不要让 `SessionStorage` 负责实时 stats 计算。
- 不要因为 compaction 降低 session cumulative usage。
- 不要把本地估算当成账单权威。
- 不要让 `Driver` 拥有 session stats；它只聚合当前 drive 的 usage facts。
- 不要把 `DriverError::ContextLimitExceeded` 的 transient failure/partial assistant 作为模型可见消息参与 recovery usage/context 计算；`PromptAssembly` source 没有 model-call usage，`Provider` source 保留实际 attempt/usage。
