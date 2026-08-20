# ADR 0149：OpenAI Raw Reasoning Progress要求Host显式Opt-In

状态：Accepted
日期：2026-08-17

> 本ADR窄化细化[ADR 0120](0120-failures-stay-with-owning-modules.md)第6条：hidden reasoning继续默认不得进入public event；唯一current例外是trusted host在OpenAI Responses provider installation上显式选择`OpenAiReasoningProgress::RawText`后，该installation的non-empty `response.reasoning_text.delta`可以进入process-local `ProgressEvent`。storage、Snapshot、final Item、transcript、diagnostics与普通telemetry仍不公开raw reasoning。

## 背景

Session progress已经使用`ItemProgressContentKind::Reasoning`向同进程host交付OpenAI `response.reasoning_summary_text.delta`，TUI无需新variant即可渲染。OpenAI Responses也可能提供`response.reasoning_text.delta`，但direct adapter此前只验证并抑制它。把raw reasoning绑定到`include_progress`、model reasoning effort或Runtime-global开关都会扩大披露范围；同时发布summary与raw会把两个语义不同的channel拼进同一provisional Item且无法撤回。

## 决策

1. `ModelProviderConfig::openai_responses(...)`保持backward-compatible并固定`SummaryOnly`。新增显式`openai_responses_with_reasoning_progress(..., OpenAiReasoningProgress::RawText, ...)`；policy属于OpenAI provider installation，不属于model capability、Session definition、Wire command或subscriber request。
2. 每个installation恰好选择一个公开reasoning stream。`SummaryOnly`发布non-empty summary delta并验证/抑制raw；`RawText`发布non-empty raw delta并验证/抑制summary。不得同时发布、拼接或根据事件到达顺序切换channel。
3. adapter记录已发布channel的`output_index`并在`response.completed` fail closed校验：SummaryOnly要求对应terminal reasoning item具有non-empty summary；RawText要求对应`content[]`具有non-empty `reasoning_text`。被抑制channel仍验证required fields并保持delivery truth，但不参与public index correlation。
4. `ReasoningSummary`与`ReasoningText`都映射到现有`content_index + ItemProgressContentKind::Reasoning` identity。raw delta复用final `StoredAssistantContent::Reasoning`的ItemId并阻止该item的summary fallback；若没有选定raw delta，existing safe terminal-summary fallback仍可发布一次。
5. 不新增terminal raw fallback，不从terminal artifact合成raw ProgressEvent，也不新增raw buffer。final Item/Snapshot仍只投影summary或固定redacted placeholder；Conversation JSONL、session transcript和Wire V1 shape不变。
6. encrypted content、signature、refusal、function-call arguments与Anthropic thinking/signature在所有policy下继续不公开。`include_progress=true`只订阅已经由host安装策略授权的progress，不是raw disclosure authorization。
7. policy在Runtime open时冻结进exact adapter并适用于该installation下所有progress-enabled Session subscribers；shared-resource reload复用same adapter/policy。Debug只显示nonsecret policy enum与payload byte count，不打印reasoning正文。
8. unbounded completion lane与oversized `ToolOutputDelta`是独立review findings；本ADR不授权修复或扩大它们。单通道选择且无terminal raw fallback避免额外重复流量。

## 可执行证据

- public Runtime + real loopback OpenAI adapter：RawText response同时发送summary/raw时，Session progress只出现raw，stable ItemId与final safe summary Item一致；
- default rich loopback包含raw canary但仍只发布summary；
- RawText streamed index若terminal只有summary或位置不匹配，以`InvalidProviderResponse/OutputStarted` fail closed；
- old/new provider constructors分别冻结SummaryOnly/RawText；
- existing Wire V1、conversation format、Anthropic suppression与final summary fallback tests保持不变。

## 后果

trusted in-process host可以选择展示OpenAI实际streamed reasoning，同时默认安装和其他provider继续closed。该opt-in会授权同一installation的全部progress-enabled subscribers，因此host必须把它视为启动时披露策略，而不是per-widget显示偏好。raw progress是可丢弃、不可恢复的provisional observation；terminal后canonical public read model仍是summary/redacted。