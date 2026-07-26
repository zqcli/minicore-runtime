# ADR 0114: Runtime观察协议使用Snapshot-First实时流

状态：Accepted
日期：2026-07-26

## 背景

V2原设计为Runtime scope和每个Session scope定义公开cursor、event replay window、Gap恢复和cursor-aligned Snapshot。该方案适合远程多客户端或离线增量同步，但MiniCore首版是本地、可嵌入Runtime；SessionSnapshot不包含完整历史，重新获取成本低，完整历史另走分页Query。

pi、Codex和Claude Code公开行为更接近“当前状态或transcript恢复 + 实时通知”：事件用于当前连接内更新，断线后重新读取状态，不承诺从公开observer offset重放。MiniCore当前没有远程daemon、离线同步或大Snapshot需求，继续维护公开cursor会引入watermark、epoch、Gap、replay buffer和ReadStamp等额外协议面。

## 决策

1. 首版删除公开`RuntimeCursor`、`SessionCursor`、`ScopedCursor`、`EventCursor`、cursor-based `ReadStamp`和`EventGap` replay协议。
2. `subscribe(scope)`建立snapshot-first实时流：订阅成功后的第一帧是该scope完整Snapshot，后续帧是实时`StateEvent`或`ProgressEvent`。
3. Snapshot capture与subscriber注册必须在对应owner内原子完成，保证第一帧Snapshot之后发生的StateEvent不会丢失，也不会把Snapshot之前的旧事件重新应用到其后。
4. StateEvent在当前subscription lifetime内按发送顺序可靠交付；subscriber背压、transport断开、Runtime restart或publisher关闭时终止stream，不缓存等待重放。Host重新subscribe并从新Snapshot恢复。
5. ProgressEvent仍可合并或丢弃；final StateEvent和下一次Snapshot携带完整final view。
6. CommandResponse只返回typed outcome，不返回cursor watermark。QueryResponse只返回typed data与可选领域revision，不返回cursor stamp。
7. Runtime scope和每个Session scope仍使用独立owner、Snapshot和event stream；不建立跨scope全局顺序。该变化不影响多个SessionExecutor并行Running。
8. 实现可以保留private monotonic generation用于原子publication、debug或测试，但该值不进入公开协议，也不承诺跨restart连续。

## 后果

- 删除公开cursor带来的watermark、epoch、Gap、replay-window和scope比较复杂度。
- Host断线或背压后必须重新订阅并重新取得Snapshot，不能增量重放缺失StateEvent。
- Snapshot-first原子注册成为关键正确性门槛；若无法保证原子性，不能用“先snapshot再subscribe”或“先subscribe再snapshot”的非原子组合替代。
- SessionStorage仍是durable truth；StateEvent仍只是observer数据，不成为第二日志。
- 未来若出现远程多客户端、离线增量同步或Snapshot成本实测过高，再以新protocol capability引入公开cursor，不为MVP预留半套replay语义。

## 修订关系

本ADR修订ADR 0108中的scoped cursor/replay决策，以及ADR 0111中Snapshot与SessionCursor原子发布的具体机制。四类Runtime能力、单SessionExecutor ownership、semantic ingress lanes和SnapshotMailbox保持不变。
