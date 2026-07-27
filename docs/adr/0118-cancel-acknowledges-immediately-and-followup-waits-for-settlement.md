# ADR 0118：Cancel立即确认，FollowUp等待结构化收口后启动

状态：Accepted
日期：2026-07-27

## 背景

原Cancel协议把command response与Turn terminal settlement绑定：只有已开始Tool得到exact outcome或进入Abandoned、`TurnInterrupted` append/apply后才返回。对于write、process和远程服务器Tool，结构化清理是必要的；pi会等待并行Tool Promise与文件I/O settle，Bash会kill process tree后等待child exit；Codex对普通Tool丢弃业务结果，但shell等Tool显式等待runtime cancellation teardown。

用户需要在发出Cancel后立即知道请求已经被接受，并继续输入后续意图。该需求不要求同一Session立即启动第二个Turn：旧write/process仍可能运行，立即启动会破坏单current Turn、单RunningOperation、ToolCall/ToolResult顺序和file mutation queue ownership。后续输入可以先进入FollowUp，旧Turn完成结构化收口后自动admit新Turn。

## 决策

1. **Cancel command在线性化sticky cancel epoch后立即返回**：

   ```rust
   CommandOutcome::CancelAccepted {
       target: PublicCancelTarget,
       cancel_epoch: u64,
   }
   ```

   该response只表示target/generation仍处于可取消状态且取消信号已经发布，不表示Turn已经terminal，也不承诺外部副作用已回滚。
2. **duplicate Cancel立即返回同一accepted cancel epoch**。stale Submit CommandId、TurnId或generation返回既有typed stale/terminal error，不能影响current或future Turn。`EmergencyControl`不再保存Cancel completion subscribers；它只保存O(1) active target、generation、sticky epoch与token。
3. **Cancel立即停止逻辑推进**。signal发布后触发current operation cancellation token；Executor观察后递增`execution_version`、进入`SessionExecutionState::Finishing`、立即发布snapshot/`session_execution_changed`，不再启动新的Model、Tool、Compaction或Steer。迟到Model/Context/Compaction结果按stale result丢弃。
4. **`Finishing`是公开的停止/收口状态**。不新增`TurnExecutionPhase::Cancelling`；当`SessionExecutionState = Finishing`时，UI必须优先显示Stopping/Finishing，current Turn phase只保留为最后工作位置的diagnostic。
5. **Tool取消采用结构化收口**：
   - 尚未越过`ToolExecutionStarted`：确认未执行后生成truthful Cancelled ToolResult；
   - write/edit：等待已提交给filesystem的I/O settle后再释放Session-local mutation permit；
   - process/shell：发送kill/cancel并等待本地process teardown；
   - remote/server Tool：发送协议支持的cancel；请求可能已提交且无法确认结果时形成`ToolAbandoned(outcome unknown)`；
   - 取消后的业务结果不能继续Turn，只能用于exact ToolResult、Abandoned判定、audit和资源回收。
6. **普通Tool future不能静默脱离owner**。Turn terminal前，child operation必须完成、被安全drop且结果路径关闭，或把process/connection/permit完整转交给明确的ToolService supervisor。MVP不建立通用detached Tool supervisor，因此内建Tool必须提供有界、可确认的本地cleanup路径。
7. **同一Session不立即启动第二个Turn**。旧Turn在`TurnInterrupted`或其他terminal entry append/apply前仍是唯一current Turn；普通Submit在Finishing返回`SessionBusy`。
8. **Finishing期间允许FollowUp**。Session未进入Unload stop-admission时，FollowUp可进入现有bounded process-local FIFO并立即返回`FollowUpQueued`；它不属于旧Turn，不触发新的Model/Tool，也不等待在Cancel command response中。旧Turnterminal后按现有FollowUp/Submit公平admission规则开启下一Turn。
9. **Steer在Cancel后拒绝**。cancel epoch发布后新的同TurnSteer返回`TurnCancelling/TurnNotRunning`；epoch前已accepted但未append的Steer被清理。FollowUp保持独立，不随单Turn Cancel清除。
10. **Cancel与commit reservation采用first-wins**。对Submit target，cancel epoch先赢则initiating append reservation失败且不会创建Turn；initiating append reservation先赢则Cancel(Submit)返回typed stale/transition error，原Submit完成并返回TurnStarted。对Turn target，cancel epoch先赢则final append reservation失败；final append reservation先赢则Cancel(Turn)返回typed terminal/transition error，不能返回`CancelAccepted`后再把Turn提交为Completed。reservation只覆盖既有短append/apply边界，不等待长operation。
11. **最终完成由StateEvent表达**。Tool settlement完成后append/apply `TurnInterrupted`并发布`turn_interrupted`；若没有马上admit的FollowUp/Submit，进入Idle后再发布`session_settled`。host通过subscription或新Snapshot观察最终事实。Cancel command transport lifetime不再绑定side-effect settlement lifetime。

## 理由

- Cancel acceptance与physical cleanup是两个不同线性化点。即时typed response满足交互反馈，terminal event保持durable truth。
- structured concurrency要求parent结束前回收或明确转交child operation。仅丢弃await结果无法停止filesystem write、process或已提交的远程请求。
- FollowUp允许用户继续表达意图，同时维持一个Session一个current Turn和一个current RunningOperation的既有深模块边界。
- 复用`SessionExecutionState::Finishing`已经足够；新增Cancelling phase会与failure、revocation和terminal cleanup产生重叠状态。

## 后果

- UI收到`CancelAccepted`后可以立即恢复输入，并将新输入路由为FollowUp；同Session执行仍等待旧Turnterminal后继续。
- Cancel command成功不等于外部副作用停止；UI可显示Stopping，最终结果以`turn_interrupted`、ToolResult/ToolAbandoned和Snapshot为准。
- `EmergencyControl`移除Cancel completion subscriber集合，状态更小；PrepareForUnload仍保留自己的shared completion generation。
- O6通过复用Finishing、即时CancelAccepted与FollowUp queue关闭。

## 测试要求

- valid Turn Cancel在sticky epoch发布后立即返回`CancelAccepted`，不等待started Tool；
- duplicate Cancel返回相同target/epoch，不创建completion waiter；
- Cancel(Submit)与initiating append reservation first-wins：accepted Cancel不创建Turn，reservation先赢时Cancel不返回accepted；
- Cancel(Turn)与final append reservation first-wins：accepted Cancel不允许final Assistant commit，reservation先赢时Cancel不返回accepted；
- CancelAccepted后立即发布Finishing snapshot/event，迟到Model/Context结果不进入projection；
- Finishing期间Steer拒绝、FollowUp可Queued、Submit返回SessionBusy；
- write取消不在filesystem I/O settle前释放mutation permit；
- shell取消kill process tree并等待本地teardown；
- remote outcome无法确认时形成ToolAbandoned，再append TurnInterrupted；
- TurnInterrupted前不启动FollowUp Turn，terminal后按公平admission启动；只有没有马上admit的work时发布session_settled；
- restart不恢复Cancel waiter或FollowUp，按committed prefix执行既有conservative recovery。

## 修订关系

本ADR修订[ADR 0111](0111-session-ingress-separates-control-and-work-lanes.md)的Cancel shared completion generation、`runtime-interface.md`的Cancel response线性化点和`session-execution.md`的Cancel流程，并关闭`docs/review/v2-design-review.md`的O6。Tool truthfulness、单SessionExecutor/Writer owner、一个current Turn和FollowUp process-local语义保持不变。