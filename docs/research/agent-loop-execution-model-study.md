# AgentLoop执行模型跨项目研究

日期：2026-07-30
状态：研究归档（非Accepted ADR）
关联：[第三轮AgentLoop设计评审](../review/v2-design-review-3.md) L2
目的：评估MiniCore同步sans-I/O AgentLoop在ADR 0124简化durable机制后的合理性，并比较Pi、Codex、OpenCode、Gemini CLI和OpenHands的实际实现。

## 0. 权威边界

当前正式架构仍以[`docs/architecture.md`](../architecture.md)、[`docs/modules/`](../modules/README.md)和Accepted ADR为权威。本文只归档源码事实、设计分析和推荐，不关闭L2，也不修改当前`next_action() + accept_*()`合同。

当前正式设计：

```text
SessionExecutor async owner
└─ crate-private同步sans-I/O AgentLoop
   ├─ next_action() → NeedModel | NeedTools | Finished
   ├─ accept_model_response(...)
   ├─ accept_committed_tool_results(...)
   └─ accept_committed_steer(...)
```

本文最新working recommendation：继续保持first-party、crate-private、sans-I/O协议逻辑，但在正式关闭L2前优先比较“pull式`next_action()`”与“accepted fact直接返回next effect”的纯reducer接口。该推荐不是Accepted决策。

## 1. 研究问题

本轮回答以下问题：

1. 同类coding-agent产品更常使用普通async loop，还是手写纯sans-I/O状态机？
2. Model/Tool I/O放在loop内外，真正差异是什么？
3. Codex和OpenCode在loop内等待Tool授权时，如何保持control path响应？
4. MiniCore的single owner、Transcript-First和committed typed delta是否必须依赖当前AgentLoop接口？
5. ADR 0124删除same-Turn recovery和durable Tool proof后，sans-I/O还剩哪些真实收益？
6. 当前设计中哪些部分合理，哪些部分可能支付了超过必要的复杂度？

## 2. 固定源码基线

| 项目 | 基线 | 主要源码 |
| --- | --- | --- |
| Pi | `@earendil-works/pi-coding-agent` 0.80.6 | `pi-agent-core/dist/agent-loop.js`、`agent.js` |
| Codex | `61a44880a85d2fd0d8770908dea5733495e571c8` | `core/src/session/turn.rs`、`tasks/regular.rs`、`state/turn.rs`、`tools/parallel.rs` |
| OpenCode | `7565e03536d19e850f9996c407f9bf5e932b5f7a` | `packages/opencode/src/session/prompt.ts`、`processor.ts`、`run-state.ts`、`permission/index.ts` |
| Gemini CLI | `3818efbbfbf8ef029ef53a6ab1093db39971ce83` | `packages/core/src/core/client.ts`、`turn.ts`、`scheduler/` |
| OpenHands SDK | `68cd02e0f4ad8276ed23ee8ccdaf77a30706429f` | `openhands/sdk/agent/agent.py`、`conversation/impl/local_conversation.py`、`conversation/state.py` |

公开仓库：

- <https://github.com/openai/codex>
- <https://github.com/anomalyco/opencode>
- <https://github.com/google-gemini/gemini-cli>
- <https://github.com/OpenHands/software-agent-sdk>

## 3. 术语

### 3.1 Loop内I/O

AgentLoop或Turn task直接持有并等待Model/Tool Future：

```rust
let response = model_gateway.generate(request).await?;
let results = tools.execute(response.tool_calls()).await?;
```

ModelGateway和Tool模块仍可保持独立；“loop内”描述的是等待过程和时序控制权属于loop，不表示provider/tool实现必须写在loop文件中。

### 3.2 Loop外I/O

AgentLoop只返回effect，不接触Future：

```text
AgentLoop → NeedModel
Executor → await ModelGateway
Executor → accept_model_response
AgentLoop → NeedTools
Executor → await ToolSet
```

协议状态保存在AgentLoop；I/O和operation状态保存在Executor。

### 3.3 First-party loop与纯状态机不是同义词

同类产品普遍自研loop，但大多写成async task/fiber。只有同时满足以下条件才属于本文所说的纯sans-I/O reducer：

- 不调用Model、Tool、Storage或Interaction I/O；
- 不等待Future；
- 只接收validated/committed typed fact；
- 只返回下一协议effect；
- 可以不依赖async runtime进行纯逻辑测试。

