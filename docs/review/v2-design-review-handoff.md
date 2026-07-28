# V2设计评审工作交接

日期：2026-07-28

用途：在另一台电脑或新的Agent会话中恢复当前工作进度。架构事实仍以`docs/architecture.md`、`docs/modules/`和Accepted ADR为权威；本文只记录评审推进状态。

## 新电脑恢复

仓库尚未clone时：

```bash
git clone https://zqcli@github.com/zqcli/minicore-runtime
cd minicore-runtime
git switch dev
git pull --ff-only origin dev
```

仓库已存在时：

```bash
cd /path/to/minicore-runtime
git switch dev
git pull --ff-only origin dev
```

恢复后验证：

```bash
git status --short --branch
git log --oneline -8
git show --stat HEAD
```

随后按顺序阅读：

1. [第一版设计评审与开放项跟进](v2-design-review.md)
2. [第三版AgentLoop设计评审](v2-design-review-3.md)
3. [Compaction模块](../modules/compaction.md)
4. [Prompt模块](../modules/prompt.md)
5. [ADR 0112：Compaction active-Turn checkpoint与模型感知预算](../adr/0112-compaction-supports-active-turn-checkpoints.md)
6. [ADR 0119：模型调用使用Session逻辑重试](../adr/0119-model-calls-use-session-logical-retries.md)
7. [ADR 0123：执行一致性使用Exact Ref、不可变快照与显式Reload](../adr/0123-identity-uses-refs-and-explicit-reload.md)
8. [架构总览](../architecture.md)

## 当前仓库状态

- 当前分支：`dev`；
- 本次换机存档前的最新功能决策提交：`76148ab docs: accept linear session replay`；本handoff存档提交位于其后；
- 最近已接受决策：ADR 0123取代ADR 0122，删除`*Fingerprint`身份族并关闭O13/O14/O15；ToolSpec-only MVP关闭O16；O1延后到首个production Tool/Sandbox adapter前；O2以接受cold load完整线性replay关闭；
- O14/O15不再是当前进行中的未决issue；其历史调查记录已被ADR 0123的方案B式决策取代（不新增Directive/Prompt fingerprint，使用private constructor、immutable content和explicit reload）；
- 当前恢复链上的关键提交为`4a3fd24`（ADR 0123收敛）、`e6966a0`（O1延后）和`76148ab`（O2关闭）；
- 本文随当前进度存档提交push到`origin/dev`后继续作为跨机器恢复入口；新环境先检查远端与最新log，不要reset用户改动；
- 仓库仍处于V2设计阶段，没有`Cargo.toml`、`src/`或自动化测试；
- 下一实现里程碑仍是阶段6–8模型调用协同交付束；
- 当前第一项工作是第三版AgentLoop评审L2；O1不在当前工作队列，O2已经关闭；
- Rig只实现`ModelGateway` private `ProviderAdapter`中的单次provider attempt，不拥有ModelGateway或AgentLoop。

## 最近进度存档

```text
4a3fd24 docs: replace fingerprints with explicit reload refs
→ ADR 0123取代ADR 0122
→ 删除当前架构中的*Fingerprint身份族
→ 共享资源使用all-or-none explicit reload

e6966a0 docs: defer sandbox capability review
→ O1保持开放但延后
→ production Tool/Sandbox adapter开始前恢复为条件性P0

76148ab docs: accept linear session replay
→ O2关闭
→ cold load完整replay全部complete entries到physical current entry
→ 不恢复process-local执行对象
→ unfinished Turn保守terminalize后Session进入Idle
→ MVP不建设ProjectionSnapshot/checkpoint index
```

换机后不要重新调查O1/O2。直接从[第三版AgentLoop设计评审](v2-design-review-3.md)的L2继续；需要核对全局状态时回到本文“下一步”和“已冻结关键决策”。

## 本轮完成

前一轮`9aea732`新增[ADR 0121](../adr/0121-workspace-updates-require-idle.md)，并同步25份现有文档：

