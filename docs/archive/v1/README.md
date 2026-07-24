# MiniCore V1 架构归档

本目录保存 **MiniCore V1** 的架构文档与历史决策，仅用于历史参考。

> **非权威**：本目录中的任何内容都**不能**作为当前实现或新开发的设计依据。V1 描述的是重构前的模块划分、调用链和持久化结构（`ResourceManager` / `ResourceSnapshotStore` 四层资源快照、`SessionRuntime` two-task 执行、stable batch writer 等），这些 owner 和不变量已被 V2 目标架构替代。

当前权威架构见：

- `docs/architecture.md` —— V2 架构总入口
- `docs/modules/` —— V2 正式模块文档
- `docs/adr/`（0100+）—— V2 当前决策
- `docs/migration/v1-to-v2.md` —— V1 → V2 版本迁移记录

## 目录结构

```text
docs/archive/v1/
├─ modules/    V1 模块文档（含旧模块总览 README）
├─ adr/        V1 决策 ADR 0001–0028（原文保持历史原貌，仅在顶部加归档说明）
├─ review/     V1 系统蓝图评审记录
├─ progress/   V1 阶段性研究/handoff 记录
├─ design/     V1 设计推导背景材料
└─ README.md   本文件
```

## 使用约束

- V1 文档描述的类型、调用方式和行为**不是** V2 的兼容目标。
- 正式文档不再链接 V1 归档；只有 `docs/migration/v1-to-v2.md` 与新 ADR 的历史依据部分可以引用它。
- V1 ADR 正文中指向旧 `refactor/` 或旧模块路径的链接属历史事实，不再维护；完整历史版本可通过 Git history 查阅。
