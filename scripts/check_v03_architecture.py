#!/usr/bin/env python3
"""Strict v0.3 production architecture scanner.

This is a purpose-built scanner, not a Rust parser. It uses a small lexer,
structured cfg(test) predicate/span handling, import expansion, root API
checking, and Tarjan SCC detection. It requires Python 3.11+ for ``tomllib``.
The active v0.2 gate remains separate until P7 removes the old production tree.
"""
from __future__ import annotations

import argparse
import re
import sys
import tomllib
from pathlib import Path

MAX_TOTAL = 20_000
MAX_FILE = 1_200
MAX_PORT = 500

FORBIDDEN_PATHS = {
    "src/runtime", "src/runtime.rs", "src/session/snapshot.rs", "src/workspace",
    "src/workspace.rs", "src/model/providers", "src/model/providers.rs",
    "src/model/port.rs", "src/model/provider.rs", "src/model/registry.rs", "src/model/transport.rs",
    "src/tools/builtins", "src/tools/builtins.rs", "src/tools/process.rs",
}
FORBIDDEN_ADAPTER_DIRECTORIES = {
    "src/storage/jsonl", "src/storage/store", "src/storage/conversation_jsonl",
}
CONCRETE_FILENAMES = {
    "anthropic.rs", "openai.rs", "provider.rs", "providers.rs", "filesystem.rs",
    "process.rs", "builtins.rs",
}
CANONICAL_PRODUCTION_FILES = {
    "src/model": {"mod.rs", "model.rs", "driver.rs", "request.rs", "response.rs", "types.rs"},
    "src/tools": {"mod.rs", "tool.rs", "set.rs", "context.rs", "input.rs", "policy.rs", "progress.rs", "types.rs"},
}
MODEL_DRIVER_ROLE_FILES = {"src/model/driver.rs"}
TOOLSET_ROLE_FILES = {"src/tools/set.rs"}
SESSION_BINDINGS_ROLE_FILES = {"src/session/bindings.rs"}
TRANSITIONAL_PRIVATE_FILES = {
    "src/model/legacy_gateway.rs",
    "src/model/legacy_provider.rs",
    "src/model/legacy_registry.rs",
    "src/tools/registry.rs",
    "src/tools/legacy_context.rs",
    "src/tools/legacy_policy.rs",
    "src/tools/legacy_types.rs",
}
ALLOWED_PORT_PATHS = {
    "src/compaction/strategy.rs", "src/context/provider.rs", "src/model/model.rs",
    "src/storage/session_log.rs",
}
FORBIDDEN_SYMBOLS = {
    "Runtime", "RuntimeClient", "RuntimeConfig", "RuntimeConfigBuilder", "RuntimeError",
    "RuntimeInner", "RuntimeSupervisor", "SessionManager", "SessionStore", "SessionSummary",
    "ManagedSession", "LoadedSession", "LoadedSessionId", "SessionSnapshot", "SnapshotHistory",
    "SnapshotShapeError", "ObservationFrame", "ObservationCursor", "ObservationEpoch",
    "SessionObservation", "ObservationEvent", "ObservationSubscription", "SessionObserver",
    "SnapshotRevision", "SnapshotCursor", "EventCursor", "Resync", "ResyncRequired",
    "Workspace", "WorkspaceAccess", "InteractionClient", "AgentSpawner", "Subagent",
    "ChildSession", "ModelGateway", "ModelProvider", "ModelResolver", "ProviderRegistry",
    "ProviderCredential", "ProviderId", "ModelSelection", "ModelId", "ProviderItemId",
    "TurnSummary", "TurnTerminalSummary", "TerminalOutcome",
    "SessionEventKind", "RuntimeEvent", "ToolRegistry", "ToolRegistryBuilder",
}
# The old runner still consumes these names through the explicitly private
# migration seam. The symbols remain forbidden everywhere else, especially in
# the public tools module and final canonical role inventory.
FORBIDDEN_SYMBOL_EXEMPTIONS = {
    "ToolRegistry": {
        "src/agent/context.rs", "src/agent/mod.rs", "src/config.rs",
        "src/runtime/runtime_impl.rs", "src/session/actor.rs", "src/tools/registry.rs",
    },
    "InteractionClient": {
        "src/agent/context.rs", "src/agent/mod.rs", "src/session/actor.rs", "src/tools/mod.rs",
    },
}
FORBIDDEN_IMPORTS = {
    ("reqwest",), ("cap_std",), ("cap_primitives",), ("fs4",),
    ("std", "fs"), ("std", "env"), ("std", "net"), ("std", "process"),
    ("tokio", "net"), ("tokio", "process"),
}
FORBIDDEN_DEPENDENCIES = {"reqwest", "cap-std", "cap-primitives", "fs4"}
PORT_FILES = {
    "src/model/model.rs", "src/model/response.rs", "src/tools/tool.rs", "src/tools/set.rs",
    "src/tools/context.rs", "src/tools/input.rs", "src/tools/policy.rs", "src/tools/progress.rs", "src/tools/types.rs",
    "src/context/provider.rs", "src/compaction/strategy.rs", "src/session/bindings.rs",
    "src/storage/session_log.rs",
}
REQUIRED_FILES = {
    "src/lib.rs", "src/config.rs", "src/error.rs", "src/ids.rs", "src/value.rs", "src/time.rs",
    "src/agent/mod.rs", "src/agent/runner.rs", "src/agent/runner_protocol.rs",
    "src/agent/turn_context.rs", "src/agent/retry.rs", "src/prompt/mod.rs", "src/prompt/builder.rs",
    "src/session/mod.rs", "src/session/runtime.rs", "src/session/handle.rs",
    "src/session/turn_handle.rs", "src/session/actor.rs", "src/session/command.rs",
    "src/session/state.rs", "src/session/event.rs", "src/session/event_stream.rs",
    "src/session/interaction.rs", "src/session/bindings.rs", "src/session/transcript.rs",
    "src/conversation/mod.rs", "src/conversation/entry.rs", "src/conversation/load.rs", "src/conversation/state.rs",
    "src/conversation/validator.rs", "src/conversation/projection.rs", "src/conversation/log.rs",
    "src/conversation/recovery.rs", "src/conversation/transcript.rs", "src/model/mod.rs", "src/model/model.rs", "src/model/response.rs", "src/model/types.rs",
    "src/conversation/view.rs",
    "src/tools/mod.rs", "src/tools/tool.rs", "src/tools/context.rs", "src/tools/input.rs", "src/tools/policy.rs", "src/tools/progress.rs", "src/tools/set.rs", "src/tools/types.rs",
    "src/context/mod.rs", "src/context/provider.rs",
    "src/compaction/mod.rs", "src/compaction/strategy.rs", "src/storage/mod.rs",
    "src/storage/session_log.rs",
}
PUBLIC_MODULES = {
    "compaction", "config", "context", "conversation", "error", "ids", "model", "session",
    "storage", "tools", "value",
}
PRIVATE_MODULES = {"agent", "prompt", "runtime", "time", "workspace"}
ROOT_EXPORTS = {
    "value": {"BoundedText"},
    "config": {"CompactionConfig", "KernelConfig", "RetryPolicy", "SemanticLimits", "SessionManifest", "SessionSpec", "TurnOptions", "UserInput"},
    "conversation": {"ConversationEntry", "ConversationSeq", "TranscriptPage", "TurnTerminal"},
    "ids": {"InteractionId", "SessionId", "SessionInstanceId", "ToolCallId", "TurnId"},
    "session": {"InteractionAnswer", "InteractionKind", "PendingInteraction", "SessionBindings", "SessionEvent", "SessionEventEnvelope", "SessionEventStream", "SessionHandle", "SessionHealth", "SessionRuntime", "SessionRuntimeOptions", "SessionState", "SessionStatus", "TurnHandle", "TurnOutcome"},
    "storage": {"AppendReceipt", "ConversationPage", "SessionLog", "SessionLogError"},
}
PORT_DECLARATIONS = {
    "src/model/model.rs": ("trait", "Model"),
    "src/tools/tool.rs": ("trait", "Tool"),
    "src/tools/policy.rs": ("trait", "ToolPolicy"),
    "src/context/provider.rs": ("trait", "ContextProvider"),
    "src/compaction/strategy.rs": ("trait", "CompactionStrategy"),
    "src/storage/session_log.rs": ("trait", "SessionLog"),
}

