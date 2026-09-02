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
- `prepare` runs under cancel/timeout/panic isolation. `PromptError::Cancelled`
  and turn-deadline map to the respective ending paths; anything else is a
  `Prompt` failure.

## DefaultProjection

`DefaultPromptProvider` projects history in order:

- optional non-empty system prompt, when configured;
- `User` -> `ModelMessage::User`;
- `Assistant` -> `ModelMessage::assistant` (an empty part list is
  `InvalidHistory`);
- `ToolResult` -> a typed tool message;
- `Summary` -> `ModelMessage::system("Conversation summary:\n{content}")`.

The fixed summary prefix is always preserved verbatim. When the summary
content would push the message past the absolute model message text ceiling
(`MAX_MODEL_MESSAGE_TEXT_BYTES`), only the content *tail* is truncated at a
UTF-8 character boundary (never mid-character), so the default provider
always projects a legal message for any legal maximum `BoundedText`.

## Core Enforcement

- Every provider's output is budget-checked at the request boundary: an
  empty prompt or one over `max_prompt_messages` fails as `Prompt`, no
  matter which provider produced it.
- No messages from an empty history is `PromptError::EmptyPrompt` for the
  default provider.
- `ModelRequest` construction performs per-message text validation and
  exchange-level tool-result consistency; a custom provider producing an
  illegal message through a public enum variant fails as `Prompt` with zero
  requests issued.