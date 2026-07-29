# V2设计评审工作交接

日期：2026-07-29

用途：在另一台电脑或新的Agent会话中恢复当前工作进度。架构事实仍以`docs/architecture.md`、`docs/modules/`和Accepted ADR为权威；本文只记录评审推进状态。

## 新电脑恢复

仓库尚未clone时：

```bash
git clone https://github.com/zqcli/minicore-runtime.git
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
2. [第二版设计评审与R6收口](v2-design-review-2.md)
3. [第三版AgentLoop设计评审](v2-design-review-3.md)
4. [架构总览与跨模块不变量索引](../architecture.md#跨模块不变量索引)
5. [Conversation与SessionStorage](../modules/conversation-storage.md)
6. [Compaction模块](../modules/compaction.md)
7. [Prompt模块](../modules/prompt.md)
8. [ADR 0119：模型调用使用Session逻辑重试](../adr/0119-model-calls-use-session-logical-retries.md)
9. [ADR 0123：执行一致性使用Exact Ref、不可变快照与显式Reload](../adr/0123-identity-uses-refs-and-explicit-reload.md)
10. [ADR 0124：Session Replay宽容恢复并收窄持久化引用链](../adr/0124-session-replay-is-tolerant-and-links-are-minimal.md)
11. [ADR 0125：ModelGateway不设置本地模型调用Permit](../adr/0125-model-gateway-has-no-local-call-permits.md)

## 当前仓库状态

- 当前分支：`dev`；
- 当前设计基线提交：`af6be9a54f9ec4706ceaaf6d64500e31fa2d5ebd`（`af6be9a docs: simplify model admission and close review R6`）；本handoff存档提交位于其后，只更新恢复信息；
- 换机后`git pull --ff-only origin dev`，预期工作树干净；先用`git log -2 --oneline`确认handoff存档提交和`af6be9a`设计基线均存在；
- 最近已接受决策：ADR 0125删除ModelGateway的Runtime global、per-provider route、per-model与per-auth-principal调用permit；共享Gateway保持跨Session直接并发，provider 429/Retry-After与cooldown仍保留；O18关闭；
- 第二轮评审R1–R6均已关闭；R6通过8条高风险INV索引、canonical owner/link纪律、旧ADR current措辞统一和删除`docs/refactor/`收口；R7与第一轮C3/O1是同一条件性Sandbox门禁；
- O14/O15不再是当前进行中的未决issue；其历史调查记录已被ADR 0123的方案B式决策取代（不新增Directive/Prompt fingerprint，使用private constructor、immutable content和explicit reload）；
- 当前恢复链上的关键提交为`af6be9a`（ADR 0125 + R6关闭）、`7b42648`（ADR 0124）、`4a3fd24`（ADR 0123收敛）、`e6966a0`（O1延后）和`76148ab`（O2关闭）；
- 本文继续作为跨机器恢复入口。新环境先检查远端与最新log，不要reset用户改动；
- 仓库仍处于V2设计阶段，没有`Cargo.toml`、`src/`或自动化测试；
- 下一实现里程碑仍是阶段6–8模型调用协同交付束；
- 当前第一项工作仍是第三版AgentLoop评审L2；`accept_committed_tool_round`已因ADR 0124改名并收窄为`accept_committed_tool_results(CommittedToolExchangeDelta)`，但`next_action()` one-shot emission/重复poll/typed error仍未冻结；O1不在当前工作队列；
- Rig只实现`ModelGateway` private `ProviderAdapter`中的单次provider attempt，不拥有ModelGateway或AgentLoop。

## 最近进度存档

```text
af6be9a docs: simplify model admission and close review R6
→ ADR 0125删除ModelGateway全部本地模型调用permit/admission queue
→ 共享Arc<ModelGateway>支持多Session直接并发provider attempt
→ O18关闭；provider 429/Retry-After/cooldown继续保留
→ R6建立INV-001/002/003、INV-101/102、INV-201、INV-301、INV-401 canonical owner索引后关闭
→ 删除docs/refactor/完整重复目录，横切ADR使用owner/link + rg残留扫描纪律

7b42648 docs: adopt tolerant session replay design
→ ADR 0124采用live strict / cold replay tolerant
→ 删除durable ToolExecutionStarted、ToolRoundCompleted和大部分proof chain
→ complete Tool exchange按matching results自动形成，Compaction使用single marker，Fork保留历史ID

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