CFG_START_RE = re.compile(r"#\s*\[\s*cfg\s*\(")
USE_RE = re.compile(r"\buse\b")
PUB_USE_RE = re.compile(r"\bpub\s+use\b")


def blank(chars: list[str], start: int, end: int) -> None:
    for index in range(start, min(end, len(chars))):
        if chars[index] not in "\r\n":
            chars[index] = " "


def matching(text: str, opening: int, left: str, right: str) -> int:
    depth = 0
    for index in range(opening, len(text)):
        if text[index] == left:
            depth += 1
        elif text[index] == right:
            depth -= 1
            if depth == 0:
                return index
    return -1


def raw_prefix(text: str, index: int) -> tuple[int, int, str] | None:
    start = index
    if text.startswith("br", index):
        index += 2
    elif text.startswith("r", index):
        index += 1
    else:
        return None
    hashes = 0
    while index < len(text) and text[index] == "#":
        hashes += 1
        index += 1
    if index >= len(text) or text[index] != '"':
        return None
    return start, index, '"' + ("#" * hashes)


def mask_rust(text: str) -> str:
    """Mask comments, ordinary/raw strings, byte strings, chars, and lifetimes."""
    chars = list(text)
    index = 0
    block_depth = 0
    while index < len(text):
        if block_depth:
            if text.startswith("/*", index):
                block_depth += 1
                blank(chars, index, index + 2)
                index += 2
            elif text.startswith("*/", index):
                block_depth -= 1
                blank(chars, index, index + 2)
                index += 2
            else:
                blank(chars, index, index + 1)
                index += 1
            continue
        if text.startswith("//", index):
            end = text.find("\n", index)
            blank(chars, index, len(text) if end < 0 else end)
            index = len(text) if end < 0 else end
            continue
        if text.startswith("/*", index):
            block_depth = 1
            blank(chars, index, index + 2)
            index += 2
            continue
        raw = raw_prefix(text, index)
        if raw:
            start, opening, terminator = raw
            close = text.find(terminator, opening + 1)
            end = len(text) if close < 0 else close + len(terminator)
            blank(chars, start, end)
            index = end
            continue
        if text.startswith('b"', index) or text[index] == '"':
            start = index
            if text.startswith('b"', index):
                index += 1
            index += 1
            escaped = False
            while index < len(text):
                character = text[index]
                if character == '"' and not escaped:
                    index += 1
                    break
                escaped = character == "\\" and not escaped
                if character != "\n":
                    chars[index] = " "
                index += 1
            blank(chars, start, index)
            continue
        char_quote = index + 1 if text.startswith("b'", index) else index
        if text[index] == "'" or text.startswith("b'", index):
            cursor = char_quote + 1
            escaped = False
            found = False
            while cursor < len(text) and text[cursor] not in "\r\n" and cursor - char_quote <= 8:
                if text[cursor] == "'" and not escaped:
                    found = True
                    cursor += 1
                    break
                escaped = text[cursor] == "\\" and not escaped
                cursor += 1
            if found:
                blank(chars, index, cursor)
                index = cursor
            else:
                # Keep lifetime identifiers intact; only the apostrophe is trivia.
                chars[index] = " "
                index += 1
            continue
        index += 1
    return "".join(chars)


