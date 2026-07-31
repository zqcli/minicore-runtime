# ADR 0124：Session Replay宽容恢复并收窄持久化引用链

状态：Partially Superseded by ADRs 0126, 0127, 0131 and 0132
日期：2026-07-29

> 2026-07-31：[ADR 0132](0132-compaction-derives-markers-from-live-stable-units.md)细化single prefix marker：`first_kept_entry_id`必须由live reducer发布的provider-valid stable-unit source与cut派生；Tool exchange不可拆，rolling summary origin是对应StoredCompaction outer EntryId。

> 2026-07-31：tolerant replay、minimal links与Tool exchange sanitizer继续保留；ADR 0127删除`StoredTurnStart`、Turn terminal entries、restart/fork closure。Replay只重建conversation facts，Load后的`current_turn`为空。

> 2026-07-31：[ADR 0131](0131-conversation-recording-excludes-session-definition-and-lifecycle.md)取代本文“durable lifecycle event进入SessionStorage”的旧表述。current JSONL只保存conversation、Interaction与Compaction；Agent/Session definition和lifecycle从entity durable owner恢复。

> 2026-07-30：tolerant replay、minimal durable links和Tool exchange sanitizer保留；strict `SessionWriter`、committed typed delta、append-before-model-visible和read-only writer admission由ADR 0126取代。EntryId generation owner由ADR 0126/Q9固定为`LiveSessionState` private Session-scoped generator；Recorder不得创建或改写ID。

## 背景

MiniCore原设计把SessionStorage同时作为conversation、执行ledger和结构证明系统。为了保证任意cold replay都得到与live apply完全相同的状态，schema逐步增加了：

- 独立`TurnContext` entry及`context_entry_id`；
- `ToolExecutionStarted`及ToolResult的`execution_started_entry_id`；
- `ToolRoundCompleted.assistant_entry_id + tool_entry_ids`；
- Compaction的scope、first/last boundary、protected entries、previous checkpoint和coverage provenance；
- Fork时对`EntryId`、`TurnId`、`ItemId`、`RequestId`及全部nested reference的重映射；
- 任意完整坏行或语义引用异常都brick整个Session的strict replay规则。

这些机制提高了ledger证明强度，也显著扩大了schema、writer/replay validator、fork和repair surface。MiniCore MVP不恢复旧provider stream、Tool task、waiter或same-Turn execution context；用户接受损坏Session只能尽力恢复可读历史，也接受部分执行事实缺失。因此无需把本地Agent transcript提升为完整的事务式执行审计系统。

同类产品采用更宽松的基线：

- pi使用`entry id + parentId`树和`ToolCallId`，坏JSON行跳过；fork复制历史entry ID；
- Codex保留thread/turn/item/call ID，rollout解析错误记录warning后继续；
- Gemini CLI保留session/message/tool-call ID，单行解析错误被忽略，并在模型调用前整理history；
- OpenHands使用event ID、parent ID、action ID和tool-call ID，缺失关系按event/view规则处理；
- Claude Code可观察transcript使用session UUID、entry UUID、parent UUID和tool-use ID，未公开完整语义repair协议。

共同点是保留关联身份，同时避免为每个持久化事实建立跨记录证明链。

## 决定

### 1. 核心Identity继续保留

MVP继续使用：

```text
AgentId
SessionId
TurnId
ItemId
RequestId
EntryId + parent_id
ToolCallId
CommandId（仅当前Runtime）
```

理由：

- `SessionId`、`TurnId`和`ItemId`分别服务Session route、Cancel/Steer和稳定UI/progress更新；
- `RequestId`允许一个Item顺序产生多个Interaction并支持断线重连；
- `EntryId + parent_id`服务branch、rewind、fork和selected-path读取；
- `ToolCallId`是ModelGateway归一化的provider/tool协议correlation：保留provider原生ID，协议未提供ID时由adapter生成response-local opaque ID；只要求在同一assistant response内唯一，durable route使用`TurnId + ItemId + ToolCallId`；
- `CommandId`服务in-flight command dedup和Turn创建前Cancel，不持久化。

这些identity不能通用合并：一个assistant entry可以生成多个Item，一个Item可以跨多个entry并拥有多个Interaction，ToolCallId还受provider协议约束。

### 2. Live append严格，cold replay宽容

