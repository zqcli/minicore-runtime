#!/usr/bin/env python3
"""Strict stdlib-only verifier for the Durable Store V1 fixture contract."""
from __future__ import annotations

import json
import re
import sys
from datetime import datetime
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent
MAX_DOCUMENT_BYTES = 1_048_576
MAX_DEPTH = 64
MAX_OBJECT_MEMBERS = 256
MAX_ARRAY_ITEMS = 4_096
MAX_STRING_BYTES = 262_144
U32_MAX = 4_294_967_295
U64_MAX = 18_446_744_073_709_551_615
AGENT_ID = re.compile(r"^agt_[0-9a-f]{32}$")
SESSION_ID = re.compile(r"^ses_[0-9a-f]{32}$")
ITEM_ID = re.compile(r"^itm_[0-9a-f]{32}$")
ENTRY_ID = re.compile(r"^ent_[0-9a-f]{32}$")
TURN_ID = re.compile(r"^trn_[0-9a-f]{32}$")
REVISION = re.compile(r"^(ar|sdr|amr|smr|wr)_[1-9][0-9]*$")
TIMESTAMP = re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z")
PROMPT_ID = re.compile(r"^[!#-$%&'()*+,-.0-~]{1,128}$")

DOCUMENTS = (
    "agent-definition.json",
    "agent-head.json",
    "session-definition.json",
    "session-head.json",
    "fork-session-definition.json",
    "fork-session-head.json",
    "genesis-fork-session-definition.json",
    "genesis-fork-session-head.json",
    "agent-definition-2.json",
    "agent-head-2-definition.json",
    "agent-head-2-metadata.json",
    "agent-head-2-status.json",
    "session-definition-2.json",
    "session-head-2-definition.json",
    "session-definition-2-workspace.json",
    "session-head-2-workspace-definition.json",
    "session-head-2-metadata.json",
    "session-head-2-lifecycle.json",
    "session-head-3-unarchive.json",
    "session-head-3-deleted.json",
)
JSONL_ASSETS = (
    "fork-source.jsonl",
    "fork-child.jsonl",
    "genesis-fork-child.jsonl",
)
MARKERS = {
    "format": "MINICORE_STORE_V1",
    "lock": ".minicore.lock",
    "published": "PUBLISHED",
    "committed": "COMMITTED",
}
PATHS = {
    "agent": "agents/<AgentId>",
    "session": "sessions/<SessionId>",
    "conversation": "sessions/<SessionId>/conversation.jsonl",
    "generation": "generations/<20-digit StorageGeneration>",
}
GENERATION_DIRS = {
    1: "00000000000000000001",
    2: "00000000000000000002",
    3: "00000000000000000003",
}
GENERATION_CASES = (
    {"name": "agent_definition", "entity": "agent", "head": "agent-head-2-definition.json", "definition": "agent-definition-2.json"},
    {"name": "agent_metadata", "entity": "agent", "head": "agent-head-2-metadata.json", "definition": None},
    {"name": "agent_status", "entity": "agent", "head": "agent-head-2-status.json", "definition": None},
    {"name": "session_definition_model", "entity": "session", "head": "session-head-2-definition.json", "definition": "session-definition-2.json"},
    {"name": "session_definition_workspace", "entity": "session", "head": "session-head-2-workspace-definition.json", "definition": "session-definition-2-workspace.json"},
    {"name": "session_metadata", "entity": "session", "head": "session-head-2-metadata.json", "definition": None},
    {"name": "session_lifecycle_archive", "entity": "session", "head": "session-head-2-lifecycle.json", "definition": None},
    {"name": "session_lifecycle_unarchive", "entity": "session", "head": "session-head-3-unarchive.json", "definition": None},
    {"name": "session_lifecycle_delete", "entity": "session", "head": "session-head-3-deleted.json", "definition": None},
)

# This is intentionally a closed, hard-coded name -> complete tuple mapping. Do
# not infer outcomes from coordinates; implementations consume this as a matrix.
OPERATION_OUTCOMES = {
    "completed",
    "ordinary_error",
    "internal_dispatch_unavailable",
    "not_applicable",
    "record_not_recorded",
    "lost_to_process_abort",
    "lost_to_response",
}

