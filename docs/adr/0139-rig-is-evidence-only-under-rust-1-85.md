# ADR 0139：Rust 1.85下Rig只作为独立证据harness

状态：Partially Superseded by ADR 0141
日期：2026-08-11

> 2026-08-12：[ADR 0141](0141-provider-calls-are-stateless-full-request.md)细化本ADR对M14 adapter的连接政策描述：adapter-owned reqwest client（每installation一个、reload复用）是全部connection policy，只描述普通stateless reqwest transport pooling eligibility，不承诺物理socket复用，也不携带auth/session state；一次invocation执行零或一次HTTP POST（pre-send cancellation/AuthMissing/validation/encoding失败可为零），若发送POST则携带完整full request，无optimization fallback/continuation。Rig拒绝、direct adapters与Rust 1.85 evidence结论不变。
>
> 2026-08-12：[ADR 0145](0145-live-provider-evidence-refines-direct-adapter-wire-truth.md)记录两个direct adapters的真实HTTPS/public Runtime release evidence，并冻结artifact-derived产品User-Agent、Anthropic unsigned thinking与omitted start-stop字段的严格消费规则。Rig仍只作为standalone evidence，production implementation与Rust 1.85边界不变。

## 背景

[ADR 0138](0138-production-provider-baseline-uses-verified-rig-contracts.md)使用exact `rig-core = 0.40.0`完成OpenAI Responses与Anthropic Messages的真实loopback reality gate，并冻结protocol terminal、response metadata、single-attempt和delivery/error合同。随后真实Rust 1.85冷编译发现，`rig-core = 0.40.0`使用Rust 1.85尚未稳定的let-chain与trait-upcasting语法，无法成为MiniCore主crate的production或dev dependency。

先前本地MSRV假绿不是依赖兼容证据：`rustup run 1.85.0 cargo`通过`PATH`调用了Homebrew stable `rustc`。显式设置`RUSTC="$(rustup which --toolchain 1.85.0 rustc)"`并使用隔离target后可稳定复现失败。crates.io metadata未声明`rust-version`，也不能替代真实编译。相邻`rig-core` 0.36.0–0.40.0同样不能由Rust 1.85编译，因此直接降级不是已验证方案。

MiniCore不能通过提升MSRV、`RUSTC_BOOTSTRAP`、跳过主crate all-targets、平台条件排除或继续使用错误compiler来隐藏该事实。与此同时，已完成的Rig loopback tests仍是有价值的SDK行为证据和provider协议参考，不应丢失。

## 决策

1. `rig-core`不进入`minicore-runtime`的dependencies、dev-dependencies或root lockfile。MiniCore production baseline继续是Rust 1.85，主crate的全部targets与features都必须由真实Rust 1.85编译和测试。

2. M14不再实现`RigProviderAdapter`。首个production实现是两个独立private adapters：

   - `OpenAiResponsesProviderAdapter`；
   - `AnthropicMessagesProviderAdapter`。

   两者直接拥有各自HTTP request、SSE parsing、terminal evidence、typed envelope、metadata allowlist和single-attempt cancellation mapping，并复用一个private、必须经Rust 1.85冷编译验证的transport client。具体transport dependency只在M14 contract slice中选择和固定，不能预先把SDK类型写入provider-neutral seam。

   M14现已固定exact `reqwest = 0.13.4`，关闭default features，仅启用`json + rustls + stream`；两个adapter共享private client construction、bounded body drain与bounded SSE framing，client显式使用`retry::never()`、`redirect::Policy::none()`和`no_proxy()`。真实Rust 1.85隔离冷编译已覆盖主crate全部targets/features：OpenAI local loopback suite通过真实`ProviderAdapter` seam验证完整`/responses` POST、bounded SSE、terminal/delivery/error、Structured mapping和cancellation；Anthropic suite同样验证完整`/v1/messages` POST、thinking/signature/redacted replay、Structured+adaptive effort、non-empty `message_delta.stop_reason` terminal、cumulative usage/cache、typed 529 proof与cancellation。后续M14 installation slice已把credential从adapter移到Gateway-owned per-attempt resolution：public host-only `CredentialSource`返回redacted typed credential，missing/cancel在adapter前保持`AuthMissing|Cancelled/NotSent`；validated `ModelProviderConfig`只在`MiniCoreRuntime::open`构建一次direct adapter/client与static source，默认Runtime仍为空catalog。stable `ModelId`与private API model name现已分离，OpenAI `response.model`与Anthropic `message_start.message.model`必须exact匹配pinned API name；Anthropic key header显式标记sensitive。两个explicit ignored live smoke tests已在stable/真实Rust 1.85编译且默认不执行；2026-08-12已在显式release环境通过OpenAI Responses与Anthropic Messages完整public Runtime-path real-credential smoke，nonsecret evidence由ADR 0145记录。