## 4. 跨项目分类

| 项目 | Loop形态 | Model/Tool I/O | 显式运行状态 | 是否纯sans-I/O reducer |
| --- | --- | --- | --- | --- |
| Pi | 程序级`async while` | loop内直接await | `activeRun`、pending tool calls | 否 |
| Codex | submission actor + async Turn task | Turn task内直接await | `ActiveTurn`、`RunningTask`、`TurnState` | 否 |
| OpenCode | Effect fiber + `while (true)` | loop/processor内直接yield/await | per-session Runner、Tool part state | 否 |
| Gemini CLI | AsyncGenerator + Tool Scheduler | generator/scheduler内 | Tool status queue/state | 否 |
| OpenHands | `run/arun + step/astep` | step内直接调用LLM/Tool | conversation status、unmatched actions | 否，混合型 |
| MiniCore计划 | sync state machine + async Executor | AgentLoop外 | AgentLoopState、RunningOperation、phase | 是 |

在本次五个可核对的开源coding-agent产品中：

```text
程序级async/effectful loop：4
显式step但step内部直接做I/O：1
MiniCore式纯NeedModel/NeedTools reducer：0
```

这不是统计学行业调查，但足以支持一个谨慎结论：**first-party AgentLoop常见，纯sans-I/O effect协议少见；产品实现的默认形态是async task/fiber。**

## 5. Pi

Pi的`runLoop`使用直接顺序控制：

```javascript
while (true) {
    const message = await streamAssistantResponse(...);

    if (message.toolCalls.length > 0) {
        const results = await executeToolCalls(...);
        currentContext.messages.push(...results);
        continue;
    }

    break;
}
```

特征：

- `await`调用栈保存当前协议位置；
- Tool完成前不会进入下一次Model；
- `Agent.activeRun`拒绝第二个并发prompt；
- Steer/FollowUp进入queue，在turn边界poll；
- loop直接维护current context和new messages；
- 没有`next_action()`重复poll问题。

Pi追求的是小而自然的产品级loop，不把协议状态独立成纯reducer。

## 6. Codex

Codex将control接收和Turn执行拆开：

```text
submission_loop
├─ 接收UserInput、Approval、Interrupt等Op
└─ ActiveTurn.task = RunningTask
   └─ async run_turn
      ├─ await model stream
      ├─ 创建Tool futures
      ├─ drain全部in-flight Tool futures
      └─ 下一次sampling
```

Codex有大量显式状态，但它们是operation/task状态，不是AgentLoop effect协议：

```text
ActiveTurn
RunningTask
TurnState.pending_approvals
TurnState.pending_user_input
MailboxDeliveryPhase
CancellationToken
```

### 6.1 Tool授权pending

Tool future请求授权时：

```rust
let (tx, rx) = oneshot::channel();
turn_state.pending_approvals.insert(approval_id, tx);
emit_approval_event();
let decision = rx.await;
```

submission loop继续接收`Op::ExecApproval`：

```text
ExecApproval
→ 从TurnState取出oneshot sender
→ send ReviewDecision
→ suspended Tool future恢复
```

因此`.await`只暂停当前Tool future，不暂停Session submission loop或整个Runtime。

### 6.2 设计取向

Codex使用一个异步Turn task自然表达Model→Tool→Model；control path、pending waiter和cancellation token解决响应性。它没有把每一步改写成`NeedModel/NeedTools`，但仍保持Model client、Tool router、session actor和storage职责分离。

## 7. OpenCode

OpenCode使用Effect-TS表达异步编排：

```typescript
while (true) {
    const handle = yield* processor.create(...)
    const result = yield* handle.process({ model, tools, messages })

    if (result === "compact") yield* compaction.create(...)
    if (finished) break
}
```

`SessionRunState`保存每Session Runner/fiber；`SessionProcessor.process()`直接消费LLM stream，AI SDK/SessionTools执行Tool。

### 7.1 Tool授权pending

Tool context中的`ask()`调用Permission service：

```typescript
const deferred = yield* Deferred.make()
pending.set(requestID, { info, deferred })
yield* events.publish(PermissionAsked, info)
yield* Deferred.await(deferred)
```

外部reply：

```typescript
const pending = state.pending.get(requestID)
state.pending.delete(requestID)
yield* Deferred.succeed(pending.deferred, undefined)
```