`SessionWriter.append()`继续严格校验MiniCore自己将要写入的新entry。合法live execution不得生成duplicate ID、非法parent、重复ToolResult、错误Interaction family或terminal后新work。

cold replay不再承诺和append使用相同拒绝规则，也不因一条坏记录拒绝整个Session：

- 最后一个未换行partial line：read-only open忽略并报告；writable open取得exclusive lease后截断；
- newline-terminated invalid JSON或unknown core variant：跳过该行并记录line/byte offset diagnostic；
- duplicate EntryId：first valid occurrence wins，后续重复行跳过；
- missing parent：保留该entry为orphan root，selected path从该entry重新开始；
- invalid cross-entry reference：对应projection忽略该关系或entry，其他projection和后续有效记录继续；
- malformed/incomplete Tool exchange：不进入模型conversation，历史/UI仍可显示能够解释的独立事实；
- terminal或Interaction closure不完整：load继续，writable recovery尽力追加明确closure，失败时Session history仍可读但Turn admission可以Unavailable。

每次load返回结构化`SessionReplayDiagnostics`。普通load不修改中段内容，不猜测parent、不合成缺失ToolResult、不重写用户历史。

### 3. SessionStorage定位收窄

SessionStorage是已写入conversation、message和durable lifecycle event的权威来源。它不再承诺记录每一次OS/remote side-effect start，也不证明外部副作用与每个结果之间存在完整write-ahead链。

当前Runtime的SessionExecutor仍严格管理副作用开始、Cancel和settlement；进程崩溃后不恢复旧Tool task，未完成ToolCall在projection中保持incomplete/abandoned，并中断旧Turn。

### 4. TurnContext内联到initiating UserMessage

删除独立`TurnContext` entry和`context_entry_id`。

`source = Input`的StoredUserMessage内联一个`StoredTurnStart`，保存长期可解释的安全信息：

- `AgentId`、exact `AgentRevisionRef`和`SessionDefinitionRevision`；当前MVP支持显式pin/upgrade，因此live Input中均必填，但仅作历史说明，不要求retained definition存在；
- 实际provider/model标识和必要generation settings；
- model-safe cwd/Workspace摘要与safe diagnostics。

不保存`WorkspaceRevision`、`WorkspaceSnapshotRef`、`ModelDefinitionVersion`或要求cold replay重新解析的execution reference。active Turn内存仍使用同一次capture得到的exact immutable objects。

### 5. 删除durable ToolExecutionStarted

`ToolExecutionStarted`从Session ledger删除。Tool side-effect开始是SessionExecutor和ToolSet的current-Runtime状态：

```text
control/authorization recheck
→ owner-local start reservation
→ executor side effect
→ exact outcome或Abandoned settlement
```

ToolResult只用`TurnId + ItemId + ToolCallId`关联ToolInvocation，`source = PreExecution | Executed`不再携带entry reference。Cancel/SecurityRevoked在side effect开始前获胜则拒绝启动；开始后获胜则best-effort cancel并等待truthful outcome或标记Abandoned。

### 6. 删除ToolRoundCompleted

删除`ToolRoundCompleted` durable event及其assistant/tool entry引用。

live路径：

- assistant tool-call response append后创建pending Tool exchange；
- 每个ToolResult独立append；
- live路径要求同一assistant response的全部ToolCall都存在exactly one matching ToolResult；conversation projector由最后一个完成该集合的tool entry生成`CommittedToolExchangeDelta`；
- SessionExecutor把该typed committed delta交给AgentLoop，随后才可开始下一次模型调用。

cold replay：

- projector按`ToolCallId`匹配assistant call与tool message；
- complete exchange按assistant call顺序进入conversation；
- cold replay对duplicate result采用first valid wins并告警；ToolResult与ToolAbandoned采用first valid terminal outcome wins；missing、orphan、identity-conflicting或abandoned-first result使assistant Tool exchange不进入模型conversation；下一条合法User、Assistant、Compaction或Turn terminal关闭尚未完成的exchange，迟到result成为orphan；
- 后续独立、合法的User/Assistant内容仍可恢复。

provider通常拒绝unmatched ToolCall，因此宽容replay不等于把非法pair直接发送给模型。Prompt assembly只消费sanitized committed conversation。

### 7. Compaction使用单一prefix marker

StoredCompaction收窄为：

```text
summary
first_kept_entry_id
optional model-call provenance
```