def line_starts(text: str) -> list[int]:
    return [0] + [match.end() for match in re.finditer("\n", text)]


def line_for(starts: list[int], position: int) -> int:
    lo, hi = 0, len(starts)
    while lo + 1 < hi:
        middle = (lo + hi) // 2
        if starts[middle] <= position:
            lo = middle
        else:
            hi = middle
    return lo


def cfg_predicate_implies_test(predicate: str) -> bool:
    value = re.sub(r"\s+", "", predicate)
    if value == "test":
        return True
    match = re.fullmatch(r"([A-Za-z_]\w*)\((.*)\)", value)
    if not match:
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


def split_cfg_arguments(value: str) -> list[str]:
    result: list[str] = []
    start = 0
    depths = {"(": 0, "{": 0, "[": 0}
    closing = {")": "(", "}": "{", "]": "[",
    }
    masked = mask_rust(value)
    for index, character in enumerate(masked):
        if character in depths:
            depths[character] += 1
        elif character in closing:
            depths[closing[character]] -= 1
            if depths[closing[character]] < 0:
                return []
        elif character == "," and not any(depths.values()):
            result.append(value[start:index].strip())
            start = index + 1
    if any(depths.values()):
        return []
    result.append(value[start:].strip())
    return result


def cfg_test_spans(text: str) -> tuple[set[int], set[str]]:
    """Return excluded line numbers and file-backed test module names."""
    lines = text.splitlines(keepends=True)
    if not lines:
        return set(), set()
    masked = mask_rust(text)
    starts = line_starts(text)
    excluded: set[int] = set()
    modules: set[str] = set()
    for match in CFG_START_RE.finditer(masked):
        bracket = masked.find("[", match.start(), match.end())
        paren = masked.rfind("(", match.start(), match.end())
        if bracket < 0 or paren < 0:
            continue
        close_paren = matching(masked, paren, "(", ")")
        close_attr = matching(masked, bracket, "[", "]")
        if close_paren < 0 or close_attr < 0:
            continue
        if not cfg_predicate_implies_test(masked[paren + 1:close_paren]):
            continue
        attr_line = line_for(starts, match.start())
        group_line = attr_line
        while group_line > 0:
            previous = lines[group_line - 1].lstrip()
            if not previous.strip() or previous.startswith(("#", "///", "//!", "//")):
                group_line -= 1
            else:
                break
        item_position = close_attr + 1
        while True:
            while item_position < len(text) and text[item_position].isspace():
                item_position += 1
            if masked.startswith("#[", item_position):
                next_close = matching(masked, item_position + 1, "[", "]")
                if next_close < 0:
                    break
                item_position = next_close + 1
                continue
            item_line = line_for(starts, item_position)
            if lines[item_line].lstrip().startswith(("///", "//!", "//")):
                item_position = starts[item_line + 1] if item_line + 1 < len(starts) else len(text)
                continue
            break
        if item_position >= len(text):
            end_line = len(lines) - 1
        else:
            cursor = item_position
            opening = -1
            semicolon = -1
            while cursor < len(masked):
                if masked[cursor] == "{":
                    opening = cursor
                    break
                if masked[cursor] == ";":
                    semicolon = cursor
                    break
                cursor += 1
            if semicolon >= 0 and (opening < 0 or semicolon < opening):
                end_line = line_for(starts, semicolon)
            elif opening >= 0:
                close_body = matching(masked, opening, "{", "}")
                end_line = line_for(starts, len(text) - 1 if close_body < 0 else close_body)
            else:
                end_line = line_for(starts, item_position)
            item_text = text[item_position:starts[end_line + 1] if end_line + 1 < len(starts) else len(text)]
            module = re.search(r"\bmod\s+([A-Za-z_]\w*)\s*(?:;|\{)", item_text)
            if module:
                modules.add(module.group(1))
        excluded.update(range(group_line, min(end_line + 1, len(lines))))
    return excluded, modules


