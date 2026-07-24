# ADR 0110: Prompt 与 Skill 使用共享、可替换 View

状态：Accepted
日期：2026-07-24

## 背景

V2评审C1、C2、C4暴露了三处不必要复杂度：PromptDefinition被复制到Agent/Session scope并通过多层override解析；Prompt同时保留三种instruction role；SkillCatalog使用revision、version和exact content hash承担严格漂移恢复。

pi、Codex和Claude Code采用更直接的常规模式：资源由Runtime共享，每个Session或Turn构造自己的有效上下文；高信任基础指令与普通User context分开；资源reload只影响后续调用，已经注入conversation的正文不回写。

## 决策

1. PromptService发布不可变`PromptResourceView`并拥有共享`PromptDefinition`。AgentDefinition保存可信System PromptId selection，SessionDefinition保存本Session的User PromptId selection；多个Session可以引用同一definition，但每个Turn独立构造PromptSet。
2. 删除`DefinitionOverrides`。Runtime required Prompt不进入selection且始终强制加入；selection引用缺失PromptId时返回typed error，不静默忽略。
3. Prompt role只保留System和User。Runtime required/base policy与Agent behavior进入System；Session、Workspace、Skill和普通输入进入User。Tool schema进入provider原生tools字段。
4. Prompt reload先构建并校验candidate `PromptResourceView`，成功后原子替换current view。active PromptSet不原地更新，future Turn捕获新view。
5. SkillService按context发布不可变`SkillView`。显式reload先构建candidate view，成功后原子替换current view；source watcher只标记dirty，不自动替换。
6. Skill不使用CatalogRevision、DefinitionVersion或exact content-hash pin作为业务恢复协议。已加载`Arc<LoadedSkill>`和已经committed的Skill正文不被reload修改；尚未加载的entry按captured location读取当前内容，并在读取时校验Workspace authorization和source stamp。
7. C3 Sandbox capability问题本轮不作变更，保持开放。

## 后果

- PromptDefinition正文由Runtime共享，Session只保存选择，删除scope override单调性问题。
- ModelGateway不再需要第三种instruction role的lowering，只映射System与conversation messages。
- PromptSet、ToolSet和captured SkillView在active Turn内仍保持不可变；Transcript-First和PromptSet唯一assembly seam不变。
- Skill采用常规弱一致性：文件在view capture后、首次lazy load前变化时，load可能读到新正文；正文一旦committed就成为固定conversation事实。
- 进程重启后若旧PromptFingerprint无法重建，旧active Turn保守中断，不使用current Prompt冒充旧Prompt继续执行。

## 关闭的评审问题

- C1：删除DefinitionOverrides，改为共享PromptResourceView和per-Session PromptId selection。
- C2：Prompt role收敛为System/User并冻结资源分配。
- C4：Skill改为reload时原子替换current SkillView，不采用strict revision/hash pin。

C3保持开放。
