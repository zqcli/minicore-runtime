# ADR 0109: Prompt、Projection 与 Session Operation 使用确定性规则

状态：Accepted
日期：2026-07-24

## 背景

V2设计评审B1/B2/B3指出三个实现前置问题：Prompt同层排序未冻结；writer append与cold replay缺少共享semantic contract；logical retry复用execution version时，若旧本地future与新operation重叠，结果身份不足。

同类项目提供了更简单的基线。Codex与Claude Code使用固定instruction层级，而不是任意priority；event-sourcing实现通常让live apply与replay使用同一reducer；pi和Codex在同一Agent/Turn内串行等待模型step，并在完整assistant/tool step后注入queued input。

MiniCore不需要为这些问题增加priority system、queue service或attempt entity。

## 决策

1. Prompt baseline采用固定顺序：Runtime required System policy → Runtime base System Prompt → Agent System instructions → Session User instructions → Workspace User instructions → ToolPromptView metadata → SkillPromptView User metadata。Prompt role只保留System和User，完整资源与reload规则见ADR 0110。
2. 不给PromptDefinition增加priority。Runtime/Agent/Session PromptDefinition层按PromptKey、PromptId、DefinitionVersion和稳定provenance source identity排序；Workspace按model-safe relative path，Tool按ToolName，Skill按SkillId排序。filesystem/discovery/HashMap顺序不得影响结果；PromptDefinition层内重复PromptKey返回DuplicateKey并fail closed。
3. SessionWriter append与cold replay共用一个pure `validate_and_project(base, entry)` semantic seam。append-time semantic validation必须等价于或强于replay validation；writer成功commit的entry必须可被projector语义接受。
4. `apply_committed`只安装append前生成的trusted delta，不能对已commit entry再次产生确定性semantic rejection。writer-accepted sequence必须通过live apply与cold replay projection等价性测试。
5. 每个Session最多一个current RunningOperation。主循环同时poll该future、deadline和SessionIngress wakeup以保持响应，但旧operation terminal/remove或安全drop并关闭结果路径前，不启动logical retry或下一operation。SessionIngress的lane划分由ADR 0111修订，不改变本条的单operation约束。
6. execution_version表示conversation/control basis，不是retry attempt编号。logical retry复用相同version和ModelCallRequest，但只能严格串行启动。provider端可能继续工作或计费不等于旧本地future仍可向SessionExecutor返回结果。
7. Steer与FollowUp分别使用`SessionIngress`中的bounded per-Turn `SteerQueue`和`FollowUpQueue`；lane内部保留普通FIFO语义，不增加priority、batch drain mode或独立状态owner。仍在队列中的消息可以按CommandId remove，撤销后不重新入队。具体ingress形状由ADR 0111修订。
8. Steer不取消Sampling、Compaction、WaitingApproval、WaitingForUserInput或ExecutingTools。当前assistant/tool step完整committed后，下一次Model调用前pop_front一条Steer并append/apply为UserMessage；等待UserQuestion时Steer只排队，不作为UserAnswer。
9. 含ToolCall的step必须先完成assistant → truthful ToolResult → tool_round_completed，再加入Steer。无ToolCall candidate final遇到queued Steer时保存为model-visible、non-terminal Assistant Continue step；queue为空时才保存Assistant Final并terminalize Turn。
10. FollowUp只在current Turn terminal后pop_front一条，重新capture TurnExecutionContext并开启新Turn。

## 后果

- PromptFingerprint不再依赖未定义priority或source发现顺序。
- 已commit JSONL不会因append/replay validator分叉而在冷启动时被同版本代码拒绝。
- Session控制请求保持响应，但logical Model/Tool/Compaction work严格串行，B3不需要operation_instance_id作为正确性前提。
- Steer FIFO与tool protocol顺序固定为`assistant tool_call → truthful tool result(s) → tool_round_completed → user steer → next model`。
- 为保存无ToolCall response后继续同一Turn，Assistant Intermediate允许一个无ToolCall Continue形态；它在append/apply时model-visible但不terminalize Turn。
- 队列保持普通容器语义；未来只有出现真实容量、去重或调度需求时才提取wrapper。

## 关闭的评审问题

- B1：Prompt scope内priority/冲突排序未定。
- B2：缺append校验覆盖replay校验与committed entry必可project不变量。
- B3：logical retry可能与旧本地operation重叠，结果坐标不足。
