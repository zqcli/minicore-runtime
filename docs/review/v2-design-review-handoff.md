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
7. [ADR 0122：Workspace fingerprint只在当前Runtime有效](../adr/0122-workspace-fingerprints-are-runtime-local.md)
8. [架构总览](../architecture.md)

## 当前仓库状态

- 当前分支：`dev`；
- 最新已接受决策：ADR 0122关闭O12；提交为`ca2b02e docs: make workspace fingerprints runtime-local`；
- O14仍是当前进行中的未决issue；本轮只完成调查和候选方案评估，没有修改权威Compaction/Prompt设计，没有创建或接受ADR 0123；
- 前一项Workspace设计提交：`9aea732 docs: require idle for workspace updates`；
- 发布条件：本handoff提交必须与`ca2b02e`一并push到`origin/dev`后，本文才作为跨机器恢复入口；本轮结束前执行该push。若新环境pull后缺少本文，先检查远端与最新log，不要reset用户改动；
- 仓库仍处于V2设计阶段，没有`Cargo.toml`、`src/`或自动化测试；
- 下一实现里程碑仍是阶段6–8模型调用协同交付束；
- Rig只实现`ModelGateway` private `ProviderAdapter`中的单次provider attempt，不拥有ModelGateway或AgentLoop。

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
- O10和O11关闭；O1 Sandbox fail-closed保持开放。O12由ADR 0122以放弃跨Runtime Workspace fingerprint恢复关闭。

本轮新增[ADR 0122](../adr/0122-workspace-fingerprints-are-runtime-local.md)，关闭O12：

- durable Agent/Session definition与conversation history继续保留；
- loaded Session、WorkspaceSnapshot、各view fingerprint、Tool grant、authorization cache和旧execution Context不跨Runtime恢复；
- load/reload/re-resolve按current definition/current authority创建新的fingerprint family；historical fingerprint只作opaque diagnostic；
- restart后的unfinished Turn保守关闭，MVP不提供exact same-Turn resume；
- 删除Workspace fingerprint canonical encoding、algorithm version和golden-vector要求。

## 当前进行中：O14调查存档

O14是`CompactionSummaryDirective`正文与fingerprint coverage是否需要独立冻结的问题，当前仍开放。权威文档仍保持原设计：

