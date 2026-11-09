# Prompt Contract

How host history becomes model messages.

## Seam

```rust
pub trait PromptProvider: Send + Sync + 'static {
    fn prepare<'a>(&'a self, request: PromptRequest<'a>) -> PromptFuture<'a>;
    // PromptRequest { loop_id, request_index, history, model, reasoning,
    //                 tools, cancellation, deadline }
    // -> Result<PreparedPrompt { messages }, PromptError>
}
```

- The host supplies the provider; `DefaultPromptProvider` is the built-in
  strict projection. The runtime never rejects a `start` because of a
  provider; provider problems surface as `Failed(Prompt)`.
- `prepare` runs under cancel/timeout/panic isolation. Every returned error,
  including `PromptError::Cancelled`, is a `Prompt` failure. Cancellation or a
  loop deadline observed by the runtime takes its respective ending path; a
  prompt-only timeout is a `Prompt` failure.

## DefaultProjection

`DefaultPromptProvider` projects history in order:

- optional non-empty system prompt, when configured;
- `User` -> `ModelMessage::User`;
- `Assistant` -> `ModelMessage::assistant` (an empty part list is
  `InvalidHistory`);
- `ToolResult` -> a typed tool message;
- `Summary` -> `ModelMessage::system("Conversation summary:\n{content}")`.

The fixed summary prefix and the entire summary content are preserved verbatim.
The provider does not truncate text or impose a per-message byte quota. Context
selection for actual model limits remains the host's responsibility.

## Core Enforcement

- Every provider's output is checked at the request boundary: an empty
  prompt fails as `Prompt`, no matter which provider produced it. The
  number and total size of messages are not capped.
- No messages from an empty history is `PromptError::EmptyPrompt` for the
  default provider.
- `ModelRequest` construction performs per-message text validation and
  exchange-level tool-result consistency; a custom provider producing an
  illegal message through a public enum variant fails as `Prompt` with zero
  requests issued.