换机后不要重新调查O1/O2/O3/O17、O18或第二轮R1–R6。直接从[第三版AgentLoop设计评审](v2-design-review-3.md)的L2继续；L3/L4已经按ADR 0124的新typed Tool exchange与Steer delta术语关闭。随后执行wire/schema freeze和Rig 0.40.0 spike。需要核对全局状态时回到本文“下一步”和“已冻结关键决策”。

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
- durable Agent/Session definitions继续使用exact revisions；ADR 0124后ledger不保存WorkspaceRevision/ModelDefinitionVersion execution binding，domain correlation继续使用`SessionId`、`TurnId`、`ItemId`、`RequestId`、`EntryId`和`ToolCallId`；
- active Turn持有同一组immutable `Arc<WorkspaceSnapshot>`、`Arc<PromptResourceView>`、`Arc<SkillView>`、`Arc<ToolSet>`、`Arc<PromptSet>`和`Arc<TurnModelSnapshot>`；private constructors阻止跨capture拼接任意view；
- Prompt/Skill/Tool/Model资源只在Runtime初始化或显式`/reload`后替换current immutable object；watcher最多标记dirty，不自动publication；active Turn继续使用old captured objects；
- `/reload`对Prompt/Skill/Tool/Model执行two-phase流程：各module只build/validate candidate，Runtime在短publication gate内整体替换private `SharedResourceRoots`（PromptResourceView、SkillResourceView、ToolResourceView、ModelCatalogView）；该bundle没有ID/version/generation，任一required candidate失败时保留完整old roots。`/reload workspace`继续要求Session Idle，非Idle返回`SessionBusy`；
- shared Prompt/Skill filesystem source在Runtime initialize或shared `/reload`时捕获；Workspace-bound Prompt/Skill source在Session load、Idle definition update或`/reload workspace`时捕获并随WorkspaceSnapshot发布；Skill可以lazy parse captured bytes，不能在Turn内按path重新读取current file；
- Tool approval在MVP只支持per-call `AllowOnce/AllowWith`，不保存`ToolGrantStore`、Turn grant或Session grant；
- logical retry复用同一个`Arc<ModelCallRequest>`，只验证Turn仍Running、`execution_version`、exact `ConversationCheckpoint.entry_id`、`current_operation`仍为持有该request的对应retry slot且control basis未变；不重新assemble，也不比较context摘要；
- Compaction operation持有同一个`Arc<CompactionPlan>`与由其组装出的同一个`Arc<ModelCallRequest>`，exact rendered directive随request固定；ADR 0124后plan使用continuous prefix、recent suffix和single marker，不再使用scope/boundary/provenance；
- ModelGateway validated model definition拥有`TokenEstimateRate`，active Turn通过`TurnModelSnapshot::token_estimator()`把同一个确定性estimator分发给PromptSet和Compaction；estimate不进入ModelUsage；
- Input UserMessage内联safe StoredTurnStart；StoredCompaction只保存summary、`first_kept_entry_id`和optional model-call metadata，不保存fingerprint/hash或旧execution ref。

## ADR 0124收口

本轮新增[ADR 0124](../adr/0124-session-replay-is-tolerant-and-links-are-minimal.md)，接受向同类Agent CLI靠拢的宽松durable基线：

- 核心identity继续保留：AgentId、SessionId、TurnId、ItemId、RequestId、EntryId/parent_id、ToolCallId；CommandId仍只在当前Runtime使用；
- live append保持strict；cold replay跳过malformed/unknown/duplicate EntryId记录，missing parent形成orphan root，invalid relation只影响对应projection并返回bounded diagnostics；duplicate ToolResult采用first valid wins；
- 删除独立TurnContext entry，Input UserMessage内联StoredTurnStart safe metadata；durable history不保存WorkspaceSnapshotRef或ModelDefinitionVersion execution binding；
- 删除durable ToolExecutionStarted；Tool start使用SessionExecutor owner-local ToolStartPermit/ToolOperationSlot；
- 删除ToolRoundCompleted；同一assistant全部calls的first terminal outcome均为matching ToolResult时，由最后一个result产生CommittedToolExchangeDelta；incomplete/abandoned-first exchange不进入model conversation；
- StoredCompaction收窄为rolling summary + first_kept_entry_id + optional model-call provenance；删除scope、protected entries、previous checkpoint和coverage provenance；
- Fork复制selected path并保留历史Entry/Turn/Item/Request/ToolCall IDs，只分配new SessionId；Entry/Turn/Item/Request ID为Session-scoped，adapter-normalized ToolCallId只要求单assistant response内唯一；
- MVP不提供repair utility，O3关闭；Prompt committed-only改为by-construction，O17关闭。

ADR 0124部分取代ADR 0104/0109/0111/0113/0115/0117/0118/0121/0123，并完全取代ADR 0112的active-Turn checkpoint形状。旧ADR正文保留历史脉络，状态和顶部修订说明指向0124。

## ADR 0125收口

本轮新增[ADR 0125](../adr/0125-model-gateway-has-no-local-call-permits.md)，删除ModelGateway本地模型调用admission：