3. ADR 0138冻结的provider合同继续有效：首版protocol scope、ordered content/tool identity、OpenAI `response.completed`、Anthropic non-empty `message_delta.stop_reason`、metadata allowlist、automatic retry为0、26-case delivery/error mapping与queued Steer规则均不改变。变化仅是production implementation choice；Rig synthetic `Final`等观察结果保留为反例和conformance输入。

4. exact `rig-core = 0.40.0` tests移动到standalone `provider-gate/` package。该package不是主crateworkspace member，主动声明`rust-version = 1.88`，拥有独立lockfile，只在current stable门禁运行。Ubuntu stable通过`scripts/check.sh`运行它，macOS与Windows native jobs也运行它；所有测试继续离线、无credential/ambient config并使用test-owned loopback server。

5. `provider-gate/`不进入Rust 1.85 job不是兼容性豁免：它明确代表一个已被production baseline拒绝、且声明更高language floor的外部SDK证据harness。Rust 1.85 job仍对真实production package执行`check --all-targets --all-features --locked`和`test --all-targets --locked`，不能按target、feature或platform跳过任何主crate代码。

6. `scripts/check-msrv.sh`必须通过`rustup which --toolchain <MSRV>`取得exact `rustc`与`cargo`，校验compiler release，清除compiler wrapper，并使用隔离`CARGO_TARGET_DIR`。`rustup run cargo`、共享stable artifacts或PATH中的裸`rustc`都不能作为MSRV证据。

7. M12/V4-P1-3以“protocol scope和contracts已冻结，Rig 0.40 production dependency已明确拒绝”关闭。M12没有实现production adapter；M13仍先关闭Tool/Sandbox gate，M14再实现两个direct provider adapters及production Tool/Sandbox adapters。

## 可执行证据

- `provider-gate/tests/m12_rig_*.rs`：exact Rig 0.40.0在current stable上的OpenAI Responses/Anthropic Messages unary、stream、terminal、metadata和error behavior；
- 历史M12 provider-neutral delivery/error mapping fixture：`docs/archive/v2/fixtures/provider-gate-m12/error-mapping-v1.json`；当前合同由P3 provider suites与AT-13执行；
- `provider-gate/Cargo.toml`与独立`Cargo.lock`：Rig evidence dependency和更高language floor显式隔离；
- root `Cargo.toml`与`Cargo.lock`：production package不含Rig；M14 shared transport固定为经真实Rust 1.85验证的exact `reqwest = 0.13.4`最小feature set；
- `src/model_gateway/provider_transport.rs`：两个direct adapters真正共享的private client construction、bounded drain、SSE framing与默认离线loopback harness；
- `src/model_gateway/provider_installation.rs`：host-only redacted credential source、explicit model descriptor、endpoint policy与OpenAI/Anthropic catalog source installation；
- `src/model_gateway/openai_responses.rs`：OpenAI direct private adapter及默认离线loopback contract suite，覆盖single POST、request/terminal mapping、exact terminal model binding、bounded fragmented SSE、metadata redaction、delivery/error与cancellation；
- `src/model_gateway/anthropic_messages.rs`：Anthropic direct private adapter及默认离线loopback contract suite，覆盖single POST、thinking/signature/redacted replay、Structured+adaptive effort、service tier、exact start model binding、sensitive key header、terminal/usage/metadata、typed 529 proof、delivery/error与cancellation；
- `tests/m14_live_provider_smoke.rs`：两个public Runtime-path explicit ignored real-network smoke harness；默认tests只编译且不读取credential或访问network；
- `scripts/check-msrv.sh`：真实compiler identity和隔离target门禁；
- `.github/workflows/ci.yml`：stable Ubuntu/macOS/Windows持续执行evidence harness，Rust 1.85执行主crate全部targets。

## 后果

M14比包装单一SDK多拥有少量provider wire代码，但接口更深且事实所有权更清晰：OpenAI与Anthropic两个slices均证明terminal、delivery、metadata和cancellation可以由真正观察wire的owner直接分类，不必先由generic SDK擦除后再旁路重建。两个adapter只复用已验证的transport construction、bounded drain与bounded framing primitives，各自仍拥有独立request、SSE terminal与typed envelope parser，没有引入会模糊协议差异的generic response model。

Rig升级或重新评估只能作为独立候选：新版本必须先通过真实Rust 1.85冷编译和同等级contract suite，才可由新ADR考虑进入production baseline。当前不为未来可能兼容的Rig预留production abstraction、feature flag或conditional dependency。

本ADR refine ADR 0106、0105、0119、0126与0138中关于首个production Rig adapter的条款；这些ADR的provider-neutral ownership、single-attempt、logical retry和first-party Turn loop原则继续有效。
