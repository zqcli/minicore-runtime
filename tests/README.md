## M12 Provider Gate

`provider-gate/tests/m12_rig_*.rs`在standalone stable-only package中驱动exact `rig-core = 0.40.0`和test-owned `127.0.0.1:0` HTTP servers。这些targets覆盖OpenAI Responses与Anthropic Messages unary/stream contracts、terminal-vs-EOF evidence、cancellation、single-request behavior、typed error envelopes与response metadata allowlists。`tests/m12_provider_error_matrix.rs`留在主crate，消费`docs/fixtures/provider-gate-m12/error-mapping-v1.json`并在Rust 1.85下冻结delivery-safe retry/normalization rules。

M12 tests必须保持offline和deterministic：不得使用external DNS/network、真实credential、ambient provider config、sleep、timeout-based absence proof、blind yield polling或unjoined server thread。Rig只存在于声明Rust 1.88并拥有独立lockfile的`provider-gate/` evidence package；root dependency/lockfile、production `src/`和public DTO不得出现Rig。`./scripts/check.sh`运行主crate与evidence package；`./scripts/check-msrv.sh`用真实Rust 1.85运行主crate全部targets。

## M14 Live Provider Smoke Tests

`tests/m14_live_provider_smoke.rs`包含两个explicit opt-in、real-network smoke tests：通过当前public typed Runtime path（`RuntimeConfig` → `ProviderRegistry` → `Runtime::open` → `SessionConfig`/`Runtime::create_session` → snapshot-first `subscribe` → `submit` → env-backed dynamic `CredentialSource` → direct provider adapter → typed transcript/terminal verification）分别驱动真实OpenAI Responses与Anthropic Messages API。两个tests都是`#[ignore]`d，**从不**在`./scripts/check.sh`、`./scripts/check-msrv.sh`或默认`cargo test`（包括`cargo test --all-targets`）中执行——这些gate只编译它们。必须显式运行`--ignored`才会执行，且每个test仍然要求其provider的opt-in env var恰好为`1`并具备完整的documented env set，否则test以仅点名缺失变量、绝不打印变量值/credential/endpoint/model值的消息panic。

显式env contract（全部required、无defaults）：

```text
# OpenAI Responses
MINICORE_LIVE_OPENAI_OPT_IN=1
MINICORE_LIVE_OPENAI_ENDPOINT=…     # 必须满足 ProviderEndpointPolicy::HttpsOnly
MINICORE_LIVE_OPENAI_API_MODEL=…    # 真实 API model 名（仅作为 private wire name，不打印）
MINICORE_LIVE_OPENAI_CREDENTIAL=…   # 仅检查 presence；值绝不打印

# Anthropic Messages
MINICORE_LIVE_ANTHROPIC_OPT_IN=1
MINICORE_LIVE_ANTHROPIC_ENDPOINT=…  # HttpsOnly
MINICORE_LIVE_ANTHROPIC_API_MODEL=…
MINICORE_LIVE_ANTHROPIC_CREDENTIAL=…
MINICORE_LIVE_ANTHROPIC_VERSION=…   # 如 2023-06-01
```

Secrets必须通过已导出的environment variable提供，**绝不可**作为command-line argument传入（进程列表/CI logs会泄漏）。建议先export完整env set，再运行：

```bash
cargo test --locked --test m14_live_provider_smoke -- --ignored              # 全部 live smoke
cargo test --locked --test m14_live_provider_smoke -- --ignored openai       # 仅 OpenAI Responses
cargo test --locked --test m14_live_provider_smoke -- --ignored anthropic    # 仅 Anthropic Messages
```

验证`--ignored`前的编译/默认行为（无网络、无env依赖）：

```bash
cargo test --locked --test m14_live_provider_smoke --no-run
cargo test --locked --test m14_live_provider_smoke   # 报告 2 ignored、0 执行
```

两个tests共享一个120s的explicit live wait bound（纯operational bound，不是absence proof），成功与失败路径总是先调用typed `Runtime::shutdown()`；temp durable root/workspace目录通过`Drop`清理。test使用public typed types only：`OpenAiResponsesProvider`/`AnthropicMessagesProvider`、`ProviderRegistry`、`RuntimeConfig`与`Runtime`构造函数，descriptor为provider `openai-live`/`anthropic-live` + model `smoke`（stable selection刻意与API model名不同）、Anthropic version来自env、max output 64、Auto+Disabled reasoning、无工具。env-backed `CredentialSource`定义在integration test内部：`resolve()`只返回async future，`std::env::var`与`ProviderCredential`解析都在future内完成；缺失/非法credential解析为`None`（typed `AuthMissing`/`NotSent`）且不打印。

2026-08-12已在显式release环境连续执行两个exact ignored tests，OpenAI Responses与Anthropic Messages完整public Runtime paths均通过。credential来自仓库外0600临时环境文件，执行后unset并删除；仓库不记录credential、private endpoint或response content。该次nonsecret evidence与暴露的User-Agent/Anthropic wire refinements见[ADR 0145](../docs/adr/0145-live-provider-evidence-refines-direct-adapter-wire-truth.md)。默认gate行为不变：tests继续ignored、离线且无ambient credential依赖。