- loaded Session的Workspace definition patch只在`SessionExecutionState::Idle`接受；Starting/Running/Finishing返回`SessionBusy`，不排队、不隐式Cancel；
- Host修改active Session Workspace的显式流程是`Cancel → wait session_settled → UpdateDefinition`；
- active Turn pin的`WorkspaceSnapshot`完全immutable；
- 删除`WorkspaceAuthorizationLease`、`WorkspaceAuthorizationControl`、`WorkspaceCommitAuthorization`及append/revoke双permit竞态；
- authority/host hard restriction通过Runtime current loaded map向对应`SessionExecutionHandle`发送sticky `SecurityRevoked`；old handle关闭后不重定向到new Executor；
- Idle直接失效old Snapshot并重新resolve；Starting取消candidate后resolve；Running/Finishing停止新operation、truthful settle started Tool、append`TurnInterrupted(SecurityRevoked)`后resolve；
- resolve success发布new Snapshot并Ready，failure进入`SessionReadiness::Unavailable(WorkspaceUnavailable)`；
- 不承诺动态撤销open OS handle、回滚已进入kernel/provider的operation或建立Runtime-global handle registry；
- O10和O11关闭；O1 Sandbox fail-closed保持开放，但延后到首个production Tool/Sandbox adapter开始前，不阻塞阶段6–8。O12先由ADR 0122以放弃跨Runtime Workspace fingerprint恢复关闭，现由ADR 0123取代为删除fingerprint族。

随后新增[ADR 0122](../adr/0122-workspace-fingerprints-are-runtime-local.md)，关闭O12（现已被ADR 0123取代）：

- durable Agent/Session definition与conversation history继续保留；
- loaded Session、WorkspaceSnapshot、各view fingerprint、Tool grant、authorization cache和旧execution Context不跨Runtime恢复；
- load/reload/re-resolve按current definition/current authority创建新的fingerprint family；historical fingerprint只作opaque diagnostic；
- restart后的unfinished Turn保守关闭，MVP不提供exact same-Turn resume；
- 删除Workspace fingerprint canonical encoding、algorithm version和golden-vector要求。

本轮新增[ADR 0123](../adr/0123-identity-uses-refs-and-explicit-reload.md)，关闭O13/O14/O15并取代ADR 0122；同轮Prompt/Tool接口收窄关闭O16：

- MVP删除所有命名`*Fingerprint`类型，不新增`WorkspaceResolutionId`、`ToolSetId`、view generation或其他替代identity；
- durable definition继续使用exact refs/revisions：`AgentRevisionRef`、`SessionDefinitionRevision`、`WorkspaceRevision`、`ModelDefinitionVersion`及对应immutable definition retention；ledger/domain correlation继续使用`SessionId`、`TurnId`、`ItemId`、`RequestId`、`EntryId`和`ToolCallId`；
- active Turn持有同一组immutable `Arc<WorkspaceSnapshot>`、`Arc<PromptResourceView>`、`Arc<SkillView>`、`Arc<ToolSet>`、`Arc<PromptSet>`和`Arc<TurnModelSnapshot>`；private constructors阻止跨capture拼接任意view；
- Prompt/Skill/Tool/Model资源只在Runtime初始化或显式`/reload`后替换current immutable object；watcher最多标记dirty，不自动publication；active Turn继续使用old captured objects；
- `/reload`对Prompt/Skill/Tool/Model执行two-phase流程：各module只build/validate candidate，Runtime在短publication gate内整体替换private `SharedResourceRoots`（PromptResourceView、SkillResourceView、ToolResourceView、ModelCatalogView）；该bundle没有ID/version/generation，任一required candidate失败时保留完整old roots。`/reload workspace`继续要求Session Idle，非Idle返回`SessionBusy`；
- shared Prompt/Skill filesystem source在Runtime initialize或shared `/reload`时捕获；Workspace-bound Prompt/Skill source在Session load、Idle definition update或`/reload workspace`时捕获并随WorkspaceSnapshot发布；Skill可以lazy parse captured bytes，不能在Turn内按path重新读取current file；
- Tool approval在MVP只支持per-call `AllowOnce/AllowWith`，不保存`ToolGrantStore`、Turn grant或Session grant；
- logical retry复用同一个`Arc<ModelCallRequest>`，只验证Turn仍Running、`execution_version`、exact `ConversationCheckpoint.entry_id`、`current_operation`仍为持有该request的对应retry slot且control basis未变；不重新assemble，也不比较context摘要；
- Compaction operation持有同一个`Arc<CompactionPlan>`（settings、budget、scope、source）与由其组装出的同一个`Arc<ModelCallRequest>`，exact rendered directive随request固定；append前验证exact source checkpoint、scope、boundaries、provenance、current Turn/version/control和actual typed entries；
- `StoredTurnContext`和`StoredCompaction`删除Prompt/Tool/Skill/Workspace/Model/Execution、transcript、plan、budget、directive、summary等fingerprint/hash字段，只保存exact durable refs、typed scope/boundaries/provenance、safe diagnostics和model-call metadata。

