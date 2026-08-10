# Durable Store V1 Fixtures

These are authoritative, stdlib-verified conformance assets for [Durable Store V1](../../formats/durable-store-v1.md). Their owning M5/M7 implementation slices consume them; they are not illustrative alternatives. `manifest.json` is closed and `verify.py` rejects undeclared or missing assets, noncanonical bytes, duplicate JSON keys, floats/NaN/Infinity, wrong scalar types, wrong nested key order, and any matrix drift.

## Asset taxonomy

- Initial Agent/Session and ordinary/Genesis fork-child definition/head goldens freeze exact compact UTF-8 + LF documents, nullable fields, creation relationships, exact `CanonicalFileUri` carrier `file:///Users/example/project`, sorted unique PromptId selections, safe text, and scalar/revision types. URI drive/UNC families remain Wire carrier fixtures/native-platform tests rather than a weak fixture regex.
- Adjacent-generation goldens freeze all six categories: Agent definition, metadata and status; Session definition, metadata and lifecycle. Session definition has separate model-only (`wr_1`) and Workspace-changing (`wr_2`) branches; lifecycle covers Open→Archived and alternative generation-3 Archived→Open / Archived→Deleted branches. Definition/metadata content must actually change, while all unlisted pointers/revisions/entity timestamps remain exact.
- `fork-source.jsonl` and `fork-child.jsonl` are the ordinary payload-anchor semantic fork pair: the provenance anchors `after_user_message` at the final copied User Item, and the verifier byte-compares every field actually present after only SessionId rebinding. The canonical rule preserves every historical field, but this two-User-entry pair does not itself demonstrate RequestId/ToolCallId; Wire conversation fixtures cover those body variants. `genesis-fork-session-definition.json`, `genesis-fork-session-head.json`, and header-only `genesis-fork-child.jsonl` separately prove Genesis provenance omits `data` and copies no entries. Fork is semantic streaming re-encode, never raw JSONL byte copy.
- `crash-matrix.json` is a closed exact tuple contract. Every case has `name`, `coordinate`, `scope`, `slice`, and the closed expected object `operationOutcome`, `runtimeState`, `reservationState`, `visibleState`, `reopenState`. Closed `operationOutcome` values are `completed`, `ordinary_error`, `internal_dispatch_unavailable`, `not_applicable`, `record_not_recorded`, `lost_to_process_abort`, and `lost_to_response`. The hard-coded mapping covers bootstrap/lease, reservation cap/collision/ambiguity, representative markerless payload failures, COMMITTED/PUBLISHED certainty, caller/response loss, Fork source-lease races, cleanup/namespace/caps, loaded Workspace publication, and Recorder job settlement/replay. Property/race tests still traverse operation internals; this table does not claim to enumerate every OS syscall.

The durable M5.0 exit consumes every `slice = m5_0 | platform_m5_0` case. M5.1 consumes all `slice = m5_1` Recorder cases through same-named deterministic tests in `conversation_storage::tests`; the closed verifier freezes that seven-case set, while the Rust tests prove each operation/health/visible/reopen tuple through the production Recorder and replay/load seams. The first behavioral Runtime slice M7 consumes `slice = m7` for owner-settled loaded Workspace publication; M5.0 must not prematurely implement SessionExecutor command behavior merely to satisfy that coordinate. Native macOS/Windows CI jobs and cross-platform tests for lock contention/reacquire/holder death plus Agent/Session/Fork process-abort recovery are implemented. `root_lease_identity_loss`, `cleanup_open_handle`, `case_alias_rejected`, and `symlink_reparse_rejected` remain pending `platform_m5_0` coordinates.

No asset contains credentials, keys, or production user data.

Run:

```bash
python3 docs/fixtures/durable-store-v1/verify.py
```
