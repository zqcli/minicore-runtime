# Session JSON v2

This document is the current durable session configuration contract implemented by [`src/storage/store.rs`](../../src/storage/store.rs). The store writes one `session.json` per session directory and rejects unknown fields, duplicate enabled tools, unsupported format versions, invalid paths, invalid text, and values outside the bounds below.

## Layout

```text
<data_dir>/runtime.lock
<data_dir>/sessions/<ses_...>/session.json
<data_dir>/sessions/<ses_...>/conversation.jsonl
```

The store worker creates `<data_dir>`, opens and exclusively locks `runtime.lock`, creates `sessions/`, removes orphan temporary creation directories, and owns the lock until shutdown completes. A session directory must contain exactly `session.json` and `conversation.jsonl`; symlinks and extra entries are rejected.

Session creation is complete-or-invisible. The worker creates a `.session-tmp-...` directory, writes and synchronizes both files, then renames the directory to the checked session identifier. There are no generations, aliases, head records, or publication markers.

## Canonical Object

The serialized object is format version `2`. Fields are emitted in this exact order:

```json
{
  "format_version": 2,
  "session_id": "ses_<32 lowercase hex>",
  "created_at": "2026-08-20T12:34:56.789Z",
  "updated_at": "2026-08-20T12:34:56.789Z",
  "workspace_root": "/absolute/workspace/root",
  "model": {
    "provider": "provider-id",
    "model": "model-id"
  },
  "system_prompt": "checked coding instructions",
  "enabled_tools": ["read_file", "write_file"],
  "compaction": {
    "trigger_tokens": 80000,
    "target_tokens": 30000
  },
  "max_tool_rounds": 16
}
```

The nested `model` object emits `provider` then `model`. The nested `compaction` object emits `trigger_tokens` then `target_tokens`.

## Field Rules

| Field | Rule |
| --- | --- |
| `format_version` | Exactly integer `2`; any other version is rejected. |
| `session_id` | Checked `ses_` identifier with a non-zero 128-bit payload encoded as 32 lowercase hexadecimal characters. |
| `created_at`, `updated_at` | Canonical UTC timestamps with millisecond precision; the store preserves their checked values. |
| `workspace_root` | Absolute UTF-8 path with no `.` or `..` path component. It is configuration data, not a portable path key. |
| `model.provider` | Checked provider identifier. |
| `model.model` | Checked model identifier. |
| `system_prompt` | Safe UTF-8 text of `0..=262,144` bytes; empty is allowed, and control characters are limited to newline and tab. |
| `enabled_tools` | Sorted unique tool names, at most 64 values. |
| `compaction.trigger_tokens` | Non-zero token threshold. |
| `compaction.target_tokens` | Non-zero and strictly less than `trigger_tokens`. |
| `max_tool_rounds` | Integer in `1..=64`. |

The `system_prompt` validator accepts a UTF-8 string when `value.len() <= 262,144`, including the empty string, and rejects every control character other than `\n` and `\t`. The complete serialized `session.json` including its final newline is at most 1 MiB. Serialization uses the production `serde_json` configuration; the checked session fields use typed integers and identifiers.

## Read and Write Rules

- Creation uses a temporary directory and an atomic rename into `sessions/`.
- `session.json` is written with `create_new`, flushed, and synchronized before publication.
- Loading reads at most 1 MiB plus one byte, rejects oversized content, deserializes through checked constructors, and verifies the embedded session ID matches the directory name.
- Unknown object fields are rejected. Missing required fields, wrong scalar types, duplicate enabled tools, and invalid nested values are rejected.
- Listing validates every non-temporary session directory by loading its `session.json`; malformed entries make the store report corruption rather than silently skipping them.
- Deletion refuses an open session, rejects symlinked/non-directory session paths, and removes only a validated session directory.
- A failed temporary create is cleaned up. Cleanup failure is reported rather than converted into a successful create failure.

The v2 runtime does not automatically translate another persistence format. An offline host migration must emit this exact object and validate it by opening the result through the v2 store.
