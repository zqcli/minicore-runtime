# MiniCore V2 Documentation

本目录默认只把current V2合同暴露给开发者和AI。历史决策仍保存在`docs/archive/`与Git history中，但不参与当前实现判断。

## 开发入口

按以下顺序阅读：

1. [开发计划](development-plan.md)：阶段、依赖、测试与退出条件；
2. [架构总览](architecture.md)：领域模型、执行结构与跨模块不变量；
3. [模块总览](modules/README.md)：canonical owner与具体接口合同；
4. [ADR索引](adr/README.md)：current、refined与historical决策分类；
5. [Wire Schema](modules/wire-schema.md)、[Conversation JSONL V1](formats/conversation-jsonl-v1.md)、[Wire V1 fixtures](fixtures/wire-v1/README.md)、[Durable Store V1](formats/durable-store-v1.md)及其[Durable Store V1 fixtures](fixtures/durable-store-v1/README.md)：public/storage representation；
6. [第四轮设计评审](review/v2-design-review-4.md)：尚未关闭的production gates。

权威顺序：

```text
architecture.md + modules/
→ current/refined ADR（以adr/README.md分类为准）
→ formats/ + fixtures/（representation与conformance）
→ development-plan.md（实施顺序，不改变领域语义）
→ migration/ + research/
→ archive/（历史，不具权威性）
```

## 当前状态

M0–M6 foundations、M7 ordinary AgentRun与M8最小Tool/Interaction/Cancel路径已完成；M9当前control/observation范围已接通Steer/FollowUp、queued cancellation、logical retry、EmergencyControl、runtime-wide CommandId dedup、Starting/Running/Finishing Snapshot及`session_execution_changed`/terminal StateEvent，并通过Wire V1 active fixtures。具体Prompt/Skill source adapter、完整Tool policy/approval、M10 Compaction、remaining Fork anchors/LiveSnapshot、active-Turn grace Unload及完整cross-platform native matrix仍pending。

开发计划M0与M1已经完成，M2按行为slice增量推进，M8.1已完成；后续主要门禁：

- M5.0–M8 foundation/behavior已完成；M9.1–M9.21当前control/observation范围已完成并统一review，覆盖queues、Steer/FollowUp、logical retry、Emergency/SecurityRevoked、public command/outcome、Snapshot和execution/terminal StateEvent；下一步为具体Skill composition与M10 Compaction，随后推进M11 Fork/full recovery conformance；
- V4-P1-3：production ProviderAdapter前关闭Rig reality与provider scope；
- V4-C0-1：production Tool/Sandbox adapter开始前关闭enforcement fail-closed合同。

## 目录角色

- `modules/`：当前semantic contract与canonical owner；`DurableState`是local entity-store physical operation owner；
- `adr/`：决策理由与successor关系，分类见[ADR索引](adr/README.md)；
- `formats/`、`fixtures/`：exact wire/storage format与测试资产（含Durable Store V1 golden/crash matrix）；
- `review/`：仍开放的评审finding；
- `research/`：非权威研究证据，见[Research索引](research/README.md)；
- `migration/`：V1到V2概念迁移说明；
- `archive/`：历史原文，不用于current实现。

## 搜索规则

仓库`.rgignore`默认排除`docs/archive/`：

```bash
rg 'SessionExecutor' docs
```

显式查询历史时使用：

```bash
rg -uu 'SessionWriter' docs/archive
```

不要从archive命中推导current Rust接口；发现current合同冲突时先修canonical module或新增ADR。