def production_view(text: str) -> tuple[str, int, set[int]]:
    lines = text.splitlines(keepends=True)
    if file_is_test_only(text):
        return "".join("\n" if line.endswith(("\n", "\r")) else "" for line in lines), 0, set(range(len(lines)))
    excluded, _ = cfg_test_spans(text)
    view = "".join(
        ("\n" if number in excluded and line.endswith("\n") else ("" if number in excluded else line))
        for number, line in enumerate(lines)
    )
    return view, len(lines) - len(excluded), excluded


def module_components(path: Path, root: Path) -> list[str]:
    parts = path.relative_to(root / "src").parts
    if parts[-1] == "lib.rs":
        return []
    if parts[-1] == "mod.rs":
        return list(parts[:-1])
    return list(parts[:-1]) + [parts[-1][:-3]]


def source_files(root: Path) -> list[Path]:
    if not (root / "src").exists():
        return []
    return sorted(path for path in (root / "src").rglob("*.rs") if path.is_file())


def read_source_texts(root: Path) -> tuple[list[Path], dict[str, str], list[str]]:
    src = root / "src"
    candidates = sorted(src.rglob("*.rs")) if src.exists() else []
    paths = [path for path in candidates if path.is_file()]
    texts: dict[str, str] = {}
    errors: list[str] = []
    for candidate in candidates:
        relative = candidate.relative_to(root).as_posix()
        if not candidate.is_file():
            errors.append(f"non-regular Rust source path: {relative}")
            continue
        try:
            texts[relative] = candidate.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError) as error:
            errors.append(f"unreadable production source: {relative}: {error}")
    return paths, texts, errors


def file_is_test_only(text: str) -> bool:
    """Recognize crate-inner cfg only before the first production item."""
    masked = mask_rust(text)
    index = 0
    while index < len(masked):
        while index < len(masked) and masked[index].isspace():
            index += 1
        if masked.startswith("#![", index):
            close = matching(masked, index + 2, "[", "]")
            if close < 0:
                return False
            attr = masked[index + 3:close]
            cfg = re.search(r"\bcfg\s*\(", attr)
            if cfg:
                opening = attr.find("(", cfg.start(), cfg.end())
                end = matching(attr, opening, "(", ")")
                if end >= 0 and cfg_predicate_implies_test(attr[opening + 1:end]):
                    return True
            index = close + 1
            continue
        if masked.startswith("#[", index):
            close = matching(masked, index + 1, "[", "]")
            if close < 0:
                return False
            index = close + 1
            continue
        # Comments are blanked by mask_rust; an inner attribute here is nested.
        return False
    return False


def test_only_files(root: Path, paths: list[Path], source_texts: dict[str, str]) -> set[str]:
    result: set[str] = set()
    for path in paths:
        relative = path.relative_to(root).as_posix()
        if relative not in source_texts:
            continue
        _, modules = cfg_test_spans(source_texts[relative])
        for name in modules:
            if path.name in {"mod.rs", "lib.rs"}:
                parent = path.parent
            else:
                parent = path.parent / path.stem
            candidates = (parent / f"{name}.rs", parent / name / "mod.rs")
            for child in candidates:
                if child.is_file():
                    result.add(child.relative_to(root).as_posix())
    return result


def split_top_level(text: str) -> list[str]:
    result: list[str] = []
    start = 0
    depth = 0
    for index, character in enumerate(text):
        if character == "{":
            depth += 1
        elif character == "}":
            depth -= 1
        elif character == "," and depth == 0:
            result.append(text[start:index])
            start = index + 1
    result.append(text[start:])
    return result


def path_segments(text: str) -> tuple[str, ...]:
    text = re.sub(r"\s*::\s*", "::", text.strip())
    return tuple(segment for segment in text.split("::") if segment)


def expand_import(expression: str, prefix: tuple[str, ...] = ()) -> list[tuple[str, ...]]:
    expression = expression.strip()
    opening = expression.find("{")
    if opening < 0:
        expression = re.sub(r"\s+as\s+[A-Za-z_]\w*\s*$", "", expression)
        return [prefix + tuple(segment for segment in path_segments(expression) if segment != "*")]
    closing = matching(expression, opening, "{", "}")
    if closing < 0:
        return []
    base = prefix + path_segments(expression[:opening].rstrip().removesuffix("::"))
    result: list[tuple[str, ...]] = []
    for item in split_top_level(expression[opening + 1:closing]):
        if item.strip():
            result.extend(expand_import(item, base))
    return result


def use_statements(masked: str, public_only: bool = False) -> list[str]:
    pattern = PUB_USE_RE if public_only else USE_RE
    result: list[str] = []
    for match in pattern.finditer(masked):
        cursor = match.end()
        depth = 0
        while cursor < len(masked):
            if masked[cursor] == "{":
                depth += 1
            elif masked[cursor] == "}":
                depth -= 1
            elif masked[cursor] == ";" and depth == 0:
                result.append(masked[match.end():cursor])
                break
            cursor += 1
    return result


def imports(masked: str) -> list[tuple[str, ...]]:
    return [path for statement in use_statements(masked) for path in expand_import(statement) if path]


