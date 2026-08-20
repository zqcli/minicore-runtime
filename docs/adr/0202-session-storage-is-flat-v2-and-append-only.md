# ADR 0202: Session Storage Is Flat v2 and Append-Only

状态：Accepted

日期：2026-08-20

## Context

A durable session needs a small filesystem contract that can be created atomically, replayed deterministically, and repaired after a process interruption. Extra generation and publication entities would add ownership without improving the current single-session core.

## Decision

The store owns one exclusive `<data_dir>/runtime.lock` and one `<data_dir>/sessions/` namespace. Each session directory is named by its checked session identifier and contains exactly `session.json` and `conversation.jsonl`. Session creation writes a temporary directory, synchronizes both files, and renames it into place. There are no generations, aliases, or background detached writers.

`session.json` is format version 2 with the exact checked field order documented in [Session JSON v2](../formats/session-json-v2.md). `conversation.jsonl` is an append-only sequence of six checked semantic variants documented in [Conversation JSONL v2](../formats/conversation-jsonl-v2.md). The conversation owner flushes and synchronizes each append before advancing its durable projection.

Replay accepts and repairs only a final partial tail. Complete malformed lines, invalid relations, sequence violations, and bounds failures are corruption. Restart repair appends explicit failed tool results and a cancelled terminal record; it never rewrites source history. A compaction summary changes prompt projection only and remains an append-only transcript record.

## Consequences

Existing data requires an explicit offline host migration. The Runtime does not guess another storage format or silently translate an ambiguous record. The flat layout makes lock ownership, atomic create, replay, and cleanup directly testable.

See [session format](../formats/session-json-v2.md), [conversation format](../formats/conversation-jsonl-v2.md), and [`src/storage/store.rs`](../../src/storage/store.rs).
