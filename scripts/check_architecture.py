#!/usr/bin/env python3
"""Static architecture and quality gates for the canonical v0.2 source graph."""
from __future__ import annotations

import bisect
import re
import sys
from collections import Counter
from pathlib import Path
from typing import Dict, Iterable, List, Optional, Sequence, Set, Tuple

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "src"
# Source gates enumerate only the canonical src tree and the active acceptance
# source; they never traverse .git, target, provider-gate target, or docs/archive.

CANONICAL_TOPS = (
    "agent",
    "compaction",
    "config",
    "conversation",
    "context",
    "error",
    "event",
    "ids",
    "model",
    "prompt",
    "runtime",
    "session",
    "storage",
    "time",
    "tools",
    "value",
    "workspace",
)

REQUIRED_DIRS = {
    "src/agent",
    "src/compaction",
    "src/conversation",
    "src/context",
    "src/model",
    "src/model/providers",
    "src/prompt",
    "src/runtime",
    "src/session",
    "src/storage",
    "src/storage/conversation",
    "src/tools",
    "src/tools/builtins",
    "src/workspace",
}

REQUIRED_FILES = {
    "src/agent/context.rs",
    "src/agent/mod.rs",
    "src/agent/runner.rs",
    "src/config.rs",
    "src/config/kernel.rs",
    "src/config/retry.rs",
    "src/config/session.rs",
    "src/config/session_spec.rs",
    "src/conversation/entry.rs",
    "src/conversation/load.rs",
    "src/conversation/log.rs",
    "src/conversation/log/append_support.rs",
    "src/conversation/log/append_tests.rs",
    "src/conversation/log/load_support.rs",
    "src/conversation/log/replay_tests.rs",
    "src/conversation/log/recovery_tests.rs",
    "src/conversation/log/transcript_close_tests.rs",
    "src/conversation/mod.rs",
    "src/conversation/projection.rs",
    "src/conversation/recovery.rs",
    "src/conversation/state.rs",
    "src/conversation/transcript.rs",
    "src/conversation/validator.rs",
    "src/conversation/view.rs",
    "src/compaction/mod.rs",
    "src/compaction/strategy.rs",
    "src/context/mod.rs",
    "src/context/provider.rs",
    "src/conversation/validator/tests.rs",
    "src/error.rs",
    "src/event.rs",
    "src/ids.rs",
    "src/lib.rs",
    "src/model/gateway.rs",
    "src/model/mod.rs",
    "src/model/provider.rs",
    "src/model/providers/anthropic.rs",
    "src/model/providers/mod.rs",
    "src/model/providers/openai.rs",
    "src/model/registry.rs",
    "src/model/transport.rs",
    "src/model/types.rs",
    "src/prompt/builder.rs",
    "src/prompt/compaction.rs",
    "src/prompt/mod.rs",
    "src/runtime/mod.rs",
    "src/runtime/runtime_impl.rs",
    "src/runtime/session_manager.rs",
    "src/session/actor.rs",
    "src/session/command.rs",
    "src/session/event.rs",
    "src/session/event_stream.rs",
    "src/session/mod.rs",
    "src/session/snapshot.rs",
    "src/session/state.rs",
    "src/session/transcript.rs",
    "src/storage/compaction_visibility.rs",
    "src/storage/conversation.rs",
    "src/storage/conversation/actor_support.rs",
    "src/storage/conversation/codec.rs",
    "src/storage/conversation/compaction.rs",
    "src/storage/conversation/usage.rs",
    "src/storage/mod.rs",
    "src/storage/session_log.rs",
    "src/storage/store.rs",
    "src/time.rs",
    "src/tools/builtins/ask_user.rs",
    "src/tools/builtins/list_directory.rs",
    "src/tools/builtins/mod.rs",
    "src/tools/builtins/path_args.rs",
    "src/tools/builtins/read_file.rs",
    "src/tools/builtins/run_command.rs",
    "src/tools/builtins/write_file.rs",
    "src/tools/context.rs",
    "src/tools/mod.rs",
    "src/tools/policy.rs",
    "src/tools/process.rs",
    "src/tools/registry.rs",
    "src/tools/types.rs",
    "src/value.rs",
    "src/workspace/mod.rs",
    "src/workspace/path.rs",
    "src/workspace/root.rs",
}

LEGACY_SOURCE_PATHS = {
    "src/agent_session_lifecycle.rs",
    "src/agent_v2",
    "src/compaction.rs",
    "src/conversation_storage.rs",
    "src/durable_state.rs",
    "src/http_transport.rs",
    "src/live_conversation.rs",
    "src/model_gateway.rs",
    "src/model_gateway",
    "src/prompt.rs",
    "src/prompt_v2",
    "src/runtime.rs",
    "src/runtime_interface.rs",
    "src/runtime_task.rs",
    "src/runtime_v2",
    "src/session_execution.rs",
    "src/session_ingress.rs",
    "src/session_residency.rs",
    "src/session_transcript.rs",
    "src/skills.rs",
    "src/tools.rs",
    "src/tools/ask_user.rs",
    "src/tools/fetch_url.rs",
    "src/tools/list_directory.rs",
    "src/tools/read_file.rs",
    "src/tools/write_file.rs",
    "src/turn_execution_context.rs",
    "src/turn_item_interaction.rs",
    "src/wire",
    "src/workspace.rs",
    "src/workspace_v2",
}

FORBIDDEN_SOURCE_TOKENS = (
    "crate::wire",
    "mod wire",
    "pub mod wire",
    "MiniCoreRuntime",
    "CommandRequest",
    "RuntimeQuery",
    "DurableState",
    "SessionResidency",
    "SessionIngress",
    "AgentRevisionRef",
    "ToolExecutionPlan",
    "ToolStartGate",
    "SessionFileMutationQueue",
    "WorkspaceAuthority",
    "PrepareUnload",
    "SecurityInvalidation",
    "RuntimeDependencyProbe",
    "_v2",
    "Fork",
    "Archive",
    "Steer",
    "FollowUp",
)

EXPECTED_ACCEPTANCE_CASES = (
    "AT-01 Model-only Turn",
    "AT-02 Read file",
    "AT-03 Edit file",
    "AT-04 Run tests",
    "AT-05 Multi-round tools",
    "AT-06 Ask user",
    "AT-07 Cancel model",
    "AT-08 Cancel process",
    "AT-09 Runtime restart",
    "AT-10 Partial JSONL",
    "AT-11 Compaction",
    "AT-12 Workspace security",
    "AT-13 Provider conformance",
    "AT-14 Session isolation",
    "AT-15 Event lag",
    "AT-16 Busy rule",
    "AT-17 Close",
    "AT-18 Custom Tool",
    "AT-19 Secret env",
    "AT-20 No legacy coupling",
)

EXPECTED_ACCEPTANCE_FUNCTIONS = (
    "at_01_model_only_turn",
    "at_02_read_file",
    "at_03_edit_file",
    "at_04_run_tests",
    "at_05_multi_round_tools",
    "at_06_ask_user",
    "at_07_cancel_model",
    "at_08_cancel_process",
    "at_09_runtime_restart",
    "at_10_partial_jsonl",
    "at_11_compaction",
    "at_12_workspace_security",
    "at_13_provider_conformance",
    "at_14_session_isolation",
    "at_15_event_lag",
    "at_16_busy_rule",
    "at_17_close",
    "at_18_custom_tool",
    "at_19_secret_env",
    "at_20_no_legacy_coupling",
)

