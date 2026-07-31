# ADR 0110: Prompt 与 Skill 使用共享、可替换 View

状态：Partially Superseded by ADR 0127
日期：2026-07-24

> 2026-07-31：Prompt/Skill shared immutable views、explicit reload和future-Turn生效规则继续有效；ADR 0127删除restart时的unfinished Turn terminalization，Load只恢复conversation并清空current Turn。

## 背景

V2评审C1、C2、C4暴露了三处不必要复杂度：PromptDefinition被复制到Agent/Session scope并通过多层override解析；Prompt同时保留三种instruction role；SkillCatalog使用revision、version和exact content hash承担严格漂移恢复。

pi、Codex和Claude Code采用更直接的常规模式：资源由Runtime共享，每个Session或Turn构造自己的有效上下文；高信任基础指令与普通User context分开；资源reload只影响后续调用，已经注入conversation的正文不回写。

## 决策

1. PromptService发布不可变`PromptResourceView`并拥有共享`PromptDefinition`。AgentDefinition保存可信System PromptId selection，SessionDefinition保存本Session的User PromptId selection；多个Session可以引用同一definition，但每个Turn独立构造PromptSet。
2. 删除`DefinitionOverrides`。Runtime required Prompt不进入selection且始终强制加入；selection引用缺失PromptId时返回typed error，不静默忽略。
3. Prompt role只保留System和User。Runtime required/base policy与Agent behavior进入System；Session、Workspace、Skill和普通输入进入User。Tool schema进入provider原生tools字段。
4. shared Prompt resource只在Runtime初始化或显式`/reload`时读取source并构建candidate `PromptResourceView`；PromptService不单独发布candidate。Runtime在Prompt/Skill/Tool/Model required candidates全部校验成功后，于短publication gate下替换四个current `Arc`。source watcher最多标记dirty，不自动替换。active PromptSet不原地更新，future Turn捕获新view。
5. SkillService发布shared `SkillResourceView`，并从Turn捕获的resource root与`WorkspaceSkillContext`构建不可变`SkillView`。shared Skill source由Runtime initialize/shared `/reload`捕获；Workspace Skill source由Session load、Idle Workspace definition update或`/reload workspace`捕获并随WorkspaceSnapshot发布。任一candidate流程失败都保留old published values。
6. Skill不使用CatalogRevision、DefinitionVersion或exact content-hash pin作为业务恢复协议。已加载`Arc<LoadedSkill>`和已经committed的Skill正文不被reload修改；尚未lazy parse/load的entry只能解析对应shared或Workspace publication中captured bytes/content，不能在Turn内按path重新读取current file。
7. C3 Sandbox capability问题本轮不作变更，保持开放。

## 后果

- PromptDefinition正文由Runtime共享，Session只保存选择，删除scope override单调性问题。
- ModelGateway不再需要第三种instruction role的lowering，只映射System与conversation messages。
- PromptSet、ToolSet和captured SkillView在active Turn内仍保持不可变；PromptSet唯一assembly seam和sanitized LiveConversation输入不变。
- Prompt/Skill采用显式reload一致性：shared文件变化只有在`/reload`成功后影响future Turn，Workspace-bound文件变化只有在Session load、Idle definition update或`/reload workspace`成功后影响future Turn；active Turn和reload失败都继续使用old captured content。正文一旦committed就成为固定conversation事实。
- 进程重启后不恢复旧Prompt/Skill execution objects；unfinished Turn保守中断，不使用current Prompt或Skill冒充旧Context继续执行。

## 关闭的评审问题

- C1：删除DefinitionOverrides，改为共享PromptResourceView和per-Session PromptId selection。
- C2：Prompt role收敛为System/User并冻结资源分配。
- C4：Skill改为shared reload时替换SkillResourceView、按Turn构建SkillView，不采用strict revision/hash pin。

C3保持开放。

## 后续修订

2026-07-28：[ADR 0123](0123-identity-uses-refs-and-explicit-reload.md)删除PromptFingerprint/Skill content-hash pin作为当前架构一致性机制，新增四个shared current roots的原子publication gate，并区分shared source capture与Session-local Workspace source capture。本文的共享view、显式reload只影响future Turn和已committed正文不回写原则保持有效。