def crate_top_paths(masked: str) -> set[str]:
    """Extract crate::TOP paths in expressions, signatures, and grouped paths."""
    targets = {
        match.group(1)
        for match in re.finditer(
            r"(?<![A-Za-z0-9_])crate\s*::\s*([A-Za-z_]\w*)", masked
        )
    }
    for match in re.finditer(r"(?<![A-Za-z0-9_])crate\s*::\s*\{", masked):
        opening = masked.find("{", match.start(), match.end())
        closing = matching(masked, opening, "{", "}")
        if closing < 0:
            continue
        for item in split_top_level(masked[opening + 1:closing]):
            path = path_segments(item)
            if path and path[0] not in {"self", "super"}:
                targets.add(path[0])
    return targets


def root_pub_use_statements(masked: str) -> list[str]:
    result: list[str] = []
    depth = 0
    index = 0
    while index < len(masked):
        if depth == 0 and re.match(r"pub\s+use\b", masked[index:]):
            match = re.match(r"pub\s+use\b", masked[index:])
            assert match is not None
            cursor = index + match.end()
            braces = 0
            while cursor < len(masked):
                if masked[cursor] == "{":
                    braces += 1
                elif masked[cursor] == "}":
                    braces -= 1
                elif masked[cursor] == ";" and braces == 0:
                    result.append(masked[index + match.end():cursor])
                    index = cursor + 1
                    break
                cursor += 1
            else:
                index += match.end()
            continue
        if masked[index] == "{":
            depth += 1
        elif masked[index] == "}":
            depth = max(0, depth - 1)
        index += 1
    return result


def root_module_declarations(masked: str) -> tuple[set[str], set[str]]:
    public: set[str] = set()
    private: set[str] = set()
    depth = 0
    for line in masked.splitlines():
        if depth == 0:
            public.update(re.findall(r"^\s*pub\s+mod\s+([A-Za-z_]\w*)\s*(?:;|\{)", line))
            private.update(re.findall(r"^\s*(?:pub\(crate\)\s+)?mod\s+([A-Za-z_]\w*)\s*(?:;|\{)", line))
            private.difference_update(public)
        depth += line.count("{") - line.count("}")
        depth = max(0, depth)
    return public, private


def export_leaves(expression: str, prefix: tuple[str, ...] = ()) -> list[tuple[tuple[str, ...], str | None, bool]]:
    opening = expression.find("{")
    if opening >= 0:
        closing = matching(expression, opening, "{", "}")
        if closing < 0:
            return []
        base = prefix + path_segments(expression[:opening].rstrip().removesuffix("::"))
        result: list[tuple[tuple[str, ...], str | None, bool]] = []
        for item in split_top_level(expression[opening + 1:closing]):
            if item.strip():
                result.extend(export_leaves(item, base))
        return result
    raw = expression.strip()
    alias_match = re.search(r"\s+as\s+([A-Za-z_]\w*)\s*$", raw)
    alias = alias_match.group(1) if alias_match else None
    if alias_match:
        raw = raw[:alias_match.start()]
    path = prefix + path_segments(raw)
    return [(path, alias, "*" in path)]


def root_export_errors(text: str) -> list[str]:
    actual: dict[str, set[str]] = {}
    errors: list[str] = []
    seen: set[tuple[str, str]] = set()
    for statement in root_pub_use_statements(mask_rust(text)):
        leaves = export_leaves(statement)
        if not leaves:
            errors.append("src/lib.rs: unsupported public re-export syntax")
            continue
        for path, alias, glob in leaves:
            if alias:
                errors.append(f"src/lib.rs: root export alias is forbidden: {alias}")
            if glob:
                errors.append("src/lib.rs: root export glob is forbidden")
            if path and path[0] == "crate":
                path = path[1:]
            if len(path) != 2 or path[0] not in ROOT_EXPORTS or path[1] == "self":
                errors.append(f"src/lib.rs: unsupported or extra root export: {'::'.join(path)}")
                continue
            key = (path[0], path[1])
            if key in seen:
                errors.append(f"src/lib.rs: duplicate root export: {path[0]}::{path[1]}")
            seen.add(key)
            actual.setdefault(path[0], set()).add(path[1])
    for owner, symbols in ROOT_EXPORTS.items():
        for symbol in sorted(symbols - actual.get(owner, set())):
            errors.append(f"src/lib.rs: missing root export: {owner}::{symbol}")
    for owner, symbols in actual.items():
        for symbol in sorted(symbols - ROOT_EXPORTS.get(owner, set())):
            errors.append(f"src/lib.rs: extra root export: {owner}::{symbol}")
    return errors


def root_surface_errors(views: dict[str, tuple[str, int]]) -> list[str]:
    text = views.get("src/lib.rs", ("", 0))[0]
    masked = mask_rust(text)
    errors = root_export_errors(text)
    modules, declared_private = root_module_declarations(masked)
    if modules != PUBLIC_MODULES:
        errors.append(
            f"src/lib.rs: public modules mismatch: expected={sorted(PUBLIC_MODULES)} actual={sorted(modules)}"
        )
    missing_private = sorted(PRIVATE_MODULES - declared_private)
    extra_private = sorted(declared_private - PRIVATE_MODULES)
    if missing_private:
        errors.append(f"src/lib.rs: missing private implementation modules: {', '.join(missing_private)}")
    if extra_private:
        errors.append(f"src/lib.rs: unexpected private modules: {', '.join(extra_private)}")
    for private in PRIVATE_MODULES:
        if re.search(rf"\bpub\s+mod\s+{private}\b", masked):
            errors.append(f"src/lib.rs: implementation module must not be public: {private}")
    for symbol in ("SessionRuntime", "SessionRuntimeOptions", "SessionHandle", "TurnHandle"):
        if not any(symbol in statement for statement in root_pub_use_statements(masked)):
            errors.append(f"src/lib.rs: missing owner/handle export: {symbol}")
    return errors