CASE_EXPECTATIONS: dict[str, tuple[str, str, str, str, str, str]] = {
    "bootstrap_fail_before_marker": ("bootstrap.marker.create.before", "store_open", "m5_0", "ordinary_error", "open_blocked", "not_applicable|not_applicable|opens"),
    "bootstrap_fail_after_marker": ("bootstrap.marker.create.after_side_effect", "store_open", "m5_0", "completed", "running", "not_applicable|not_applicable|opens"),
    "bootstrap_scaffold_reopen": ("bootstrap.reopen.empty_scaffold", "store_open", "m5_0", "completed", "running", "not_applicable|not_applicable|opens"),
    "bootstrap_markerless_nonempty_rejected": ("bootstrap.markerless.nonempty", "store_open", "m5_0", "ordinary_error", "open_blocked", "not_applicable|not_applicable|open_blocked"),
    "lock_contention": ("root_lock.try_exclusive.contended", "store_open", "platform_m5_0", "ordinary_error", "open_blocked", "not_applicable|not_applicable|opens"),
    "lock_reacquire": ("root_lock.release_then_reacquire", "store_open", "platform_m5_0", "completed", "running", "not_applicable|not_applicable|opens"),
    "lock_holder_death": ("root_lock.holder_process_death", "store_open", "platform_m5_0", "completed", "running", "not_applicable|not_applicable|opens"),
    "reservation_collection_cap": ("reservation.collection.cap_before_barrier", "agent_create", "m5_0", "ordinary_error", "running", "unburned|invisible|opens"),
    "reservation_collision": ("reservation.create_new.definite_collision", "agent_create", "m5_0", "completed", "running", "burned|published_new|opens"),
    "reservation_collision_exhausted": ("reservation.create_new.collision_32", "agent_create", "m5_0", "ordinary_error", "running", "unburned|invisible|opens"),
    "reservation_fail_after_create": ("reservation.create_new.after_side_effect", "agent_create", "m5_0", "ordinary_error", "running", "burned|invisible|opens"),
    "reservation_indeterminate": ("reservation.create_new.indeterminate_readback", "session_create", "m5_0", "internal_dispatch_unavailable", "closing", "indeterminate|invisible|recovery_decides_burned_or_unburned_or_blocked"),
    "head_create_failure": ("payload.head.create.before", "session_create", "m5_0", "ordinary_error", "running", "burned|invisible|opens"),
    "definition_partial_write": ("payload.definition.write.partial", "session_create", "m5_0", "ordinary_error", "running", "burned|invisible|opens"),
    "conversation_partial_write": ("payload.conversation.write.partial", "fork", "m5_0", "ordinary_error", "running", "burned|invisible|opens"),
    "payload_sync_failure": ("payload.file.sync.before_committed", "session_create", "m5_0", "ordinary_error", "running", "burned|invisible|opens"),
    "create_before_committed": ("generation.committed.create.before", "session_create", "m5_0", "ordinary_error", "running", "burned|invisible|opens"),
    "create_before_published": ("entity.published.create.before", "session_create", "m5_0", "ordinary_error", "running", "burned|invisible|opens"),
    "create_committed_marker_sync_then_published": ("generation1.committed.file_sync.after_side_effect", "session_create", "m5_0", "completed", "closing", "burned|published_new|opens"),
    "create_after_published": ("entity.published.create.after_side_effect", "session_create", "m5_0", "completed", "running", "burned|published_new|opens"),
    "marker_fail_after_readback": ("entity.published.create.after_side_effect.readback", "fork", "m5_0", "completed", "running", "burned|published_new|opens"),
    "published_marker_sync_failure": ("entity.published.file_sync.after_side_effect", "session_create", "m5_0", "completed", "closing", "burned|published_new|opens"),
    "post_marker_sync_completed_before_closing": ("entity.published.parent_sync.after_marker", "session_create", "m5_0", "completed", "closing", "burned|published_new|opens"),
    "committed_corrupt_poisoned": ("entity.published.readback.payload_missing", "session_create", "m5_0", "internal_dispatch_unavailable", "closing", "burned|not_applicable|open_blocked"),
    "indeterminate_poisoned": ("entity.published.readback.indeterminate", "session_create", "m5_0", "internal_dispatch_unavailable", "closing", "burned|not_applicable|recovery_decides_old_new_or_blocked"),
    "update_before_committed_old": ("generation.committed.create.before", "session_update", "m5_0", "ordinary_error", "running", "not_applicable|published_old|old_visible"),
    "update_after_committed_new": ("generation.committed.create.after_side_effect", "session_update", "m5_0", "completed", "running", "not_applicable|published_new|new_visible"),
    "update_committed_marker_sync_failure": ("generation.committed.file_sync.after_side_effect", "session_update", "m5_0", "completed", "closing", "not_applicable|published_new|new_visible"),
    "update_committed_payload_missing": ("generation.committed.readback.payload_missing", "session_update", "m5_0", "internal_dispatch_unavailable", "closing", "not_applicable|not_applicable|open_blocked"),
    "update_post_committed_directory_sync_completed_before_closing": ("generation.parent_sync.after_committed", "session_update", "m5_0", "completed", "closing", "not_applicable|published_new|new_visible"),
    "same_process_staging_cleanup_failure": ("generation.cleanup.after_precommit_failure", "session_update", "m5_0", "ordinary_error", "closing", "not_applicable|published_old|old_visible"),
    "response_lost_after_publication": ("response.transport.lost_after_published", "session_create", "m5_0", "lost_to_response", "running", "burned|published_new|entity_visible"),
    "caller_drop_before_durable_barrier": ("caller.drop.before_durable_commit", "fork", "m5_0", "ordinary_error", "running", "burned|invisible|opens"),
    "caller_drop_after_durable_barrier": ("caller.drop.after_durable_commit", "fork", "m5_0", "lost_to_response", "running", "burned|published_new|entity_visible"),
    "join_panic_after_possible_marker": ("tracked_job.join_panic.after_possible_marker", "session_create", "m5_0", "internal_dispatch_unavailable", "closing", "burned|not_applicable|recovery_decides_old_new_or_blocked"),
    "root_lease_identity_loss": ("root_lock.identity_lost", "runtime", "platform_m5_0", "internal_dispatch_unavailable", "closing", "not_applicable|not_applicable|open_blocked"),
    "fork_partial_reencode": ("fork.stream_reencode.partial", "fork", "m5_0", "ordinary_error", "running", "burned|invisible|opens"),
    "fork_complete_reencode": ("fork.stream_reencode.complete", "fork", "m5_0", "completed", "running", "burned|published_new|opens"),
    "fork_load_waits_for_lease": ("fork.recorded_lease.race_load", "fork", "m5_0", "completed", "running", "burned|published_new|opens"),
    "fork_append_waits_for_lease": ("fork.recorded_lease.race_append", "fork", "m5_0", "completed", "running", "burned|published_new|opens"),
    "fork_tail_truncate_waits_for_lease": ("fork.recorded_lease.race_tail_truncate", "fork", "m5_0", "completed", "running", "burned|published_new|opens"),
    "fork_self_source_rejected": ("fork.provenance.self_source", "fork", "m5_0", "ordinary_error", "running", "unburned|invisible|opens"),
    "create_abort_before_published": ("process_abort.entity.create.before_published", "session_create", "m5_0", "lost_to_process_abort", "terminated", "burned|invisible|entity_invisible"),
    "create_abort_after_published": ("process_abort.entity.create.after_published", "session_create", "m5_0", "lost_to_process_abort", "terminated", "burned|published_new|entity_visible"),
    "update_abort_before_committed": ("process_abort.generation.update.before_committed", "session_update", "m5_0", "lost_to_process_abort", "terminated", "not_applicable|published_old|old_visible"),
    "update_abort_after_committed": ("process_abort.generation.update.after_committed", "session_update", "m5_0", "lost_to_process_abort", "terminated", "not_applicable|published_new|new_visible"),
    "workspace_post_commit_install_failure": ("workspace.snapshot.install.after_commit", "session_update", "m7", "internal_dispatch_unavailable", "closing", "not_applicable|published_new|new_visible"),
    "malformed_staging_without_reservation": ("recovery.staging.missing_reservation", "store_open", "m5_0", "ordinary_error", "open_blocked", "not_applicable|not_applicable|open_blocked"),
    "cleanup_open_handle": ("recovery.cleanup.open_handle", "store_open", "platform_m5_0", "ordinary_error", "open_blocked", "not_applicable|not_applicable|open_blocked"),
    "case_alias_rejected": ("recovery.namespace.case_alias", "store_open", "platform_m5_0", "ordinary_error", "open_blocked", "not_applicable|not_applicable|open_blocked"),
    "symlink_reparse_rejected": ("recovery.namespace.link_or_reparse", "store_open", "platform_m5_0", "ordinary_error", "open_blocked", "not_applicable|not_applicable|open_blocked"),
    "root_cap_plus_one": ("recovery.root.cap_plus_one", "store_open", "m5_0", "ordinary_error", "open_blocked", "not_applicable|not_applicable|open_blocked"),
    "fixed_directory_cap_plus_one": ("recovery.fixed_directory.cap_plus_one", "store_open", "m5_0", "ordinary_error", "open_blocked", "not_applicable|not_applicable|open_blocked"),
    "generation_collection_cap_plus_one": ("recovery.generations.cap_plus_one", "store_open", "m5_0", "ordinary_error", "open_blocked", "not_applicable|not_applicable|open_blocked"),
    "recorder_partial_tail_replay": ("recorder.write.partial_tail", "recorder", "m5_1", "record_not_recorded", "recorder_degraded", "not_applicable|partial_tail|partial_tail_ignored_or_truncated"),
    "recorder_full_line_replay": ("recorder.write.full_line_after_side_effect", "recorder", "m5_1", "record_not_recorded", "recorder_degraded", "not_applicable|complete_line|complete_line_replayed"),
    "recorder_spawn_failure": ("recorder.job.spawn_failure", "recorder", "m5_1", "record_not_recorded", "recorder_degraded", "not_applicable|recorded_prefix|recorded_prefix_replayed"),
    "recorder_caller_drop_before_barrier": ("recorder.caller_drop.before_write", "recorder", "m5_1", "record_not_recorded", "recorder_degraded", "not_applicable|recorded_prefix|recorded_prefix_replayed"),
    "recorder_caller_drop_after_barrier": ("recorder.caller_drop.after_write", "recorder", "m5_1", "completed", "recorder_healthy", "not_applicable|complete_line|complete_line_replayed"),
    "recorder_join_panic": ("recorder.job.join_panic", "recorder", "m5_1", "record_not_recorded", "recorder_degraded", "not_applicable|partial_or_complete_tail|replay_decides_prefix_or_line"),
    "recorder_shutdown_join": ("recorder.shutdown.waits_settlement", "recorder", "m5_1", "completed", "closing", "not_applicable|complete_line|complete_line_replayed"),
}


