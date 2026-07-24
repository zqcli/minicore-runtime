# ADR 0104: SessionStorage 是 durable truth

状态：Accepted
日期：2026-07-24

## 背景

一个 Session 的 conversation 与 execution ledger 需要一个明确的 durable owner：任意模型调用只能看到已提交的历史，crash 之后可恢复，而 branch/fork/compaction 又要保持可检查。

- 若允许 Prompt assembly、stream buffer 或裸 message vector 直接构造模型可见 conversation，durable truth 会散落到多个来源；
- 若把 chat history 与 operational events 分成两个 durable log，会形成 dual truth，需要额外 snapshot、ordering 与 repair 协议；
- 若把一个 finalized assistant response 拆成多条 entry，或把物理写入耦合成业务 batch 协议，会引入 response group、usage 重复、fingerprint 与 fork remap 复杂度；
- crash 后若自动把半个 ToolRound 提升为模型可见，模型可能看到未经当前 owner 仲裁的旧结果。

需要一个单一 durable truth，配合唯一 write seam 和 committed-only projection。详见 [Conversation 与 SessionStorage 模块](../modules/conversation-storage.md)。

## 决策

- `SessionStorage` 是 Session conversation/execution ledger 的唯一 durable truth；物理 layout 是 per-session append-only by-entry JSONL tree（`sessions/<SessionId>.jsonl`）。
- `SessionWriter::append(SessionEntryDraft)` 是已创建 Session 全部 runtime ledger mutation 的唯一 write seam；不提供 `append_raw_json`、`replace_history`、`write_projection` 等旁路。
- `SessionHeader` 是第一行，仅由 create/fork staging 原子写入且 immutable；其后每一物理 line 是一条完整 newline-terminated `StoredSessionEntry`，也是 process-crash visibility unit。
- entry body 固定为 `StoredEntryBody = TurnContext | Message | Event | Compaction`；Message role 为 `user | assistant | tool`，一个 finalized logical model response 保存为一个 assistant entry，其 `content[]` 按 canonical 顺序保存 reasoning/text/tool_call，usage 随该 entry 保存。
- 任意模型调用只能从 committed transcript 构建 conversation：`CommittedConversationView` 没有 public constructor，只能来自 SessionStorage replay 或成功 `CommittedSessionEntry` delta apply。
- 全部 projection delta 由 SessionStorage trusted projectors 生成（不接受 caller-provided delta），以 `AdvanceOnly | Append | Replace` 推进 `CommittedConversationState/View/Delta`；每个成功 append 推进 `ConversationCheckpoint`，projection apply 失败即丢弃 hot projection 并从 durable current entry reload。
- entry 用 `EntryId + parent_id` 形成 immutable history tree；current entry 是最后成功 append 的 entry；stable checkpoint 由 boundary/tree projection 提供。不建立 Branch entity。
- fork 对 selected parent path 做 deep copy 并 remap target-local identities（EntryId/parent_id、TurnId/ItemId/RequestId、operation key），保留 ToolCallId 与 historical exact refs 的 source-scoped 语义。
- crash 后不把半个 ToolRound 提升为模型可见：assistant intermediate 与 tool message 在 `tool_round_completed` event 前都不 model-visible；baseline 不自动补写 completion event、不自动重放 outcome-unknown Tool、不生成 synthetic ToolResult。
- projection snapshot、session index 与 search database 只是 rebuildable cache，不是第二事实来源。
- 明确不引入：chat/event dual log、Branch entity、SQLite baseline、content-addressed DAG、业务 batch 协议。

## 后果

- durable truth 单点化：模型输入、Runtime event、snapshot、usage 与 index 全部由同一 entry tree replay/投影，不存在需要对账的第二来源。
- 唯一 write seam 让 idempotency、validation、operation key 与 receipt 集中，append/apply 成为可见性与 side-effect 的唯一线性化点。
- by-entry 增加 line count 与 append 次数，但降低单 line 复杂度，并让 inspect、history branch 与 fork 更灵活。
- committed-only 模型可见性配合保守 recovery，保证 crash 后模型永远看不到 uncompleted ToolRound，代价是被 abandon 的 in-flight Tool 需重新发起而非自动续接。
- rebuildable cache 可随时重建，schema 演进不影响 durable truth，但要求 projector 与 durable enum 保持 fail-closed。

## 历史

本 ADR 属 V2 决策集，取代以下 V1 决策，原文见 `docs/archive/v1/adr/`：

- ADR 0024（Session storage 采用 by-entry JSONL）；
- ADR 0019（Session 写入使用统一 trusted batch writer；物理 batch 已被 by-entry 取代）；
- ADR 0023（Driver 从一个 committed ConversationSeed 启动）。