## O14调查存档（历史，已由ADR 0123关闭）

O14曾是`CompactionSummaryDirective`正文与fingerprint coverage是否需要独立冻结的问题。以下保留为历史调查记录，不再是当前待办。当前权威决策见[ADR 0123](../adr/0123-identity-uses-refs-and-explicit-reload.md)：不新增`CompactionSummaryDirectiveFingerprint`，Directive由Compaction唯一private constructor创建，模板/格式不兼容变化递增`CompactionSummaryFormatVersion`，operation复用同一个`Arc<CompactionPlan>`与`Arc<ModelCallRequest>`。

```rust
pub struct CompactionSummaryDirective {
    format_version: CompactionSummaryFormatVersion,
    scope: CompactionSummaryScope,
    instruction: Arc<str>,
    max_output_tokens: NonZeroU32,
    max_summary_bytes: usize,
}
```

已确认的运行流程：

```text
NeedModel / ContextOverflow
→ Compaction::plan()
→ Compaction::build_summary_directive(&plan)
→ PromptSet::assemble(CompactionSummary { source, directive })
→ immutable Arc<ModelCallRequest>
→ ModelGateway single provider attempt
→ Compaction::validate_summary()
→ StoredCompaction append/apply
→ rebuild AgentLoop segment并重新assemble AgentRun
```

本轮场景分析结论：一次Compaction只使用一个Directive。此前用于说明风险的Directive A/B表示不同版本、配置或custom instruction产生的两种可能正文，不表示一次请求同时发送两个指令。MiniCore MVP当前没有manual/standalone compact、用户自定义compact instruction、extension替换hook、dynamic directive reload或process restart后的same-Turn resume；logical retry继续复用同一个`Arc<ModelCallRequest>`。

同类产品证据：

- pi允许`/compact [instructions]`，将custom instructions追加到实际summary prompt，保存summary和`firstKeptEntryId`，未发现独立Directive content fingerprint；
- Codex使用固定或配置的`compact_prompt`，把实际prompt放入history，transport retry循环复用同一份内存input/history，未发现独立Directive fingerprint；
- Gemini CLI用`getCompressionPrompt(config)`生成System instruction，summary后以同一prompt执行verification，测试直接断言prompt正文；
- Claude compaction API使用版本化类型（例如`compact_20260112`）并把custom instructions作为请求字段，没有公开独立content hash。

证据复核入口：

- pi文档：<https://pi.dev/docs/latest/compaction>；本机安装源码：`$USERPROFILE/AppData/Local/nvm4w/v24.14.0/node_modules/@earendil-works/pi-coding-agent/dist/core/compaction/compaction.js`，关键词`generateSummary`、`customInstructions`、`SUMMARIZATION_SYSTEM_PROMPT`；
- Codex源码：<https://github.com/openai/codex/blob/main/codex-rs/core/src/compact.rs>与<https://github.com/openai/codex/blob/main/codex-rs/prompts/templates/compact/prompt.md>，关键词`compact_prompt`、`history.clone()`、retry loop；
- Gemini CLI源码：<https://github.com/google-gemini/gemini-cli/blob/main/packages/core/src/context/chatCompressionService.ts>与<https://github.com/google-gemini/gemini-cli/blob/main/packages/core/src/prompts/snippets.ts>，关键词`getCompressionPrompt`、`systemInstruction`、verification；
- Claude官方文档：<https://platform.claude.com/docs/en/build-with-claude/compaction>，关键词`compact_20260112`和`instructions`。

