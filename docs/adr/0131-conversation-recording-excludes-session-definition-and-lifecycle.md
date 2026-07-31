# ADR 0131：Conversation Recording不保存Session Definition与Lifecycle事件

状态：Accepted
日期：2026-07-31

> [ADR 0134](0134-public-and-conversation-wire-use-bounded-v1-schemas.md)与[Format V1](../formats/conversation-jsonl-v1.md)进一步flatten `StoredMessage/StoredEvent` wrappers为六种Turn-scoped body variants，并使StoredSessionEntry.turn_id required；仍只保存User/Assistant/Tool、Interaction与Compaction，owner边界不变。

## 背景

ADR 0126/0127已经把JSONL收缩为inline best-effort conversation recording，并删除Turn start/terminal ledger。Conversation Storage仍保留`StoredEvent::SessionDefinitionChanged`与`SessionLifecycleChanged`，但这些mutation可以发生在Unloaded Session：Create发布`Open + Unloaded`且不创建Recorder，Archive/Delete要求Unloaded，definition/metadata update也可在Unloaded执行。Unloaded owner没有LiveSessionState EntryIdGenerator；loaded active Turn期间写definition event又会引入第二record producer。

保留这两个variant会迫使实现旁路SessionRecorder、让Recorder mutex顺序定义跨owner history，或只记录部分configuration timeline。Fork随后还会把source Session lifecycle记录复制进child conversation，产生错误ownership。

## 决策

1. Session JSONL只保存可以组成conversation/history的事实：User/Assistant/Tool messages、Interaction requested/resolved和StoredCompaction。删除`StoredEvent::SessionDefinitionChanged`、`SessionLifecycleChanged`及其payload types。
2. Agent/Session definition、metadata与durable lifecycle只由各自entity durable store保存为current head/revision/status。Create/Update/Archive/Unarchive/Delete不调用SessionRecorder，不分配conversation EntryId，也不成为loaded active Turn的第二record producer。
3. `SessionHeader`继续由Create/Fork staging写入，保存file identity、created_at和initial Agent/Session definition refs。Header没有EntryId，不是mutation timeline、current definition owner、authorization proof或old execution recovery source；current state始终从Agent/Session durable owner读取。
4. Runtime query/snapshot/typed StateEvent提供current observer state，但它们不是可重放日志。restart从entity head恢复Agent/Session状态，从JSONL恢复conversation；不通过conversation event重建definition/lifecycle。metadata专用event kind仍由Runtime public protocol freeze决定，不影响no-JSONL规则。
5. 所有`StoredSessionEntry`仍由loaded LiveSessionState在validation后、apply前分配EntryId并经同一SessionRecorder顺序attempt。Interaction是合法recordable fact，因为其request/resolution只在loaded active execution中产生；Session definition/lifecycle不满足该条件。
6. Load先读取current durable Session/Agent definition，再replay recorded conversation prefix并构造new loaded execution。JSONL缺少definition/lifecycle history不会减少Load所需事实；replay不尝试把历史配置投影为current execution environment。
7. Fork只复制selected conversation/history path。child definition由fork lifecycle owner在publication时从source durable definition复制并创建child-local revision；child transcript不含source definition/lifecycle timeline。
8. future若需要configuration audit，必须建立独立audit/definition-history seam，使用entity owner自己的revision、retention和atomic store；不能重新借用conversation EntryId tree。

## 后果

- conversation schema与single-producer/EntryId ownership闭合，Create/Archive/Delete无需伪造loaded recorder。
- JSONL不提供Session配置与lifecycle审计轨迹；MVP只保证current durable head和conversation history。
- Fork、replay和history query只解释conversation facts，不再处理source/child lifecycle歧义。
- Runtime event断线后以new Snapshot/entity query恢复current state，不回放旧definition/lifecycle transition。

## 测试要求

- Create只写SessionHeader，不写SessionLifecycleChanged entry；
- loaded/unloaded definition或metadata update都不append conversation JSONL；
- Archive/Unarchive/Delete不需要Recorder或EntryIdGenerator；
- active Turn期间future-only definition update不成为第二record producer；
- Load从current entity head + recorded conversation prefix恢复；
- loaded/unloaded Fork均只复制conversation facts，child definition来自lifecycle staging；
- format-v1 StoredEntryBody使用UserMessage/AssistantMessage/ToolMessage/InteractionRequested/InteractionResolved/Compaction六个flat variants，不恢复StoredEvent wrapper；
- current canonical docs不再定义两个已删除variant/payload。

## 修订关系

本ADR补充ADR 0127的conversation-only recording范围，并取代ADR 0104/0124中“conversation JSONL保存configuration/durable lifecycle event”的历史表述。它不删除SessionHeader initial provenance，不改变Interaction ordering、Compaction marker或tolerant replay。