Question使用同样的`pending Map + Deferred + event + reply/reject`模式。

Effect fiber等待授权时，Runtime仍可处理HTTP/RPC请求并resolve Deferred。授权等待不要求把Tool I/O移到AgentLoop之外。

## 8. Gemini CLI

Gemini CLI使用AsyncGenerator处理Model stream，并由Scheduler拥有Tool operational lifecycle：

```text
Validating
→ AwaitingApproval
→ Scheduled
→ Executing
→ Success | Error | Cancelled
```

这些状态直接驱动policy、approval、执行和结果，不是交给外部Executor的纯effect。Model有pending ToolCall时不会继续普通sampling；Scheduler完成Tool后再把结果送回chat。

## 9. OpenHands

OpenHands显式提供`step()/astep()`：

```text
run/arun while loop
→ step/astep
→ 如果有unmatched ActionEvent，先执行pending action
→ 否则调用LLM生成新action
```

但`step/astep`内部仍直接做LLM和Tool I/O，因此属于混合型，而不是MiniCore式纯reducer。

OpenHands将没有matching Observation的ActionEvent视为pending work。MiniCore不应照搬该durable重执行语义：Tool可能已经产生副作用，只是Observation未持久化；ADR 0124明确选择restart不自动重跑incomplete Tool。

## 10. 为什么重型产品偏向Loop内I/O

### 10.1 控制流更自然

```text
await Model
→ await Tools
→ await Model
```

代码结构直接等于业务流程，编译器或Effect runtime保存instruction pointer和局部变量。

### 10.2 反馈天然配对

`await model()`返回值天然属于当前调用，不需要额外的`accept_model_response()`状态校验。

### 10.3 状态更少

产品通常只有async task/fiber state与少量operation status，不再额外维护AgentLoop issued state。

### 10.4 Pending Interaction适合oneshot/Deferred

授权本质是创建waiter、发布请求、等待外部resolve。Codex和OpenCode证明async等待不等于阻塞Runtime。

### 10.5 不恢复旧调用栈

这些产品普遍接受crash后丢弃旧Future、根据transcript恢复或终止旧Turn，不需要将AgentLoop建成可序列化workflow。

### 10.6 垂直集成降低边界成本

Codex和OpenCode同时控制UI、Runtime、Tool、Storage和Provider路径，可以接受更强的内部耦合。OpenCode还直接利用AI SDK stream/tool执行，拆成外部effect会增加adapter成本。

## 11. MiniCore当前设计的真实收益

### 11.1 First-party控制

不让Rig/SDK runner拥有第二conversation、Tool registry、approval或terminal规则。该决定仍然合理，并与同类产品自研loop的方向一致。

### 11.2 Committed-only可以由类型强化

AgentLoop不接收execution-local ToolResult vector，只接收Storage private-constructor生成的`CommittedToolExchangeDelta`。这可以在接口层阻止未commit或不完整exchange进入下一次Model。

### 11.3 纯逻辑测试

Model→Tool→Model协议可同步单元/property test，无需fake provider、timer或Tool executor。

### 11.4 I/O owner明确

Model retry、Tool side-effect start、Interaction、Compaction、Cancel和terminal全部留在SessionExecutor及各deep module。

## 12. 不应归因给sans-I/O的收益

以下正确性主要来自SessionExecutor和Storage，而不是AgentLoop是否同步：

| 正确性 | 主要owner |
| --- | --- |
| 同Session只有一个current operation | SessionExecutor |
| Cancel/Tool start first-wins | SessionExecutor/ToolOperationSlot |
| 多Session并发 | per-session Executor |
| append/apply后model-visible | SessionStorage/PromptSet |
| complete Tool exchange | Conversation projector |
| logical retry复用同一request | SessionExecutor/RunningOperation |
| Snapshot-first | Runtime publisher |
| crash后中断旧Turn | Session recovery |

Codex/OpenCode也证明Cancel响应和approval等待不要求纯sans-I/O AgentLoop；独立actor/control handler、cancellation token和oneshot/Deferred足以实现。

## 13. ADR 0124后的变化

ADR 0124删除：

- same-Turn crash resume；
- durable `ToolExecutionStarted`；
- durable `ToolRoundCompleted`；
- active-Turn checkpoint/protected frontier；
- 大部分cross-entry proof chain；
- old AgentLoop/provider stream/Tool task/waiter恢复。

