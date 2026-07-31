# Session Recording Omits Turn Lifecycle

状态：Accepted
日期：2026-07-31

MiniCore的Session JSONL用于恢复conversation transcript、history tree与必要解释元数据。它不承担Turn execution ledger职责，不保存`StoredTurnStart`、`TurnCompleted`、`TurnInterrupted`或`TurnFailed`。该决策关闭Q10：cold Load不推断、不合成也不追加旧Turn terminal。

> 2026-07-31：[ADR 0129](0129-user-message-contributions-use-part-level-safe-provenance.md)冻结UserMessage解释元数据：conversation正文继续承担恢复正确性；contribution stamp只保存safe part-level origin，损坏stamp不能导致正文丢失。

## 背景

ADR 0126已经把`LiveSessionState`确定为current-process truth，并把Session recording降级为inline best-effort前缀。继续记录Turn start/terminal会产生不对称生命周期：process可以在start之后、terminal之前退出，Recorder也可以在live terminal之后Degraded。文件随后包含unfinished Turn，迫使Load或Fork选择隐式边界、live recovery projection或synthetic closure。

Pi、Gemini CLI和Claude Code主要保存conversation transcript；Codex虽然保存Turn lifecycle events，hard crash后仍原样保留缺失terminal的rollout，并在prompt history中处理悬空call。OpenHands只有在持久化workflow execution status的产品定位下执行startup repair。MiniCore MVP需要conversation resume、history、fork和diagnostics，没有durable workflow、Turn审计或same-Turn crash resume需求。

## 决策

1. `StoredSessionEntry.turn_id`继续作为conversation correlation和history grouping identity，不表达durable `Running | Completed | Interrupted | Failed`状态。
2. Input与Steer都只记录`StoredUserMessage`；删除`StoredUserMessage.turn_start`和`StoredTurnStart`。实际响应模型继续保存在`StoredAssistantMessage.model`，Session/Agent definition由各自durable owner保存。
3. 删除`StoredEvent::TurnCompleted`、`StoredEvent::TurnInterrupted`、`StoredEvent::TurnFailed`和`StoredTurnTerminal`。
4. `TurnStatus`、`TurnStarted`与三个互斥terminal StateEvent保留为loaded Session的live interface。它们不进入JSONL，也不在restart后重放。
5. Turn creation在Input live apply时线性化；Agent lifecycle/admission permit在`SessionRecorder.record().await`前释放。`TurnStarted` publication仍等待当前Input record attempt。`Cancel(Submit(command_id))`在整个Starting阶段有效：Input已apply时绑定同一Turn并阻止ActiveTurnTask spawn。
6. 正常完成记录Final Assistant conversation fact。Interrupted或Failed只完成live settlement和StateEvent publication，不创建synthetic conversation entry。
7. cold replay只重建recorded conversation、history grouping和diagnostics，排除incomplete Tool exchange，不恢复ActiveTurnTask、Pending Interaction waiter或旧TurnStatus。Load完成后`current_turn = None`，Session进入`Idle`或`WorkspaceUnavailable`。
8. Fork复制selected conversation path并发布`Open + Unloaded` child；不追加fork-specific closure，也不把source active Turn状态复制到child。future Load建立`current_turn = None`的Idle view。
9. `ListTurns`和`GetTurn`继续按recorded `TurnId`分组conversation Items，但历史结果不携带execution status。当前loaded Turn状态只从`SessionSnapshot.current_turn`和实时StateEvent读取。
10. recovery不合成ToolResult、Interaction resolution、assistant result或Turn terminal。下一次Model仍只消费provider-valid sanitized conversation。

## 后果

- Q10关闭为Not Applicable；Load不执行recovery append，Recorder初始化顺序不再受closure约束。
- Turn admission不再持有Agent lifecycle gate等待filesystem I/O；Starting阶段的CommandId持续覆盖Input publication前的取消窗口。
- hard crash后可能留下UserMessage、Assistant ToolCall或若干完整ToolResult，且没有持久化的中断原因；UI显示recorded transcript并允许新Submit。
- Final Assistant足以表达conversation中存在一次正常响应，但JSONL不提供可审计的Turn outcome统计。
- Cancel、SecurityRevoked、Runtime failure和Model failure在当前process内仍有typed terminal StateEvent；restart后这些live outcome不恢复。
- Fork、replay和schema更小，避免为了best-effort terminal维护implicit closure、restart reason和重复terminal规则。
- future若出现durable workflow、审计或跨restart任务状态需求，应建立独立execution ledger/module，不能重新把partial lifecycle混入conversation transcript。

## 修订关系

本ADR部分取代ADR 0103的durable Turn boundary、ADR 0105/0108/0114的conservative terminalization、ADR 0117的start-commit permit、ADR 0118的Submit transition reservation与terminal append、ADR 0121的terminal append、ADR 0123/0124的`StoredTurnStart`，以及ADR 0126的Q10 recovery closure与terminal recording条款。async Turn loop、live terminal StateEvent、inline best-effort conversation recording、tolerant replay和complete Tool exchange规则继续有效。