EXPECTED_ROOT_EXPORTS = {
    "config": {
        "ConfigError",
        "CompactionConfig",
        "KernelConfig",
        "RetryPolicy",
        "RetryPolicyError",
        "RuntimeConfig",
        "RuntimeConfigBuilder",
        "SemanticLimits",
        "SessionConfig",
        "SessionManifest",
        "SessionSpec",
        "TurnOptions",
        "UserInput",
    },
    "conversation": {"ConversationEntry", "ConversationSeq", "TranscriptPage", "TurnTerminal"},
    "error": {
        "PublicErrorCode",
        "PublicErrorSummary",
        "RuntimeError",
        "SessionError",
    },
    "event": {"SessionEventKind"},
    "ids": {
        "IdError",
        "IdGenerationError",
        "InteractionId",
        "RuntimeIdError",
        "SessionId",
        "SessionInstanceId",
        "ToolCallId",
        "ToolCallIdError",
        "TurnId",
    },
    "runtime": {"Runtime", "SessionSummary"},
    "session": {
        "SessionEvent",
        "SessionEventStream",
        "SessionSnapshot",
        "SessionStatus",
        "SnapshotHistory",
        "SnapshotShapeError",
        "TerminalOutcome",
        "TranscriptEntry",
        "TranscriptToolCall",
        "TurnOutcome",
        "TurnSummary",
        "TurnTerminalSummary",
    },
    "storage": {"AppendReceipt", "ConversationPage", "SessionLog", "SessionLogError"},
    "value": {"BoundedText"},
}

EXPECTED_PUBLIC_MODULES = {
    "compaction",
    "config",
    "conversation",
    "context",
    "error",
    "event",
    "ids",
    "model",
    "runtime",
    "session",
    "storage",
    "tools",
    "value",
    "workspace",
}

EXPECTED_DIRECT_DEPENDENCIES = {
    "cap-std",
    "cap-primitives",
    "getrandom",
    "serde",
    "serde_json",
    "thiserror",
    "time",
    "tokio",
    "tokio-util",
    "fs4",
    "futures-util",
    "reqwest",
}

DIRECT_DEP_CONSUMERS = {
    "cap-std": [("src/workspace/root.rs", "use cap_std::fs::")],
    "cap-primitives": [
        ("src/workspace/root.rs", "use cap_primitives::fs::FollowSymlinks;"),
    ],
    "getrandom": [("src/ids.rs", "getrandom::fill")],
    "serde": [("src/error.rs", "use serde::")],
    "serde_json": [("src/model/types.rs", "use serde_json::Value")],
    "thiserror": [("src/config.rs", "use thiserror::Error;")],
    "time": [
        ("src/time.rs", "use ::time::OffsetDateTime;"),
        ("src/time.rs", "use ::time::format_description::well_known::Rfc3339;"),
    ],
    "tokio": [("src/runtime/runtime_impl.rs", "use tokio::runtime::Handle;")],
    "tokio-util": [
        ("src/agent/context.rs", "use tokio_util::sync::CancellationToken;"),
    ],
    "fs4": [("src/storage/store.rs", "use fs4::fs_std::FileExt;")],
    "futures-util": [("src/agent/runner.rs", "use futures_util::FutureExt;")],
    "reqwest": [("src/model/providers/openai.rs", "use reqwest::header")],
}

REMOVED_DEPENDENCIES = ("base64", "regex-syntax", "same-file", "file-id")
FORBIDDEN_MANIFEST_TOKENS = ("heavy-tests", "raw_value", "arbitrary_precision")

EXPECTED_MODULE_VISIBILITY = {
    "src/agent/mod.rs": {"context": "private", "runner": "private"},
    "src/conversation/mod.rs": {
        "entry": "private",
        "load": "private",
        "log": "private",
        "projection": "private",
        "recovery": "private",
        "state": "private",
        "transcript": "public",
        "validator": "private",
        "view": "private",
    },
    "src/compaction/mod.rs": {"strategy": "private"},
    "src/context/mod.rs": {"provider": "private"},
    "src/model/mod.rs": {
        "gateway": "private",
        "provider": "private",
        "providers": "private",
        "registry": "private",
        "transport": "crate",
        "types": "private",
    },
    "src/model/providers/mod.rs": {"anthropic": "private", "openai": "private"},
    "src/prompt/mod.rs": {"builder": "private", "compaction": "private"},
    "src/runtime/mod.rs": {"runtime_impl": "private", "session_manager": "private"},
    "src/session/mod.rs": {
        "actor": "crate",
        "command": "crate",
        "event": "private",
        "event_stream": "crate",
        "snapshot": "private",
        "state": "private",
        "transcript": "crate",
    },
    "src/storage/mod.rs": {
        "conversation": "crate",
        "compaction_visibility": "private",
        "session_log": "private",
        "store": "crate",
    },
    "src/storage/conversation.rs": {
        "actor_support": "private",
        "codec": "private",
        "compaction": "private",
        "usage": "private",
    },
    "src/tools/mod.rs": {
        "builtins": "private",
        "context": "private",
        "policy": "private",
        "process": "private",
        "registry": "private",
        "types": "private",
    },
    "src/tools/builtins/mod.rs": {
        "ask_user": "private",
        "list_directory": "private",
        "path_args": "private",
        "read_file": "private",
        "run_command": "private",
        "write_file": "private",
    },
    "src/workspace/mod.rs": {"path": "private", "root": "private"},
}

FUNCTION_RE = re.compile(
    r"(?m)^[ \t]*(?:(?:pub(?:\([^)]*\))?|async|unsafe|const)\s+)*"
    r"fn\s+([A-Za-z_][A-Za-z0-9_]*)\b"
)
CRATE_DIRECT_RE = re.compile(r"\bcrate\s*::\s*([A-Za-z_][A-Za-z0-9_]*)\b")
DELETED_TOP_MODULES = {
    "agent_session_lifecycle",
    "agent_v2",
    "compaction",
    "conversation_storage",
    "durable_state",
    "http_transport",
    "live_conversation",
    "model_gateway",
    "prompt_v2",
    "runtime_interface",
    "runtime_task",
    "session_execution",
    "session_ingress",
    "session_residency",
    "session_transcript",
    "skills",
    "runtime_v2",
    "wire",
    "workspace_v2",
}
ALLOW_ATTRIBUTE_RE = re.compile(r"#\[\s*allow\s*\((.*?)\)\s*\]", re.DOTALL)


def relative(path: Path) -> str:
    return path.relative_to(ROOT).as_posix()


def read_utf8(path: Path, errors: List[str]) -> Optional[str]:
    try:
        return path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as error:
        errors.append(f"{relative(path)}: cannot read as UTF-8: {error}")
        return None


def source_files(errors: List[str]) -> Dict[str, str]:
    result: Dict[str, str] = {}
    for path in sorted(SRC.rglob("*.rs")):
        text = read_utf8(path, errors)
        if text is not None:
            result[relative(path)] = text
    return result


def check_source_paths(source_paths: Set[str]) -> List[str]:
    errors: List[str] = []
    for directory in sorted(REQUIRED_DIRS):
        if not (ROOT / directory).is_dir():
            errors.append(f"missing required source directory: {directory}")
    for path in sorted(REQUIRED_FILES):
        if not (ROOT / path).is_file():
            errors.append(f"missing required source file: {path}")

    expected = set(REQUIRED_FILES)
    missing = sorted(expected - source_paths)
    unexpected = sorted(source_paths - expected)
    if missing:
        errors.append(f"canonical source files missing: {', '.join(missing)}")
    if unexpected:
        errors.append(f"unexpected canonical source files: {', '.join(unexpected)}")

    for path in sorted(LEGACY_SOURCE_PATHS):
        if (ROOT / path).exists():
            errors.append(f"legacy source path still exists: {path}")
    for path in sorted(SRC.rglob("*")):
        path_name = relative(path)
        if "_v2" in path_name:
            errors.append(f"legacy _v2 source path still exists: {path_name}")
    return errors


