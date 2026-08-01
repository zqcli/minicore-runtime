# ADR 0104: SessionStorage 是 durable truth

状态：Superseded by ADR 0126
日期：2026-07-24

> 2026-07-30：ADR 0126删除SessionStorage durable truth、`SessionWriter`和physical commit barrier。JSONL tree与tolerant replay继续保留，但只记录live事件流的best-effort前缀。

> 2026-07-31：ADR 0127进一步删除Turn start/terminal与cold recovery closure；[ADR 0131](../../../adr/0131-conversation-recording-excludes-session-definition-and-lifecycle.md)又把JSONL限定为conversation/Interaction/Compaction facts，Session definition、metadata和lifecycle由entity owner保存。

> 2026-07-29修订：ADR 0124保留SessionStorage作为已写入conversation/message/lifecycle history的单一durable owner和唯一write seam；删除完整execution-ledger、shared strict append/replay validator、ToolExecutionStarted/ToolRoundCompleted证明链和Fork identity remap。cold replay改为局部skip/isolate并返回diagnostics。

## 背景

一个 Session 的 conversation 与 execution ledger 需要一个明确的 durable owner：任意模型调用只能看到已提交的历史，crash 之后可恢复，而 branch/fork/compaction 又要保持可检查。

- 若允许 Prompt assembly、stream buffer 或裸 message vector 直接构造模型可见 conversation，durable truth 会散落到多个来源；
- 若把 chat history 与 operational events 分成两个 durable log，会形成 dual truth，需要额外 snapshot、ordering 与 repair 协议；
- 若把一个 finalized assistant response 拆成多条 entry，或把物理写入耦合成业务 batch 协议，会引入 response group、usage 重复、fingerprint 与 fork remap 复杂度；
- crash 后若自动把半个 ToolRound 提升为模型可见，模型可能看到未经当前 owner 仲裁的旧结果。

需要一个单一 durable truth，配合唯一 write seam 和 committed-only projection。详见 [Conversation 与 SessionStorage 模块](../../../modules/conversation-storage.md)。

## 决策

- `SessionStorage`是Session已写入conversation/message/lifecycle history的唯一durable truth；物理layout是per-session append-only by-entry JSONL tree（`sessions/<SessionId>.jsonl`）。
- `SessionWriter::append(SessionEntryDraft)` 是已创建 Session 全部 runtime ledger mutation 的唯一 write seam；不提供 `append_raw_json`、`replace_history`、`write_projection` 等旁路。
- `SessionHeader` 是第一行，仅由 create/fork staging 原子写入且 immutable；其后每一物理 line 是一条完整 newline-terminated `StoredSessionEntry`，也是 process-crash visibility unit。
- entry body使用typed User/Assistant/Tool message、durable lifecycle/Interaction event与Compaction entry；Turn start的safe历史说明内联于Input UserMessage。一个finalized logical model response保存为一个assistant entry，其`content[]`按canonical顺序保存reasoning/text/tool_call，usage随该entry保存。
- 任意模型调用只能从 committed transcript 构建 conversation：`CommittedConversationView` 没有 public constructor，只能来自 SessionStorage replay 或成功 `CommittedSessionEntry` delta apply。
- 全部 projection delta 由 SessionStorage trusted projectors 生成（不接受 caller-provided delta），以 `AdvanceOnly | Append | Replace` 推进 `CommittedConversationState/View/Delta`；每个成功 append 推进 `ConversationCheckpoint`，projection apply 失败即丢弃 hot projection 并从 durable current entry reload。
- live writer执行strict validation并生成storage-owned trusted delta；cold replay顺序扫描完整记录，对malformed/duplicate/orphan/invalid relation执行局部skip或isolate。两条路径共享typed entry与projector语义，但cold replay不要求复现live rejection合同。完整规则见[INV-001与INV-002](../../../architecture.md#跨模块不变量索引)。
- entry用`EntryId + parent_id`形成immutable history tree；current entry是最后成功append的entry，`ConversationCheckpoint`只引用selected ledger head。不建立Branch entity。
- fork复制selected path并保留历史Entry/Turn/Item/Request/ToolCall IDs，只分配new SessionId；future append生成fresh ID。
- crash后不把incomplete Tool exchange提升为模型可见：含ToolCall的assistant只有在全部matching terminal results形成provider-valid complete exchange后进入conversation。baseline不补写completion marker、不自动重放outcome-unknown Tool、不生成synthetic ToolResult；完整规则见[INV-003](../../../architecture.md#跨模块不变量索引)。
- MVP不实现projection snapshot、byte-offset/checkpoint index、physical segmentation或vacuum；session index与search database只是rebuildable cache。cold load扫描全部newline-terminated records，重建sanitized projections并保守关闭unfinished Turn后进入Idle或Unavailable。
- 明确不引入：chat/event dual log、Branch entity、SQLite baseline、content-addressed DAG、业务 batch 协议。

## 后果

- durable truth 单点化：模型输入、Runtime event、snapshot、usage 与 index 全部由同一 entry tree replay/投影，不存在需要对账的第二来源。
- 唯一write seam让validation、乐观并发（`expected_current_entry`）与receipt集中；append/apply是durable/model-visible事实的线性化点。Tool side-effect start由current-Runtime owner-local reservation按[INV-401](../../../architecture.md#跨模块不变量索引)线性化，不伪装成storage event。storage-ack unknown时poison writer并保守终结，恢复靠实际文件与sanitized projection判断，不做durable operation-key溯源重建。
- by-entry 增加 line count 与 append 次数，但降低单 line 复杂度，并让 inspect、history branch 与 fork 更灵活。
- committed-only模型可见性配合宽容recovery，保证crash后模型看不到incomplete Tool exchange；被abandon的in-flight Tool不会自动续接。
- rebuildable cache可随时重建，schema演进不影响durable truth；测试分别覆盖strict live append与tolerant replay，不再要求两者返回相同错误。
- cold load时间随complete entry数量线性增长是MVP接受的取舍；已loaded Session切换不触发storage replay，Compaction只降低model-visible conversation。只有真实性能数据证明该取舍不可接受时，才重新评估projection acceleration。

## 历史

本 ADR 属 V2 决策集，取代以下 V1 决策，原文见 `docs/archive/v1/adr/`：

- ADR 0024（Session storage 采用 by-entry JSONL）；
- ADR 0019（Session 写入使用统一 trusted batch writer；物理 batch 已被 by-entry 取代）；
- ADR 0023（Driver 从一个 committed ConversationSeed 启动）。
