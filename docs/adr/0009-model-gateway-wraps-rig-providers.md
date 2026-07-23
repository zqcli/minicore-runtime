# ModelGateway Wraps Rig Providers

Amended by [ADR 0026](0026-model-gateway-uses-one-deep-async-operation.md): Rig remains a private ProviderAdapter, while ModelGateway exposes one deep `generate_model_turn(...)` operation and does not expose Rig streams or provider sessions.

MiniCore will reuse Rig provider clients and provider protocol handling behind a private `ModelGateway` adapter, instead of exposing Rig provider types or implementing first-party OpenAI/Anthropic/Gemini HTTP clients. The decision keeps provider SDK churn and credentials inside a single runtime-owned boundary while preserving MiniCore control over model selection, auth redaction, hooks, fallback, usage normalization, error taxonomy, cancellation and protocol-safe events.

**Consequences**

- SessionExecutor creates `ModelCallRequest` from a Turn-pinned `TurnModelSnapshot` and PromptSet-produced `AssembledModelContext`; AgentLoop/Driver never resolves provider, endpoint, auth or payload details.
- `ProviderCatalog` and `AuthStore` are private ModelGateway implementation modules rather than caller-owned runtime state.
- `AuthStore` secrets may only flow through ModelGateway internals into the private Rig provider adapter.
- Raw provider payload hooks are privileged and may remain disabled until the private Rig adapter can provide a redacted, stable hook shape.