def forbidden_path_errors(root: Path, test_files: set[str]) -> list[str]:
    errors: list[str] = []
    for relative in sorted(FORBIDDEN_PATHS):
        path = root / relative
        if path.is_dir():
            if relative in FORBIDDEN_ADAPTER_DIRECTORIES:
                errors.append(f"forbidden production path: {relative}")
            else:
                production = any(
                    child.relative_to(root).as_posix() not in test_files for child in path.rglob("*.rs")
                )
                if production:
                    errors.append(f"forbidden production path: {relative}")
        elif path.exists() and relative not in test_files:
            errors.append(f"forbidden production path: {relative}")
    for path in source_files(root):
        relative = path.relative_to(root).as_posix()
        if relative in test_files or relative in ALLOWED_PORT_PATHS or relative.startswith("src/storage/"):
            continue
        name = path.name.casefold()
        if name in CONCRETE_FILENAMES or name.endswith("_jsonl.rs"):
            errors.append(f"forbidden concrete adapter path: {relative}")
    return errors


def forbidden_import(path: tuple[str, ...]) -> bool:
    return any(path[:len(prefix)] == prefix for prefix in FORBIDDEN_IMPORTS)


def forbidden_external_paths(masked: str) -> list[str]:
    compact = re.sub(r"\s*::\s*", "::", masked)
    errors: list[str] = []
    for prefix in FORBIDDEN_IMPORTS:
        rendered = "::".join(prefix)
        direct = rf"(?<![A-Za-z0-9_:]){re.escape(rendered)}"
        absolute = rf"(?<![A-Za-z0-9_])::{re.escape(rendered)}"
        if re.search(direct, compact) or re.search(absolute, compact):
            errors.append(rendered)
    return errors


def storage_allowlist_errors(
    root: Path,
    test_files: set[str],
    source_texts: dict[str, str],
) -> list[str]:
    errors: list[str] = []
    allowed = {"src/storage/mod.rs", "src/storage/session_log.rs"}

    def test_only(relative: str) -> bool:
        return test_helper_with_empty_production_view(relative, source_texts, test_files)

    storage = root / "src/storage"
    if not storage.exists():
        return errors
    for child in storage.iterdir():
        relative = child.relative_to(root).as_posix()
        if relative in allowed:
            continue
        if child.is_dir():
            rust_children = [path for path in child.rglob("*.rs") if path.is_file()]
            if not rust_children or any(
                not test_only(path.relative_to(root).as_posix()) for path in rust_children
            ):
                errors.append(f"forbidden production storage implementation: {relative}")
            continue
        if child.suffix == ".rs" and not test_only(relative):
            errors.append(f"forbidden production storage implementation: {relative}")
    return errors


def test_helper_with_empty_production_view(
    relative: str,
    source_texts: dict[str, str],
    test_files: set[str],
) -> bool:
    raw = source_texts.get(relative)
    if raw is None or not raw.strip():
        return False
    if relative in test_files:
        view = ""
        excluded = set(range(len(raw.splitlines(keepends=True))))
    else:
        view, _line_count, excluded = production_view(raw)
    return bool(excluded) and not mask_rust(view).strip()


def canonical_allowlist_errors(
    root: Path,
    test_files: set[str],
    source_texts: dict[str, str],
) -> list[str]:
    errors: list[str] = []

    for directory, allowed_names in CANONICAL_PRODUCTION_FILES.items():
        path = root / directory
        if not path.exists():
            continue
        if not path.is_dir():
            errors.append(f"forbidden production {directory} path: {directory}")
            continue
        for child in path.iterdir():
            relative = child.relative_to(root).as_posix()
            if child.is_file():
                if relative in TRANSITIONAL_PRIVATE_FILES:
                    continue
                if child.name in allowed_names:
                    continue
                if child.suffix == ".rs" and test_helper_with_empty_production_view(relative, source_texts, test_files):
                    continue
                errors.append(f"forbidden production {directory} path: {relative}")
                continue
            if not child.is_dir():
                errors.append(f"forbidden production {directory} path: {relative}")
                continue
            descendants = list(child.rglob("*"))
            rust_files = [descendant for descendant in descendants if descendant.is_file() and descendant.suffix == ".rs"]
            empty_directories = [descendant for descendant in descendants if descendant.is_dir() and not any(descendant.iterdir())]
            if not rust_files or empty_directories or any(
                not test_helper_with_empty_production_view(descendant.relative_to(root).as_posix(), source_texts, test_files)
                for descendant in rust_files
            ) or any(descendant.is_file() and descendant.suffix != ".rs" for descendant in descendants):
                errors.append(f"forbidden production {directory} path: {relative}")
    return errors