因此AgentLoop当前不再承担：

```text
durable execution checkpoint
side-effect audit proof
crash-resumable workflow
historical delta replay target
```

它只剩live protocol reduction：

```text
validated model response
→ Tool path或candidate path
complete committed exchange
→ 下一次Model
committed Steer
→ 下一次Model
```

这没有让纯reducer失效，但显著降低了它必须独立存在、必须使用pull式接口的必要性。

## 14. Single Owner的准确含义

`SessionExecutor async owner`表示：

- 每个loaded Session只有一个权威mutable-state协调者；
- Model/Tool可以有独立Future，但只能返回typed result；
- 只有Executor可写SessionStorage、推进AgentLoop、消费queue、安装/清除RunningOperation和决定terminal；
- async I/O等待期间Executor仍处理Emergency/Lifecycle/Interaction/Snapshot。

Single owner不等于“只有一个task”，也不要求AgentLoop必须sans-I/O。Codex使用submission actor + RunningTask实现类似目标；MiniCore选择把I/O Future直接归SessionExecutor current operation管理。

## 15. Committed Typed Delta的准确含义

通俗表达：

> AgentLoop不会因为“Tool刚执行完”就允许下一次Model。只有Storage确认Assistant ToolCall和全部matching ToolResult已经成功写入、结构完整且可进入模型conversation后，AgentLoop才继续。

准确边界：

- validated model response可以让AgentLoop选择`NeedTools`或candidate；
- 任何会改变下一次模型conversation的Tool/Steer事实，必须通过committed typed delta推进conversation basis；
- delta证明的是“已commit并被projection接纳”，不是“外部Tool副作用exactly once”。

设计缘由：

- 防止AgentLoop内存history与Storage history分叉；
- 防止部分Tool exchange进入provider；
- 处理多Tool逆序完成；
- 隔离append失败和旧operation结果；
- 保持Prompt只消费sanitized committed conversation。

## 16. 当前pull接口的问题

当前interface：

```rust
next_action() -> AgentLoopAction
accept_model_response(...) -> ()
accept_committed_tool_results(...) -> ()
accept_committed_steer(...) -> ()
```

它暴露了一个调用方必须学习的时序协议：

```text
next_action只能成功一次
等待结果期间不能再次poll
结果必须调用匹配的accept
accept成功后才能再次next_action
```

因此产生L2，并需要private issued marker、`ActionAlreadyIssued`和重复poll测试。

该接口的历史来源是V1 Rig `AgentRun::next_step()`/Driver pull seam。ADR 0115改为first-party实现时明确保留既有interface，没有重新比较transition-returning reducer。当前它更像历史继承，而不是ADR 0124后重新证明的必要形状。

## 17. 三种候选方案

### 方案A：保留当前pull式AgentLoop

```rust
next_action() -> Effect
accept_*() -> ()
```

要求：

- `Option::take()`或issued子状态；
- 重复poll返回typed `ActionAlreadyIssued`；
- 未发action调用`accept_*`返回`UnexpectedInput`；
- failure/Cancel不回滚issued；
- logical retry不重新poll。

优点：正式文档改动最少。
缺点：保留L2、自建时序协议和AgentLoop/RunningOperation双状态配对。

### 方案B：纯reducer，accepted fact直接返回effect

```rust
impl AgentLoop {
    fn from_seed(
        seed: ConversationSeed,
    ) -> Result<(Self, AgentLoopEffect), AgentLoopError>;

    fn accept_model_response(
        &mut self,
        response: FinalizedAssistantResponse,
    ) -> Result<AgentLoopEffect, AgentLoopError>;

    fn accept_committed_tool_results(
        &mut self,
        delta: CommittedToolExchangeDelta,
    ) -> Result<AgentLoopEffect, AgentLoopError>;

    fn accept_committed_steer(
        &mut self,
        delta: CommittedSteerDelta,
    ) -> Result<AgentLoopEffect, AgentLoopError>;
}

pub(crate) enum AgentLoopEffect {
    NeedModel { output_contract: OutputContract },
    NeedTools {
        response: FinalizedAssistantResponse,
        calls: Arc<[ToolCall]>,
    },
    CandidateReady {
        candidate: FinalizedAssistantResponse,
    },
}
```

行为：

