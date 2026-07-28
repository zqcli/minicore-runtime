# ADR 0122：Workspace fingerprint只在当前Runtime有效

状态：Accepted
日期：2026-07-28

## 背景

O12要求冻结`WorkspaceFingerprint`及各Workspace view fingerprint在Runtime重启、Session reload和fork后的恢复策略。旧建议倾向从durable definition、authority policy和canonicalization algorithm确定性重建，并为跨进程相等性建立versioned canonical encoding与golden vectors。

当前MVP已经选择更保守的恢复边界：Agent、Session definition和conversation history是durable truth；loaded Session、active operation、WorkspaceSnapshot、Tool grant、authorization cache、process handle和security signal都属于Runtime执行态。restart后unfinished Turn按HostRestart/RecoveryContextUnavailable收口，不恢复旧AgentLoop、Tool task或WorkspaceSnapshot。fork也不复制Session-scoped Tool grant。

pi、Codex、Gemini CLI、OpenHands和Claude Code均以恢复conversation/history为主，并在新进程按current cwd、workspace roots、settings、sandbox和permission policy重新建立执行环境。高权限模式、后台进程、临时目录扩展、approval状态和旧工具连接通常不恢复。

## 决定

1. `WorkspaceFingerprint`、`WorkspacePromptFingerprint`、`WorkspaceSkillFingerprint`、`WorkspaceAccessFingerprint`和`WorkspaceToolFingerprint`都是**Runtime-instance-local opaque identity**。它们只标识当前Runtime中一次成功resolve产生的`WorkspaceSnapshot`及其窄view。
2. 每次load、unload后的重新load、SecurityRevoked后的重新resolve或Runtime restart都创建新的fingerprint family。即使durable Workspace definition和effective grants相同，新值也不要求与旧值相等。
3. MVP不为Workspace fingerprint定义跨进程canonical encoding、hash algorithm version、确定性重建或golden vector。实现可以使用process-local generation、随机opaque value或内部hash；调用方只能比较当前Runtime内的typed value，不能解析内容。
4. 同一个`WorkspaceSnapshot`投影出的全部窄view必须绑定同一次resolution generation。`ToolSet`、`PromptSet`、`SkillView`和authorization-sensitive cache只使用当前view fingerprint做同Runtime cross-binding或cache invalidation。
5. `ToolGrantStore`是Runtime-instance-local。Turn/Session grant只在当前Runtime且matching current `WorkspaceAccessFingerprint + PolicyRevision`时有效；Runtime restart、fork或Workspace重新resolve后不恢复、不迁移旧grant。旧grant entry即使仍留在内存，也因fingerprint不匹配而不可达。
6. `StoredTurnContext`可以保存当时的Workspace、Prompt、Tool、Skill和Execution fingerprints作为opaque historical diagnostic/correlation value。cold replay不得重算这些值、与current view比较后授权、或用它们恢复旧Context。
7. MVP不提供process restart后的exact same-Turn resume。没有terminal fact的Turn按既有conservative recovery关闭；future Turn从durable current SessionDefinition和current authority capture全新的WorkspaceSnapshot、PromptSet、ToolSet和SkillView。
8. Runtime restart后Session仍可从durable definition与conversation重新load；“不恢复Workspace fingerprint”只删除旧执行态恢复，不删除durable Session/history能力。Workspace或authority当前不可用时，Session进入`Unavailable`并fail closed。
9. 未来若引入durable Tool grant、跨设备Session执行迁移或exact same-Turn resume，必须同时定义可持久化execution manifest、authority proof和fingerprint encoding，并通过新的ADR重新打开本决策。

## 理由

- Workspace fingerprint当前只服务同Runtime snapshot/view一致性、cache key和grant binding；没有真实跨进程调用方。
- 放弃旧fingerprint恢复与既有restart边界一致，避免为不会恢复的WorkspaceSnapshot、Tool grant和execution Context建设独立持久协议。
- 每次resolve生成新family会保守失效旧cache和grant，不会把旧权限带入current authority。
- durable conversation继续提供历史和审计；opaque historical fingerprint足以关联同一Turn当时捕获的子view，不承担授权职责。

## 后果

- O12关闭；删除Workspace fingerprint跨进程确定性重建和golden-vector要求。
- Runtime restart或Workspace重新resolve后，用户可能需要重新审批Session-scoped Tool grant；这是有意的fail-closed行为。
- `ExecutionContextFingerprint`及包含Workspace child fingerprint的Prompt/Tool fingerprint不能作为cold-resume proof；persisted值只用于历史diagnostic。
- O14/O15仍可为真正需要稳定内容identity的Compaction directive和Prompt正文定义versioned content hash；本ADR只收窄Workspace snapshot/view identity。

## 修订关系

本ADR补充[ADR 0101](0101-workspace-ownership.md)的Workspace fingerprint身份语义，并关闭[ADR 0121](0121-workspace-updates-require-idle.md)保留的O12。SessionStorage durable truth、conservative unfinished-Turn recovery、Idle-only Workspace update和SecurityRevoked settlement保持不变。