历史上曾讨论但当时未接受的两个方案（现由ADR 0123关闭，保留为调查脉络）：

```text
方案A：新增CompactionSummaryDirectiveFingerprint
→ 独立hash instruction/scope/format/output limits
→ 加入PromptAssemblyProof和AssembledModelContextFingerprint

方案B：不新增中间fingerprint类型
→ Compaction::build_summary_directive成为唯一constructor
→ Directive字段private，固定template按format_version + scope生成
→ 模板语义变化必须bump CompactionSummaryFormatVersion
→ 最终exact rendered正文由现有AssembledModelContextFingerprint覆盖
→ retry继续复用同一Arc<ModelCallRequest>
```

历史调查当时形成的working hypothesis倾向方案B。ADR 0123随后正式接受“不新增Directive fingerprint/private constructor + format version + 同一Arc request”的方向；旧“必须重新确认A/B”的要求不再适用。

历史上下一步曾需要冻结的问题（已由ADR 0123关闭）：

1. 是否定义`AssembledModelContextFingerprint`覆盖CompactionSummary rendered sections（已删除fingerprint路径，改为同一个`Arc<ModelCallRequest>`）。
2. `CompactionSummaryDirective`字段是否改为private，以及PromptSet需要哪些只读getter（已采用private constructor方向）。
3. instruction正文或summary section语义变化是否强制bump`CompactionSummaryFormatVersion`（已确认不兼容变化递增format version；不使用hash golden vector）。
4. MVP是否确认不支持manual/custom/plugin-provided compact instructions（已确认MVP不支持；未来需求另立ADR）。
5. O14与O15的边界（两者均由ADR 0123关闭）。

历史说明：本轮早期曾按方案B形成临时ADR/文档草案并完成独立cross-document consistency review，随后回退。ADR 0123最终接受了该方向并扩大为全局identity/reload决策；旧“方案A/方案B未定”的描述只作为调查脉络保存。

同步时还修正了直接受影响的旧术语和接口漂移：

- `TurnControlGate`成为Cancel/SecurityRevoked与controlled append/Tool start的唯一first-wins control permit；
- WaitingForUserInput不持有Workspace permit，仍处理SecurityRevoked；
- Compaction、ModelGateway、Tool、Prompt、Skill、Interaction和Runtime Interface统一使用SecurityRevoked语义；
- Migration capture graph恢复为`AgentPromptSelection / SessionPromptSelection`和`ToolCallingCapabilities`；
- Architecture明确Runtime private拥有PromptService、ToolService、SkillService和ModelGateway四个共享深模块。

## 已关闭评审项

| ID | 决议 |
| --- | --- |
| O2 | MVP接受cold load完整线性replay；不建设ProjectionSnapshot/checkpoint index，unfinished Turn保守收口后Idle |
| O4 | ADR 0116：Session-local file mutation queue；跨Session不协调 |
| O5 | ADR 0117：single owner、短guard、typed permit；不建设全局lock rank |
| O6 | ADR 0118：即时CancelAccepted、Finishing结构化收口、FollowUp等待旧Turnterminal |
| O7 | 保持同步Prompt assembly；缺少真实性能数据，不增加额外机制 |
| O8 | ADR 0119：Gateway single attempt，SessionExecutor有限logical retry |
| O9 | ADR 0120：ModelGateway response validation与四个non-retryable error reason |
| O10 | ADR 0121删除revoke-before-commit状态；Idle update失败保留old definition/Snapshot |
| O11 | ADR 0121不承诺dynamic open-handle revocation；SecurityRevoked复用Cancel settlement |
| O12 | ADR 0122曾收窄Workspace/view fingerprint；ADR 0123取代并删除fingerprint族 |
| O13 | ADR 0123：不抽共享pinning/fingerprint value module；由各deep module private immutable interface保证一致性 |
| O14 | ADR 0123：不新增CompactionSummaryDirectiveFingerprint；private constructor + format version + Arc plan/request |
| O15 | ADR 0123：不定义PromptFingerprint；Prompt正文由explicit reload发布的immutable captured content承载 |
| O16 | MVP不增加独立Tool guidelines；Tool User metadata从Direct ToolSpec name/description确定性投影 |

