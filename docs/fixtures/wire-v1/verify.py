#!/usr/bin/env python3
"""Structural smoke checks for the wire-v1 documentation fixtures."""

from __future__ import annotations

import base64
import json
import re
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent
RUNTIME_ID = re.compile(r"^(agt|ses|trn|itm|req|ent|cmd|irk)_[0-9a-f]{32}$")
REVISION = re.compile(r"^(ar|sdr|amr|smr|wr)_([1-9][0-9]*)$")
PAGE_CURSOR = re.compile(r"^pc1_[A-Za-z0-9_-]{43}$")
U64_MAX = 18_446_744_073_709_551_615
RUNTIME_ID_PREFIX_BY_KEY = {
    "agentId": "agt",
    "sessionId": "ses",
    "sourceSessionId": "ses",
    "turnId": "trn",
    "expectedTurnId": "trn",
    "itemId": "itm",
    "requestId": "req",
    "entryId": "ent",
    "parentId": "ent",
    "firstKeptEntryId": "ent",
    "commandId": "cmd",
    "targetCommandId": "cmd",
    "resolutionKey": "irk",
    "acceptedEntryIds": "ent",
    "selectedPath": "ent",
    "historicalItemIds": "itm",
    "removedCommandIds": "cmd",
}
ENTRY_FIELDS = ["entryId", "parentId", "sessionId", "turnId", "timestamp", "body"]
HEADER_FIELDS = [
    "formatVersion",
    "sessionId",
    "createdAt",
    "initialAgent",
    "initialDefinitionRevision",
]

