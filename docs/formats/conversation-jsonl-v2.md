# Conversation JSONL v2

This is the append-only conversation contract implemented by [`src/session/conversation.rs`](../../src/session/conversation.rs) and [`src/session/conversation/codec.rs`](../../src/session/conversation/codec.rs). Each physical record is one compact JSON object followed by `LF`. The serializer validates the semantic shape before emitting bytes, and the deserializer rejects unknown fields and invalid relations.

## Record Order

Every record starts with `type`. The remaining fields are emitted in the exact order below.

### `user`

```json
{"type":"user","seq":1,"turn_id":"trn_<id>","timestamp":"...","text":"..."}
```

Fields: `type`, `seq`, `turn_id`, `timestamp`, `text`.

### `assistant`

```json
{"type":"assistant","seq":2,"turn_id":"trn_<id>","timestamp":"...","text":"...","reasoning":null,"tool_calls":[],"usage":null}
```

Fields: `type`, `seq`, `turn_id`, `timestamp`, `text`, `reasoning`, `tool_calls`, `usage`. `text` and `reasoning` may be absent semantically only when another visible assistant part or a tool call is present. A response cannot be an empty assistant record.

Each `tool_calls` item is the model-owned serialized `ToolCall` object with fields in this order: `tool_call_id`, `name`, `arguments`, `call_index`. Tool-call indexes start at zero within a response round and are contiguous; identifiers are unique within that round.

### `tool_result`

```json
{"type":"tool_result","seq":3,"turn_id":"trn_<id>","timestamp":"...","call_id":"call-id","result":{"text":"...","is_error":false}}
```

Fields: `type`, `seq`, `turn_id`, `timestamp`, `call_id`, `result`. The `result` object emits `text` then `is_error`.

### `interaction`

```json
{"type":"interaction","seq":4,"turn_id":"trn_<id>","timestamp":"...","interaction_id":"int_<id>","question":{"interaction_id":"int_<id>","question":"...","choices":null},"answer":{"text":"..."}}
```

Fields: `type`, `seq`, `turn_id`, `timestamp`, `interaction_id`, `question`, `answer`. `UserQuestion` emits `interaction_id`, `question`, `choices`; `UserAnswer` emits `text`. The outer and nested interaction identifiers must match.

An interaction is durable transcript evidence but is not included in model-visible prompt messages. Transcript projection still returns it with its question and answer text.

### `summary`

```json
{"type":"summary","seq":5,"timestamp":"...","through_seq":4,"text":"..."}
```

Fields: `type`, `seq`, `timestamp`, `through_seq`, `text`. `through_seq = 0` is the allowed initial/genesis boundary. Otherwise it must name an existing prior complete boundary. In every case `through_seq < seq` for the summary record, and a later summary must advance the prior summary boundary.

### `turn_terminal`

```json
{"type":"turn_terminal","seq":6,"turn_id":"trn_<id>","timestamp":"...","outcome":"completed"}
```

Fields: `type`, `seq`, `turn_id`, `timestamp`, `outcome`. The outcome is one of `completed`, `failed`, `cancelled`, or `cancelled_by_restart`. A normal producer does not create `cancelled_by_restart`; the restart repair path appends it only after unresolved tool work has been settled.

## Bounds and Shape

| Item | Bound or rule |
| --- | --- |
| User/assistant/tool-result text | At most 262,144 UTF-8 bytes; non-empty where the owning value requires content; only newline and tab control characters are allowed. |
| Summary text | At most 65,536 UTF-8 bytes and non-empty. |
| Complete physical line | At most 1 MiB including the newline. |
| Complete file | At most 1 GiB. |
| Complete entries | At most 1,000,000. |
| Sequence | Positive `u64`; the first record is `1`, later records must increase, and later gaps are allowed. |
| Tool exchange | Tool results must match calls in the current assistant exchange; duplicate or unknown calls corrupt replay. |
| Terminal | One terminal outcome closes a turn; a second terminal for that turn is corruption. |
| Summary boundary | `through_seq = 0` is the initial/genesis boundary; otherwise it must be an existing prior complete boundary, and it must always be less than the summary record's `seq`. |

All checked identifiers use their canonical string forms. Interaction question and answer text are safe UTF-8 values of `1..=8192` bytes. `choices` is optional; when present it has `1..=32` safe UTF-8 choices, each `1..=1024` bytes. The persisted `UserQuestion` and `UserAnswer` DTO constructors permit newline and tab but reject every other control character. The interaction context request check may be stricter and reject all control characters before persistence; the persisted DTO constructor remains the serialization/replay authority. `Timestamp` is the canonical UTC millisecond representation. `serde_json` output is compact and preserves the declared field order; the format is compared as UTF-8 bytes, not normalized text.

## Append and Replay

`ConversationLog::append` validates a new semantic entry, reserves the next sequence, serializes one line, and delegates the physical append to the store-owned worker. The line is written, flushed, and synchronized before the in-memory projection is advanced. Failed append or projection work does not fabricate a successful terminal result.

`append_summary` is a separate stale-safe operation. It requires the expected current sequence and boundary, appends one `summary` record, and updates the prompt/compaction projection without rewriting prior lines. Source entries remain available to snapshots and transcript pages.

Replay reads bounded complete lines in order. For a `summary`, `through_seq = 0` uses the initial/genesis boundary; a nonzero `through_seq` must be present in the set of prior complete boundaries, must be less than that summary's `seq`, and must advance the previous summary boundary. A final partial tail is accepted, truncated to the last complete newline, synchronized, and marked as repaired. A complete malformed UTF-8/JSON line, invalid field shape, sequence violation, relation violation, duplicate call result, incomplete terminal relation, invalid summary boundary, or oversized line/file returns corruption with physical line and byte offset information.

If restart observes an assistant tool exchange with unresolved calls, it appends an error `tool_result` for each unresolved call with text `cancelled by restart`, then appends `turn_terminal` with outcome `cancelled_by_restart`. These repair records are idempotent: an already completed repair is not duplicated on the next open.

## Projections

The prompt projection includes user records, assistant text/reasoning/tool calls, and matched tool results. Interaction records are intentionally omitted from model messages. A terminal record closes the current turn; summaries replace only the prompt prefix through their boundary and do not remove durable source records.

The typed transcript projection exposes all six variants:

- `User { seq, turn_id, text }`
- `Assistant { seq, turn_id, text, tool_calls }`
- `ToolResult { seq, turn_id, call_id, text, is_error }`
- `Interaction { seq, turn_id, interaction_id, question, answer }`
- `Summary { seq, through_seq, text }`
- `Terminal { seq, turn_id, outcome }`

`Runtime::transcript` accepts an optional exclusive `after_seq` cursor and a page size from `1..=200`. The page returns `next_after_seq` when more records remain. Paging reads a bounded snapshot projection and does not mutate the file.

Conversation usage aggregates only persisted assistant usage according to the conservative usage owner. It is not reconstructed from token text. Compaction consumes terminal-aware prompt views, preserves the current turn, and records its summary append before the next model admission.