def check_source_tokens(sources: Dict[str, str]) -> List[str]:
    errors: List[str] = []
    for path in sorted(sources):
        text = sources[path]
        if "#[path" in text:
            errors.append(f"{path}: #[path] module aliases are forbidden")
        for token in FORBIDDEN_SOURCE_TOKENS:
            if token in text:
                errors.append(f"{path}: forbidden source token {token}")
        for attribute in ALLOW_ATTRIBUTE_RE.finditer(text):
            if re.search(r"(?:^|[\s,])(?:clippy::)?dead_code(?:$|[\s,])", attribute.group(1)):
                line = text.count("\n", 0, attribute.start()) + 1
                errors.append(f"{path}:{line}: dead_code allow is forbidden")
    return errors


def check_acceptance_surface() -> List[str]:
    path = ROOT / "tests/v2_acceptance.rs"
    errors: List[str] = []
    if not path.is_file():
        return ["missing acceptance source: tests/v2_acceptance.rs"]
    text = read_utf8(path, errors)
    if text is None:
        return errors
    if re.search(r"#\s*\[\s*ignore\b", text):
        errors.append("tests/v2_acceptance.rs: acceptance tests must not use #[ignore]")

    for case in EXPECTED_ACCEPTANCE_CASES:
        count = text.count('"' + case + '"')
        if count != 1:
            errors.append(
                f"tests/v2_acceptance.rs: {case!r} must occur exactly once, found {count}"
            )

    found_functions = Counter(
        match.group(1)
        for match in FUNCTION_RE.finditer(text)
        if match.group(1).startswith("at_")
    )
    expected_functions = set(EXPECTED_ACCEPTANCE_FUNCTIONS)
    for name in EXPECTED_ACCEPTANCE_FUNCTIONS:
        if found_functions[name] != 1:
            errors.append(
                f"tests/v2_acceptance.rs: {name} must occur exactly once, "
                f"found {found_functions[name]}"
            )
    unexpected = sorted(set(found_functions) - expected_functions)
    if unexpected:
        errors.append(
            "tests/v2_acceptance.rs: unexpected acceptance functions: "
            + ", ".join(unexpected)
        )
    return errors


def parse_root_pub_uses(
    text: str,
) -> Tuple[Dict[str, Set[str]], List[str], List[str]]:
    text = mask_rust(text)
    parsed: Dict[str, Set[str]] = {}
    owners: List[str] = []
    unsupported: List[str] = []
    statements = re.finditer(r"(?s)\bpub\s+use\b.*?;", text)
    grouped = re.compile(
        r"(?s)\s*pub\s+use\s+([A-Za-z_][A-Za-z0-9_]*)\s*::\s*\{(.*?)\}\s*;\s*"
    )
    plain = re.compile(
        r"(?s)\s*pub\s+use\s+([A-Za-z_][A-Za-z0-9_]*)\s*::\s*"
        r"([A-Za-z_][A-Za-z0-9_]*)\s*;\s*"
    )
    for statement_match in statements:
        statement = statement_match.group(0)
        grouped_match = grouped.fullmatch(statement)
        plain_match = plain.fullmatch(statement)
        if grouped_match is not None:
            owner = grouped_match.group(1)
            symbols = {
                symbol.strip()
                for symbol in grouped_match.group(2).split(",")
                if symbol.strip()
            }
        elif plain_match is not None:
            owner, symbol = plain_match.groups()
            symbols = {symbol}
        else:
            unsupported.append(" ".join(statement.split()))
            continue
        owners.append(owner)
        parsed.setdefault(owner, set()).update(symbols)
    return parsed, owners, unsupported


def parse_mod_declarations(text: str) -> Dict[str, str]:
    text = mask_rust(text)
    declarations: Dict[str, str] = {}
    pattern = re.compile(
        r"(?m)^\s*(?:(pub)\s*(?:\(\s*([^)]*?)\s*\))?\s+)?"
        r"mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*(?:;|\{)"
    )
    for match in pattern.finditer(text):
        public, scope, name = match.groups()
        scope = None if scope is None else "".join(scope.split())
        if public is None:
            visibility = "private"
        elif scope is None:
            visibility = "public"
        elif scope == "crate":
            visibility = "crate"
        else:
            visibility = f"pub({scope})"
        if name in declarations:
            declarations[name] = "<duplicate>"
        else:
            declarations[name] = visibility
    return declarations


def inline_mod_declarations(text: str) -> Dict[str, str]:
    text = mask_rust(text)
    declarations: Dict[str, str] = {}
    pattern = re.compile(
        r"(?m)^\s*(?:(pub)\s*(?:\(\s*([^)]*?)\s*\))?\s+)?"
        r"mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*\{"
    )
    for match in pattern.finditer(text):
        public, scope, name = match.groups()
        scope = None if scope is None else "".join(scope.split())
        if public is None:
            visibility = "private"
        elif scope is None:
            visibility = "public"
        elif scope == "crate":
            visibility = "crate"
        else:
            visibility = f"pub({scope})"
        declarations[name] = visibility
    return declarations


def strip_test_items(text: str) -> str:
    lines = text.splitlines(keepends=True)
    excluded, _ = cfg_test_spans(text)
    return "".join(
        line if index not in excluded else ("\n" if line.endswith("\n") else "")
        for index, line in enumerate(lines)
    )


def check_module_visibility(sources: Dict[str, str]) -> List[str]:
    errors: List[str] = []
    canonical_mod_files = {path for path in REQUIRED_FILES if path.endswith("/mod.rs")}
    mapped_mod_files = {
        path for path in EXPECTED_MODULE_VISIBILITY if path.endswith("/mod.rs")
    }
    if canonical_mod_files != mapped_mod_files:
        errors.append(
            "module visibility map must cover every canonical mod.rs: "
            f"missing={sorted(canonical_mod_files - mapped_mod_files)} "
            f"extra={sorted(mapped_mod_files - canonical_mod_files)}"
        )
    for path, expected in sorted(EXPECTED_MODULE_VISIBILITY.items()):
        text = sources.get(path, "")
        cleaned = strip_test_items(text)
        actual = parse_mod_declarations(cleaned)
        test_modules = cfg_test_spans(text)[1]
        expected_production = {
            name: visibility
            for name, visibility in expected.items()
            if name not in test_modules
        }
        if actual != expected_production:
            errors.append(
                f"{path}: module declaration visibility mismatch: "
                f"expected={expected_production} actual={actual}"
            )
        raw = parse_mod_declarations(text)
        for name in sorted(test_modules & set(expected)):
            if raw.get(name) != expected[name]:
                errors.append(
                    f"{path}: cfg(test) module visibility mismatch for {name}: "
                    f"expected={expected[name]} actual={raw.get(name)}"
                )
        inline = inline_mod_declarations(cleaned)
        for name, visibility in sorted(inline.items()):
            errors.append(
                f"{path}: production inline module {name} ({visibility}) is forbidden; "
                "use a file-backed module"
            )
    return errors