def cargo_dependency_packages(root: Path) -> tuple[set[str], str | None]:
    manifest_path = root / "Cargo.toml"
    if not manifest_path.exists():
        return set(), None
    try:
        raw = manifest_path.read_bytes()
        document = tomllib.loads(raw.decode("utf-8"))
    except (OSError, tomllib.TOMLDecodeError, UnicodeDecodeError) as error:
        return set(), f"Cargo.toml: unreadable or invalid TOML: {error}"
    result: set[str] = set()

    def add_table(table: object) -> None:
        if not isinstance(table, dict):
            return
        for dependency, specification in table.items():
            result.add(str(dependency).replace("_", "-"))
            if isinstance(specification, dict) and isinstance(specification.get("package"), str):
                result.add(specification["package"].replace("_", "-"))

    dependency_tables = ("dependencies", "dev-dependencies", "build-dependencies")

    def add_dependency_tables(value: object) -> None:
        if not isinstance(value, dict):
            return
        for table_name in dependency_tables:
            add_table(value.get(table_name))

    add_dependency_tables(document)
    workspace = document.get("workspace")
    if isinstance(workspace, dict):
        add_dependency_tables(workspace)

    def target_dependencies(value: object) -> None:
        if not isinstance(value, dict):
            return
        for target in value.values():
            add_dependency_tables(target)

    target_dependencies(document.get("target"))
    if isinstance(workspace, dict):
        target_dependencies(workspace.get("target"))
    return result, None


def import_targets(root: Path, relative: str, text: str, owners: set[str]) -> set[str]:
    parts = module_components(root / relative, root)
    if not parts:
        return set()
    targets: set[str] = set()
    masked = mask_rust(text)
    parsed_paths = imports(masked)
    for top in crate_top_paths(masked):
        parsed_paths.append(("crate", top))
    for path in parsed_paths:
        target: str | None = None
        if path[0] == "crate" and len(path) > 1:
            target = path[1]
        elif path[0] == "super":
            count = 0
            while count < len(path) and path[count] == "super":
                count += 1
            if count == len(parts) and count < len(path):
                target = path[count]
        if target and target != parts[0]:
            targets.add(target)
    return targets


def top_level_declaration(masked: str, kind: str, name: str) -> bool:
    depth = 0
    pattern = re.compile(rf"^\s*pub\s+{kind}\s+{re.escape(name)}\b")
    for line in masked.splitlines():
        if depth == 0 and pattern.search(line):
            return True
        depth += line.count("{") - line.count("}")
        depth = max(0, depth)
    return False


def port_declaration_errors(views: dict[str, tuple[str, int]]) -> list[str]:
    errors: list[str] = []
    for relative, (kind, name) in PORT_DECLARATIONS.items():
        masked = mask_rust(views.get(relative, ("", 0))[0])
        if not top_level_declaration(masked, kind, name):
            errors.append(f"typed Port declaration missing or wrong kind: {relative} {kind} {name}")
    role_paths = sorted(
        relative for relative in TOOLSET_ROLE_FILES
        if relative in views and mask_rust(views[relative][0]).strip()
    )
    if len(role_paths) != 1:
        errors.append(
            "typed Port ToolSet role requires exactly one production file: "
            f"expected={sorted(TOOLSET_ROLE_FILES)} actual={role_paths}"
        )
    else:
        relative = role_paths[0]
        kind, name = ("struct", "ToolSet")
        if not top_level_declaration(mask_rust(views[relative][0]), kind, name):
            errors.append(f"typed Port declaration missing or wrong kind: {relative} {kind} {name}")
    bindings_paths = sorted(
        relative for relative in SESSION_BINDINGS_ROLE_FILES
        if relative in views and mask_rust(views[relative][0]).strip()
    )
    if len(bindings_paths) != 1:
        errors.append(
            "typed Port SessionBindings role requires exactly one production file: "
            f"expected={sorted(SESSION_BINDINGS_ROLE_FILES)} actual={bindings_paths}"
        )
    else:
        relative = bindings_paths[0]
        kind, name = ("struct", "SessionBindings")
        if not top_level_declaration(mask_rust(views[relative][0]), kind, name):
            errors.append(f"typed Port declaration missing or wrong kind: {relative} {kind} {name}")
    return errors


def responsibility_errors(views: dict[str, tuple[str, int]]) -> list[str]:
    errors: list[str] = []
    role_groups = (
        ("model driver", MODEL_DRIVER_ROLE_FILES),
        ("tools ToolSet", TOOLSET_ROLE_FILES),
        ("session bindings", SESSION_BINDINGS_ROLE_FILES),
    )
    for label, candidates in role_groups:
        actual = sorted(
            relative for relative in candidates
            if relative in views and mask_rust(views[relative][0]).strip()
        )
        if not actual:
            errors.append(f"missing required production role: {label} ({sorted(candidates)})")
        elif len(actual) > 1:
            errors.append(f"multiple production files for required role {label}: {actual}")
    return errors


def port_direction_errors(root: Path, views: dict[str, tuple[str, int]]) -> list[str]:
    owners = {
        module_components(root / relative, root)[0]
        for relative in views
        if module_components(root / relative, root)
    }
    errors: list[str] = []
    port_paths = set(PORT_DECLARATIONS) | TOOLSET_ROLE_FILES | SESSION_BINDINGS_ROLE_FILES
    for relative in sorted(port_paths):
        if relative not in views:
            continue
        for target in sorted(import_targets(root, relative, views[relative][0], owners) & {"session", "agent", "runtime"}):
            errors.append(f"Port dependency violation: {relative} imports top-level {target}")
    return errors