```text
from_seed → NeedModel
accept_model_response → NeedTools | CandidateReady
accept_committed_tool_results → NeedModel
accept_committed_steer → NeedModel
```

优点：

- action由状态转换直接返回，天然one-shot；
- 删除`next_action()`、issued marker和重复poll；
- 保留纯测试、typed committed input和I/O ownership；
- `CandidateReady`比`Finished`更符合Executor仍需仲裁的事实。

缺点：需要修订ADR 0115、Session Execution、Turn Context、review/handoff；调用方仍必须保证返回effect只启动一次，最终执行安全继续依赖single owner/current operation。

### 方案C：Codex式async Turn task

```text
Session control actor
└─ async Turn task
   ├─ await ModelGateway
   ├─ await ToolSet
   ├─ approval via oneshot/Deferred
   └─ return terminal result
```

优点：控制流最自然、状态较少、行业参考最多。
缺点：需要重新引入RunTask/RunLink或等价窄host seam；必须证明不形成第二writer、第二conversation、第二terminal owner，并重新设计Transcript-First commit callback、Steer、Compaction和retry边界。

## 18. 公正评估

| MiniCore决定 | 当前评估 |
| --- | --- |
| first-party loop，不使用Rig高阶runner | 合理，行业一致 |
| SessionExecutor single owner | 合理，是主要并发正确性来源 |
| PromptSet唯一assembly seam | 合理 |
| Storage committed-only projection | 合理，但实现成本高于同类产品 |
| Tool policy/approval/sandbox外置 | 合理 |
| AgentLoop no-I/O | 可辩护、有测试价值，但不是必要条件 |
| 独立三态reducer | 有局部化收益，必要性一般 |
| `next_action + issued + accept_*` | 当前证据下偏重 |
| durable Interaction request/resolution | 比Codex/OpenCode更强；有审计/重连价值，但不恢复旧waiter |
| AgentLoop/RunningOperation/Phase三份配对状态 | 需要重点防止drift |

MiniCore整体不是“方向错误”。它选择了更强的ownership和committed-only纪律，适合可嵌入Runtime core；但ADR 0124删除durable workflow目标后，当前pull interface为这些原则支付了超过必要的复杂度。

## 19. Working Recommendation

在首个AgentLoop实现前，优先正式比较方案A与方案B。当前研究倾向方案B：

```text
保留：
first-party concrete implementation
crate-private
sans-I/O
validated model input
committed typed Tool/Steer input
Compaction Replace后reseed
SessionExecutor single owner

删除：
next_action polling
issued marker
ActionAlreadyIssued
Finished误导命名
```

不建议在当前阶段直接切换方案C。Codex/OpenCode证明async Turn task可行，但MiniCore若采用它，需要重新打开已经关闭的RunTask/second-owner/Transcript-First host seam设计，变更范围显著大于方案B。

## 20. 额外一致性问题

本轮完整审查发现与AgentLoop相邻但独立的问题：

```text
model-gateway.md：RunningOperation::WaitForModelRetry
session-execution.md canonical enum：只有Model | Tools | Compaction
ADR 0119：要求current_operation仍为对应retry slot
```

在SessionExecutor实现前必须统一logical retry wait的owner-local表示。该问题不应塞进AgentLoop，也不应由ModelGateway拥有。

## 21. 下一步

1. 对L2做正式二选一：保留pull one-shot，或采用transition-returning effect reducer。
2. 若选择方案B，新增ADR或修订ADR 0115，并同步：
   - `docs/modules/session-execution.md`；
   - `docs/modules/turn-execution-context.md`；
   - `docs/review/v2-design-review-3.md`；
   - `docs/review/v2-design-review-handoff.md`；
   - `CONTEXT.md`与migration中的AgentLoop术语。
3. 冻结logical retry wait slot表示。
4. 完成wire/schema freeze。
5. 进入ScriptedProviderAdapter vertical slice和Rig 0.40.0 provider spike。
6. 首个production Tool/Sandbox adapter前重新激活O1/R7。

## 22. 不应从本文推导的结论

本文不接受以下结论：

- MiniCore必须改成Pi/Codex式monolithic async loop；
- async语法天然破坏Transcript-First；
- pure reducer可以替代SessionExecutor single owner；
- committed typed delta提供Tool exactly-once；
- AgentLoop需要成为public trait、插件或稳定扩展seam；
- 当前研究已经关闭L2。
