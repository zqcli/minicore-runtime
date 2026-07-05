# ModelGateway Wraps Rig Providers

MiniCore will reuse Rig provider clients and provider protocol handling behind a private `ModelGateway` adapter, instead of exposing Rig provider types or implementing first-party OpenAI/Anthropic/Gemini HTTP clients. The decision keeps provider SDK churn and credentials inside a single runtime-owned boundary while preserving MiniCore control over model selection, auth redaction, hooks, fallback, usage normalization, error taxonomy, cancellation and protocol-safe events.

**Consequences**

- `Driver` only passes `ModelSelection` through `ModelCallRequest`; it never resolves API keys, provider registry entries, base URLs or provider payloads.
- `ProviderRegistry` is a catalog and capability source, not a credential store or provider client pool.
- `AuthStore` secrets may only flow through `ModelGateway` internals into the Rig provider request.
- Raw provider payload hooks are privileged and may remain disabled until the private Rig adapter can provide a redacted, stable hook shape.