## 下一步

O2/O13/O14/O15/O16已关闭。O1保持开放但从当前工作队列移出；下一轮评审/实现前门禁顺序改为：

```text
1. 第三版AgentLoop评审：L2必须在首个AgentLoop实现前冻结，随后处理L3/L4
2. R2：token估算器owner按ADR0123术语回写（不使用TurnModelFingerprint）
3. wire/schema freeze：serde/casing、public ID生成策略、基础类型；不要恢复ContentHash/fingerprint freeze
4. 阶段6–8：Rig 0.40.0 spike + ScriptedProviderAdapter ordinary AgentRun → ContextOverflow → CompactionSummary → StoredCompaction append/apply → reassemble
5. O1条件门禁：开始首个production Tool/Sandbox adapter前重新激活并关闭Sandbox capability fail-closed
```

其他开放项：`O1 O3 O17 O18`。O1当前延后且不阻塞阶段6–8，但开始production Tool/Sandbox adapter时立即升级为P0门禁；O3属于storage/运维硬化；O17/O18可按真实需求或实现触碰时处理。O2已关闭：MVP cold load完整replay全部complete entries并在conservative recovery后进入Idle，不建设ProjectionSnapshot/checkpoint index。O16已收窄为MVP无独立Tool guidelines。不要重新打开O13/O14/O15，除非新ADR提出超出MVP的durable grant、跨设备execution migration、manual/custom/plugin compaction或adversarial tamper detection需求。

后续实现顺序仍是：

```text
Rig 0.40.0 integration spike
→ ScriptedProviderAdapter ordinary AgentRun
→ ContextOverflow
→ CompactionSummary
→ StoredCompaction append/apply
→ reassemble并继续AgentRun
```

## 已冻结关键决策

- 每个loaded Session只有一个`SessionExecutor`、一个`SessionWriter`、一个current Turn和一个current `RunningOperation`；
- AgentLoop是crate-private同步sans-I/O状态机，只输出`NeedModel | NeedTools | Finished`；
- ModelGateway每次invocation最多一个provider attempt，SDK retry=0；SessionExecutor拥有有限logical retry；
- PromptSet是唯一模型上下文组装seam，模型可见动态事实必须来自committed conversation；
- 文件mutation只在单Session内按canonical file key FIFO，多文件/open-world Tool整批Serial；
- Cancel/SecurityRevoked可以立即关闭新operation，但越过`ToolExecutionStarted`的Tool必须truthful settlement；
- Workspace definition只在Idle更新，active Turn不热替换Snapshot或任何派生view；
- 执行一致性使用exact refs、immutable Arc、explicit reload和structural validation；不定义`*Fingerprint`身份族或任何替代generation/ID；
- Workspace restart/re-resolve不恢复旧Snapshot或authorization-sensitive cache；MVP不保存跨调用Tool grant；
- Snapshot-first是公开观察协议，Snapshot不是durable execution checkpoint。

## 本轮验证

本handoff期望提交前至少执行：

```text
git diff --check
Markdown fenced-code parity
modified Markdown relative-link existence check
O12/O13/O14/O15 fingerprint/hash/reload残留扫描与O16 guidelines扫描
独立semantic/cross-document review
```

本轮是docs-only变更；仓库没有production代码或测试入口，因此通常不运行cargo/test。

O14调查阶段还核对了pi本机安装源码、Codex与Gemini CLI公开源码以及Claude compaction文档。方案B临时草案曾在回退前通过`git diff --check`和独立semantic consistency review；ADR 0123最终接受“不新增Directive fingerprint/private constructor+format version+Arc request”的方向，并扩展为全局identity/reload决策。

## 提交纪律

继续工作前先运行：

```bash
git status --short --branch
git log --oneline -8
```

提交前运行：

```bash
git diff --check
git status --short
```

不要使用`git reset --hard`或撤销不属于当前任务的工作树改动。Markdown变更继续检查相对链接和代码围栏。