def check_public_surface(sources: Dict[str, str]) -> List[str]:
    errors: List[str] = []
    lib = sources.get("src/lib.rs", "")
    lib_clean = strip_test_items(lib)
    declarations = parse_mod_declarations(lib_clean)
    public_modules = {
        name for name, visibility in declarations.items() if visibility == "public"
    }
    private_modules = {
        name for name, visibility in declarations.items() if visibility == "private"
    }
    other_visibility = {
        name: visibility
        for name, visibility in declarations.items()
        if visibility not in {"private", "public"}
    }
    if public_modules != EXPECTED_PUBLIC_MODULES:
        errors.append(
            "src/lib.rs: public modules mismatch: "
            f"expected={sorted(EXPECTED_PUBLIC_MODULES)} actual={sorted(public_modules)}"
        )
    if private_modules != {"agent", "prompt", "time"}:
        errors.append(
            "src/lib.rs: private modules mismatch: "
            f"expected=['agent', 'prompt', 'time'] actual={sorted(private_modules)}"
        )
    if other_visibility:
        errors.append(f"src/lib.rs: unsupported module visibility: {other_visibility}")
    inline = inline_mod_declarations(lib_clean)
    for name, visibility in sorted(inline.items()):
        errors.append(
            f"src/lib.rs: production inline module {name} ({visibility}) is forbidden; "
            "use a file-backed module"
        )
    lib_masked = mask_rust(lib_clean)
    if re.search(r"(?m)^\s*pub\s+use\s+[^;]*::\s*\*\s*;", lib_masked):
        errors.append("src/lib.rs: glob root exports are forbidden")

    exports, export_owners, unsupported_exports = parse_root_pub_uses(lib_clean)
    duplicate_owners = sorted(
        owner for owner, count in Counter(export_owners).items() if count != 1
    )
    if duplicate_owners:
        errors.append(
            "src/lib.rs: each root reexport owner must have one exact block: "
            + ", ".join(duplicate_owners)
        )
    if unsupported_exports:
        errors.append(
            "src/lib.rs: unsupported root reexport syntax: "
            + ", ".join(unsupported_exports)
        )
    if exports != EXPECTED_ROOT_EXPORTS:
        errors.append(
            "src/lib.rs: root reexports mismatch: "
            f"expected={_format_mapping(EXPECTED_ROOT_EXPORTS)} "
            f"actual={_format_mapping(exports)}"
        )
    if set(exports) - set(EXPECTED_ROOT_EXPORTS):
        errors.append("src/lib.rs: root model/tools/workspace reexports are forbidden")

    errors.extend(check_module_visibility(sources))
    tools_mod = mask_rust(strip_test_items(sources.get("src/tools/mod.rs", "")))
    session_mod = mask_rust(strip_test_items(sources.get("src/session/mod.rs", "")))
    storage_mod = mask_rust(strip_test_items(sources.get("src/storage/mod.rs", "")))
    tools_declarations = parse_mod_declarations(tools_mod)
    if tools_declarations.get("builtins") != "private":
        errors.append("src/tools/mod.rs: builtins must remain private")
    session_declarations = parse_mod_declarations(session_mod)
    if session_declarations.get("actor") != "crate":
        errors.append("src/session/mod.rs: actor must be crate-private")
    storage_declarations = parse_mod_declarations(storage_mod)
    if storage_declarations.get("store") != "crate":
        errors.append("src/storage/mod.rs: store must be crate-private")
    actor_source = mask_rust(strip_test_items(sources.get("src/session/actor.rs", "")))
    store_source = mask_rust(strip_test_items(sources.get("src/storage/store.rs", "")))
    crate_struct = r"(?m)^\s*pub\s*\(\s*crate\s*\)\s+struct\s+{}\b"
    if not re.search(crate_struct.format("SessionActor"), actor_source):
        errors.append("src/session/actor.rs: SessionActor must be crate-private")
    if not re.search(crate_struct.format("SessionStore"), store_source):
        errors.append("src/storage/store.rs: SessionStore must be crate-private")
    return errors


def dependency_section(cargo: str, name: str) -> str:
    match = re.search(
        rf"(?ms)^\[{re.escape(name)}\]\s*\n(.*?)(?=^\[[^\]]+\]\s*$|\Z)", cargo
    )
    return match.group(1) if match else ""


def dependency_names(section: str) -> Set[str]:
    names: Set[str] = set()
    for line in section.splitlines():
        line = line.split("#", 1)[0].strip()
        match = re.match(r"([A-Za-z0-9_-]+)\s*=", line)
        if match:
            names.add(match.group(1))
    return names


def check_dependencies(sources: Dict[str, str]) -> Tuple[List[str], List[str]]:
    errors: List[str] = []
    cargo_path = ROOT / "Cargo.toml"
    cargo = read_utf8(cargo_path, errors) or ""
    section = dependency_section(cargo, "dependencies")
    names = dependency_names(section)
    if names != EXPECTED_DIRECT_DEPENDENCIES:
        errors.append(
            "Cargo.toml: direct dependency mismatch: "
            f"expected={sorted(EXPECTED_DIRECT_DEPENDENCIES)} actual={sorted(names)}"
        )

    for dependency, consumers in DIRECT_DEP_CONSUMERS.items():
        for path, token in consumers:
            text = sources.get(path)
            if text is None or token not in text:
                errors.append(
                    f"Cargo.toml: dependency {dependency} lacks consumer {path}:{token}"
                )

    for dependency in REMOVED_DEPENDENCIES:
        if re.search(rf"(?m)^\s*{re.escape(dependency)}\s*=", section):
            errors.append(f"Cargo.toml: removed direct dependency remains: {dependency}")
    if re.search(r"(?m)^\[features\]\s*$", cargo):
        errors.append("Cargo.toml: obsolete [features] table remains")
    for token in FORBIDDEN_MANIFEST_TOKENS:
        if re.search(rf"(?<![A-Za-z0-9_]){re.escape(token)}(?![A-Za-z0-9_])", cargo):
            errors.append(f"Cargo.toml: forbidden manifest token remains: {token}")

    reqwest = re.search(r"(?ms)^\s*reqwest\s*=\s*\{(.*?)\}", section)
    if reqwest is None:
        errors.append("Cargo.toml: reqwest must use an explicit inline dependency table")
    else:
        spec = reqwest.group(1)
        features_match = re.search(r"features\s*=\s*\[([^]]*)\]", spec)
        features = set(re.findall(r'"([^"]+)"', features_match.group(1))) if features_match else set()
        if "json" in features or features != {"rustls", "stream"}:
            errors.append(
                "Cargo.toml: reqwest features must be exactly rustls and stream, "
                f"found={sorted(features)}"
            )
        if not re.search(r"default-features\s*=\s*false", spec):
            errors.append("Cargo.toml: reqwest default-features=false is required")
        if not re.search(r'version\s*=\s*"=0.13.4"', spec):
            errors.append("Cargo.toml: reqwest version must remain exactly =0.13.4")

    return errors, sorted(names)


def _blank(chars: List[str], start: int, end: int) -> None:
    for index in range(start, end):
        if chars[index] not in "\r\n":
            chars[index] = " "


