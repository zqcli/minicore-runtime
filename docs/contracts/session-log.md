# SessionLog Port Contract

`SessionLog` is the only public persistence Port. Core accepts one already acquired, exclusive adapter for one SessionRuntime; repository and adapter selection remain Host responsibilities.

Source: [`src/conversation/session_log.rs`](../../src/conversation/session_log.rs), publicly reexported through [`src/storage/mod.rs`](../../src/storage/mod.rs). Evidence: [`session_log_contract.rs`](../../tests/session_log_contract.rs), the focused ConversationLog tests under [`src/conversation/log/`](../../src/conversation/log/), and already-initialized/conflict owner behavior in [`session_runtime_lifecycle_evidence.rs`](../../tests/session_runtime_lifecycle_evidence.rs).

## Exact Interface

```rust
pub type LogFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, SessionLogError>> + Send + 'a>>;

pub trait SessionLog: Send + 'static {
    fn initialize<'a>(
        &'a mut self,
        manifest: SessionManifest,
    ) -> LogFuture<'a, ConversationSeq>;

    fn load_manifest<'a>(&'a mut self) -> LogFuture<'a, SessionManifest>;

    fn read_page<'a>(
        &'a mut self,
        after: Option<ConversationSeq>,
        limit: usize,
    ) -> LogFuture<'a, ConversationPage>;

    fn append<'a>(
        &'a mut self,
        expected_head: ConversationSeq,
        entries: Vec<ConversationEntry>,
    ) -> LogFuture<'a, AppendReceipt>;

    fn close<'a>(&'a mut self) -> LogFuture<'a, ()>;
}
```

The Port is `Send + 'static`, not `Sync`. Every operation requires `&mut self`, allowing one actor to serialize access without requiring the adapter to implement its own concurrent mutation protocol.

## Manifest And Initialization

`initialize` receives the complete checked v3 `SessionManifest`. A new log must atomically establish that manifest and an empty Conversation, return `ConversationSeq::ZERO`, and reject reinitialization.

`load_manifest` returns the durable manifest for binding/spec/identity checks before replay. The adapter must not substitute Host metadata or a repository record for this manifest.

## Pages

`read_page(after, limit)` returns:

- ordered entries strictly after `after`;
- at most `limit` entries;
- `next_after` when another page remains;
- one `observed_head` stable for the replay/read contract.

Empty-page, cursor, sequence, head-drift, and pagination rules are validated by Core. Adapters must not skip, duplicate, reorder, or rewrite entries.

## Append Receipt

`append(expected_head, entries)` is one atomic compare-and-append operation. Success returns:

- `previous_head == expected_head`;
- `new_head` equal to the final appended sequence;
- `appended == entries.len()`.

The batch must commit all entries or none. A mismatched expected head is `Conflict`. A receipt that does not prove the exact requested prefix extension is a contract violation and is treated conservatively.

## Error Classification

`SessionLogErrorKind` is exactly:

- `NotInitialized`;
- `AlreadyInitialized`;
- `Conflict`;
- `Corrupt`;
- `Unavailable`;
- `UnknownOutcome`;
- `Closed`;
- `Internal`.

`SessionLogError` carries a redacted `DiagnosticSummary`; Display exposes the kind, not raw adapter messages.

`UnknownOutcome` means the caller cannot know whether mutation happened. Core must not retry blindly or advance confirmed state. During an active Session it degrades durability and blocks new submit. During restart repair it prevents readiness. Timeout, panic, cancellation after admission, and malformed success receipts may also be classified as durability unknown by the Core wrapper.

## Close

`close` is mandatory and attempted exactly once by the owner path. Explicit shutdown waits for it. Failed open also attempts close and retains that result as secondary evidence when another primary open failure already exists.

A close timeout or panic is not retried. Runtime Drop is only cancellation; Hosts must call `shutdown` for a close-complete durability barrier.

## Host Adapter Duties

The Host must:

- acquire and enforce any writer lease before passing the adapter to Core;
- keep one adapter bound to one durable Session identity;
- implement atomic expected-head append and stable page reads;
- classify known failure versus unknown outcome truthfully;
- bound and redact diagnostics;
- make operation futures cancellation-safe and release resources when dropped;
- implement concrete durability, filesystem/database policy, migrations, listing, deletion, backup, and retention outside Core.

Core provides no JSONL, filesystem, SQL, repository, lease, or migration implementation.
