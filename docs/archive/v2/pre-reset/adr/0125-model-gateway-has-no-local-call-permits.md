# ADR 0125：ModelGateway不设置本地模型调用Permit

状态：Partially Superseded by ADR 0141
日期：2026-07-29

> 2026-08-12：[ADR 0141](0141-provider-calls-are-stateless-full-request.md)细化本ADR的connection/principal条款：MiniCore没有auth-principal identity类型；连接政策就是adapter-owned的无状态reqwest client（stateless transport pooling eligibility），不存在connection pool隔离键或route/principal cooldown state。no-local-permit决策本身不变。

## 背景

ModelGateway曾计划同时提供Runtime global、per-provider route、per-model和optional per-auth-principal四级并发permit，并为permit wait定义FIFO/no-starvation与stream lifetime规则。复核pi、Codex、Gemini CLI、OpenHands和Claude Code可验证行为后，没有发现单用户Agent runtime普遍采用这类多级模型请求permit；同类实现通常依赖每Session单active run、局部串行或provider的429/`Retry-After`反馈。云provider配额主要按organization/project、model family和RPM/TPM等时间窗口计算，active stream数量不能准确表达这些额度；本地推理backend的固定slot也属于backend自身明确暴露的容量，而不是MiniCore可以从`ModelId`推断的通用事实。

权威设计见[ModelGateway](../modules/model-gateway.md)和[Session Execution](../modules/session-execution.md)。

## 决策

1. MVP删除ModelGateway的Runtime global、per-provider route、per-model和per-auth-principal模型调用permit，不实现`ModelConcurrencyController`、本地admission queue、FIFO/no-starvation scheduler或`ModelSchedulingClass`。
2. 共享`Arc<ModelGateway>`必须支持多个Session并发调用；实现不得用Gateway-wide mutex或其他长guard包围credential resolution、provider request或stream读取。每个Session最多一个ActiveTurnTask，task内最多await一个current model call；这不形成跨Session模型调用限制。
3. Provider SDK/HTTP connection pool可以在其implementation内部管理socket和transport资源；该行为不成为MiniCore模型调用admission policy，也不进入ModelGateway interface、Turn identity或SessionStorage。
4. Provider返回的`RateLimited`、`QuotaExceeded`、typed `Retry-After`和route/principal cooldown继续由ModelGateway规范化。cooldown只对已知受限scope fast-fail，Gateway不sleep；SessionExecutor仍按ADR 0119裁决有限logical retry。
5. 多Session并发可能同时触发provider/backend限流，这是MVP接受的运行结果。未来只有真实容量来源、first-token latency SLO或生产遥测证明需要本地admission时，才以新ADR设计窄范围治理；不得仅根据provider或model名称猜测并发容量。

## 后果

- ModelGateway共享实例不会把多个Session串行化，模型请求可直接并发进入各自provider attempt。
- 删除permit acquisition order、nested queue、公平调度、取消等待和stream-held permit等实现与测试组合。
- 第一版评审O18的触发前提“Model permits被长stream占满”被删除，因此O18关闭；未来若host或backend引入新的显式admission queue，应根据实际SLO重新评审交互延迟隔离。