```rust
pub struct CompactionSummaryDirective {
    pub format_version: CompactionSummaryFormatVersion,
    pub scope: CompactionSummaryScope,
    pub instruction: Arc<str>,
    pub max_output_tokens: NonZeroU32,
    pub max_summary_bytes: usize,
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

已讨论但尚未接受的两个方案：

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

当前调查形成的working hypothesis倾向方案B：它更符合MVP、single producer和deep module原则，也与同类产品持有完整请求的实现接近。该倾向不具有决策权重，不能作为实现依据；接手者必须重新根据O14的真实需求和下列冻结问题正式确认A/B。

下一步需要冻结的具体问题：

1. `AssembledModelContextFingerprint`对CompactionSummary variant是否覆盖exact rendered System/User sections、directive正文、source、purpose和NoToolCalls。
2. `CompactionSummaryDirective`字段是否改为private，以及PromptSet需要哪些只读getter。
3. instruction正文或summary section语义变化是否强制bump`CompactionSummaryFormatVersion`，测试使用rendered snapshot还是hash golden vector。
4. MVP是否确认不支持manual/custom/plugin-provided compact instructions；若未来需要，在哪个版本重新评估专用Directive identity。
5. O14与O15的边界：O14只处理CompactionSummary请求；O15继续处理ordinary Runtime/Agent/Session Prompt正文、`PromptFingerprint`、cache和DefinitionVersion。

本轮曾按方案B形成临时ADR/文档草案并完成独立cross-document consistency review；该review只证明草案内部一致，不证明方案B优于方案A。用户明确要求O14保持未决后，草案已完整回退，未进入git history。

同步时还修正了直接受影响的旧术语和接口漂移：

- `TurnControlGate`成为Cancel/SecurityRevoked与controlled append/Tool start的唯一first-wins control permit；
- WaitingForUserInput不持有Workspace permit，仍处理SecurityRevoked；
- Compaction、ModelGateway、Tool、Prompt、Skill、Interaction和Runtime Interface统一使用SecurityRevoked语义；
- Migration capture graph恢复为`AgentPromptSelection / SessionPromptSelection`和`ToolCallingCapabilities`；
- Architecture明确Runtime private拥有PromptService、ToolService、SkillService和ModelGateway四个共享深模块。

## 已关闭评审项

| ID | 决议 |
| --- | --- |
| O4 | ADR 0116：Session-local file mutation queue；跨Session不协调 |
| O5 | ADR 0117：single owner、短guard、typed permit；不建设全局lock rank |
| O6 | ADR 0118：即时CancelAccepted、Finishing结构化收口、FollowUp等待旧Turnterminal |
| O7 | 保持同步Prompt assembly；缺少真实性能数据，不增加额外机制 |
| O8 | ADR 0119：Gateway single attempt，SessionExecutor有限logical retry |
| O9 | ADR 0120：ModelGateway response validation与四个non-retryable error reason |
| O10 | ADR 0121删除revoke-before-commit状态；Idle update失败保留old definition/Snapshot |
| O11 | ADR 0121不承诺dynamic open-handle revocation；SecurityRevoked复用Cancel settlement |
| O12 | ADR 0122：Workspace/view fingerprint仅当前Runtime有效；restart重新resolve且不恢复grant/cache |

## 下一步

从O14继续，先在方案A与方案B之间作正式决策，再处理Prompt正文identity：

```text
O14 确认是否新增Directive fingerprint
→ 冻结Directive constructor/template/version/assembled coverage
→ O15 Prompt正文与PromptFingerprint
```

接手后的第一项操作是执行以下检查，再按上面的5个冻结问题重新评估方案A/B：

```bash
git log --oneline -3
git log -1 --oneline -- docs/review/v2-design-review-handoff.md
git log --all --oneline --grep='ADR 0123\|deterministic template'
test ! -e docs/adr/0123-compaction-directive-uses-deterministic-template.md
rg '^\| O14 ' docs/review/v2-design-review.md
rg -n 'pub instruction: Arc<str>|fingerprinted.*CompactionSummaryDirective' docs/modules/compaction.md
```

预期结果：log包含`ca2b02e`和本文handoff提交；ADR 0123搜索为空且文件不存在；O14仍显示“部分开放”；Compaction模块仍保留原始public `instruction`与fingerprinted Directive表述，说明方案A/B尚未写入权威设计。

其他开放项：`O1 O2 O3 O13 O16 O17 O18`。其中O1是首个production Tool/Sandbox adapter前的P0门禁；O2/O3属于storage/运维硬化；O13不要重新引入authorization lease，只评估真正同构的source stamp/pinning value。

第三版AgentLoop评审仍有`L2–L4`开放；`L2`必须在首个AgentLoop实现前冻结。完成O14/O15后，应回到[第三版评审](v2-design-review-3.md)核对实现前门禁。

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
- Workspace及其view fingerprint只在当前Runtime有效；restart/re-resolve不恢复旧Snapshot、grant或cache；
- Snapshot-first是公开观察协议，Snapshot不是durable execution checkpoint。

## 本轮验证

已执行并通过：

```text
git diff --check
Markdown fenced-code parity
modified Markdown relative-link existence check
O12/cold-resume/fingerprint-recovery残留扫描
独立semantic/cross-document review
```

本轮是docs-only变更；仓库没有production代码或测试入口，因此未运行cargo/test。独立review发现的条件式same-Turn cold resume残留、authority revision reference未落Schema和stable fingerprint歧义均已修正。

O14调查阶段还核对了pi本机安装源码、Codex与Gemini CLI公开源码以及Claude compaction文档。方案B临时草案已在回退前通过`git diff --check`和独立semantic consistency review；当前handoff只存档调查结果，不代表O14关闭，也不表示方案B已经获得架构批准。

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
