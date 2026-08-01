# MiniCore V2 Documentation

本目录默认只把current V2合同暴露给开发者和AI。历史决策仍保存在`docs/archive/`与Git history中，但不参与当前实现判断。

## 开发入口

按以下顺序阅读：

1. [开发计划](development-plan.md)：阶段、依赖、测试与退出条件；
2. [架构总览](architecture.md)：领域模型、执行结构与跨模块不变量；
3. [模块总览](modules/README.md)：canonical owner与具体接口合同；
4. [ADR索引](adr/README.md)：current、refined与historical决策分类；
5. [Wire Schema](modules/wire-schema.md)、[Conversation JSONL V1](formats/conversation-jsonl-v1.md)和[Wire V1 Fixtures](fixtures/wire-v1/README.md)：public/storage representation；
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

Rust crate的M1 Wire foundation与owner semantic spine已经进入`dev`：ProtocolLimits/counters、bounded dynamic JSON、bounded JSON Schema、typed bootstrap codec，以及Prompt/Tools/Model/Turn/Interaction semantic values与Compaction stored-provenance/stable-kind leaves均已实现并通过Fast、MSRV与heavy gates。完整public/storage codec、LiveConversation、SessionRecorder、SessionExecutor、ActiveTurnTask和provider行为尚未实现。

开发计划M0与M1已经完成，当前暂停于M2前等待review；后续主要门禁：

- M5.0：冻结single-process durable entity store、CAS与跨entity/conversation staging协议；
- V4-P1-3：production ProviderAdapter前关闭Rig reality与provider scope；
- V4-C0-1：production Tool/Sandbox adapter开始前关闭enforcement fail-closed合同。

## 目录角色

- `modules/`：当前semantic contract与canonical owner；
- `adr/`：决策理由与successor关系，分类见[ADR索引](adr/README.md)；
- `formats/`、`fixtures/`：exact wire/storage format与测试资产；
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