def mask_rust(text: str) -> str:
    """Mask comments and literals while preserving newlines and Rust braces."""
    chars = list(text)
    length = len(text)
    index = 0
    block_depth = 0
    while index < length:
        if block_depth:
            if text.startswith("/*", index):
                block_depth += 1
                _blank(chars, index, index + 2)
                index += 2
            elif text.startswith("*/", index):
                block_depth -= 1
                _blank(chars, index, index + 2)
                index += 2
            else:
                if text[index] not in "\r\n":
                    chars[index] = " "
                index += 1
            continue

        if text.startswith("//", index):
            end = text.find("\n", index)
            end = length if end == -1 else end
            _blank(chars, index, end)
            index = end
            continue
        if text.startswith("/*", index):
            block_depth = 1
            _blank(chars, index, index + 2)
            index += 2
            continue

        raw = re.match(r"(?:br|r)(#+)?\"", text[index:])
        if raw:
            hashes = raw.group(1) or ""
            delimiter = '"' + hashes
            content_start = index + len(raw.group(0))
            end = text.find(delimiter, content_start)
            end = length if end == -1 else end + len(delimiter)
            _blank(chars, index, end)
            index = end
            continue

        if text[index] == '"':
            end = index + 1
            escaped = False
            while end < length:
                character = text[end]
                if character == "\n":
                    break
                if character == '"' and not escaped:
                    end += 1
                    break
                if character == "\\" and not escaped:
                    escaped = True
                else:
                    escaped = False
                end += 1
            _blank(chars, index, end)
            index = end
            continue

        if text[index] == "'":
            end = index + 1
            escaped = False
            found = False
            while end < length and text[end] not in "\r\n":
                character = text[end]
                if character == "'" and not escaped:
                    found = True
                    end += 1
                    break
                if character == "\\" and not escaped:
                    escaped = True
                else:
                    escaped = False
                end += 1
            if found:
                _blank(chars, index, end)
                index = end
                continue

        index += 1
    return "".join(chars)


def matching_brace(masked: str, opening: int) -> Optional[int]:
    depth = 0
    for position in range(opening, len(masked)):
        if masked[position] == "{":
            depth += 1
        elif masked[position] == "}":
            depth -= 1
            if depth == 0:
                return position
    return None


def split_top_level(masked: str) -> List[str]:
    entries: List[str] = []
    start = 0
    depths = {"{": 0, "(": 0, "[": 0}
    closing = {"}": "{", ")": "(", "]": "["}
    for position, character in enumerate(masked):
        if character in depths:
            depths[character] += 1
        elif character in closing:
            depths[closing[character]] = max(0, depths[closing[character]] - 1)
        elif character == "," and not any(depths.values()):
            entries.append(masked[start:position])
            start = position + 1
    entries.append(masked[start:])
    return entries


def extract_crate_targets(text: str) -> Set[str]:
    """Return top-level crate imports; super:: cannot cross a top-level owner."""
    masked = mask_rust(text)
    targets = {match.group(1) for match in CRATE_DIRECT_RE.finditer(masked)}
    for match in re.finditer(r"\bcrate\s*::\s*\{", masked):
        opening = match.end() - 1
        closing = matching_brace(masked, opening)
        if closing is None:
            continue
        for entry in split_top_level(masked[opening + 1 : closing]):
            entry = re.sub(r"^\s*(?:pub(?:\([^)]*\))?\s+)?", "", entry)
            first = re.match(r"([A-Za-z_][A-Za-z0-9_]*)", entry.strip())
            if first and first.group(1) not in {"self", "super"}:
                targets.add(first.group(1))
    return targets