`first_kept_entry_id`表示summary后第一个原样保留的model-visible entry；`None`表示summary覆盖当时全部model-visible内容。删除：

- `CompactionScope`；
- `ConversationBoundary` first/last；
- `protected_entries`；
- `previous_checkpoint`；
- active instruction segment和coverage frontier/provenance。

planner只在sanitized provider-valid conversation边界切prefix。后续Compaction可以把上一条summary作为普通source再次摘要，形成单一rolling summary。MVP允许摘要覆盖旧initiating/Steer UserMessage，不承诺active Turn原始指令永久保留。

### 8. Fork保留历史identity

Fork deep-copy selected root-to-anchor path，创建新SessionHeader和新`SessionId`，但保留复制entry中的：

```text
EntryId / parent_id
TurnId / ItemId / RequestId
ToolCallId
historical payload
```

新append为EntryId及MiniCore生成的Turn/Item/Request ID使用fresh随机值，避免与复制历史冲突；adapter-normalized ToolCallId不要求Session-wide唯一。ID语义按`SessionId + local semantic key`路由；不要求跨Session全局唯一。Fork不再重写nested refs，也不做source/target ID等价证明。复制路径中的orphan或ignored entry按普通宽容replay处理。

### 9. 不提供MVP repair utility

O3关闭。MVP普通loader提供：

- warning与结构化diagnostics；
- malformed line skip；
- orphan/duplicate/reference问题隔离；
- partial-tail安全截断；
- read-only原始文件导出能力由host或通用文件工具承担。

不建设管理员repair command、自动reparent、语义重建或原地中段重写。未来只有出现真实用户数据和明确恢复需求时再设计独立maintenance工具。

## 后果

- Session损坏后的可用性提高；一条坏记录不再brick整个历史。
- append与cold replay不再共享“同一错误即拒绝”的合同；live reducer仍可复用projection逻辑，但replay必须有显式skip/isolate策略。
- Session ledger不能作为完整副作用审计记录；它仍准确表示成功写入的消息、Interaction、ToolResult、terminal和Compaction事实。
- Tool exchange、Compaction和Fork schema显著缩小，删除大部分nested EntryId引用和remap代码。
- 历史重放可能缺失部分消息或形成多个orphan roots；diagnostics必须让host能够告知用户发生了数据损失。
- exact Agent/Session/Workspace/Model对象仍用于live Turn一致性；durable history不再要求这些旧对象永久可解析。

## 修订关系

本ADR：

- 部分取代ADR 0104关于SessionStorage完整execution-ledger truth的强表述；
- 部分取代ADR 0109第3、4、9条的append/replay等价与ToolRoundCompleted要求；
- 取代ADR 0112的active-Turn checkpoint、typed scope/boundary/provenance设计，保留model-aware summary budget和SessionExecutor编排；
- 部分取代ADR 0111、0113、0115、0117、0118、0121中`ToolExecutionStarted` durable linearization和`tool_round_completed` gate表述；
- 部分取代ADR 0123第2、7、10、16、17条的durable Workspace/Model execution refs与proof links、Fork remap、ToolExecutionStarted、StoredTurnContext和StoredCompaction形状；Agent/Session historical exact refs继续保留；
- 关闭`docs/review/v2-design-review.md`中的O3。

未改变：

- single SessionExecutor/Writer owner；
- immutable TurnExecutionContext和explicit reload；
- ModelGateway single provider attempt与Session logical retry；
- Tool policy、approval、Sandbox和Session-local file mutation queue；
- Interaction request-before-notify与resolution-before-resume；
- Cancel即时确认和已开始Tool truthful settlement；
- Snapshot-first Runtime observation；
- AgentLoop为crate-private同步sans-I/O状态机。

## 实现约束

- wire schema仍使用typed IDs，serde casing和MiniCore生成ID的UUID策略在wire/schema freeze中统一；ToolCallId保持adapter-normalized opaque string，不承诺UUID；
- append strict validator与replay tolerant reducer分别测试，禁止把replay skip逻辑用于live append；
- 每个skip/isolate必须产生bounded、redacted diagnostic；
- model input sanitizer必须有complete/missing/duplicate/orphan ToolCall/ToolResult及ToolResult/ToolAbandoned冲突测试；
- Fork必须验证复制后新Session可以宽容replay，并证明新增entry不会与复制ID冲突；
- Compaction必须验证marker存在、缺失和指向ignored entry三种replay结果。
