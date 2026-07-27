# ADR 0120：失败由事实拥有模块分类，恢复由执行拥有者决定

状态：Accepted
日期：2026-07-27

## 背景

MiniCore已经为Model、Tool、Storage、Workspace、Prompt、Skill和Compaction定义了局部typed error或truthful outcome，但缺少一条统一的跨模块规则，说明raw external failure、module error、Session recovery、durable terminal和public projection如何衔接。Issue O9首先暴露了该问题：Provider返回unexpected ToolCall、invalid structured output或finish/content不一致时，可能被分别解释成request error、provider protocol error或transient retry。

同类实现提供了互补经验：Codex倾向typed transport/stream classification；Gemini CLI显式识别malformed/unexpected model response；pi和OpenHands把可修正的Tool参数错误转成Tool error result让模型继续；Claude Agent SDK让retry和terminal reason对Host可见。MiniCore采用这些局部模式，但不引入它们的字符串分类、多层fallback、completed-output blind retry或大型公共terminal taxonomy。

## 决定

1. raw SDK、HTTP、filesystem或executor error只能存在于具体adapter implementation中。跨module seam只传递该module拥有的typed、redacted error或truthful outcome。
2. 发生事实的module负责分类；掌握Turn、control、checkpoint和durable state的owner负责恢复。ModelGateway分类模型调用与响应错误，ToolSet结算Tool outcome，SessionStorage分类commit certainty，SessionExecutor决定logical retry、Compaction、Turn interruption、Turn failure或Session unavailable。
3. 不新增`ErrorService`、通用Error module、错误registry、共同`RecoverableError` trait、全局severity或`retryable: bool`策略。错误message不得驱动控制流。
4. 命名表达值的真实职责：operation failure使用`Error`，原因使用`Reason`，业务事实使用`Result`或`Outcome`，外部控制终止使用`Interruption`，durable不可恢复终态使用`Failure`，public稳定标识使用`Code`。新设计不使用`Kind`作为错误分类后缀，也不使用`Violation`作为错误名称。
5. 内部error不直接持久化。SessionStorage只保存truthful domain facts和terminal facts；StateEvent只从append/apply成功后的projection产生，ProgressEvent和telemetry不影响正确性。
6. public protocol只暴露稳定、可行动、已脱敏的信息。raw provider body、credential、Prompt正文、绝对路径、Tool参数、hidden reasoning和内部source chain不得进入storage、event、snapshot或普通telemetry。
7. 本ADR先只应用于Issue O9的模型响应路径。其他module现有error名称和interface保持不变，后续遇到真实设计或实现需求时再按本ADR逐步收口，不做全仓库机械改名。现有`TurnFailure.retryable`暂作public/durable兼容字段，不得反向驱动内部retry policy。

## Issue O9的首次应用

ModelGateway在构造`ModelCallResult`前执行provider-neutral response validation：

```text
ProviderAttemptResult
→ normalize provider wire
→ validate finish/content consistency
→ validate OutputContract
→ ModelCallResult或ModelCallError
```

`ModelCallError`使用`reason: ModelCallErrorReason`表达原因。新增四个直接reason：

```text
UnexpectedToolCall
InvalidStructuredOutput
InvalidProviderResponse
IncompleteResponse
```

- 当前调用不允许ToolCall却收到ToolCall，返回`UnexpectedToolCall`。
- Structured response包含non-empty Text但不能按exact JSON解析或不满足schema时，返回`InvalidStructuredOutput`；MVP不做repair、coercion或从Markdown fence提取JSON。Structured的空Stop/Unknown仍按`IncompleteResponse`处理。
- finish reason、terminal content、stream/final index或provider wire语义自相矛盾，返回`InvalidProviderResponse`。
- `Length`、`ContentFiltered`、空Stop/Unknown、reasoning-only terminal或其他明确不完整结果返回`IncompleteResponse`；`Refused`但没有non-empty refusal text返回`InvalidProviderResponse`。
- finalized refusal text仍是successful response with `ModelFinishReason::Refused`；pre-generation safety block仍是`SafetyBlocked` error。
- 上述四个reason都发生在request已经离开`NotSent | RejectedBeforeExecution`安全状态之后，或在completed response validation期间；MVP不执行logical retry，不append assistant entry，也不执行Tool。SessionExecutor使用现有non-retryable Model TurnFailure收口；durable failure命名以后按真实公共协议需求再迁移。
- 可解析但不符合ToolSpec的Tool arguments不是O9 Provider response error；它继续由ToolSet形成PreExecution failed ToolResult，让模型在完整ToolRound后修正。

`OutputContract::Structured`在MVP中要求`tools`为空；普通AgentRun需要Tool时先完成普通ToolRound，再发起新的Structured调用。AgentLoop只接收ModelGateway已经验证的`FinalizedAssistantResponse`，不重复解析Provider错误。

## 后果

- O9和第三轮评审L1可以通过同一个ModelGateway决策表关闭。
- 新Provider只需把raw wire/error映射到现有provider-neutral response和error reason，不注册自己的recovery policy。
- 对非法或截断ToolCall采取fail closed，恢复率低于content blind retry，但不会产生越权副作用、重复计费或Transcript-First旁路。
- 其他错误体系仍按现有module ownership运行；本ADR不是立即重构整个仓库的授权。

## 被否决方案

### 新增全局Error module或ErrorService

否决原因：它会成为第二个策略owner，把Model delivery、Storage commit、Tool side effect和Turn control压成语义模糊的共同分类。

### 对所有异常输出自动重试一次

否决原因：Provider已经完成generation；同request blind replay可能重复计费，且无法修复确定性的contract或adapter bug。

### 自动修复JSON或忽略finish/content冲突

否决原因：修复会改变Provider实际输出，宽松接收可能让截断或越权ToolCall进入副作用pipeline。