def allowed_session_log_conversation_use(path: str, text: str) -> bool:
    if path != "src/storage/session_log.rs":
        return False
    masked = mask_rust(text)
    statements = list(
        re.finditer(
            r"(?ms)^\s*(?:pub(?:\([^)]*\))?\s+)?use\s+crate\s*::\s*conversation\s*[^;]*;",
            masked,
        )
    )
    if len(statements) != 1:
        return False
    statement = statements[0]
    depth = 0
    for character in masked[: statement.start()]:
        if character == "{":
            depth += 1
        elif character == "}":
            depth -= 1
    if depth != 0:
        return False
    compact_statement = re.sub(r"\s+", "", statement.group(0))
    match = re.fullmatch(r"usecrate::conversation::\{([^{}]*)\};", compact_statement)
    if match is None:
        return False
    symbols = [symbol for symbol in match.group(1).split(",") if symbol]
    if len(symbols) != 2 or set(symbols) != {"ConversationEntry", "ConversationSeq"}:
        return False

    chars = list(masked)
    _blank(chars, statement.start(), statement.end())
    without_import = "".join(chars)
    if re.search(r"\bcrate\s*::\s*conversation\b", without_import):
        return False
    for group in re.finditer(r"\bcrate\s*::\s*\{", without_import):
        closing = matching_brace(without_import, group.end() - 1)
        if closing is not None and re.search(
            r"\bconversation\b", without_import[group.end() : closing]
        ):
            return False

    compact = re.sub(r"\s+", "", without_import)
    allowed_struct_fragments = (
        "pubstructConversationPage{pubentries:Vec<ConversationEntry>,"
        "pubnext_after:Option<ConversationSeq>,pubobserved_head:ConversationSeq,}",
        "pubstructAppendReceipt{pubprevious_head:ConversationSeq,"
        "pubnew_head:ConversationSeq,pubappended:usize,}",
    )
    if any(compact.count(fragment) != 1 for fragment in allowed_struct_fragments):
        return False
    trait_match = re.search(
        r"\bpub\s+trait\s+SessionLog\s*:\s*Send\s*\+\s*'static\s*\{",
        without_import,
    )
    if trait_match is None:
        return False
    trait_end = matching_brace(without_import, trait_match.end() - 1)
    if trait_end is None:
        return False
    trait_body = without_import[trait_match.end() : trait_end]
    methods = list(re.finditer(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)\b", trait_body))
    method_sections: Dict[str, str] = {}
    for index, method in enumerate(methods):
        end = methods[index + 1].start() if index + 1 < len(methods) else len(trait_body)
        method_sections[method.group(1)] = trait_body[method.start() : end]
    expected_methods = {
        "initialize": (1, 0, 0),
        "read_page": (1, 0, 1),
        "append": (1, 1, 1),
    }
    if any(name not in method_sections for name in expected_methods):
        return False
    for name, (seq_count, entry_count, named_result_count) in expected_methods.items():
        section = method_sections[name]
        if len(re.findall(r"\bConversationSeq\b", section)) != seq_count:
            return False
        if len(re.findall(r"\bConversationEntry\b", section)) != entry_count:
            return False
        result_name = "ConversationPage" if name == "read_page" else "AppendReceipt"
        if len(re.findall(rf"\b{result_name}\b", section)) != named_result_count:
            return False
    for name, section in method_sections.items():
        if name not in expected_methods and re.search(
            r"\bConversation(?:Entry|Seq|Page)\b", section
        ):
            return False
    if any(
        name not in method_sections
        for name in ("load_manifest", "close")
    ):
        return False
    return (
        len(re.findall(r"\bConversationEntry\b", compact)) == 2
        and len(re.findall(r"\bConversationSeq\b", compact)) == 7
        and len(re.findall(r"\bConversationPage\b", compact)) == 1
        and len(re.findall(r"\bAppendReceipt\b", compact)) == 1
    )


def omit_semantic_edge(path: str, target: str, text: str) -> bool:
    return target == "conversation" and allowed_session_log_conversation_use(path, text)


def root_type_import_errors(path: str, text: str) -> List[str]:
    masked = mask_rust(text)
    errors: List[str] = []
    if re.search(r"(?m)^\s*(?:pub\s+)?use\s+crate\s*::\s*[A-Z]", masked):
        errors.append(f"{path}: internal root-type reexport consumption is forbidden")
    for match in re.finditer(r"(?ms)^\s*(?:pub\s+)?use\s+crate\s*::\s*\{(.*?)\};", masked):
        for entry in split_top_level(match.group(1)):
            first = re.match(r"\s*([A-Za-z_][A-Za-z0-9_]*)", entry)
            if first and first.group(1)[0].isupper():
                errors.append(f"{path}: internal root-type reexport consumption is forbidden")
                break
    return errors


def line_starts(text: str) -> List[int]:
    starts = [0]
    for match in re.finditer("\n", text):
        starts.append(match.end())
    return starts


def line_for_position(starts: Sequence[int], position: int) -> int:
    return bisect.bisect_right(starts, position) - 1


def attribute_end(lines: Sequence[str], start: int) -> int:
    depth = 0
    seen = False
    for index in range(start, len(lines)):
        for character in lines[index]:
            if character == "[":
                depth += 1
                seen = True
            elif character == "]" and seen:
                depth -= 1
        if seen and depth <= 0:
            return index
    return len(lines) - 1


def split_cfg_arguments(value: str) -> List[str]:
    masked = mask_rust(value)
    entries: List[str] = []
    start = 0
    depths = {"(": 0, "{": 0, "[": 0}
    closing = {")": "(",
        "}": "{",
        "]": "[",
    }
    for position, character in enumerate(masked):
        if character in depths:
            depths[character] += 1
        elif character in closing:
            depths[closing[character]] -= 1
            if depths[closing[character]] < 0:
                return []
        elif character == "," and not any(depths.values()):
            entries.append(value[start:position].strip())
            start = position + 1
    if any(depths.values()):
        return []
    entries.append(value[start:].strip())
    return entries


def cfg_predicate_implies_test(predicate: str) -> bool:
    value = predicate.strip()
    if value == "test":
        return True
    match = re.fullmatch(r"([A-Za-z_][A-Za-z0-9_]*)\s*\((.*)\)", value, re.DOTALL)
    if match is None:
        return False
    operator, inner = match.groups()
    arguments = split_cfg_arguments(inner)
    if not arguments or any(argument == "" for argument in arguments):
        return False
    if operator == "all":
        return any(cfg_predicate_implies_test(argument) for argument in arguments)
    if operator == "any":
        return all(cfg_predicate_implies_test(argument) for argument in arguments)
    return False


def is_cfg_test_attribute(lines: Sequence[str], start: int) -> Tuple[bool, int]:
    end = attribute_end(lines, start)
    compact = re.sub(r"\s+", "", "".join(lines[start : end + 1]))
    if not (compact.startswith("#[cfg(") and compact.endswith(")]")):
        return False, end
    predicate = compact[len("#[cfg(") : -2]
    return cfg_predicate_implies_test(predicate), end


def item_end(masked: str, starts: Sequence[int], start_line: int) -> int:
    cursor = starts[start_line]
    depth = 0
    saw_body = False
    while cursor < len(masked):
        character = masked[cursor]
        if not saw_body:
            if character == ";":
                return line_for_position(starts, cursor)
            if character == "{":
                saw_body = True
                depth = 1
        else:
            if character == "{":
                depth += 1
            elif character == "}":
                depth -= 1
                if depth == 0:
                    return line_for_position(starts, cursor)
        cursor += 1
    return len(starts) - 1


def cfg_test_spans(text: str) -> Tuple[Set[int], Set[str]]:
    lines = text.splitlines(keepends=True)
    if not lines:
        return set(), set()
    starts = line_starts(text)
    masked = mask_rust(text)
    excluded: Set[int] = set()
    module_names: Set[str] = set()
    index = 0
    while index < len(lines):
        stripped = lines[index].lstrip()
        if not stripped.startswith("#["):
            index += 1
            continue
        is_test, attr_end = is_cfg_test_attribute(lines, index)
        if not is_test:
            index += 1
            continue
        item_start = attr_end + 1
        while item_start < len(lines):
            if not lines[item_start].strip():
                item_start += 1
                continue
            if lines[item_start].lstrip().startswith("#["):
                item_start = attribute_end(lines, item_start) + 1
                continue
            break
        if item_start >= len(lines):
            excluded.update(range(index, len(lines)))
            break
        end = item_end(masked, starts, item_start)
        excluded.update(range(index, end + 1))
        item_text = "".join(lines[item_start : end + 1])
        module = re.search(r"\bmod\s+([A-Za-z_][A-Za-z0-9_]*)\s*(?:;|\{)", item_text)
        if module:
            module_names.add(module.group(1))
        index = end + 1
    return excluded, module_names


def child_module_path(path: str, name: str) -> str:
    source = Path(path)
    if source.name == "mod.rs":
        directory = source.parent
    else:
        directory = source.parent / source.stem
    return (directory / (name + ".rs")).as_posix()


def test_only_paths(sources: Dict[str, str]) -> Set[str]:
    result: Set[str] = set()
    for path in sorted(sources):
        _, modules = cfg_test_spans(sources[path])
        for module in modules:
            candidate = child_module_path(path, module)
            if candidate in sources:
                result.add(candidate)
    return result


def production_views(sources: Dict[str, str]) -> Dict[str, Tuple[str, int, Set[int]]]:
    test_only = test_only_paths(sources)
    result: Dict[str, Tuple[str, int, Set[int]]] = {}
    for path in sorted(sources):
        text = sources[path]
        lines = text.splitlines(keepends=True)
        if path in test_only:
            excluded = set(range(len(lines)))
        else:
            excluded, _ = cfg_test_spans(text)
        view = "".join(
            line if index not in excluded else ("\n" if line.endswith("\n") else "")
            for index, line in enumerate(lines)
        )
        result[path] = (view, len(lines) - len(excluded), excluded)
    return result


def function_records(text: str) -> Tuple[List[Tuple[str, int, int, int]], List[str]]:
    masked = mask_rust(text)
    starts = line_starts(text)
    records: List[Tuple[str, int, int, int]] = []
    errors: List[str] = []
    for match in FUNCTION_RE.finditer(masked):
        name = match.group(1)
        cursor = match.end()
        opening: Optional[int] = None
        declaration_end: Optional[int] = None
        while cursor < len(masked):
            if masked[cursor] == "{":
                opening = cursor
                break
            if masked[cursor] == ";":
                declaration_end = cursor
                break
            cursor += 1
        start_line = line_for_position(starts, match.start()) + 1
        if declaration_end is not None:
            records.append((name, start_line, start_line, 1))
            continue
        if opening is None:
            errors.append(f"{name} at line {start_line} has no body or declaration terminator")
            continue
        depth = 0
        cursor = opening
        closing: Optional[int] = None
        while cursor < len(masked):
            if masked[cursor] == "{":
                depth += 1
            elif masked[cursor] == "}":
                depth -= 1
                if depth == 0:
                    closing = cursor
                    break
            cursor += 1
        if closing is None:
            errors.append(f"{name} at line {start_line} has an unbalanced body")
            continue
        end_line = line_for_position(starts, closing) + 1
        records.append((name, start_line, end_line, end_line - start_line + 1))
    return records, errors


def top_module(path: str) -> Optional[str]:
    parts = Path(path).parts
    if len(parts) < 2 or parts[0] != "src":
        return None
    if len(parts) == 2:
        return Path(parts[1]).stem
    return parts[1]


def module_components(path: str) -> List[str]:
    parts = list(Path(path).parts)
    if len(parts) < 2 or parts[0] != "src":
        return []
    components = parts[1:]
    if components[-1] == "mod.rs":
        components.pop()
    else:
        components[-1] = Path(components[-1]).stem
    return components


def super_escape_errors(path: str, text: str) -> List[str]:
    components = module_components(path)
    if not components:
        return []
    masked = mask_rust(text)
    starts = line_starts(text)
    prefix = re.compile(r"(?<![:A-Za-z0-9_])super\s*::")
    errors: List[str] = []
    for match in prefix.finditer(masked):
        count = 1
        cursor = match.end()
        while True:
            next_prefix = re.match(r"\s*super\s*::", masked[cursor:])
            if next_prefix is None:
                break
            count += 1
            cursor += next_prefix.end()
        if count >= len(components):
            line = line_for_position(starts, match.start()) + 1
            errors.append(
                f"{path}:{line}: {count} leading super:: segments escape module "
                f"depth {len(components)}; use explicit crate::TOP"
            )
    return errors


def tarjan(edges: Dict[str, Set[str]]) -> List[Set[str]]:
    index = 0
    indices: Dict[str, int] = {}
    lowlinks: Dict[str, int] = {}
    stack: List[str] = []
    on_stack: Set[str] = set()
    components: List[Set[str]] = []

    def visit(node: str) -> None:
        nonlocal index
        indices[node] = index
        lowlinks[node] = index
        index += 1
        stack.append(node)
        on_stack.add(node)
        for target in sorted(edges[node]):
            if target not in indices:
                visit(target)
                lowlinks[node] = min(lowlinks[node], lowlinks[target])
            elif target in on_stack:
                lowlinks[node] = min(lowlinks[node], indices[target])
        if lowlinks[node] == indices[node]:
            component: Set[str] = set()
            while True:
                target = stack.pop()
                on_stack.remove(target)
                component.add(target)
                if target == node:
                    break
            components.append(component)

    for node in sorted(edges):
        if node not in indices:
            visit(node)
    return sorted(components, key=lambda component: tuple(sorted(component)))


def expected_sccs() -> Set[frozenset]:
    return {frozenset({name}) for name in CANONICAL_TOPS}


def format_sccs(components: Iterable[Set[str]]) -> str:
    return "[" + ", ".join(
        "{" + ",".join(sorted(component)) + "}"
        for component in sorted(components, key=lambda component: tuple(sorted(component)))
    ) + "]"


def check_sizes_and_graph(
    sources: Dict[str, str],
) -> Tuple[List[str], List[str], Dict[str, Tuple[str, int, Set[int]]], Dict[str, Set[str]]]:
    errors: List[str] = []
    warnings: List[str] = []
    views = production_views(sources)
    test_only = test_only_paths(sources)
    file_counts = {path: data[1] for path, data in views.items()}
    source_total = sum(len(text.splitlines()) for text in sources.values())
    production_total = sum(file_counts.values())
    over_files = {path: count for path, count in file_counts.items() if count > 1_500}
    if over_files:
        errors.append("production file line limit exceeded; sorted counts follow:")
        errors.extend(
            f"  {count:6d} {path}"
            for path, count in sorted(file_counts.items(), key=lambda item: (-item[1], item[0]))
        )
    if source_total > 40_000:
        errors.append(f"src Rust total exceeds 40,000 lines: {source_total}")
    if production_total > 25_000:
        errors.append(f"production Rust total exceeds 25,000 lines: {production_total}")

    for path in sorted(views):
        records, parse_errors = function_records(views[path][0])
        for parse_error in parse_errors:
            errors.append(f"{path}: production function parse error: {parse_error}")
        all_records, _ = function_records(sources[path])
        excluded = views[path][2]
        for name, start, end, span in all_records:
            if span <= 200:
                continue
            is_test = path in test_only or (start - 1) in excluded
            if is_test:
                warnings.append(f"test function exceeds 200 lines: {path}:{start} {name} ({span})")
            else:
                errors.append(f"production function exceeds 200 lines: {path}:{start} {name} ({span})")

    test_root = ROOT / "tests"
    if test_root.is_dir():
        for path in sorted(test_root.rglob("*.rs")):
            text = read_utf8(path, errors)
            if text is None:
                continue
            records, _ = function_records(text)
            for name, start, _end, span in records:
                if span > 200:
                    warnings.append(f"integration function exceeds 200 lines: {relative(path)}:{start} {name} ({span})")

    edges: Dict[str, Set[str]] = {name: set() for name in CANONICAL_TOPS}
    for path, (view, _count, _excluded) in views.items():
        top = top_module(path)
        if top is None or top not in edges or path == "src/lib.rs":
            continue
        errors.extend(super_escape_errors(path, view))
        errors.extend(root_type_import_errors(path, view))
        for target in extract_crate_targets(view):
            if target not in edges:
                if target in DELETED_TOP_MODULES:
                    errors.append(f"{path}: edge targets deleted top module {target}")
                continue
            if omit_semantic_edge(path, target, view):
                continue
            # Self loops do not create cross-owner SCCs and are intentionally ignored.
            if target != top:
                edges[top].add(target)
    components = tarjan(edges)
    actual_sccs = {frozenset(component) for component in components}
    if actual_sccs != expected_sccs():
        errors.append(
            "module SCC baseline mismatch: "
            f"expected={format_sccs([set(component) for component in expected_sccs()])} "
            f"actual={format_sccs(components)}"
        )
    return errors, warnings, views, edges


def internal_self_checks() -> List[str]:
    errors: List[str] = []
    cfg_sample = """\
#[cfg(test)]
#[path = "test_module.rs"]
mod tests;
#[cfg(all(test, unix))]
#[allow(dead_code)]
mod platform_tests {
    fn hidden() {}
}
#[cfg(any(test, windows))]
fn any_test() {}
#[cfg(not(test))]
fn not_test() {}
#[cfg(feature = "test")]
fn feature_test() {}
#[cfg(any(test, all(test, unix)))]
fn nested_test() {}
fn production() {}
"""
    lines = cfg_sample.splitlines(keepends=True)
    excluded, modules = cfg_test_spans(cfg_sample)
    cleaned = "".join(
        line if index not in excluded else ("\n" if line.endswith("\n") else "")
        for index, line in enumerate(lines)
    )
    removed = ("mod tests", "mod platform_tests", "fn nested_test")
    retained = ("fn any_test", "fn not_test", "fn feature_test", "fn production")
    if any(marker in cleaned for marker in removed):
        errors.append("internal cfg(test) self-check failed to strip implied-test items")
    if any(marker not in cleaned for marker in retained):
        errors.append("internal cfg(test) self-check failed to retain conservative items")
    if "#[path" in cleaned or modules != {"tests", "platform_tests"}:
        errors.append("internal cfg(test) self-check lost production/path/module behavior")
    predicate_cases = {
        "test": True,
        "all(test, unix)": True,
        "any(test, windows)": False,
        "not(test)": False,
        'feature = "test"': False,
        "any(test, all(test, unix))": True,
    }
    for predicate, expected in predicate_cases.items():
        if cfg_predicate_implies_test(predicate) != expected:
            errors.append(
                "internal cfg predicate self-check mismatch: "
                f"{predicate!r} expected={expected}"
            )

    grouped = """\
use crate :: model :: Model;
use crate::{
    agent::{RetryPolicy},
    prompt,
    self as root,
    super::ignored,
    session::{actor::{SessionActor}, transcript::TranscriptPage},
};
"""
    expected_targets = {"model", "agent", "prompt", "session"}
    if extract_crate_targets(grouped) != expected_targets:
        errors.append(
            "internal crate-import self-check mismatch: "
            f"expected={sorted(expected_targets)} actual={sorted(extract_crate_targets(grouped))}"
        )
    depth_three = "src/model/providers/anthropic.rs"
    if not super_escape_errors(depth_three, "use super::super::super::Value;\n"):
        errors.append("internal super escape self-check missed a depth-three escape")
    if super_escape_errors(depth_three, "use super::Value;\n") or super_escape_errors(
        depth_three, "use super::super::Value;\n"
    ):
        errors.append("internal super escape self-check rejected an in-owner path")

    visibility = parse_mod_declarations(
        "mod private_mod;\npub /* comment */ (crate) mod crate_mod;\n"
        "pub mod public_mod;\nmod inline_mod { }\n"
    )
    expected_visibility = {
        "private_mod": "private",
        "crate_mod": "crate",
        "public_mod": "public",
        "inline_mod": "private",
    }
    inline_visibility = inline_mod_declarations("mod inline_mod { }\n")
    comment_inline = inline_mod_declarations("pub /* comment */ mod leaked { }\n")
    comment_export, comment_owners, unsupported_exports = parse_root_pub_uses(
        "pub /* comment */ use model /* comment */ :: ProviderRegistry;\n"
        "pub use model::Other as Alias;\n"
    )
    if (
        visibility != expected_visibility
        or inline_visibility != {"inline_mod": "private"}
        or comment_inline != {"leaked": "public"}
        or comment_export != {"model": {"ProviderRegistry"}}
        or comment_owners != ["model"]
        or unsupported_exports != ["pub use model::Other as Alias;"]
    ):
        errors.append(
            "internal visibility self-check mismatch: "
            f"expected={expected_visibility} actual={visibility} "
            f"inline={inline_visibility} comment_inline={comment_inline} "
            f"comment_export={comment_export} comment_owners={comment_owners} "
            f"unsupported_exports={unsupported_exports}"
        )

    exact_port_use = "use crate::conversation::{ConversationEntry, ConversationSeq};\n"
    exact_port_source = """\
use crate::conversation::{ConversationEntry, ConversationSeq};
pub struct ConversationPage {
    pub entries: Vec<ConversationEntry>,
    pub next_after: Option<ConversationSeq>,
    pub observed_head: ConversationSeq,
}
pub struct AppendReceipt {
    pub previous_head: ConversationSeq,
    pub new_head: ConversationSeq,
    pub appended: usize,
}
pub trait SessionLog: Send + 'static {
    fn initialize<'a>(&'a mut self, manifest: SessionManifest) -> LogFuture<'a, ConversationSeq>;
    fn read_page<'a>(&'a mut self, after: Option<ConversationSeq>, limit: usize) -> LogFuture<'a, ConversationPage>;
    fn append<'a>(&'a mut self, expected_head: ConversationSeq, entries: Vec<ConversationEntry>) -> LogFuture<'a, AppendReceipt>;
    fn load_manifest<'a>(&'a mut self) -> LogFuture<'a, SessionManifest>;
    fn close<'a>(&'a mut self) -> LogFuture<'a, ()>;
}
"""
    if not allowed_session_log_conversation_use(
        "src/storage/session_log.rs", exact_port_source
    ):
        errors.append("internal SessionLog conversation edge self-check rejected exact use")
    rejected_port_sources = [
        exact_port_source.replace(
            exact_port_use,
            "fn helper() {\n    use crate::conversation::{ConversationEntry, ConversationSeq};\n}\n",
        ),
        exact_port_source.replace(
            exact_port_use,
            "use crate::conversation::{ConversationEntry, ConversationSeq, TurnId};\n",
        ),
        exact_port_source.replace(
            exact_port_use,
            "use crate::conversation::{ConversationEntry as Entry, ConversationSeq};\n",
        ),
        exact_port_source.replace(exact_port_use, "use crate::conversation::*;\n"),
        exact_port_source.replace(
            exact_port_use,
            "use crate::{conversation::{ConversationEntry, ConversationSeq}};\n",
        ),
        exact_port_source
        + "fn helper() { let _ = crate::conversation::ConversationEntry; }\n",
        exact_port_source.replace(
            "    pub observed_head: ConversationSeq,\n",
            "    pub observed_head: ConversationSeq,\n    pub extra: ConversationSeq,\n",
        ),
    ]
    for fixture in rejected_port_sources:
        if allowed_session_log_conversation_use("src/storage/session_log.rs", fixture):
            errors.append("internal SessionLog conversation edge self-check allowed broader use")

    real_port = read_utf8(ROOT / "src/storage/session_log.rs", errors)
    if real_port is not None:
        bidirectional = {"conversation": {"storage"}, "storage": {"conversation"}}
        filtered = {node: set(targets) for node, targets in bidirectional.items()}
        if omit_semantic_edge("src/storage/session_log.rs", "conversation", real_port):
            filtered["storage"].discard("conversation")
        if any(len(component) > 1 for component in tarjan(filtered)):
            errors.append("internal SessionLog graph self-check retained exact Port cycle")

        broader_port = real_port + "\nfn helper() { let _ = crate::conversation::ConversationEntry; }\n"
        broader_filtered = {
            node: set(targets) for node, targets in bidirectional.items()
        }
        if omit_semantic_edge(
            "src/storage/session_log.rs", "conversation", broader_port
        ):
            broader_filtered["storage"].discard("conversation")
        if not any(
            component == {"conversation", "storage"}
            for component in tarjan(broader_filtered)
        ):
            errors.append("internal SessionLog graph self-check missed broader cycle")
    return errors


def _format_mapping(mapping: Dict[str, Set[str]]) -> str:
    return "{" + ", ".join(
        f"{owner}=[{','.join(sorted(symbols))}]"
        for owner, symbols in sorted(mapping.items())
    ) + "}"


def main() -> int:
    errors: List[str] = internal_self_checks()
    warnings: List[str] = []
    sources = source_files(errors)
    source_paths = set(sources)
    errors.extend(check_source_paths(source_paths))
    errors.extend(check_source_tokens(sources))
    errors.extend(check_acceptance_surface())
    errors.extend(check_public_surface(sources))
    dependency_errors, direct_dependencies = check_dependencies(sources)
    errors.extend(dependency_errors)
    size_errors, size_warnings, views, edges = check_sizes_and_graph(sources)
    errors.extend(size_errors)
    warnings.extend(size_warnings)

    for warning in warnings:
        print(f"warning: {warning}", file=sys.stderr)
    if errors:
        print("architecture gate failed:", file=sys.stderr)
        for error in errors:
            print(f"- {error}", file=sys.stderr)
        return 1

    production_counts = {path: data[1] for path, data in views.items()}
    max_file_path, max_file_count = max(
        production_counts.items(), key=lambda item: (item[1], item[0])
    )
    max_function_path = "<none>"
    max_function_name = "<none>"
    max_function_span = 0
    for path, (view, _count, _excluded) in views.items():
        for name, _start, _end, span in function_records(view)[0]:
            if span > max_function_span:
                max_function_path = path
                max_function_name = name
                max_function_span = span
    production_total = sum(production_counts.values())
    components = tarjan(edges)
    print(
        "architecture gate passed: "
        f"source_files={len(sources)} "
        f"production_loc={production_total} "
        f"max_file={max_file_path}:{max_file_count} "
        f"max_function={max_function_path}:{max_function_name}:{max_function_span} "
        f"direct_deps={','.join(direct_dependencies)} "
        f"scc={format_sccs(components)}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
