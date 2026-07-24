# docs/refactor/（已迁移，待 review 后删除）

> **状态：已迁移 · 非权威 · 待删除**
>
> 本目录是 V2 重构期的目标架构草稿。其内容已**深度提炼并迁移**到正式位置，本目录仅暂时保留供 review 比对，review 完成后将删除。**请勿再把本目录作为权威依据。**

## 内容去向

| 本目录文件 | 已迁移到 |
| --- | --- |
| `runtime-interface.md` / `agent-session-lifecycle.md` / `turn-execution-context.md` / `turn-item-interaction.md` / `conversation-storage.md` / `session-execution.md` / `model-gateway.md` / `compaction.md` | `../modules/<同名>.md` |
| `workspace-subsystem.md` | `../modules/workspace.md` |
| `prompt-subsystem.md` | `../modules/prompt.md` |
| `tool-subsystem.md` | `../modules/tools.md` |
| `skill-subsystem.md` | `../modules/skills.md` |
| `minicore-domain-model.md` | 并入 `../architecture.md`（领域模型章节） |
| `refactoring-roadmap.md` | `../migration/v1-to-v2.md` |
| `session-execution-progress.md` | 研究/handoff 记录，迁移完成后由 Git history 保留 |

当前权威架构：`../architecture.md` + `../modules/`（12 篇）+ `../adr/`（0100–0108）+ `../migration/v1-to-v2.md`。