class DuplicateKey(ValueError):
    pass


def no_duplicates(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise DuplicateKey(key)
        result[key] = value
    return result


def reject_constant(value: str) -> Any:
    raise ValueError(f"non-finite JSON constant {value}")


def reject_float(value: str) -> Any:
    raise ValueError(f"floating JSON number {value}")


def decode(raw: bytes, path: Path) -> Any:
    try:
        return json.loads(
            raw,
            object_pairs_hook=no_duplicates,
            parse_constant=reject_constant,
            parse_float=reject_float,
        )
    except (UnicodeDecodeError, json.JSONDecodeError, DuplicateKey, ValueError) as error:
        raise AssertionError(f"invalid JSON: {path.name}: {error}") from error


def canonical(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8")


def exact_string(value: Any, label: str) -> str:
    assert type(value) is str and value, f"{label}: expected nonempty string"
    return value


def exact_int(value: Any, label: str) -> int:
    assert type(value) is int, f"{label}: expected JSON integer, not bool/float"
    return value


def exact_bool(value: Any, label: str) -> bool:
    assert type(value) is bool, f"{label}: expected JSON boolean"
    return value


def check_safe_text(value: str, label: str, *, allow_empty: bool = False) -> None:
    assert type(value) is str and (allow_empty or value), f"{label}: expected safe text"
    for char in value:
        code = ord(char)
        assert not (code == 0 or code == 0x1B or code == 0x7F or 0x80 <= code <= 0x9F), label
        assert code >= 0x20 or char in {"\t", "\n"}, label


def check_bounds(value: Any, depth: int = 1) -> None:
    assert depth <= MAX_DEPTH, "JSON depth exceeds cap"
    assert type(value) in {dict, list, str, int, bool} or value is None, type(value)
    if type(value) is str:
        assert len(value.encode("utf-8")) <= MAX_STRING_BYTES, "string exceeds cap"
    elif type(value) is list:
        assert len(value) <= MAX_ARRAY_ITEMS, "array exceeds cap"
        for item in value:
            check_bounds(item, depth + 1)
    elif type(value) is dict:
        assert len(value) <= MAX_OBJECT_MEMBERS, "object exceeds cap"
        for key, item in value.items():
            assert type(key) is str
            check_bounds(key, depth + 1)
            check_bounds(item, depth + 1)


def expect_keys(value: Any, keys: list[str], label: str = "object") -> dict[str, Any]:
    assert type(value) is dict, f"{label}: expected object"
    assert list(value) == keys, f"{label}: {list(value)} != {keys}"
    return value


def check_timestamp(value: Any, label: str) -> str:
    value = exact_string(value, label)
    assert TIMESTAMP.fullmatch(value), value
    datetime.fromisoformat(value.removesuffix("Z") + "+00:00")
    return value


def check_id(value: Any, pattern: re.Pattern[str], label: str) -> str:
    value = exact_string(value, label)
    assert pattern.fullmatch(value), value
    assert set(value.split("_", 1)[1]) != {"0"}, value
    return value


def check_revision(value: Any, kind: str, label: str) -> int:
    value = exact_string(value, label)
    assert REVISION.fullmatch(value) and value.startswith(kind + "_"), value
    revision = int(value.split("_", 1)[1])
    assert 1 <= revision <= U64_MAX, value
    return revision


def check_prompt_ids(value: Any, label: str) -> list[str]:
    assert type(value) is list, label
    for prompt_id in value:
        prompt_id = exact_string(prompt_id, label)
        assert PROMPT_ID.fullmatch(prompt_id), prompt_id
        assert all(char not in {'"', "\\", "/"} for char in prompt_id), prompt_id
    assert value == sorted(value, key=lambda item: item.encode("utf-8")), label
    assert len(value) == len(set(value)), label
    return value


def check_metadata(value: Any, kind: str, *, agent: bool) -> dict[str, Any]:
    value = expect_keys(value, ["revision", "name", "description", "updatedAt"], "metadata")
    check_revision(value["revision"], kind, "metadata.revision")
    if agent:
        assert type(value["name"]) is str and value["name"], "Agent name must be non-null/nonempty"
        check_safe_text(value["name"], "metadata.name")
    else:
        assert value["name"] is None or type(value["name"]) is str
        if type(value["name"]) is str:
            check_safe_text(value["name"], "metadata.name")
    assert value["description"] is None or type(value["description"]) is str
    if type(value["description"]) is str:
        check_safe_text(value["description"], "metadata.description", allow_empty=True)
    check_timestamp(value["updatedAt"], "metadata.updatedAt")
    return value


def check_current_definition(value: Any, kind: str, generation: int) -> tuple[int, int]:
    value = expect_keys(value, ["revision", "storageGeneration"], "currentDefinition")
    revision = check_revision(value["revision"], kind, "currentDefinition.revision")
    definition_generation = exact_int(
        value["storageGeneration"], "currentDefinition.storageGeneration"
    )
    assert 1 <= definition_generation <= generation
    return revision, definition_generation


def check_agent_definition(value: Any) -> dict[str, Any]:
    value = expect_keys(value, ["agentId", "revision", "promptIds", "createdAt"], "agent definition")
    check_id(value["agentId"], AGENT_ID, "agentId")
    check_revision(value["revision"], "ar", "agent definition revision")
    check_prompt_ids(value["promptIds"], "agent promptIds")
    check_timestamp(value["createdAt"], "agent definition createdAt")
    return value


def check_agent_head(value: Any) -> dict[str, Any]:
    value = expect_keys(value, ["entity", "agentId", "storageGeneration", "previousStorageGeneration", "currentDefinition", "metadata", "status", "createdAt"], "agent head")
    assert value["entity"] == "agent"
    check_id(value["agentId"], AGENT_ID, "agent head id")
    generation = exact_int(value["storageGeneration"], "agent storageGeneration")
    assert 1 <= generation <= 1_000_000
    if generation == 1:
        assert value["previousStorageGeneration"] is None
    else:
        assert exact_int(value["previousStorageGeneration"], "agent previousStorageGeneration") == generation - 1
    check_current_definition(value["currentDefinition"], "ar", generation)
    check_metadata(value["metadata"], "amr", agent=True)
    assert value["status"] in {"enabled", "disabled", "deleted"}
    check_timestamp(value["createdAt"], "agent head createdAt")
    return value


def check_workspace(value: Any) -> dict[str, Any]:
    value = expect_keys(value, ["revision", "primaryRoot", "additionalRoots", "cwd"], "workspace")
    check_revision(value["revision"], "wr", "workspace revision")
    root = expect_keys(value["primaryRoot"], ["key", "path", "requestedAccess", "sources"], "primaryRoot")
    assert root["key"] == "repo"
    # CanonicalFileUri is Wire-owned. This POSIX asset intentionally tests its
    # exact canonical carrier; drives/UNC remain Wire/native-platform coverage.
    assert root["path"] == "file:///Users/example/project"
    assert root["requestedAccess"] in {"read_only", "read_write"}
    sources = expect_keys(root["sources"], ["prompt", "skill"], "sources")
    assert exact_bool(sources["prompt"], "sources.prompt") is True
    assert exact_bool(sources["skill"], "sources.skill") is True
    assert value["additionalRoots"] == []
    cwd = expect_keys(value["cwd"], ["root", "relativePath"], "cwd")
    assert cwd["root"] == "repo"
    assert cwd["relativePath"] in {"src", "tests"}
    return value


def check_session_definition(value: Any) -> dict[str, Any]:
    value = expect_keys(value, ["sessionId", "revision", "agent", "workspace", "model", "promptIds", "createdAt"], "session definition")
    check_id(value["sessionId"], SESSION_ID, "sessionId")
    check_revision(value["revision"], "sdr", "session definition revision")
    agent = expect_keys(value["agent"], ["agentId", "revision"], "session definition agent")
    check_id(agent["agentId"], AGENT_ID, "session definition agent id")
    check_revision(agent["revision"], "ar", "session definition agent revision")
    check_workspace(value["workspace"])
    model = expect_keys(value["model"], ["selection", "reasoning", "maxOutputTokens"], "model")
    selection = expect_keys(model["selection"], ["providerId", "modelId"], "model selection")
    check_safe_text(selection["providerId"], "providerId")
    check_safe_text(selection["modelId"], "modelId")
    assert model["reasoning"] in {"auto", "disabled", "low", "medium", "high"}
    assert model["maxOutputTokens"] is None or (type(model["maxOutputTokens"]) is int and 1 <= model["maxOutputTokens"] <= U32_MAX)
    check_prompt_ids(value["promptIds"], "session promptIds")
    check_timestamp(value["createdAt"], "session definition createdAt")
    return value


def check_anchor(value: Any) -> None:
    assert type(value) is dict, "fork anchor: expected object"
    if value.get("type") == "genesis":
        assert list(value) == ["type"] and value == {"type": "genesis"}, value
        return
    expect_keys(value, ["type", "data"], "payload fork anchor")
    assert value["type"] in {"before_user_message", "after_user_message", "before_final_agent_message", "after_final_agent_message"}
    data = expect_keys(value["data"], ["itemId"], "fork anchor data")
    check_id(data["itemId"], ITEM_ID, "fork itemId")


def check_provenance(value: Any) -> None:
    value = expect_keys(value, ["sourceSessionId", "source", "anchor"], "fork provenance")
    check_id(value["sourceSessionId"], SESSION_ID, "sourceSessionId")
    assert value["source"] in {"live_snapshot", "recorded_history"}
    check_anchor(value["anchor"])


def check_session_head(value: Any, *, fork: bool) -> dict[str, Any]:
    value = expect_keys(value, ["entity", "sessionId", "storageGeneration", "previousStorageGeneration", "currentDefinition", "metadata", "lifecycle", "forkProvenance", "createdAt"], "session head")
    assert value["entity"] == "session"
    check_id(value["sessionId"], SESSION_ID, "session head id")
    generation = exact_int(value["storageGeneration"], "session storageGeneration")
    assert 1 <= generation <= 1_000_000
    if generation == 1:
        assert value["previousStorageGeneration"] is None
    else:
        assert exact_int(value["previousStorageGeneration"], "session previousStorageGeneration") == generation - 1
    check_current_definition(value["currentDefinition"], "sdr", generation)
    check_metadata(value["metadata"], "smr", agent=False)
    assert value["lifecycle"] in {"open", "archived", "deleted"}
    if fork:
        check_provenance(value["forkProvenance"])
        assert value["forkProvenance"]["sourceSessionId"] != value["sessionId"]
    else:
        assert value["forkProvenance"] is None
    check_timestamp(value["createdAt"], "session head createdAt")
    return value


def load_document(name: str) -> Any:
    path = ROOT / name
    raw = path.read_bytes()
    assert len(raw) <= MAX_DOCUMENT_BYTES, name
    assert raw.endswith(b"\n") and raw.count(b"\n") == 1, name
    assert not raw.startswith(b"\xef\xbb\xbf") and b"\r" not in raw, name
    value = decode(raw[:-1], path)
    assert canonical(value) == raw[:-1], f"noncanonical JSON: {name}"
    check_bounds(value)
    return value


def check_generation_transitions(parsed: dict[str, Any]) -> None:
    ad1, ah1 = parsed["agent-definition.json"], parsed["agent-head.json"]
    ad2 = parsed["agent-definition-2.json"]
    ah2d = parsed["agent-head-2-definition.json"]
    ah2m = parsed["agent-head-2-metadata.json"]
    ah2s = parsed["agent-head-2-status.json"]

    assert ad2["agentId"] == ad1["agentId"] == ah1["agentId"] == ah2d["agentId"] == ah2m["agentId"] == ah2s["agentId"]
    assert ad2["revision"] == "ar_2" and ad2["promptIds"] != ad1["promptIds"]
    assert ad2["createdAt"] > ad1["createdAt"]
    assert ah2d["storageGeneration"] == 2 and ah2d["previousStorageGeneration"] == 1
    assert ah2d["currentDefinition"] == {"revision": "ar_2", "storageGeneration": 2}
    assert ah2d["metadata"] == ah1["metadata"] and ah2d["status"] == ah1["status"] and ah2d["createdAt"] == ah1["createdAt"]
    assert ah2m["storageGeneration"] == 2 and ah2m["previousStorageGeneration"] == 1
    assert ah2m["currentDefinition"] == {"revision": "ar_1", "storageGeneration": 1}
    assert ah2m["metadata"]["revision"] == "amr_2"
    assert (ah2m["metadata"]["name"], ah2m["metadata"]["description"]) != (ah1["metadata"]["name"], ah1["metadata"]["description"])
    assert ah2m["status"] == ah1["status"] and ah2m["createdAt"] == ah1["createdAt"]
    assert ah2s["storageGeneration"] == 2 and ah2s["previousStorageGeneration"] == 1
    assert ah2s["currentDefinition"] == ah1["currentDefinition"] and ah2s["metadata"] == ah1["metadata"]
    assert ah2s["status"] == "disabled" and ah2s["createdAt"] == ah1["createdAt"]

    sd1, sh1 = parsed["session-definition.json"], parsed["session-head.json"]
    sd2 = parsed["session-definition-2.json"]
    sh2d = parsed["session-head-2-definition.json"]
    sd2w = parsed["session-definition-2-workspace.json"]
    sh2w = parsed["session-head-2-workspace-definition.json"]
    sh2m = parsed["session-head-2-metadata.json"]
    sh2a = parsed["session-head-2-lifecycle.json"]
    sh3u = parsed["session-head-3-unarchive.json"]
    sh3x = parsed["session-head-3-deleted.json"]

    session_ids = {value["sessionId"] for value in (sd1, sh1, sd2, sh2d, sd2w, sh2w, sh2m, sh2a, sh3u, sh3x)}
    assert session_ids == {sd1["sessionId"]}
    assert sd2["revision"] == "sdr_2" and sd2["agent"] == sd1["agent"]
    assert sd2["workspace"] == sd1["workspace"] and sd2["model"] != sd1["model"] and sd2["promptIds"] == sd1["promptIds"]
    assert sd2["createdAt"] > sd1["createdAt"]
    assert sh2d["storageGeneration"] == 2 and sh2d["previousStorageGeneration"] == 1
    assert sh2d["currentDefinition"] == {"revision": "sdr_2", "storageGeneration": 2}
    assert sh2d["metadata"] == sh1["metadata"] and sh2d["lifecycle"] == sh1["lifecycle"] and sh2d["forkProvenance"] is None and sh2d["createdAt"] == sh1["createdAt"]

    assert sd2w["revision"] == "sdr_2" and sd2w["agent"] == sd1["agent"]
    assert sd2w["workspace"]["revision"] == "wr_2" and sd2w["workspace"] != sd1["workspace"]
    assert sd2w["model"] == sd1["model"] and sd2w["promptIds"] == sd1["promptIds"] and sd2w["createdAt"] > sd1["createdAt"]
    assert sh2w["storageGeneration"] == 2 and sh2w["previousStorageGeneration"] == 1
    assert sh2w["currentDefinition"] == {"revision": "sdr_2", "storageGeneration": 2}
    assert sh2w["metadata"] == sh1["metadata"] and sh2w["lifecycle"] == sh1["lifecycle"] and sh2w["forkProvenance"] is None and sh2w["createdAt"] == sh1["createdAt"]

    assert sh2m["storageGeneration"] == 2 and sh2m["previousStorageGeneration"] == 1
    assert sh2m["currentDefinition"] == sh1["currentDefinition"] and sh2m["lifecycle"] == sh1["lifecycle"]
    assert sh2m["metadata"]["revision"] == "smr_2"
    assert (sh2m["metadata"]["name"], sh2m["metadata"]["description"]) != (sh1["metadata"]["name"], sh1["metadata"]["description"])
    assert sh2m["forkProvenance"] is None and sh2m["createdAt"] == sh1["createdAt"]

    assert sh2a["storageGeneration"] == 2 and sh2a["previousStorageGeneration"] == 1
    assert sh2a["currentDefinition"] == sh1["currentDefinition"] and sh2a["metadata"] == sh1["metadata"]
    assert sh2a["lifecycle"] == "archived" and sh2a["forkProvenance"] is None and sh2a["createdAt"] == sh1["createdAt"]
    for branch, lifecycle in ((sh3u, "open"), (sh3x, "deleted")):
        assert branch["storageGeneration"] == 3 and branch["previousStorageGeneration"] == 2
        assert branch["currentDefinition"] == sh2a["currentDefinition"] and branch["metadata"] == sh2a["metadata"]
        assert branch["lifecycle"] == lifecycle and branch["forkProvenance"] is None and branch["createdAt"] == sh2a["createdAt"]


def decode_jsonl(name: str) -> list[tuple[Any, bytes]]:
    raw = (ROOT / name).read_bytes()
    assert raw.endswith(b"\n") and b"\r" not in raw and not raw.startswith(b"\xef\xbb\xbf"), name
    lines = raw.splitlines()
    assert lines, name
    values = [decode(line, ROOT / name) for line in lines]
    for value, line in zip(values, lines, strict=True):
        assert canonical(value) == line, f"noncanonical JSONL line: {name}"
        check_bounds(value)
    return list(zip(values, lines, strict=True))


def check_fixture_user_message_body(value: Any, label: str) -> str:
    body = expect_keys(value, ["type", "data"], f"{label} body")
    assert body["type"] == "user_message"
    data = expect_keys(body["data"], ["itemId", "source", "content"], f"{label} user data")
    item_id = check_id(data["itemId"], ITEM_ID, f"{label} itemId")
    assert data["source"] in {"input", "steer"}
    content = expect_keys(data["content"], ["parts", "contributionStamps"], f"{label} content")
    assert type(content["parts"]) is list and len(content["parts"]) == 1
    part = expect_keys(content["parts"][0], ["type", "data"], f"{label} part")
    assert part["type"] == "text"
    text = expect_keys(part["data"], ["text"], f"{label} text")
    check_safe_text(text["text"], f"{label} text value")
    assert content["contributionStamps"] == []
    return item_id


def check_fork_reencode(parsed: dict[str, Any]) -> None:
    source_records = decode_jsonl("fork-source.jsonl")
    child_records = decode_jsonl("fork-child.jsonl")
    source = [value for value, _ in source_records]
    child = [value for value, _ in child_records]
    assert len(source) == len(child)
    source_header = expect_keys(source[0], ["type", "data"], "source header envelope")
    child_header = expect_keys(child[0], ["type", "data"], "child header envelope")
    assert source_header["type"] == child_header["type"] == "session_header"
    source_data = expect_keys(source_header["data"], ["formatVersion", "sessionId", "createdAt", "initialAgent", "initialDefinitionRevision"], "source Header")
    child_data = expect_keys(child_header["data"], ["formatVersion", "sessionId", "createdAt", "initialAgent", "initialDefinitionRevision"], "child Header")
    assert source_data["sessionId"] == "ses_22222222222222222222222222222222"
    assert child_data["sessionId"] == "ses_33333333333333333333333333333333"
    assert exact_int(source_data["formatVersion"], "source Header formatVersion") == 1
    assert exact_int(child_data["formatVersion"], "child Header formatVersion") == 1
    assert source_data["initialAgent"] == child_data["initialAgent"] == {"agentId": "agt_11111111111111111111111111111111", "revision": "ar_1"}
    assert source_data["initialDefinitionRevision"] == child_data["initialDefinitionRevision"] == "sdr_1"
    check_timestamp(source_data["createdAt"], "source Header createdAt")
    check_timestamp(child_data["createdAt"], "child Header createdAt")
    source_definition = parsed["session-definition.json"]
    source_head = parsed["session-head.json"]
    child_definition = parsed["fork-session-definition.json"]
    child_head = parsed["fork-session-head.json"]
    assert source_data["sessionId"] == source_definition["sessionId"] == source_head["sessionId"]
    assert source_data["initialAgent"] == source_definition["agent"]
    assert source_data["initialDefinitionRevision"] == source_definition["revision"]
    assert source_data["createdAt"] == source_head["createdAt"]
    assert child_data["sessionId"] == child_definition["sessionId"] == child_head["sessionId"]
    assert child_data["initialAgent"] == child_definition["agent"]
    assert child_data["initialDefinitionRevision"] == child_definition["revision"]
    assert child_data["createdAt"] == child_definition["createdAt"] == child_head["createdAt"]
    assert child_head["forkProvenance"]["sourceSessionId"] == source_data["sessionId"]
    assert child_head["forkProvenance"]["sourceSessionId"] != child_data["sessionId"]

    copied_item_ids: list[str] = []
    previous_entry_id: str | None = None
    for (source_line, _), (child_line, child_bytes) in zip(
        source_records[1:], child_records[1:], strict=True
    ):
        source_entry = expect_keys(source_line, ["type", "data"], "source entry envelope")
        child_entry = expect_keys(child_line, ["type", "data"], "child entry envelope")
        assert source_entry["type"] == child_entry["type"] == "entry"
        source_data = expect_keys(source_entry["data"], ["entryId", "parentId", "sessionId", "turnId", "timestamp", "body"], "source entry")
        child_data = expect_keys(child_entry["data"], ["entryId", "parentId", "sessionId", "turnId", "timestamp", "body"], "child entry")
        check_id(source_data["entryId"], ENTRY_ID, "source entryId")
        check_id(source_data["turnId"], TURN_ID, "source turnId")
        check_timestamp(source_data["timestamp"], "source entry timestamp")
        if source_data["parentId"] is not None:
            check_id(source_data["parentId"], ENTRY_ID, "source parentId")
        assert source_data["parentId"] == previous_entry_id
        assert source_data["sessionId"] == "ses_22222222222222222222222222222222"
        assert child_data["sessionId"] == "ses_33333333333333333333333333333333"
        copied_item_ids.append(check_fixture_user_message_body(source_data["body"], "source"))
        assert check_fixture_user_message_body(child_data["body"], "child") == copied_item_ids[-1]
        source_rebound = dict(source_data)
        source_rebound["sessionId"] = child_data["sessionId"]
        assert source_rebound == child_data, "only entry sessionId may be rebound"
        assert canonical({"type": "entry", "data": source_rebound}) == child_bytes
        previous_entry_id = source_data["entryId"]

    ordinary_anchor = {"type": "after_user_message", "data": {"itemId": "itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}
    assert parsed["fork-session-head.json"]["forkProvenance"]["anchor"] == ordinary_anchor
    ordinary_last = expect_keys(child[-1], ["type", "data"], "ordinary child final envelope")
    ordinary_last_body = expect_keys(ordinary_last["data"]["body"], ["type", "data"], "ordinary child final body")
    assert ordinary_last_body["type"] == "user_message"
    assert ordinary_last_body["data"]["itemId"] == ordinary_anchor["data"]["itemId"]
    assert copied_item_ids[-1] == ordinary_anchor["data"]["itemId"]

    genesis_records = decode_jsonl("genesis-fork-child.jsonl")
    assert len(genesis_records) == 1
    genesis_header, _ = genesis_records[0]
    genesis_envelope = expect_keys(genesis_header, ["type", "data"], "genesis child header envelope")
    assert genesis_envelope["type"] == "session_header"
    genesis_data = expect_keys(genesis_envelope["data"], ["formatVersion", "sessionId", "createdAt", "initialAgent", "initialDefinitionRevision"], "genesis child Header")
    assert exact_int(genesis_data["formatVersion"], "genesis Header formatVersion") == 1
    assert genesis_data["sessionId"] == "ses_44444444444444444444444444444444"
    assert genesis_data["sessionId"] not in {source_data["sessionId"], child_data["sessionId"]}
    check_timestamp(genesis_data["createdAt"], "genesis Header createdAt")
    assert parsed["genesis-fork-session-head.json"]["forkProvenance"]["anchor"] == {"type": "genesis"}
    genesis_definition = parsed["genesis-fork-session-definition.json"]
    genesis_head = parsed["genesis-fork-session-head.json"]
    assert genesis_definition["sessionId"] == genesis_data["sessionId"] == genesis_head["sessionId"]
    assert genesis_head["forkProvenance"]["sourceSessionId"] == source_definition["sessionId"]
    assert genesis_head["forkProvenance"]["sourceSessionId"] != genesis_data["sessionId"]
    assert genesis_data["initialAgent"] == genesis_definition["agent"]
    assert genesis_data["initialDefinitionRevision"] == genesis_definition["revision"]
    assert genesis_data["createdAt"] == genesis_head["createdAt"]


def check_goldens(manifest: dict[str, Any]) -> None:
    assert tuple(manifest["documents"]) == DOCUMENTS
    parsed = {name: load_document(name) for name in DOCUMENTS}
    for name in ("agent-definition.json", "agent-definition-2.json"):
        check_agent_definition(parsed[name])
    for name in ("agent-head.json", "agent-head-2-definition.json", "agent-head-2-metadata.json", "agent-head-2-status.json"):
        check_agent_head(parsed[name])
    for name in (
        "session-definition.json",
        "fork-session-definition.json",
        "genesis-fork-session-definition.json",
        "session-definition-2.json",
        "session-definition-2-workspace.json",
    ):
        check_session_definition(parsed[name])
    for name in (
        "session-head.json",
        "session-head-2-definition.json",
        "session-head-2-workspace-definition.json",
        "session-head-2-metadata.json",
        "session-head-2-lifecycle.json",
        "session-head-3-unarchive.json",
        "session-head-3-deleted.json",
    ):
        check_session_head(parsed[name], fork=False)
    check_session_head(parsed["fork-session-head.json"], fork=True)
    check_session_head(parsed["genesis-fork-session-head.json"], fork=True)

    assert parsed["agent-definition.json"]["agentId"] == parsed["agent-head.json"]["agentId"]
    assert parsed["session-definition.json"]["sessionId"] == parsed["session-head.json"]["sessionId"]
    assert parsed["session-definition.json"]["agent"] == {"agentId": parsed["agent-definition.json"]["agentId"], "revision": parsed["agent-definition.json"]["revision"]}
    exact_agent_ref = {"agentId": parsed["agent-definition.json"]["agentId"], "revision": parsed["agent-definition.json"]["revision"]}
    for definition_name in (
        "session-definition.json",
        "fork-session-definition.json",
        "genesis-fork-session-definition.json",
        "session-definition-2.json",
        "session-definition-2-workspace.json",
    ):
        assert parsed[definition_name]["agent"] == exact_agent_ref
    agent_head = parsed["agent-head.json"]
    session_head = parsed["session-head.json"]
    assert agent_head["storageGeneration"] == 1 and agent_head["previousStorageGeneration"] is None
    assert agent_head["currentDefinition"] == {"revision": "ar_1", "storageGeneration": 1}
    assert agent_head["metadata"]["revision"] == "amr_1" and agent_head["status"] == "enabled"
    assert agent_head["createdAt"] == parsed["agent-definition.json"]["createdAt"] == agent_head["metadata"]["updatedAt"]
    assert session_head["storageGeneration"] == 1 and session_head["previousStorageGeneration"] is None
    assert session_head["currentDefinition"] == {"revision": "sdr_1", "storageGeneration": 1}
    assert session_head["metadata"]["revision"] == "smr_1" and session_head["lifecycle"] == "open" and session_head["forkProvenance"] is None
    assert parsed["session-definition.json"]["revision"] == "sdr_1" and parsed["session-definition.json"]["workspace"]["revision"] == "wr_1"
    assert session_head["createdAt"] == parsed["session-definition.json"]["createdAt"] == session_head["metadata"]["updatedAt"]
    for definition_name, head_name in (
        ("fork-session-definition.json", "fork-session-head.json"),
        ("genesis-fork-session-definition.json", "genesis-fork-session-head.json"),
    ):
        definition, head = parsed[definition_name], parsed[head_name]
        assert definition["sessionId"] == head["sessionId"] and definition["revision"] == "sdr_1"
        assert definition["workspace"]["revision"] == "wr_1"
        assert head["storageGeneration"] == 1 and head["previousStorageGeneration"] is None
        assert head["currentDefinition"] == {"revision": "sdr_1", "storageGeneration": 1}
        assert head["metadata"]["revision"] == "smr_1" and head["lifecycle"] == "open"
        assert head["forkProvenance"]["source"] == "recorded_history"
        assert definition["createdAt"] == head["createdAt"] == head["metadata"]["updatedAt"]
    check_generation_transitions(parsed)
    check_fork_reencode(parsed)


def check_manifest() -> dict[str, Any]:
    manifest = load_document("manifest.json")
    expect_keys(manifest, ["version", "documents", "jsonl", "generationCases", "crashMatrix"], "manifest")
    assert exact_int(manifest["version"], "manifest version") == 1
    assert type(manifest["documents"]) is list and tuple(manifest["documents"]) == DOCUMENTS
    assert type(manifest["jsonl"]) is list and tuple(manifest["jsonl"]) == JSONL_ASSETS
    assert manifest["crashMatrix"] == "crash-matrix.json"
    assert type(manifest["generationCases"]) is list
    for case in manifest["generationCases"]:
        expect_keys(case, ["name", "entity", "head", "definition"], "generation case")
    assert tuple(manifest["generationCases"]) == GENERATION_CASES
    expected_assets = set(DOCUMENTS) | set(JSONL_ASSETS) | {"manifest.json", "crash-matrix.json", "README.md", "verify.py"}
    entries = list(ROOT.iterdir())
    assert all(path.is_file() and not path.is_symlink() for path in entries), entries
    actual_assets = {path.name for path in entries}
    assert actual_assets == expected_assets, (actual_assets, expected_assets)
    return manifest


def check_crash_matrix() -> None:
    value = load_document("crash-matrix.json")
    expect_keys(value, ["version", "cases"], "crash matrix")
    assert exact_int(value["version"], "crash version") == 1
    assert type(value["cases"]) is list and len(value["cases"]) == len(CASE_EXPECTATIONS)
    observed: dict[str, tuple[str, str, str, str, str, str]] = {}
    for case in value["cases"]:
        expect_keys(case, ["name", "coordinate", "scope", "slice", "expected"], "crash case")
        expected = expect_keys(case["expected"], ["operationOutcome", "runtimeState", "reservationState", "visibleState", "reopenState"], "crash expected")
        name = exact_string(case["name"], "crash name")
        tuple_value = (
            exact_string(case["coordinate"], "coordinate"),
            exact_string(case["scope"], "scope"),
            exact_string(case["slice"], "slice"),
            exact_string(expected["operationOutcome"], "operationOutcome"),
            exact_string(expected["runtimeState"], "runtimeState"),
            "|".join((exact_string(expected["reservationState"], "reservationState"), exact_string(expected["visibleState"], "visibleState"), exact_string(expected["reopenState"], "reopenState"))),
        )
        assert tuple_value[3] in OPERATION_OUTCOMES
        assert tuple_value[2] in {"m5_0", "platform_m5_0", "m5_1", "m7"}
        assert name not in observed, f"duplicate crash case {name}"
        observed[name] = tuple_value
    assert observed == CASE_EXPECTATIONS, (observed, CASE_EXPECTATIONS)


def check_constants() -> None:
    assert OPERATION_OUTCOMES == {
        "completed",
        "ordinary_error",
        "internal_dispatch_unavailable",
        "not_applicable",
        "record_not_recorded",
        "lost_to_process_abort",
        "lost_to_response",
    }
    assert MARKERS == {"format": "MINICORE_STORE_V1", "lock": ".minicore.lock", "published": "PUBLISHED", "committed": "COMMITTED"}
    assert PATHS == {"agent": "agents/<AgentId>", "session": "sessions/<SessionId>", "conversation": "sessions/<SessionId>/conversation.jsonl", "generation": "generations/<20-digit StorageGeneration>"}
    assert GENERATION_DIRS == {1: "00000000000000000001", 2: "00000000000000000002", 3: "00000000000000000003"}
    for generation, name in GENERATION_DIRS.items():
        assert len(name) == 20 and name.isdecimal() and int(name) == generation

    # Constants are meaningful only if the declared assets actually realize
    # them. Keep this asset/manifest validation here rather than comparing
    # isolated literals with another isolated literal.
    expected_assets = set(DOCUMENTS) | set(JSONL_ASSETS) | {"manifest.json", "crash-matrix.json", "README.md", "verify.py"}
    entries = list(ROOT.iterdir())
    assert all(path.is_file() and not path.is_symlink() for path in entries), entries
    assert {path.name for path in entries} == expected_assets
    manifest = load_document("manifest.json")
    expect_keys(manifest, ["version", "documents", "jsonl", "generationCases", "crashMatrix"], "constants manifest")
    assert manifest["version"] == 1 and tuple(manifest["documents"]) == DOCUMENTS
    assert tuple(manifest["jsonl"]) == JSONL_ASSETS and manifest["crashMatrix"] == "crash-matrix.json"
    assert tuple(manifest["generationCases"]) == GENERATION_CASES


def main() -> int:
    try:
        check_constants()
        manifest = check_manifest()
        check_goldens(manifest)
        check_crash_matrix()
    except (AssertionError, OSError) as error:
        print(f"durable-store-v1 fixture verification failed: {error}", file=sys.stderr)
        return 1
    print("durable-store-v1 fixtures verified")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
