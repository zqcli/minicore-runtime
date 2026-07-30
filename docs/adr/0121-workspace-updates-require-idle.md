# ADR 0121：Workspace定义只在Idle更新，安全撤权中断当前Turn

状态：Partially Superseded by ADRs 0124 and 0126
日期：2026-07-27

> 2026-07-30：Workspace Idle-only update、SecurityRevoked和Tool settlement保留；TurnInterrupted先apply live并best-effort record，recording failure不影响Workspace readiness。

> 2026-07-29修订：Idle-only Workspace update、SecurityRevoked和重新resolve规则保持有效。Tool start/settlement改用owner-local ToolOperationSlot，不再写`ToolExecutionStarted`或补`tool_round_completed`。

## 背景

既有设计允许active Turn期间提交ordinary Workspace update，并为restrictive update引入`WorkspaceAuthorizationLease`、`WorkspaceAuthorizationControl`和`WorkspaceCommitAuthorization`：先撤销旧lease，再与workspace-dependent append排序并中断Turn。该能力进一步产生两个开放问题：restrictive update在revoke后durable commit失败的状态不一致（O10），以及已经打开的OS file handle不会随lease自动失效（O11）。

pi、Codex和Claude Code等同类产品通常在Session、Tool或sandbox启动前冻结cwd与权限；配置变化通过Cancel、interrupt或下一次执行生效，不提供active execution中的open-handle动态撤权。MiniCore当前仍处于设计阶段，没有真实需求证明热更新Workspace值得承担跨平台handle revocation和append/revoke竞态复杂度。

## 决定

