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
3. [ADR 0122：Workspace fingerprint只在当前Runtime有效](../adr/0122-workspace-fingerprints-are-runtime-local.md)
4. [ADR 0121：Workspace定义只在Idle更新](../adr/0121-workspace-updates-require-idle.md)
5. [架构总览](../architecture.md)
6. [Workspace模块](../modules/workspace.md)
7. [Session Execution模块](../modules/session-execution.md)

## 当前仓库状态

- 当前分支：`dev`；
- 最新决策：ADR 0122关闭O12；实际提交hash以`git log --oneline -8`首行为准；
- 前一项Workspace设计提交：`9aea732 docs: require idle for workspace updates`；
- 本轮完成后应已push到`origin/dev`；若本地显示ahead/behind，先检查远端与最新log，不要reset用户改动；
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

从O14继续，冻结CompactionSummaryDirective正文的fingerprint coverage，然后处理Prompt正文identity：

```text
O14 CompactionSummaryDirective fingerprint coverage
→ O15 Prompt正文与PromptFingerprint
```

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