EXPECTED_LIMITS = {
    "transport": {
        "maxRequestBytes": 1_048_576,
        "maxResponseBytes": 8_388_608,
        "maxRuntimeSnapshotBytes": 8_388_608,
        "maxSessionSnapshotBytes": 8_388_608,
        "maxStateEventBytes": 8_388_608,
        "maxProgressEventBytes": 65_536,
        "maxJsonDepth": 64,
        "maxObjectMembers": 256,
        "maxArrayItems": 4_096,
        "maxStringBytes": 262_144,
    },
    "text": {
        "maxTextIntentBytes": 131_072,
        "maxCommandInputBytes": 32_768,
        "maxCommandOutputBytes": 65_536,
        "maxDisplayNameBytes": 256,
        "maxDescriptionBytes": 8_192,
        "maxPublicSummaryBytes": 8_192,
        "maxDiagnosticCodeBytes": 64,
        "maxDiagnosticMessageBytes": 2_048,
    },
    "catalog": {
        "maxCommandPathSegments": 16,
        "maxCommandArguments": 64,
        "maxCommandCatalogEntries": 1_024,
    },
    "paging": {"maxPageSize": 200, "maxPageCursorBytes": 256},
    "prompt": {
        "maxSkillsPerIntent": 32,
        "maxUserMessageParts": 64,
        "maxMessagePartBytes": 131_072,
        "maxUserMessageBytes": 524_288,
    },
    "workspace": {
        "maxWorkspaceRoots": 16,
        "maxAbsolutePathUriBytes": 8_192,
        "maxRelativePathBytes": 4_096,
        "maxRelativePathSegments": 256,
    },
    "queues": {"maxSubmitAdmissions": 16, "maxSteers": 32, "maxFollowUps": 32},
    "interaction": {
        "maxToolApprovalOptions": 16,
        "maxInteractionQuestions": 32,
        "maxChoicesPerQuestion": 64,
        "maxAnswerTextBytes": 16_384,
        "maxInteractionAnswerBytes": 65_536,
        "maxInteractionViewBytes": 131_072,
    },
    "observation": {
        "maxActiveItems": 64,
        "maxItemViewBytes": 65_536,
        "maxPendingInteractions": 16,
        "maxSnapshotDiagnostics": 50,
        "maxQueryDiagnosticsPerScope": 100,
    },
    "embeddedJson": {
        "value": {
            "maxEncodedBytes": 65_536,
            "maxDepth": 32,
            "maxArrayItems": 256,
            "maxObjectMembers": 256,
            "maxStringBytes": 16_384,
            "maxNumberLiteralBytes": 64,
        },
        "schema": {
            "maxEncodedBytes": 65_536,
            "maxDepth": 32,
            "maxNodes": 4_096,
            "maxPropertiesRequiredOrEnumItems": 256,
            "maxRegexBytes": 1_024,
        },
    },
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


def decode(raw: bytes) -> Any:
    return json.loads(raw, object_pairs_hook=no_duplicates)


def walk(value: Any, key: str | None = None) -> None:
    if isinstance(value, str):
        expected_prefix = RUNTIME_ID_PREFIX_BY_KEY.get(key or "")
        if expected_prefix:
            match = RUNTIME_ID.fullmatch(value)
            assert match and match.group(1) == expected_prefix, f"invalid runtime ID: {value}"
            assert set(value.split("_", 1)[1]) != {"0"}, f"all-zero runtime ID: {value}"
        if key in {"revision", "expectedRevision", "definitionRevision", "metadataRevision", "initialDefinitionRevision"}:
            match = REVISION.fullmatch(value)
            assert match, f"invalid revision: {value}"
            assert int(match.group(2)) <= U64_MAX, f"revision overflow: {value}"
        if key == "cursor":
            assert PAGE_CURSOR.fullmatch(value), f"invalid page cursor: {value}"
            encoded = value.removeprefix("pc1_")
            decoded = base64.urlsafe_b64decode(encoded + "=")
            assert len(decoded) == 32, f"invalid page cursor payload: {value}"
            canonical = base64.urlsafe_b64encode(decoded).rstrip(b"=").decode()
            assert canonical == encoded, f"noncanonical page cursor: {value}"
    elif isinstance(value, list):
        for item in value:
            walk(item, key)
    elif isinstance(value, dict):
        if key == "arguments":
            check_dynamic_object(value)
        for child_key, item in value.items():
            walk(item, child_key)


def check_dynamic_object(value: dict[str, Any]) -> None:
    keys = list(value)
    assert keys == sorted(keys, key=lambda item: item.encode("utf-8")), f"unsorted dynamic JSON keys: {keys}"
    for child in value.values():
        if isinstance(child, dict):
            check_dynamic_object(child)
        elif isinstance(child, list):
            for item in child:
                if isinstance(item, dict):
                    check_dynamic_object(item)


def check_expected_message(message: dict[str, Any]) -> None:
    role = message.get("role")
    if role in {"user", "assistant"}:
        assert list(message) == ["role", "content"], message
        assert isinstance(message["content"], list) and message["content"], message
        allowed = {"text"} if role == "user" else {"reasoning", "text", "tool_call"}
        for part in message["content"]:
            assert list(part) == ["type", "data"] and part["type"] in allowed, part
            if part["type"] == "text":
                assert list(part["data"]) == ["text"] and isinstance(part["data"]["text"], str), part
            elif part["type"] == "reasoning":
                assert list(part["data"]) == [
                    "text",
                    "summary",
                    "encrypted",
                    "signature",
                    "providerItemId",
                ], part
            else:
                assert list(part["data"]) == ["toolCallId", "name", "arguments"], part
                check_dynamic_object(part["data"]["arguments"])
    elif role == "tool":
        assert list(message) == ["role", "toolCallId", "content"], message
        assert list(message["content"]) == ["parts"], message
        for part in message["content"]["parts"]:
            assert list(part) == ["type", "data"] and part["type"] == "text", part
            assert list(part["data"]) == ["text"], part
    else:
        raise AssertionError(f"unknown expected model role: {message}")


def leaf_paths(value: dict[str, Any], prefix: str = "") -> set[str]:
    result: set[str] = set()
    for key, child in value.items():
        path = f"{prefix}.{key}" if prefix else key
        if isinstance(child, dict):
            result.update(leaf_paths(child, path))
        else:
            result.add(path)
    return result


def remove_pointer(value: Any, pointer: str) -> None:
    parts = [part.replace("~1", "/").replace("~0", "~") for part in pointer.split("/")[1:]]
    current = value
    for part in parts[:-1]:
        current = current[int(part)] if isinstance(current, list) else current[part]
    final = parts[-1]
    if isinstance(current, list):
        del current[int(final)]
    else:
        del current[final]


def check_declared_public_fault(path: Path, expected: dict[str, Any]) -> None:
    code = expected.get("code")
    raw = path.read_bytes()
    if code == "duplicate_key":
        try:
            decode(raw)
        except DuplicateKey:
            return
        raise AssertionError(f"duplicate-key fixture has no duplicate: {path}")

    value = json.loads(raw)
    if code == "unknown_input_field":
        markers = {"future", "futureField", "extra"}
        assert any(marker.encode() in raw for marker in markers), path
    elif code == "wrong_json_type":
        assert not isinstance(value["commandId"], str), path
    elif code == "noncanonical_id":
        assert isinstance(value["commandId"], str)
        assert not RUNTIME_ID.fullmatch(value["commandId"]), path
    elif code == "unknown_input_variant":
        assert value["command"]["type"] == "future_command", path
    elif code == "duration_out_of_range":
        retry_after = value["completion"]["data"]["retry"]["data"]["retryAfter"]
        assert retry_after > 86_400_000, path
    elif code == "unknown_output_variant":
        assert value["type"] == "future_frame", path
    else:
        raise AssertionError(f"unverified declared public fault {code}: {path}")


def check_public() -> None:
    for path in sorted((ROOT / "public" / "valid").glob("*.json")):
        raw = path.read_bytes()
        assert raw.endswith(b"\n"), path
        value = decode(raw)
        walk(value)
        canonical = json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode()
        assert canonical == raw[:-1], f"noncanonical public golden: {path}"

    for family in ("input", "output"):
        for path in sorted((ROOT / "public" / "invalid" / family).glob("*.json")):
            if path.name == "duplicate-key.json":
                try:
                    decode(path.read_bytes())
                except DuplicateKey:
                    continue
                raise AssertionError(f"duplicate-key fixture has no duplicate: {path}")
            json.loads(path.read_bytes())

    manifest = decode((ROOT / "public" / "manifest.json").read_bytes())
    vectors = manifest["vectors"]
    paths = [vector["path"] for vector in vectors]
    assert len(paths) == len(set(paths)), "duplicate public manifest path"
    actual = {
        str(path.relative_to(ROOT / "public"))
        for path in (ROOT / "public").rglob("*.json")
        if path.name != "manifest.json"
    }
    assert set(paths) == actual, (set(paths) - actual, actual - set(paths))

    for vector in vectors:
        path = ROOT / "public" / vector["path"]
        expected = vector["expected"]
        assert path.exists(), path
        if expected.get("decode") in {"rejected", "protocol_error"}:
            check_declared_public_fault(path, expected)
        if expected.get("canonicalReencode") == "same_bytes":
            value = decode(path.read_bytes())
            canonical = json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode()
            assert canonical == path.read_bytes()[:-1], path
        if "ignoredJsonPointers" in expected:
            value = decode(path.read_bytes())
            for pointer in expected["ignoredJsonPointers"]:
                remove_pointer(value, pointer)
            canonical = json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode() + b"\n"
            target = ROOT / "public" / expected["canonicalReencodePath"]
            assert canonical == target.read_bytes(), (path, target)

    welcome = decode((ROOT / "public" / "valid" / "protocol-welcome.json").read_bytes())
    assert welcome["type"] == "welcome"
    assert welcome["data"]["limits"] == EXPECTED_LIMITS

    file_uris = decode((ROOT / "public" / "carriers" / "file-uri.json").read_bytes())
    assert file_uris["target"] == "CanonicalFileUri"
    valid_wires = [case["wire"] for case in file_uris["valid"]]
    invalid_wires = [case["wire"] for case in file_uris["invalid"]]
    assert len(valid_wires) == len(set(valid_wires))
    assert len(invalid_wires) == len(set(invalid_wires))
    assert not (set(valid_wires) & set(invalid_wires))
    for case in file_uris["valid"]:
        assert list(case) == ["wire", "family", "authority", "decodedPath"], case
        assert case["wire"].startswith("file://") and len(case["wire"].encode()) <= 8_192
        if case["family"] == "posix":
            assert case["authority"] is None and case["decodedPath"].startswith("/"), case
        elif case["family"] == "drive":
            assert case["authority"] is None and re.match(r"^[A-Z]:/", case["decodedPath"]), case
        elif case["family"] == "unc":
            assert isinstance(case["authority"], str) and case["authority"] == case["authority"].lower(), case
            assert case["decodedPath"] and not case["decodedPath"].startswith("/"), case
        else:
            raise AssertionError(case)
    for case in file_uris["invalid"]:
        assert list(case) == ["wire", "reason"] and case["reason"], case

    negotiation = decode((ROOT / "public" / "protocol-negotiation-cases.json").read_bytes())
    runtime_versions = {
        (version["major"], version["minor"])
        for version in negotiation["runtimeSupportedVersions"]
    }
    runtime_capabilities = negotiation["runtimeCapabilities"]
    for case in negotiation["cases"]:
        hello = decode((ROOT / "public" / case["helloPath"]).read_bytes())
        response = decode((ROOT / "public" / case["expectedResponsePath"]).read_bytes())
        client_versions = {
            (version["major"], version["minor"])
            for version in hello["supportedVersions"]
        }
        common = sorted(runtime_versions & client_versions)
        if not common:
            assert response["type"] == "reject"
            assert response["data"]["reason"] == case["expectedRejectReason"]
            assert response["data"]["supportedVersions"] == negotiation["runtimeSupportedVersions"]
            continue
        selected = {"major": common[-1][0], "minor": common[-1][1]}
        client_capabilities = set(hello["capabilities"]["values"])
        expected_capabilities = [
            capability
            for capability in runtime_capabilities
            if capability in client_capabilities
        ]
        assert selected == case["expectedSelectedVersion"]
        assert expected_capabilities == case["expectedCapabilities"]
        assert response["type"] == "welcome"
        assert response["data"]["selectedVersion"] == selected
        assert response["data"]["runtime"]["protocolVersion"] == selected
        assert response["data"]["capabilities"]["values"] == expected_capabilities


def iter_complete_lines(path: Path) -> list[bytes]:
    raw = path.read_bytes()
    parts = raw.split(b"\n")
    complete = parts[:-1]
    return [line[:-1] if line.endswith(b"\r") else line for line in complete]


def check_jsonl() -> None:
    golden = ROOT / "conversation" / "golden"
    for path in sorted(golden.glob("*.jsonl")):
        raw = path.read_bytes()
        assert raw.endswith(b"\n"), path
        header_session: str | None = None
        for number, line in enumerate(raw.split(b"\n")[:-1], 1):
            value = decode(line)
            walk(value)
            canonical = json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode()
            assert canonical == line, f"noncanonical JSONL golden: {path}:{number}"
            if value["type"] == "session_header":
                assert number == 1
                assert list(value["data"]) == HEADER_FIELDS
                header_session = value["data"]["sessionId"]
            else:
                assert value["type"] == "entry", (path, number)
                assert list(value["data"]) == ENTRY_FIELDS, (path, number)
                assert value["data"]["sessionId"] == header_session, (path, number)

    for sidecar in sorted(golden.glob("*.expected.json")):
        trace = decode(sidecar.read_bytes())
        jsonl = sidecar.with_name(sidecar.name.removesuffix(".expected.json") + ".jsonl")
        rows = [decode(line) for line in jsonl.read_bytes().split(b"\n")[:-1]]
        entries = [row for row in rows if row.get("type") == "entry"]
        positions = {row["data"]["entryId"]: index for index, row in enumerate(entries)}
        ask = trace["assertions"]["askUserExclusiveFirst"]
        required_order = ask["requiredPhysicalOrder"]
        assert [positions[entry_id] for entry_id in required_order] == sorted(
            positions[entry_id] for entry_id in required_order
        ), sidecar
        assistant = next(row for row in entries if row["data"]["entryId"] == ask["assistantEntryId"])
        assistant_calls = [
            part["data"]["toolCallId"]
            for part in assistant["data"]["body"]["data"]["content"]
            if part["type"] == "tool_call"
        ]
        terminal_calls = [
            row["data"]["body"]["data"]["toolCallId"]
            for row in entries
            if row["data"]["body"]["type"] == "tool_message"
        ]
        ordering = trace["assertions"]["toolOrdering"]
        assert assistant_calls == ordering["modelVisibleOrder"], sidecar
        assert terminal_calls == ordering["physicalTerminalOrder"], sidecar

    corruption = ROOT / "conversation" / "corruption"
    replay_codes = {
        "partial_tail",
        "oversized_line",
        "invalid_utf8",
        "malformed_json",
        "invalid_entry",
        "unknown_record_variant",
        "unknown_entry_variant",
        "duplicate_entry_id",
        "missing_parent",
        "session_mismatch",
        "invalid_relation",
        "invalid_contribution_stamp",
        "duplicate_contribution_stamp",
        "invalid_tool_exchange",
        "invalid_interaction_relation",
        "invalid_compaction_marker",
        "diagnostics_truncated",
        "history_too_large",
    }
    for path in sorted(corruption.glob("*.expected.json")):
        value = decode(path.read_bytes())
        walk(value)
        common = {
            "load",
            "acceptedEntryIds",
            "selectedPath",
            "sanitizedModelMessages",
            "historicalItemIds",
            "tail",
        }
        assert common <= set(value), (path, common - set(value))
        assert "diagnostics" in value or "diagnosticsByMode" in value, path
        if value["load"] == "fails":
            assert {"openedSessionId", "error"} <= set(value), path
        for message in value["sanitizedModelMessages"]:
            assert isinstance(message, dict), path
            check_expected_message(message)
        if path.name == "entry-session-mismatch.expected.json":
            assert value["identityAssertions"] == {
                "mismatchedLineReservesEntryId": False,
                "laterMatchingSessionReuseAccepted": True,
            }
        accepted = value.get("acceptedEntryIds", [])
        selected = value.get("selectedPath", [])
        assert len(accepted) == len(set(accepted)), path
        assert set(selected) <= set(accepted), path
        diagnostic_groups = []
        diagnostic_groups.extend(value.get("diagnostics", []))
        for group in value.get("diagnosticsByMode", {}).values():
            diagnostic_groups.extend(group)
        assert all(item["code"] in replay_codes for item in diagnostic_groups), path

    intentionally_malformed = {"duplicate-header-key.jsonl", "malformed-middle.jsonl"}
    for path in sorted(corruption.glob("*.jsonl")):
        header_session: str | None = None
        observed_entry_ids: set[str] = set()
        for number, line in enumerate(iter_complete_lines(path), 1):
            try:
                value = decode(line)
            except (DuplicateKey, json.JSONDecodeError, UnicodeDecodeError):
                assert path.name in intentionally_malformed, (path, number)
                continue
            walk(value)
            if value.get("type") == "session_header":
                header_session = value["data"].get("sessionId")
            elif value.get("type") == "entry":
                observed_entry_ids.add(value["data"]["entryId"])
                assert list(value["data"]) == ENTRY_FIELDS or "futureField" in value["data"], (path, number)
                assert "sessionId" in value["data"], (path, number)
                if path.name == "entry-session-mismatch.jsonl" and number == 2:
                    assert value["data"]["sessionId"] != header_session, (path, number)
                else:
                    assert value["data"]["sessionId"] == header_session, (path, number)
        expected_path = path.with_name(f"{path.stem}.expected.json")
        assert expected_path.exists(), expected_path
        expected = decode(expected_path.read_bytes())
        assert set(expected.get("acceptedEntryIds", [])) <= observed_entry_ids, path

    partial = corruption / "partial-tail.jsonl"
    expected = decode((corruption / "partial-tail.expected.json").read_bytes())
    raw = partial.read_bytes()
    assert not raw.endswith(b"\n")
    assert raw.rfind(b"\n") + 1 == expected["tail"]["truncateOffset"]

    crlf = (corruption / "crlf-input.jsonl").read_bytes()
    canonical = (golden / "crlf-canonical.jsonl").read_bytes()
    assert b"\r\n" in crlf
    assert crlf.replace(b"\r\n", b"\n") == canonical


def main() -> None:
    check_public()
    check_jsonl()
    boundaries = decode((ROOT / "recipes" / "boundary-cases.json").read_bytes())
    names = [case["name"] for case in boundaries["cases"]]
    assert len(names) == len(set(names)), "duplicate boundary case"
    required = {
        "header_line_boundary",
        "header_line_oversized",
        "entry_line_boundary",
        "entry_line_oversized",
        "file_boundary",
        "file_oversized",
        "complete_entry_count_boundary",
        "complete_entry_count_oversized",
        "bounded_json_input_bytes_boundary",
        "bounded_json_input_bytes_oversized",
        "bounded_json_output_bytes_boundary",
        "bounded_json_output_bytes_oversized",
        "bounded_schema_input_bytes_boundary",
        "bounded_schema_input_bytes_oversized",
        "bounded_schema_output_bytes_boundary",
        "bounded_schema_output_bytes_oversized",
        "bounded_schema_depth_boundary",
        "bounded_schema_depth_oversized",
        "bounded_schema_properties_boundary",
        "bounded_schema_properties_oversized",
        "bounded_schema_required_boundary",
        "bounded_schema_required_oversized",
        "bounded_schema_enum_boundary",
        "bounded_schema_enum_oversized",
        "diagnostic_detail_cap",
    }
    assert required <= set(names), required - set(names)
    protocol_cases = decode((ROOT / "recipes" / "protocol-limit-cases.json").read_bytes())
    assert protocol_cases["limits"] == EXPECTED_LIMITS
    assert protocol_cases["probeContract"] == {
        "operation": "invoke_named_owner_validator_with_measured_metric",
        "context": "minimal_valid_non_target_fields",
        "payloadGeneration": "not_implied; integration payloads live in boundary-cases.json and public manifest vectors",
        "assertion": "boundary passes this validator; boundaryPlusOne fails this validator",
    }
    assert protocol_cases["forEachLeaf"]["boundary"] == "accepted_by_owning_limit_validator"
    assert protocol_cases["forEachLeaf"]["boundaryPlusOne"] == "rejected_by_owning_limit_validator"
    cursor_case = protocol_cases["specialCases"]["paging.maxPageCursorBytes"]
    assert cursor_case["canonicalV1CarrierBytes"] == 47
    assert cursor_case["target"] == "PageCursor input allocation buffer"
    diagnostic_case = protocol_cases["specialCases"]["text.maxDiagnosticMessageBytes"]
    assert diagnostic_case["replayDetailBytes"] == 512
    for key in ("embeddedJson.value.maxEncodedBytes", "embeddedJson.schema.maxEncodedBytes"):
        assert set(protocol_cases["specialCases"][key]["integrationCases"]) <= set(names)
    for path in leaf_paths(EXPECTED_LIMITS):
        matches = []
        for selector in protocol_cases["validatorSelectors"]:
            if path in selector.get("paths", []):
                matches.append(selector["validator"])
            if path.startswith(selector.get("pathPrefix", "\0")):
                matches.append(selector["validator"])
        assert len(matches) == 1, (path, matches)
    print("wire-v1 fixture checks passed")


if __name__ == "__main__":
    main()