- 删除Runtime global、per-provider route、per-model和per-auth-principal四级permit，以及`ModelConcurrencyController`、FIFO/no-starvation queue和`ModelSchedulingClass`；
- 共享`Arc<ModelGateway>`支持多个Session直接并发进入独立provider attempt，实现不得持有Gateway-wide长guard跨credential/provider I/O；
- 每Session最多一个current model `RunningOperation`保持不变，但不形成跨Session串行；
- provider `RateLimited`、`QuotaExceeded`、typed `Retry-After`和route/principal cooldown继续保留，由SessionExecutor按ADR 0119裁决logical retry；
- O18因permit-wait触发前提被删除而关闭；只有未来重新引入明确admission queue并出现真实SLO证据时才重开。

## 第二轮评审状态

[第二轮设计评审](v2-design-review-2.md)已按当前权威文档重新登记：

- R1 Submit admission、R2 token estimator owner、R3 Runtime字段/四个共享模块、R4 migration capture副本、R5 CONTEXT旧资源术语均已关闭；
- R6已关闭：`architecture.md`索引INV-001/002/003、INV-101/102、INV-201、INV-301和INV-401，各自链接唯一canonical owner；非owner文档只保留本地职责摘要与链接；
- `docs/refactor/`已删除；横切ADR固定按canonical owner、interface消费者、review/handoff、archive顺序回写，并以`rg`扫描被删除的旧术语；不建设覆盖所有普通约束的全局不变量数据库；
- R7与第一轮C3/O1相同，继续延后到首个production Tool/Sandbox adapter前关闭；
- 第二轮非阻塞项中，协议完备性、wire/schema freeze、Gemini枚举和CompactionSettings来源仍需后续决议。

## O14调查存档（历史，已由ADR 0123关闭）

O14曾是`CompactionSummaryDirective`正文与fingerprint coverage是否需要独立冻结的问题。以下保留为历史调查记录，不再是当前待办。ADR 0123决定不新增`CompactionSummaryDirectiveFingerprint`，使用private constructor、format version和同一个immutable request；ADR 0124随后删除scope/boundary/provenance，但不重新打开O14。当前Directive合同见[Compaction](../modules/compaction.md)。

```rust
pub struct CompactionSummaryDirective {
    format_version: CompactionSummaryFormatVersion,
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
| O3 | ADR 0124：live append strict、cold replay tolerant；MVP不建设repair utility |
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
| O17 | Prompt只接收sanitized CommittedConversationView；未提交draft/incomplete Tool exchange无法构造输入 |
| O18 | ADR 0125：删除ModelGateway本地调用permit/admission queue；多Session直接并发进入provider attempt |

## 下一步

O2/O3/O13/O14/O15/O16/O17已关闭。O1保持开放但从当前工作队列移出；下一轮评审/实现前门禁顺序改为：

```text
1. 第三版AgentLoop评审：L2必须在首个AgentLoop实现前冻结；L3/L4均已关闭
2. wire/schema freeze：serde/casing、MiniCore-generated Session-scoped public ID策略、ToolCallId opaque wire格式、基础类型、StoredTurnStart/StoredCompaction schema
3. 阶段6–8：Rig 0.40.0 spike + ScriptedProviderAdapter ordinary AgentRun → complete Tool exchange → ContextOverflow → single-marker CompactionSummary → append/apply → reassemble
4. O1条件门禁：开始首个production Tool/Sandbox adapter前重新激活并关闭Sandbox capability fail-closed
```

第一轮评审的O项只剩`O1`。O1当前延后且不阻塞阶段6–8，但开始production Tool/Sandbox adapter时立即升级为P0门禁；O18已由ADR 0125删除permit-wait触发前提并关闭。O2/O3/O17均已关闭：cold load保持O(n)扫描但采用tolerant replay，不建设ProjectionSnapshot/checkpoint index或repair utility；Prompt只消费sanitized committed view。不要重新打开O13/O14/O15，除非新ADR提出超出MVP的durable grant、跨设备execution migration、manual/custom/plugin compaction或adversarial tamper detection需求。

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
- Cancel/SecurityRevoked可以立即关闭新operation，但已取得ToolStartPermit并进入Running/Settling的Tool必须truthful settlement；
- Workspace definition只在Idle更新，active Turn不热替换Snapshot或任何派生view；
- live执行一致性使用exact refs、immutable Arc和explicit reload；durable history只保存safe StoredTurnStart/StoredModelDescriptor，不把old refs当成restart execution checkpoint；
- Workspace restart/re-resolve不恢复旧Snapshot或authorization-sensitive cache；MVP不保存跨调用Tool grant；
- SessionStorage live append strict、cold replay tolerant；complete Tool exchange按matching results自动形成，Compaction使用single marker，Fork保留历史ID；
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