1. loaded Session的Workspace definition update只在`SessionExecutionState::Idle`时接受。`Starting | Running | Finishing`立即返回现有`SessionBusy`，不排队、不隐式Cancel，也不等待当前Turn结束。unloaded Session可以更新durable definition，下次load时resolve。
2. active Turn捕获的`Arc<WorkspaceSnapshot>`在整个Turn生命周期内完全不可变。Turn不重新读取current Workspace、不获得新增root/grant，也不因definition update原地替换PromptSet、SkillView或ToolSet。
3. 删除active-Turn动态lease模型：`WorkspaceAuthorizationLease`、`WorkspaceAuthorizationControl`和`WorkspaceCommitAuthorization`不进入MVP。WorkspaceSnapshot及其Prompt/Skill/Tool/Access views不携带可撤销lease，也不暴露`check_authorization()`或`authorize_commit()`。
4. Workspace definition update在Idle下按以下顺序执行：validate/CAS candidate → resolve complete candidate → durable commit新的`SessionDefinitionRevision`与`WorkspaceRevision` → publish new `SessionWorkspaceState::Ready`。resolve或commit失败时旧definition与旧Snapshot保持current，不存在“已revoke但未提交”的中间状态。
5. authority hard restriction不是Workspace definition update。WorkspaceAuthority或host先发布新的authority/policy事实，再通过Runtime current loaded map向受影响`SessionExecutionHandle`发送handle-scoped sticky `EmergencyControl::SecurityRevoked`；存在candidate/current Turn时绑定该handle内的current control epoch。old/unloaded handle关闭后不能把signal重定向到new Executor。该signal不携带lease identity、generation identity，也不创建假的WorkspaceRevision或SessionDefinitionRevision。
6. SessionExecutor观察`SecurityRevoked`后立即关闭new Turn admission和新Model、Tool、source read、workspace-dependent append。Idle时直接标记旧Snapshot不可admit并重新resolve；Starting时取消candidate且不创建领域Turn；Running/Finishing时递增execution version并进入/保持Finishing。Prepared且start reservation未获胜的Tool安全取消；已经进入Running的Tool按[INV-401](../architecture.md#跨模块不变量索引)保存exact outcome或`ToolAbandoned`。有active Turn时最终append/apply`TurnInterrupted(SecurityRevoked)`。
7. candidate清理或Turn terminal后，loaded Session使用durable current `SessionDefinition.workspace`和current authority重新resolve。success时发布new Snapshot、retire signal并恢复Ready/Idle；failure时retire execution signal但进入`SessionReadiness::Unavailable(WorkspaceUnavailable)`。FollowUp可以在Finishing期间排队，但只能在terminal、重新resolve和Ready之后admit；失败时明确拒绝。
8. `SecurityRevoked`与admission、Tool start/append使用现有owner-local admission gate、`EmergencyControl` epoch和`TurnControlGate` first-wins语义：signal先赢则candidate/new operation不得开始；admission/controlled append或owner-local Tool start reservation先赢，则对应candidate被取消、短append完成或已开始副作用truthful settlement。无需第二个Workspace permit。
9. MiniCore不承诺authority hard restriction会使已经打开的OS fd、子进程handle或远端资源立即失效，也不承诺回滚已进入kernel或provider的副作用。它只保证signal获胜后不启动新的MiniCore-sanctioned operation；in-flight work按Cancel settlement收口。无法强制Sandbox capability仍由O1在pre-execution fail closed，不通过RevocableHandle补偿。
10. restart不恢复old WorkspaceSnapshot、security signal、Tool task或handle。若durable truth不能证明unfinished Turn因security restriction终止，recovery使用`HostRestart`或`RecoveryContextUnavailable`，不猜测`SecurityRevoked`；load时始终按current authority重新resolve。

## 理由

- Workspace definition是Session配置，不是需要在active Turn中热替换的运行时资源。Idle-only与Turn-pinned immutable Context最一致。
- 删除动态lease后，O10的“revoke成功、definition commit失败”和O11的“open handle继续写”状态均不可构造。
- authority hard restriction仍保留紧急安全出口，但复用已经存在的Cancel/Finishing/truthful settlement机制，不建立第二套handle lifecycle owner。
- `SessionBusy`、Snapshot-first observer和Cancel/FollowUp协议已经存在，Host可以实现`Cancel → wait session_settled → UpdateDefinition`，无需新增WaitForIdle或queued update协议。

## 后果

- Workspace update UX从热更新变为Turn间更新；长Turn中修改配置需要先Cancel。
- 删除Workspace lease/control/commit authorization相关字段、同步排序和测试；ADR 0111/0117中的Workspace revoke特殊分支由本ADR修订。
- `TurnInterruptionKind::SecurityRevoked`与sticky EmergencyControl保留，但它表示authority/host安全事件，不表示Workspace definition patch。
- O10和O11关闭；O1 Sandbox enforcement继续开放。O12先由[ADR 0122](0122-workspace-fingerprints-are-runtime-local.md)收窄Workspace fingerprint恢复策略，后由[ADR 0123](0123-identity-uses-refs-and-explicit-reload.md)删除Workspace fingerprint族并取代ADR 0122。
- handle-relative open仍可作为O1/TOCTOU防护的platform adapter实现问题，但不再用于承诺active-Turn动态revocation。

## 被否决方案

### 保留restrictive hot update并新增RevocableHandle

否决原因：OS无法可靠撤销已进入kernel的I/O，通用handle registry会重新引入resource lifecycle、跨await同步和truth ownership复杂度。

### 所有Workspace变化都只影响下一Turn且不打断安全事件

否决原因：managed hard deny或host安全事件仍需要停止新的操作；仅等待Turn自然结束不能满足fail-closed要求。

### Workspace update自动Cancel当前Turn并排队应用

否决原因：把配置update与Turn control合并会增加隐式长命令、队列和失败恢复语义。MVP让Host显式Cancel并在Idle后重试update更简单。

## 修订关系

本ADR修订ADR 0101的restrictive update lease模型、ADR 0111的WorkspaceAuthorizationRevoked ingress、ADR 0117的WorkspaceCommitAuthorization排序，以及相关Workspace/Session模块文档。ADR 0116的Session-local file mutation queue、ADR 0118的Cancel settlement和ADR 0120的module-local failure ownership保持不变。
