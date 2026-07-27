# V2设计评审工作交接

日期：2026-07-27

用途：在另一台电脑或新的Agent会话中恢复当前工作进度。架构事实仍以`docs/architecture.md`、`docs/modules/`和Accepted ADR为权威；本文只记录评审推进状态。

## 恢复入口

```bash
git switch dev
git pull --ff-only origin dev
git log --oneline -8
```

随后阅读：

1. [第一版设计评审与开放项跟进](v2-design-review.md)
2. [第二版设计评审](v2-design-review-2.md)
3. [第三版AgentLoop设计评审](v2-design-review-3.md)
4. [架构总览](../architecture.md)

## 当前仓库状态

- 当前分支：`dev`；
- 仓库仍处于V2设计冻结阶段，生产实现尚未启动；
- 当前没有`Cargo.toml`、`src/`或自动化测试；
- 下一实现里程碑仍是阶段6–8模型调用协同交付束；
- Rig只实现`ModelGateway` private `ProviderAdapter`中的单次provider attempt，不拥有ModelGateway或AgentLoop。

## 最近完成

- `c8bf700`：新增第三版AgentLoop设计评审；
- `32f0841`：统一Rig provider adapter职责；
- `5e21993`：通过ADR 0116/0117关闭O4文件mutation并发和O5异步同步纪律；
- `60ea813`：通过ADR 0118关闭O6，Cancel立即返回`CancelAccepted`，Finishing期间FollowUp排队，旧Turn结构化收口后再启动下一Turn；
- 本轮关闭O7：同步Prompt assembly是纯内存线性操作，1000条约1 MB消息预计约1–30 ms；保持当前同步实现，不增加offload、work budget、counter或observer。

## 已关闭评审项

| ID | 决议 |
| --- | --- |
| O4 | ADR 0116：Session-local file mutation queue；跨Session不协调 |
| O5 | ADR 0117：single owner、短guard、typed permit；不建设全局lock rank |
| O6 | ADR 0118：即时CancelAccepted、Finishing结构化收口、FollowUp等待旧Turnterminal |
| O7 | 保持同步Prompt assembly；缺少真实性能数据，不增加额外机制 |

## 下一步

从O8继续：Gateway transparent retry与SessionExecutor logical retry目前各自有局部上限，可能形成attempt、backoff和总耗时相乘。需要结合pi、Codex、Gemini CLI、OpenHands等实现，判断是否需要Turn-scoped共享`ModelCallBudget`，以及该预算由谁拥有、如何跨Gateway与Executor扣减。

O8完成后依次处理：

```text
O9  provider输出违约分类与retry语义
O11 open-handle revocation窗口
O12 Workspace/view fingerprint恢复策略
O14 CompactionSummaryDirective fingerprint coverage
O15 Prompt正文与PromptFingerprint
```

其他仍开放项：O1、O2、O3、O10、O13、O16、O17、O18。状态与优先级以`v2-design-review.md`的“当前问题跟进总览”为准。

## 已冻结关键决策

- 每个loaded Session只有一个`SessionExecutor`、一个`SessionWriter`、一个current Turn和一个current `RunningOperation`；
- AgentLoop是crate-private同步sans-I/O状态机，只输出`NeedModel | NeedTools | Finished`；
- ModelGateway拥有model resolution、credential、retry/fallback、attempt lifecycle和provider-neutral terminal result；
- 文件mutation只在单Session内按canonical file key FIFO，多文件/open-world Tool整批Serial；
- Cancel可以立即确认业务请求，但已开始write/process/remote Tool必须truthful settlement；
- Cancel后同Session不立即启动第二Turn，新输入进入FollowUp；
- Prompt assembly保持同步；当前不为理论性能风险增加异步operation或观测设施。

## 提交前检查惯例

```bash
git diff --check
git status --short
```

Markdown变更还需检查相对链接；提交时只暂存当前评审相关文件，不撤销工作树中可能存在的用户改动。