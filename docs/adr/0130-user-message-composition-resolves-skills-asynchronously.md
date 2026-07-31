# ADR 0130：用户消息Composition异步解析Captured Skill

状态：Accepted
日期：2026-07-31

## 背景

ADR 0128/0129已经要求Prompt正文在publication前materialize，并要求Skill/Workspace contribution在live UserMessage apply前完成exact authorization。current module仍把`TurnExecutionContext::compose_input/compose_steer`写成同步方法，而`SkillService::load()`是异步lazy parse；Context也没有可调用的SkillService handle。把load塞入PromptSet会破坏同步、确定、纯内存的唯一assembly seam；改成Turn内读取current Skill path又会破坏immutable capture和explicit reload。

Submit的整个Starting阶段还必须可由`Cancel(Submit CommandId)`或SecurityRevoked终止。async Skill load若没有owner-local control与await后重验，会形成迟到composition result仍可apply或task spawn的窗口。

## 决策

1. `TurnExecutionContext::resolve_user_message(PromptIntent)`是Input与Steer共享的唯一async composition seam。它先同步拒绝duplicate SkillId，再按intent顺序从本Turn captured SkillView取得entry，await SkillService lazy parse，经SkillInjector产生typed contributions，最后调用同步`PromptSet::compose_user_message()`返回`CanonicalUserMessage`。
2. TurnExecutionContext私有持有同一次capture得到的`Arc<SkillService>`、`Arc<SkillViewContext>`和`Arc<SkillView>`。SkillView私有绑定创建它的exact SkillViewContext；SkillService load接收该view与其中的entry，并验证entry membership、captured source authorization和provenance。调用方不能把future/current view或任意entry拼入旧Context。
3. `SkillService::load()`只解析entry已经捕获的bytes或读取等价content cache；不查询current shared root、future SkillView或filesystem path。drop一个load waiter是cancellation-safe的：shared parse/cache工作可以继续，但不会向已取消的composition发布LoadedSkill或live mutation。
4. `PromptSet::compose_user_message()`保持同步、确定、纯内存。PromptSet不持有SkillService，不执行Skill/Workspace source I/O，也不接收未完成authorization的contribution。required contribution任一失败时整条message失败，不apply部分body或Skill。
5. SessionExecutor在接受Submit后先安装`CommandId + TurnId + control_generation + observed emergency epoch`的Starting candidate target，再创建并pin capture/composition future。Starting subloop使用`select`同时处理该future、Cancel/SecurityRevoked和Lifecycle signal；它不持有live-state、Agent/Session lifecycle或Workspace guard跨await，也不创建第二个candidate owner/task。
6. composition future返回后，Input live apply前必须重验same Submit CommandId、candidate TurnId、control_generation、latest emergency epoch、Agent/Session readiness和Workspace authority。Cancel或SecurityRevoked先赢时drop result且不创建Turn。Input已经live apply时沿用ADR 0127：完成record attempt、发布`TurnStarted`、阻止ActiveTurnTask spawn，再发布live interruption。
7. ActiveTurnTask在Steer safe point复用同一个`resolve_user_message()`。它在await期间同时观察EmergencyControl/Lifecycle；返回后重验same active TurnId、control_generation、source ConversationRevision和captured Context，成功后才apply Steer。reload不使old Context失效，Steer继续解析old captured bytes。
8. Session execution拥有control/revalidation；SkillService只拥有captured content load，PromptSet只拥有canonical normalization。不得把candidate target、CancellationToken、live-state handle或control generation放入SkillService/PromptSet。

## 后果

- Input与Steer共享一条可调用路径，async只存在于captured contribution resolve阶段；模型上下文assembly继续同步纯内存。
- Starting期间控制actor可以响应Cancel/SecurityRevoked，迟到load结果无法apply或spawn task。
- TurnExecutionContext多持有一个Runtime-shared SkillService Arc和exact SkillViewContext Arc，但不持有current root、path resolver或mutable cache state。
- cache parse可以在最后一个waiter取消后继续完成，这是performance implementation detail；conversation correctness只取决于仍current的resolve caller是否通过await后重验。
- pre-Turn Cancel的public typed completion仍由Runtime public protocol freeze决定；本ADR只冻结内部first-wins与无live apply行为。

## 测试要求

- Submit/Steer无Skill与单/多Skill都经过同一resolve seam；
- duplicate/missing/stale/source-mismatch或required contribution失败不apply部分message；
- load期间Cancel(Submit CommandId)或SecurityRevoked先赢时无Turn、无task spawn；
- Input apply后Cancel仍先`TurnStarted`再Interrupted，且不spawn task；
- Steer与reload竞态继续使用old captured bytes；
- resolve future被drop后迟到cache结果不进入conversation；
- no ordinary guard跨Skill load await；
- PromptSet composition/assembly不执行Skill正文I/O。

## 修订关系

本ADR细化ADR 0110的lazy Skill规则、ADR 0123的immutable capture/explicit reload、ADR 0126的SessionExecutor control actor与ActiveTurnTask结构，以及ADR 0129的原子UserMessage contribution composition。它不改变part-level safe provenance、conversation-only recording或public Cancel response shape。