def graph_edges(root: Path, views: dict[str, tuple[str, int]]) -> dict[str, set[str]]:
    owners = {
        module_components(root / relative, root)[0]
        for relative in views
        if relative != "src/lib.rs" and module_components(root / relative, root)
    }
    edges = {owner: set() for owner in owners}
    for relative, (text, _count) in views.items():
        if relative != "src/lib.rs":
            parts = module_components(root / relative, root)
            if parts:
                edges[parts[0]].update(import_targets(root, relative, text, owners) & owners)
    return edges


def strongly_connected(edges: dict[str, set[str]]) -> list[list[str]]:
    index = 0
    stack: list[str] = []
    on_stack: set[str] = set()
    indices: dict[str, int] = {}
    low: dict[str, int] = {}
    result: list[list[str]] = []

    def visit(node: str) -> None:
        nonlocal index
        indices[node] = low[node] = index
        index += 1
        stack.append(node)
        on_stack.add(node)
        for target in sorted(edges.get(node, ())):
            if target not in indices:
                visit(target)
                low[node] = min(low[node], low[target])
            elif target in on_stack:
                low[node] = min(low[node], indices[target])
        if low[node] == indices[node]:
            component: list[str] = []
            while True:
                target = stack.pop()
                on_stack.remove(target)
                component.append(target)
                if target == node:
                    break
            if len(component) > 1:
                result.append(sorted(component))

    for node in sorted(edges):
        if node not in indices:
            visit(node)
    return result


def scan(root: Path) -> list[str]:
    paths, source_texts, errors = read_source_texts(root)
    test_files = test_only_files(root, paths, source_texts)
    test_files.update(
        relative for relative, text in source_texts.items() if file_is_test_only(text)
    )
    errors.extend(
        f"required production path is a directory: {required}"
        for required in REQUIRED_FILES
        if (root / required).is_dir()
    )
    errors.extend(forbidden_path_errors(root, test_files))
    views: dict[str, tuple[str, int]] = {}
    total = 0
    for path in paths:
        relative = path.relative_to(root).as_posix()
        if relative not in source_texts or relative in test_files:
            continue
        raw_text = source_texts[relative]
        view, line_count, _excluded = production_view(raw_text)
        views[relative] = (view, line_count)
        total += line_count
        if line_count > MAX_FILE:
            errors.append(f"{relative}: production file exceeds {MAX_FILE} lines ({line_count})")
        if relative in PORT_FILES and line_count > MAX_PORT:
            errors.append(f"{relative}: public Port file exceeds {MAX_PORT} lines ({line_count})")
        masked = mask_rust(view)
        for symbol in FORBIDDEN_SYMBOLS:
            if relative in TRANSITIONAL_PRIVATE_FILES or relative in FORBIDDEN_SYMBOL_EXEMPTIONS.get(symbol, set()):
                continue
            if re.search(rf"\b{re.escape(symbol)}\b", masked):
                errors.append(f"{relative}: forbidden production symbol {symbol}")
        if any(forbidden_import(path) for path in imports(masked)):
            errors.append(f"{relative}: forbidden production import")
        for forbidden in forbidden_external_paths(masked):
            errors.append(f"{relative}: forbidden production import/token {forbidden}")
    if total > MAX_TOTAL:
        errors.append(f"production Rust exceeds {MAX_TOTAL} lines ({total})")
    dependencies, toml_error = cargo_dependency_packages(root)
    if toml_error:
        errors.append(toml_error)
    for dependency in sorted(dependencies & FORBIDDEN_DEPENDENCIES):
        errors.append(f"Cargo.toml: forbidden direct dependency {dependency}")
    errors.extend(root_surface_errors(views))
    errors.extend(canonical_allowlist_errors(root, test_files, source_texts))
    errors.extend(storage_allowlist_errors(root, test_files, source_texts))
    errors.extend(f"module cycle: {' -> '.join(component)}" for component in strongly_connected(graph_edges(root, views)))
    for required in sorted(REQUIRED_FILES):
        if required not in views or not mask_rust(views[required][0]).strip():
            errors.append(f"missing required production file: {required}")
    errors.extend(responsibility_errors(views))
    errors.extend(port_declaration_errors(views))
    errors.extend(port_direction_errors(root, views))
    return sorted(set(errors))


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("path", nargs="?", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        if __package__:
            from .check_v03_architecture_test import self_test
        else:
            from check_v03_architecture_test import self_test

        self_test()
        print("v0.3 architecture scanner self-test passed: cfg predicates/spans, imports, super depth, root API, Cargo TOML, paths, sizes, SCCs, cfg exclusion, lexer masking")
        return 0
    errors = scan(args.path.resolve())
    if errors:
        print("v0.3 architecture gate failed:", file=sys.stderr)
        print("\n".join(f"- {error}" for error in errors), file=sys.stderr)
        return 1
    print(f"v0.3 architecture gate passed: production_files={len(source_files(args.path))}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
