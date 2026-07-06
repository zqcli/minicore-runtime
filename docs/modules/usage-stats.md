# UsageStats

`UsageStats` 描述模型 token 消耗、会话累计统计和当前上下文占用的统一口径。它不是 UI 组件，也不是 provider SDK；它是运行时把 provider usage、本地估算和会话状态归一化为 UI 可展示数据的能力。

核心原则：

```text
Usage / Cost Tokens != Context Usage
```

- `Usage` 表示模型调用真实消耗，用于展示本次 run、会话累计和后续成本估算。
- `ContextUsage` 表示下一次模型请求会占用多少上下文窗口，用于上下文进度条、预检和压缩触发。

UI 不应自己从消息估算 token。UI 只消费 `usage_updated`、`run_finished { usage }`、`agent_runtime_protocol::RuntimeSnapshot.active_session.session_stats` 和 `agent_runtime_protocol::RuntimeSnapshot.active_session.context_usage`。

## 参考经验

pi-agent-core 的压缩逻辑优先使用 provider 返回的 assistant usage；如果找不到有效 usage，才用本地字符估算。它的上下文估算是：

```text
if last valid assistant usage exists:
  context_tokens = provider_usage_tokens + estimate(messages after that assistant)
else:
  context_tokens = estimate(all messages)
```

有效 assistant usage 必须来自非 `aborted`、非 `error` 的 assistant message，且 token 数大于 0。pi 的本地估算使用保守字符启发式：文本约 `chars / 4`，图片按固定大块估算，tool call 使用工具名加 JSON 参数长度，tool result/bash/summary 使用内容长度估算。

Codex 的协议把 usage 拆成：

```rust
pub struct TokenUsage {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_output_tokens: i64,
    pub total_tokens: i64,
}

pub struct TokenUsageInfo {
    pub total_token_usage: TokenUsage,
    pub last_token_usage: TokenUsage,
    pub model_context_window: Option<i64>,
}
```

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
    pub purpose: UsagePurpose,
    pub usage: TokenUsage,
    pub source: UsageSource,
    pub raw_provider_usage: Option<serde_json::Value>,
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

`ModelCallUsage` 是单次模型调用事实；`UsageSummary` 是一次 run 内所有模型调用的聚合。`raw_provider_usage` 只允许作为内部 redacted diagnostic，默认不进入 `AgentRuntimeProtocol`、`SessionEntry`、hook context 或日志明文。

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

MVP 可以采用 pi 的保守字符启发式：

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
- 提交前的 overflow 风险提示。

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
  persist recoverable usage facts or assistant aggregate usage;
  do not calculate realtime stats.

UI
  renders usage views from agent_runtime_protocol::Event / agent_runtime_protocol::RuntimeSnapshot;
  does not calculate authoritative token usage.
```

`UsageSummary` for a run is the sum of all model calls in that run, not just the last model call. If a run contains multiple model/tool/model cycles, `run_finished { usage }` must include all model calls in that run.

`DriverEvent::ModelCallFinished` 应携带 `Option<ModelCallUsage>`。`run_finished { usage }` 和 `DriveResult::Completed { usage }` 才携带 run-level `UsageSummary`，避免多次模型调用重复计数或丢失 per-call provider/model/source。

## Session Persistence

MVP 可以把 run-level aggregate usage 存在 assistant message 或 run result 附近：

```text
AssistantMessage.usage = UsageSummary for the run or assistant turn
RunFinished.usage = same aggregate view for UI
```

中期建议增加专门 session entry：

```rust
pub enum SessionEntry {
    Usage {
        base: EntryBase,
        run_id: Option<RunId>,
        model_call_id: ModelCallId,
        purpose: UsagePurpose,
        provider_id: String,
        model_id: String,
        usage: TokenUsage,
        source: UsageSource,
    },
    // ...
}

pub enum UsagePurpose {
    AgentRun,
    CompactionSummary,
    Retry,
    Background,
}
```

这样 UI 后续可以区分正常回答、压缩摘要、retry 和后台任务的 token 消耗。

## AgentRuntimeEvents And RuntimeSnapshot

`usage_updated` 是实时 UI 更新事件，生命周期顺序以 [AgentRuntimeEvents](agent-runtime-events.md) 为准；它不代表持久化完成。可恢复边界仍然看 `persistence_save_point`。

推荐发送时机：

```text
model call finished
  → usage_updated { run_usage, session_stats?, context_usage? }

run finished
  → run_finished { usage: final run usage }
  → usage_updated { run_usage, session_stats, context_usage }

compaction finished
  → usage_updated { context_usage after compaction }

resources/tools/system prompt changed
  → usage_updated { context_usage estimated with new prompt materials }
```

`agent_runtime_protocol::RuntimeSnapshot.active_session` 应包含 `session_stats: Option<SessionStatsView>` 和 `context_usage: Option<ContextUsageView>`。完整结构定义以 [AgentRuntimeProtocol](agent-runtime-protocol.md) 为准。

UI 重连后应以 snapshot 为权威恢复 usage 面板，而不是要求 runtime 重放所有历史 `usage_updated`。

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
- 不要把 context overflow failure 作为模型可见 assistant message 参与 retry usage/context 计算。
