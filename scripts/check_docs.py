#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import re
import sys
import tempfile
import tomllib
import unicodedata
from collections import Counter
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from urllib.parse import unquote

if __package__:
    from .check_v03_architecture import (
        file_is_test_only,
        mask_rust,
        matching,
        production_view,
        read_source_texts,
        rust_file_prefix_end,
        test_only_files,
    )
    from .generate_acceptance_v03 import GATE_EVIDENCE, load_mapping, render_acceptance
else:
    from check_v03_architecture import (
        file_is_test_only,
        mask_rust,
        matching,
        production_view,
        read_source_texts,
        rust_file_prefix_end,
        test_only_files,
    )
    from generate_acceptance_v03 import GATE_EVIDENCE, load_mapping, render_acceptance

ROOT = Path(__file__).resolve().parent.parent
SKIP_PARTS = {".git", "target"}
INLINE_LINK_RE = re.compile(r"!?\[[^\]]*\]\(([^)]+)\)")
REFERENCE_LINK_RE = re.compile(
    r'^\s*\[[^\]]+\]:\s*(?:<([^>]+)>|([^\s]+))', re.MULTILINE
)
FENCE_RE = re.compile(r"^[ \t]*(`{3,}|~{3,})", re.MULTILINE)
HEADING_RE = re.compile(r"^#{1,6}\s+(.+?)\s*#*\s*$", re.MULTILINE)
HTML_ID_RE = re.compile(r'<(?:a|span)\s+(?:[^>]*?\s)?(?:id|name)=["\']([^"\']+)["\']', re.IGNORECASE)
RUST_NO_RUN_RE = re.compile(r"```rust,no_run\n(.*?)\n```", re.DOTALL)
ACCEPTANCE_IDS = tuple(f"AT-K{index:02d}" for index in range(1, 86))
BASELINE_COMMIT = "2fd7104"
BASELINE_METRICS = {
    "production_loc": 15_483,
    "raw_src_lines": 48_055,
    "src_rust_files": 174,
    "production_content_files": 83,
}
REQUIRED_P8_DOCS = (
    "docs/contracts/session-runtime-lifecycle.md",
    "docs/contracts/session-state.md",
    "docs/contracts/event-stream.md",
    "docs/contracts/conversation.md",
    "docs/contracts/session-log.md",
    "docs/contracts/model.md",
    "docs/contracts/tool-policy-interaction.md",
    "docs/contracts/cancellation.md",
    "docs/contracts/extensions.md",
    "docs/integration/host-boundary.md",
    "docs/migrations/v0.2-to-v0.3.md",
    "docs/acceptance-v0.3.md",
    "docs/release-v0.3.md",
)
README_EXAMPLE = "examples/session_runtime_lifecycle.rs"
NON_AUTHORITY_CURRENT = {
    "minicore-runtime-v0.3-session-runtime-refactor-spec.md",
}


def relative_parts(path: Path) -> tuple[str, ...]:
    return path.relative_to(ROOT).parts


def markdown_files() -> list[Path]:
    current: list[Path] = []
    for path in ROOT.rglob("*.md"):
        parts = relative_parts(path)
        if SKIP_PARTS.intersection(parts) or "archive" in parts:
            continue
        current.append(path)
    return sorted(current)


def link_targets(text: str) -> list[str]:
    targets = [match.group(1).strip() for match in INLINE_LINK_RE.finditer(text)]
    targets.extend(
        (match.group(1) or match.group(2)).strip()
        for match in REFERENCE_LINK_RE.finditer(text)
    )
    return targets


def split_target(target: str) -> tuple[str, str | None]:
    if target.startswith("<") and target.endswith(">"):
        target = target[1:-1]
    elif " " in target:
        target = target.split(None, 1)[0]
    file_part, separator, fragment = target.partition("#")
    return unquote(file_part), unquote(fragment) if separator else None


def slugify_heading(value: str) -> str:
    value = re.sub(r"!?\[([^\]]+)\]\([^)]+\)", r"\1", value)
    value = re.sub(r"<[^>]+>", "", value)
    value = value.replace("`", "").lower().strip()
    kept: list[str] = []
    for character in value:
        category = unicodedata.category(character)
        if character.isspace():
            kept.append("-")
        elif character in {"-", "_"} or category[0] in {"L", "N"}:
            kept.append(character)
    return re.sub(r"-+", "-", "".join(kept)).strip("-")


def anchors(path: Path) -> set[str]:
    text = path.read_text(encoding="utf-8")
    result = set(HTML_ID_RE.findall(text))
    seen: Counter[str] = Counter()
    for heading in HEADING_RE.findall(text):
        base = slugify_heading(heading)
        suffix = seen[base]
        seen[base] += 1
        result.add(base if suffix == 0 else f"{base}-{suffix}")
    return result


def check_markdown(path: Path) -> list[str]:
    errors: list[str] = []
    text = path.read_text(encoding="utf-8")
    relative = path.relative_to(ROOT)

    fences = Counter(marker[0] for marker in FENCE_RE.findall(text))
    for marker, count in fences.items():
        if count % 2:
            errors.append(f"{relative}: unbalanced {marker * 3} fenced code blocks")

    parts = relative_parts(path)
    skip_historical_links = (
        "pre-reset" in parts
        or (
            len(parts) >= 4
            and parts[:3] == ("docs", "archive", "v2")
            and parts[3] in {"adr", "review"}
        )
    )
    if not skip_historical_links:
        for raw_target in link_targets(text):
            if not raw_target or raw_target.startswith(("#", "http://", "https://", "mailto:")):
                if raw_target.startswith("#") and raw_target[1:] not in anchors(path):
                    errors.append(f"{relative}: missing local anchor {raw_target}")
                continue

            file_part, fragment = split_target(raw_target)
            resolved = (path.parent / file_part).resolve() if file_part else path.resolve()
            try:
                resolved.relative_to(ROOT)
            except ValueError:
                errors.append(f"{relative}: link escapes repository {raw_target}")
                continue
            if not resolved.exists():
                errors.append(f"{relative}: missing link target {raw_target}")
                continue
            if fragment and resolved.suffix.lower() == ".md" and fragment not in anchors(resolved):
                errors.append(f"{relative}: missing anchor {raw_target}")

    return errors


def check_adr_index() -> list[str]:
    adr_dir = ROOT / "docs/adr"
    adr_paths = {
        path.stem[:4]: path
        for path in adr_dir.glob("[0-9][0-9][0-9][0-9]-*.md")
    }
    adr_files = set(adr_paths)
    index_text = (adr_dir / "README.md").read_text(encoding="utf-8")
    section_matches = list(
        re.finditer(
            r"^## (Current|Current With Later Refinements|Historical / Superseded)$",
            index_text,
            re.MULTILINE,
        )
    )
    entry_matches = list(
        re.finditer(
            r"^\| \[([0-9]{4})\]\(([^)]+)\) \| (.+) \|$",
            index_text,
            re.MULTILINE,
        )
    )
    entries = [match.group(1) for match in entry_matches]
    counts = Counter(entries)
    indexed = set(entries)
    errors: list[str] = []
    if adr_files != indexed:
        errors.append(
            "docs/adr/README.md: ADR index mismatch "
            f"missing={sorted(adr_files - indexed)} extra={sorted(indexed - adr_files)}"
        )
    duplicates = sorted(adr for adr, count in counts.items() if count != 1)
    if duplicates:
        errors.append(f"docs/adr/README.md: ADRs must have one classification: {duplicates}")

    expected_status = {
        "Current": "Accepted",
        "Current With Later Refinements": "Partially Superseded",
        "Historical / Superseded": "Fully Superseded",
    }
    for entry in entry_matches:
        adr, target, details = entry.groups()
        section = next(
            (
                heading.group(1)
                for heading in reversed(section_matches)
                if heading.start() < entry.start()
            ),
            None,
        )
        target_path = (adr_dir / target).resolve()
        if Path(target).stem[:4] != adr:
            errors.append(f"docs/adr/README.md: [{adr}] points to mismatched target {target}")
        if not target_path.exists():
            continue
        status_match = re.search(
            r"^状态：(.+)$", target_path.read_text(encoding="utf-8"), re.MULTILINE
        )
        if section is None or status_match is None:
            errors.append(f"docs/adr/README.md: {adr} lacks section or declared status")
            continue
        if not status_match.group(1).startswith(expected_status[section]):
            errors.append(
                f"docs/adr/README.md: {adr} is in {section} but declares "
                f"{status_match.group(1)}"
            )
        if section != "Current":
            references = set(re.findall(r"\b([0-9]{4})\b", details))
            if not references:
                errors.append(f"docs/adr/README.md: {adr} lacks refinement/successor IDs")
            unknown = sorted(references - adr_files)
            if unknown:
                errors.append(
                    f"docs/adr/README.md: {adr} references unknown ADRs {unknown}"
                )
    return errors


def current_authority_files() -> list[Path]:
    return [
        ROOT / "README.md",
        ROOT / "CONTEXT.md",
        ROOT / "docs/README.md",
        ROOT / "docs/acceptance-v0.3.md",
        ROOT / "docs/architecture.md",
        ROOT / "docs/development-plan.md",
        ROOT / "docs/release-v0.3.md",
        ROOT / "docs/modules/README.md",
        ROOT / "docs/adr/README.md",
        *(ROOT / relative for relative in REQUIRED_P8_DOCS if relative.startswith("docs/contracts/")),
        ROOT / "docs/integration/host-boundary.md",
        ROOT / "docs/migrations/v0.2-to-v0.3.md",
        ROOT / "tests/README.md",
        *sorted((ROOT / "docs/adr").glob("020[0-3]-*.md")),
    ]


def authority_inventory_errors(
    actual: set[str],
    authority: set[str],
    non_authority: set[str],
) -> list[str]:
    errors: list[str] = []
    missing = authority - actual
    unexpected = actual - authority - non_authority
    if missing or unexpected:
        errors.append(
            "current Markdown inventory mismatch "
            f"missing={sorted(missing)} unexpected={sorted(unexpected)}"
        )
    overlap = authority & non_authority
    if overlap:
        errors.append(f"current Markdown inventory has conflicting authority roles: {sorted(overlap)}")
    return errors


def check_authority_inventory() -> list[str]:
    actual = {path.relative_to(ROOT).as_posix() for path in markdown_files()}
    authority = {path.relative_to(ROOT).as_posix() for path in current_authority_files()}
    return authority_inventory_errors(actual, authority, NON_AUTHORITY_CURRENT)


def check_migration_status() -> list[str]:
    path = ROOT / "docs/migration-v0.1-v0.2.md"
    if not path.exists():
        return []
    text = path.read_text(encoding="utf-8")
    folded = text.casefold()
    required = (
        "this is the final breaking migration guide",
        "the typed `runtime` replaces",
        "no compatibility wrappers",
        "does not automatically read or transform the historical store v1 layout",
        "p8 reset closure is complete",
        "p9 documentation status:",
    )
    errors: list[str] = []
    for phrase in required:
        if phrase not in folded:
            errors.append(
                f"{path.relative_to(ROOT)}: missing final migration status phrase {phrase}"
            )
    for phrase in ("p8 public switch pending", "p8 switch/deletion remains deferred"):
        if phrase in folded:
            errors.append(
                f"{path.relative_to(ROOT)}: stale migration status {phrase}"
            )
    return errors


def check_readme_example() -> list[str]:
    path = ROOT / "README.md"
    if not path.exists():
        return []
    text = path.read_text(encoding="utf-8")
    examples = RUST_NO_RUN_RE.findall(text)
    source = next((example for example in examples if "ToolSet::builder()" in example), None)
    if source is None:
        return ["README.md: missing primary rust,no_run ToolSet example"]
    required = (
        "ToolSet::builder()",
        "builder.register(host_tool);",
        "builder.build()?",
        "ToolContext",
    )
    errors = [
        f"README.md: primary ToolSet example is missing {snippet}"
        for snippet in required
        if snippet not in source
    ]
    for forbidden in ("ToolRegistry", "RuntimeConfig", "Runtime::open", "tools.build()"):
        if forbidden in source:
            errors.append(f"README.md: transitional facade appears in ToolSet example: {forbidden}")
    return errors


def check_readme_runtime_example() -> list[str]:
    path = ROOT / "README.md"
    if not path.exists():
        return []
    text = path.read_text(encoding="utf-8")
    required_intro = (
        "embeddable single-session Agent Execution Kernel",
        "One `SessionRuntime` owns exactly one loaded Session",
        "A Host manages multiple `SessionRuntime` instances",
        "all concrete storage, model, tool, workspace, and product capabilities",
    )
    errors = [
        f"README.md: missing final Host-boundary introduction {snippet}"
        for snippet in required_intro
        if snippet not in text
    ]
    example_path = ROOT / README_EXAMPLE
    if not example_path.is_file():
        errors.append(f"missing compiled lifecycle example: {README_EXAMPLE}")
        return errors
    example_text = example_path.read_text(encoding="utf-8")
    start_marker = "// README_LIFECYCLE_START\n"
    end_marker = "// README_LIFECYCLE_END"
    if start_marker not in example_text or end_marker not in example_text:
        errors.append(f"{README_EXAMPLE}: missing README synchronization markers")
        return errors
    expected = example_text.split(start_marker, 1)[1].split(end_marker, 1)[0].rstrip()
    examples = RUST_NO_RUN_RE.findall(text)
    source = next((example for example in examples if "SessionRuntime::load(" in example), None)
    if source is None:
        errors.append("README.md: missing full rust,no_run SessionRuntime lifecycle example")
        return errors
    if source != expected:
        errors.append(
            f"README.md: lifecycle example is not synchronized with {README_EXAMPLE}"
        )
    required = (
        "SessionRuntimeOptions::new(",
        "KernelConfig::default_checked()?",
        "SessionRuntime::load(session_id, opened_log, options).await?",
        "let events_result = session.take_events();",
        "let handle = session.handle();",
        "handle.watch_state()",
        "let (event_task, turn_result) = match events_result",
        "match handle.submit(input, TurnOptions::default()).await",
        "let shutdown_result = session.shutdown().await;",
        "let event_result = match event_task",
        "shutdown_result.map_err(|error| Box::new(error) as Box<dyn Error>)?;",
        "event_result?;",
        "turn_result",
    )
    errors.extend(
        f"README.md: lifecycle example is missing {snippet}"
        for snippet in required
        if snippet not in source
    )
    captured = source.find("let (event_task, turn_result)")
    shutdown = source.find("let shutdown_result = session.shutdown().await")
    event_join = source.find("let event_result = match event_task")
    propagated_shutdown = source.find("shutdown_result.map_err(")
    propagated_event = source.find("event_result?;")
    propagated_turn = source.rfind("turn_result")
    if not (
        0 <= captured < shutdown < event_join < propagated_shutdown < propagated_event
        < propagated_turn
    ):
        errors.append("README.md: event task must be awaited after SessionRuntime shutdown")
    for forbidden in (
        "LocalWorkspace",
        "Jsonl",
        "OpenAI",
        "Anthropic",
        "ToolRegistry",
        "Runtime::open",
    ):
        if forbidden in source:
            errors.append(f"README.md: lifecycle example uses nonexistent adapter {forbidden}")
    return errors


def check_p8_documents() -> list[str]:
    errors: list[str] = []
    for relative in REQUIRED_P8_DOCS:
        path = ROOT / relative
        if not path.is_file():
            errors.append(f"missing required P8 document: {relative}")
    return errors


@dataclass(frozen=True)
class RustAttribute:
    start: int
    end: int
    raw: str
    masked: str
    inner: bool


@dataclass(frozen=True)
class RustInlineModule:
    name: str
    item_start: int
    opening: int
    closing: int
    path_override: str | None
    path_error: str | None


@dataclass(frozen=True)
class RustFileModule:
    name: str
    path_attribute: str | None
    path_error: str | None
    physical_segments: tuple[str, ...]


def matching_backward(text: str, closing: int, opening: str, closing_token: str) -> int:
    depth = 1
    cursor = closing - 1
    while cursor >= 0:
        if text[cursor] == closing_token:
            depth += 1
        elif text[cursor] == opening:
            depth -= 1
            if depth == 0:
                return cursor
        cursor -= 1
    return -1


def outer_attributes_before(source: str, masked: str, item_start: int) -> list[RustAttribute]:
    cursor = item_start
    attributes: list[RustAttribute] = []
    while True:
        while cursor > 0 and masked[cursor - 1].isspace():
            cursor -= 1
        if cursor == 0 or masked[cursor - 1] != "]":
            break
        closing = cursor - 1
        opening = matching_backward(masked, closing, "[", "]")
        if opening < 1 or masked[opening - 1] != "#":
            break
        start = opening - 1
        attributes.append(
            RustAttribute(
                start,
                closing + 1,
                source[start:closing + 1],
                masked[start:closing + 1],
                False,
            )
        )
        cursor = start
    attributes.reverse()
    return attributes


def inner_attributes_from(masked: str, raw: str, start_pos: int) -> list[RustAttribute]:
    cursor = start_pos
    attributes: list[RustAttribute] = []
    while True:
        while cursor < len(masked) and masked[cursor].isspace():
            cursor += 1
        if not masked.startswith("#![", cursor):
            break
        opening = cursor + 2
        closing = matching(masked, opening, "[", "]")
        if closing < 0:
            break
        attributes.append(
            RustAttribute(
                cursor,
                closing + 1,
                raw[cursor:closing + 1],
                masked[cursor:closing + 1],
                True,
            )
        )
        cursor = closing + 1
    return attributes


def crate_inner_attributes(source: str, masked: str) -> list[RustAttribute]:
    return inner_attributes_from(masked, source, rust_file_prefix_end(source))


def inner_attribute_prefix_valid(
    masked: str,
    start_pos: int,
    attributes: list[RustAttribute],
) -> bool:
    cursor = attributes[-1].end if attributes else start_pos
    while cursor < len(masked) and masked[cursor].isspace():
        cursor += 1
    return not masked.startswith("#!", cursor)


def attribute_body(attribute: RustAttribute, *, raw: bool = False) -> str:
    text = attribute.raw if raw else attribute.masked
    opening = text.find("[")
    return text[opening + 1:-1]


def cfg_predicate(attribute: RustAttribute) -> str | None:
    match = re.fullmatch(r"\s*cfg\s*\((.*)\)\s*", attribute_body(attribute), re.DOTALL)
    return None if match is None else match.group(1)


def attribute_is_cfg_attr(attribute: RustAttribute) -> bool:
    return re.match(r"\s*cfg_attr\b", attribute_body(attribute)) is not None


def attribute_is_test(attribute: RustAttribute) -> bool:
    return re.fullmatch(
        r"\s*(?:tokio\s*::\s*)?test(?:\s*\(.*\))?\s*",
        attribute_body(attribute),
        re.DOTALL,
    ) is not None


def attribute_is_ignore(attribute: RustAttribute) -> bool:
    return re.fullmatch(
        r"\s*ignore(?:\s*=.*)?\s*",
        attribute_body(attribute),
        re.DOTALL,
    ) is not None


def attribute_path(attribute: RustAttribute) -> str | None:
    match = re.fullmatch(
        r'\s*path\s*=\s*"([^"\r\n]+)"\s*',
        attribute_body(attribute, raw=True),
        re.DOTALL,
    )
    return None if match is None else match.group(1)


def normalized_rust_module_path(value: str) -> str | None:
    if not value or "\\" in value or "\x00" in value:
        return None
    if re.match(r"^[A-Za-z]:", value):
        return None
    path = PurePosixPath(value)
    if path.is_absolute() or ".." in path.parts:
        return None
    return path.as_posix()


def validated_path_attribute(
    attributes: list[RustAttribute],
) -> tuple[str | None, str | None]:
    path_attributes = [
        attribute
        for attribute in attributes
        if re.match(r"\s*path\b", attribute_body(attribute, raw=True))
    ]
    if len(path_attributes) > 1:
        return None, "duplicate #[path] attributes"
    if not path_attributes:
        return None, None
    value = attribute_path(path_attributes[0])
    if value is None:
        return None, "malformed #[path] attribute"
    normalized = normalized_rust_module_path(value)
    if normalized is None:
        return None, "#[path] must be a nonempty relative UTF-8 path without '..'"
    return normalized, None


def cfg_attributes_enable_tests(attributes: list[RustAttribute]) -> bool:
    for attribute in attributes:
        body = attribute_body(attribute)
        if attribute_is_cfg_attr(attribute):
            return False
        predicate = cfg_predicate(attribute)
        if re.match(r"\s*cfg\b", body) and predicate is None:
            return False
        if predicate is not None and re.sub(r"\s+", "", predicate) != "test":
            return False
    return True


def inline_modules(source: str, masked: str) -> list[RustInlineModule]:
    pattern = re.compile(
        r"^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?mod[ \t]+"
        r"(?P<name>[A-Za-z_]\w*)[ \t\r\n]*\{",
        re.MULTILINE,
    )
    modules: list[RustInlineModule] = []
    for item in pattern.finditer(masked):
        opening = masked.find("{", item.start(), item.end())
        if opening < 0:
            continue
        closing = matching(masked, opening, "{", "}")
        if closing >= 0:
            attributes = outer_attributes_before(source, masked, item.start())
            path_override, path_error = validated_path_attribute(attributes)
            modules.append(
                RustInlineModule(
                    item.group("name"),
                    item.start(),
                    opening,
                    closing,
                    path_override,
                    path_error,
                )
            )
    return modules


def delimiter_stack_before(masked: str, position: int) -> list[tuple[str, int]] | None:
    stack: list[tuple[str, int]] = []
    pairs = {"}": "{", ")": "(", "]": "["}
    for index, token in enumerate(masked[:position]):
        if token in "{([":
            stack.append((token, index))
        elif token in pairs:
            if not stack or stack[-1][0] != pairs[token]:
                return None
            stack.pop()
    return stack


def item_is_module_scope(
    masked: str,
    position: int,
    modules: list[RustInlineModule],
) -> bool:
    stack = delimiter_stack_before(masked, position)
    if stack is None:
        return False
    module_openings = {
        module.opening
        for module in modules
        if module.opening < position < module.closing
    }
    return all(token == "{" and opening in module_openings for token, opening in stack)


def attributed_test(source: str, name: str) -> bool:
    masked = mask_rust(source)
    prefix_end = rust_file_prefix_end(source)
    file_attributes = crate_inner_attributes(source, masked)
    if not inner_attribute_prefix_valid(masked, prefix_end, file_attributes):
        return False
    if not cfg_attributes_enable_tests(file_attributes):
        return False
    modules = inline_modules(source, masked)
    pattern = re.compile(
        rf"^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?(?:async[ \t]+)?fn[ \t]+"
        rf"{re.escape(name)}\b",
        re.MULTILINE,
    )
    for function in pattern.finditer(masked):
        if not item_is_module_scope(masked, function.start(), modules):
            continue
        if not enclosing_module_cfgs_enable_tests(
            source,
            masked,
            function.start(),
            modules,
        ):
            continue
        attributes = outer_attributes_before(source, masked, function.start())
        if not cfg_attributes_enable_tests(attributes):
            continue
        if any(attribute_is_ignore(attribute) for attribute in attributes):
            continue
        if any(attribute_is_test(attribute) for attribute in attributes):
            return True
    return False


def file_enabled_for_acceptance(source: str) -> bool:
    masked = mask_rust(source)
    prefix_end = rust_file_prefix_end(source)
    attributes = crate_inner_attributes(source, masked)
    return inner_attribute_prefix_valid(
        masked,
        prefix_end,
        attributes,
    ) and cfg_attributes_enable_tests(attributes)


def enclosing_module_cfgs_enable_tests(
    source: str,
    masked: str,
    position: int,
    modules: list[RustInlineModule],
) -> bool:
    for module in modules:
        if module.opening < position < module.closing:
            attributes = outer_attributes_before(source, masked, module.item_start)
            if not cfg_attributes_enable_tests(attributes):
                return False
            inner_attributes = inner_attributes_from(masked, source, module.opening + 1)
            if not inner_attribute_prefix_valid(
                masked,
                module.opening + 1,
                inner_attributes,
            ):
                return False
            if not cfg_attributes_enable_tests(inner_attributes):
                return False
    return True


def inline_module_enabled(
    source: str,
    masked: str,
    module: RustInlineModule,
    modules: list[RustInlineModule],
) -> bool:
    if not item_is_module_scope(masked, module.item_start, modules):
        return False
    if not enclosing_module_cfgs_enable_tests(
        source,
        masked,
        module.item_start,
        modules,
    ):
        return False
    attributes = outer_attributes_before(source, masked, module.item_start)
    if not cfg_attributes_enable_tests(attributes):
        return False
    inner_attributes = inner_attributes_from(masked, source, module.opening + 1)
    return inner_attribute_prefix_valid(
        masked,
        module.opening + 1,
        inner_attributes,
    ) and cfg_attributes_enable_tests(inner_attributes)


def inline_physical_segments(modules: list[RustInlineModule]) -> tuple[str, ...]:
    segments: list[str] = []
    for module in modules:
        if module.path_override is None:
            segments.append(module.name)
        elif module.path_override != ".":
            segments.extend(PurePosixPath(module.path_override).parts)
    return tuple(segments)


def file_backed_modules(source: str) -> tuple[list[RustFileModule], list[str]]:
    masked = mask_rust(source)
    modules = inline_modules(source, masked)
    pattern = re.compile(
        r"^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?mod[ \t]+"
        r"(?P<name>[A-Za-z_]\w*)[ \t]*;",
        re.MULTILINE,
    )
    result: list[RustFileModule] = []
    errors: list[str] = []
    for module in modules:
        if inline_module_enabled(source, masked, module, modules) and module.path_error:
            errors.append(f"inline module {module.name}: {module.path_error}")
    for declaration in pattern.finditer(masked):
        if not item_is_module_scope(masked, declaration.start(), modules):
            continue
        if not enclosing_module_cfgs_enable_tests(
            source,
            masked,
            declaration.start(),
            modules,
        ):
            continue
        attributes = outer_attributes_before(source, masked, declaration.start())
        if not cfg_attributes_enable_tests(attributes):
            continue
        path_attribute, path_error = validated_path_attribute(attributes)
        containing = sorted(
            (
                module
                for module in modules
                if module.opening < declaration.start() < module.closing
            ),
            key=lambda module: module.opening,
        )
        if any(module.path_error is not None for module in containing):
            continue
        result.append(
            RustFileModule(
                declaration.group("name"),
                path_attribute,
                path_error,
                inline_physical_segments(containing),
            )
        )
    return result, errors


def module_directory(source_path: Path, inline_segments: tuple[str, ...]) -> Path:
    if source_path.name in {"lib.rs", "main.rs", "mod.rs"}:
        base = source_path.parent
    else:
        base = source_path.parent / source_path.stem
    return base.joinpath(*inline_segments)


def normal_module_candidates(
    source_path: Path,
    name: str,
    inline_segments: tuple[str, ...],
) -> tuple[Path, Path]:
    base = module_directory(source_path, inline_segments)
    return base / f"{name}.rs", base / name / "mod.rs"


def resolve_test_reachable_sources(root: Path) -> tuple[set[str], list[str]]:
    source_root = (root / "src").resolve()
    crate_root = source_root / "lib.rs"
    if not crate_root.is_file():
        return set(), ["Rust evidence reachability root is missing: src/lib.rs"]
    reachable: set[str] = set()
    errors: list[str] = []
    queue = [crate_root]
    while queue:
        current = queue.pop()
        relative = current.relative_to(root.resolve()).as_posix()
        if relative in reachable:
            continue
        try:
            source = current.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError) as error:
            errors.append(f"Rust evidence reachability cannot read {relative}: {error}")
            continue
        if not file_enabled_for_acceptance(source):
            continue
        reachable.add(relative)
        declarations, declaration_errors = file_backed_modules(source)
        errors.extend(f"Rust module {relative}: {error}" for error in declaration_errors)
        for declaration in declarations:
            if declaration.path_error is not None:
                errors.append(
                    f"Rust module {relative}::{declaration.name}: {declaration.path_error}"
                )
                continue
            if declaration.path_attribute is not None:
                base = module_directory(current, declaration.physical_segments)
                candidates = ((base / declaration.path_attribute).resolve(),)
            else:
                candidates = tuple(
                    candidate.resolve()
                    for candidate in normal_module_candidates(
                        current,
                        declaration.name,
                        declaration.physical_segments,
                    )
                )
            existing = [candidate for candidate in candidates if candidate.is_file()]
            if len(existing) != 1:
                state = "unresolved" if not existing else "ambiguous"
                errors.append(f"Rust module {relative}::{declaration.name} is {state}")
                continue
            child = existing[0]
            try:
                child.relative_to(source_root)
            except ValueError:
                errors.append(f"Rust module {relative}::{declaration.name} escapes src")
                continue
            queue.append(child)
    return reachable, errors


def load_cargo_manifest(root: Path) -> tuple[dict | None, list[str]]:
    path = root / "Cargo.toml"
    if not path.is_file():
        return None, ["Cargo.toml is missing"]
    try:
        with path.open("rb") as manifest_file:
            manifest = tomllib.load(manifest_file)
    except (OSError, tomllib.TOMLDecodeError) as error:
        return None, [f"Cargo.toml cannot be parsed: {error}"]
    if not isinstance(manifest.get("package"), dict):
        return None, ["Cargo.toml has no [package] table"]
    return manifest, []


def normalized_target_path(value: object) -> str | None:
    if not isinstance(value, str) or not value:
        return None
    path = PurePosixPath(value)
    if path.is_absolute() or ".." in path.parts:
        return None
    return path.as_posix()


def explicit_targets(manifest: dict, kind: str) -> list[dict]:
    value = manifest.get(kind, [])
    if not isinstance(value, list):
        return []
    return [target for target in value if isinstance(target, dict)]


def effective_target_path(target: dict, directory: str) -> str | None:
    if "path" in target:
        return normalized_target_path(target.get("path"))
    name = target.get("name")
    if not isinstance(name, str) or not name:
        return None
    return f"{directory}/{name}.rs"


def matching_explicit_targets(manifest: dict, kind: str, path: str) -> list[dict]:
    return [
        target
        for target in explicit_targets(manifest, kind)
        if effective_target_path(target, f"{kind}s") == path
        and isinstance(target.get("name"), str)
        and target["name"]
    ]


def has_required_features(target: dict) -> bool:
    required = target.get("required-features", [])
    return required not in (None, [])


def cargo_lib_evidence_error(manifest: dict, root: Path | None = None) -> str | None:
    package = manifest.get("package", {})
    explicit = "lib" in manifest
    if not explicit:
        if isinstance(package, dict) and package.get("autolib", True) is False:
            return "autolib = false leaves the package without an explicit [lib] target"
        if root is not None and not (root / "src/lib.rs").is_file():
            return "automatic library target src/lib.rs is missing"
        return None
    library = manifest.get("lib")
    if not isinstance(library, dict):
        return "[lib] is not a table"
    path = normalized_target_path(library.get("path", "src/lib.rs"))
    if path != "src/lib.rs":
        return "effective [lib] path must be src/lib.rs"
    if root is not None and not (root / path).is_file():
        return "explicit [lib] path src/lib.rs is missing"
    if library.get("test", True) is False:
        return "[lib] test = false disables source test evidence"
    if library.get("harness", True) is False:
        return "[lib] harness = false disables attributed source tests"
    if has_required_features(library):
        return "[lib] has non-guaranteed required-features"
    return None


def cargo_test_target_error(manifest: dict, path: str) -> str | None:
    package = manifest.get("package", {})
    autotests = package.get("autotests", True) is not False
    matches = matching_explicit_targets(manifest, "test", path)
    if len(matches) > 1:
        return f"multiple explicit [[test]] targets match {path}"
    if not matches:
        if autotests:
            return None
        return f"autotests = false leaves {path} without an explicit [[test]] target"
    target = matches[0]
    if target.get("test", True) is False:
        return f"explicit [[test]] target for {path} has test = false"
    if target.get("harness", True) is False:
        return f"explicit [[test]] target for {path} has harness = false"
    if has_required_features(target):
        return f"explicit [[test]] target for {path} has non-guaranteed required-features"
    return None


def cargo_example_target_error(manifest: dict, path: str) -> str | None:
    package = manifest.get("package", {})
    autoexamples = package.get("autoexamples", True) is not False
    matches = matching_explicit_targets(manifest, "example", path)
    if len(matches) > 1:
        return f"multiple explicit [[example]] targets match {path}"
    if not matches:
        if autoexamples:
            return None
        return f"autoexamples = false leaves {path} without an explicit [[example]] target"
    if has_required_features(matches[0]):
        return f"explicit [[example]] target for {path} has non-guaranteed required-features"
    return None


def cargo_rust_evidence_error(
    manifest: dict,
    path: str,
    root: Path | None = None,
) -> str | None:
    if path.startswith("src/"):
        return cargo_lib_evidence_error(manifest, root)
    if path.startswith("tests/"):
        return cargo_test_target_error(manifest, path)
    return None


def check_cargo_lifecycle_target() -> list[str]:
    manifest, errors = load_cargo_manifest(ROOT)
    if manifest is None:
        return errors
    library_error = cargo_lib_evidence_error(manifest, ROOT)
    if library_error is not None:
        return [f"Cargo.toml: lifecycle example cannot use library: {library_error}"]
    error = cargo_example_target_error(manifest, README_EXAMPLE)
    return [] if error is None else [f"Cargo.toml: {error}"]


def rust_evidence_error(
    root: Path,
    file: str,
    name: str,
    reachable_sources: set[str],
    manifest: dict | None = None,
) -> str | None:
    relative = Path(file)
    if relative.suffix != ".rs":
        return f"Rust evidence file is not .rs: {file}"
    parts = relative.parts
    if parts and parts[0] == "tests":
        if len(parts) != 2:
            return f"nested tests path is not a Cargo integration target: {file}"
    elif parts and parts[0] == "src":
        if relative.as_posix() not in reachable_sources:
            return f"Rust source evidence is not reachable from src/lib.rs: {file}"
    else:
        return f"Rust evidence path is outside src and direct tests targets: {file}"
    if manifest is not None:
        target_error = cargo_rust_evidence_error(manifest, relative.as_posix(), root)
        if target_error is not None:
            return target_error
    target = (root / relative).resolve()
    try:
        target.relative_to(root.resolve())
    except ValueError:
        return f"Rust evidence escapes repository: {file}"
    if not target.is_file():
        return f"Rust evidence file is missing: {file}"
    if not attributed_test(target.read_text(encoding="utf-8"), name):
        return f"test is missing, ignored, cfg-disabled, or unattributed: {file}::{name}"
    return None


def acceptance_mapping_errors(mapping: dict, root: Path) -> list[str]:
    errors: list[str] = []
    manifest, manifest_errors = load_cargo_manifest(root)
    errors.extend(f"scripts/acceptance_v03.json: {error}" for error in manifest_errors)
    if mapping.get("schema_version") != 1:
        errors.append("scripts/acceptance_v03.json: schema_version must be 1")
    criteria = mapping.get("criteria")
    if not isinstance(criteria, list):
        return errors + ["scripts/acceptance_v03.json: criteria must be a list"]
    identifiers = [item.get("id") for item in criteria if isinstance(item, dict)]
    if identifiers != list(ACCEPTANCE_IDS):
        errors.append(
            "scripts/acceptance_v03.json: acceptance IDs must be exactly AT-K01..AT-K85 in order"
        )
    reachable_sources, reachability_errors = resolve_test_reachable_sources(root)
    errors.extend(
        f"scripts/acceptance_v03.json: {error}" for error in reachability_errors
    )
    for item in criteria:
        if not isinstance(item, dict):
            errors.append("scripts/acceptance_v03.json: every criterion must be an object")
            continue
        identifier = item.get("id", "<missing>")
        if not isinstance(item.get("group"), str) or not item["group"].strip():
            errors.append(f"scripts/acceptance_v03.json: {identifier} has no group")
        if not isinstance(item.get("criterion"), str) or not item["criterion"].strip():
            errors.append(f"scripts/acceptance_v03.json: {identifier} has no criterion text")
        if item.get("status") != "Passed":
            errors.append(f"scripts/acceptance_v03.json: {identifier} status must be Passed")
        evidence = item.get("evidence")
        if not isinstance(evidence, list) or not evidence:
            errors.append(f"scripts/acceptance_v03.json: {identifier} has no evidence")
            continue
        for entry in evidence:
            if not isinstance(entry, dict):
                errors.append(f"scripts/acceptance_v03.json: {identifier} evidence is not an object")
                continue
            kind = entry.get("kind")
            if kind == "gate":
                gate = entry.get("gate")
                if gate not in GATE_EVIDENCE:
                    errors.append(
                        f"scripts/acceptance_v03.json: {identifier} uses unsupported gate {gate}"
                    )
                    continue
                gate_path = root / GATE_EVIDENCE[gate]["file"]
                if not gate_path.is_file():
                    errors.append(
                        f"scripts/acceptance_v03.json: {identifier} gate file is missing {gate_path}"
                    )
                continue
            if kind != "rust_test":
                errors.append(
                    f"scripts/acceptance_v03.json: {identifier} uses unsupported evidence kind {kind}"
                )
                continue
            file = entry.get("file")
            name = entry.get("test")
            if not isinstance(file, str) or not isinstance(name, str):
                errors.append(
                    f"scripts/acceptance_v03.json: {identifier} Rust evidence needs file and test"
                )
                continue
            evidence_error = rust_evidence_error(
                root,
                file,
                name,
                reachable_sources,
                manifest,
            )
            if evidence_error is not None:
                errors.append(
                    f"scripts/acceptance_v03.json: {identifier} {evidence_error}"
                )
    return errors


def acceptance_markdown_errors(mapping: dict, text: str) -> list[str]:
    if text == render_acceptance(mapping):
        return []
    return [
        "docs/acceptance-v0.3.md: content differs from scripts/acceptance_v03.json; "
        "run python3 scripts/generate_acceptance_v03.py"
    ]


def check_acceptance_matrix() -> list[str]:
    path = ROOT / "docs/acceptance-v0.3.md"
    if not path.exists():
        return []
    mapping = load_mapping(ROOT / "scripts/acceptance_v03.json")
    errors = acceptance_mapping_errors(mapping, ROOT)
    errors.extend(acceptance_markdown_errors(mapping, path.read_text(encoding="utf-8")))
    return errors


def current_source_metrics(root: Path) -> tuple[dict[str, int], list[str]]:
    paths, source_texts, source_errors = read_source_texts(root)
    if source_errors:
        return {}, source_errors
    test_files = test_only_files(root, paths, source_texts)
    test_files.update(
        relative for relative, text in source_texts.items() if file_is_test_only(text)
    )
    production_loc = 0
    production_content_files = 0
    for path in paths:
        relative = path.relative_to(root).as_posix()
        if relative in test_files:
            continue
        _view, line_count, _excluded = production_view(source_texts[relative])
        production_loc += line_count
        if line_count != 0:
            production_content_files += 1
    raw_src_lines = sum(
        len(path.read_text(encoding="utf-8").splitlines())
        for path in (root / "src").rglob("*.rs")
        if path.is_file()
    )
    return {
        "production_loc": production_loc,
        "raw_src_lines": raw_src_lines,
        "src_rust_files": len(paths),
        "production_content_files": production_content_files,
    }, []


def metric_row(label: str, baseline: int, current: int) -> str:
    delta = current - baseline
    return f"| {label} | {baseline:,} | {current:,} | {delta:+,} |"


def check_release_document() -> list[str]:
    path = ROOT / "docs/release-v0.3.md"
    if not path.exists():
        return []
    text = path.read_text(encoding="utf-8")
    errors: list[str] = []
    for index in range(1, 16):
        decision = f"D-{index:02d}"
        if len(re.findall(rf"\b{decision}\b", text)) != 1:
            errors.append(f"docs/release-v0.3.md: {decision} must appear exactly once")
    metrics, metric_errors = current_source_metrics(ROOT)
    errors.extend(f"docs/release-v0.3.md: metric source error {error}" for error in metric_errors)
    metric_labels = {
        "production_loc": "cfg(test)-excluded production LOC",
        "raw_src_lines": "raw `src/**/*.rs` lines",
        "src_rust_files": "`src` Rust files",
        "production_content_files": "files with production content",
    }
    for key, label in metric_labels.items():
        if key not in metrics:
            continue
        row = metric_row(label, BASELINE_METRICS[key], metrics[key])
        if row not in text:
            errors.append(f"docs/release-v0.3.md: missing current metric row {row}")
    required = (
        "v0.3 release validation complete and ready for publication",
        "There is no compatibility layer",
        "All functional criteria",
        "production_files=143",
        "37 package records",
        "39 records were removed, 0 added",
        f"baseline commit `{BASELINE_COMMIT}`",
        "production view used by the authoritative architecture scanner",
        "macOS and Windows",
        "Cross-platform validation is complete across Linux, macOS, and Windows",
        "pending Interaction recovery across restart",
        "plugin ABI",
        "per-Turn model override or hot model swap",
    )
    errors.extend(
        f"docs/release-v0.3.md: missing release evidence {snippet}"
        for snippet in required
        if snippet not in text
    )
    return errors


def stale_p8_status_errors(paths: list[Path], root: Path) -> list[str]:
    stale = (
        "P8 user documentation is also pending",
        "P8 remains pending",
        "P8 user documentation and release acceptance are next",
        "there is not yet a current Host-boundary guide",
        "do not yet exist",
        "awaiting remote `Cargo.lock` regeneration",
    )
    errors: list[str] = []
    for path in paths:
        if not path.exists():
            continue
        text = path.read_text(encoding="utf-8")
        for phrase in stale:
            if phrase in text:
                relative = path.relative_to(root) if path.is_relative_to(root) else path
                errors.append(f"{relative}: stale P8 status {phrase}")
    return errors


def check_p8_status() -> list[str]:
    return stale_p8_status_errors(markdown_files(), ROOT)


def check_tool_surface_docs() -> list[str]:
    path = ROOT / "README.md"
    if not path.exists():
        return []
    text = path.read_text(encoding="utf-8")
    forbidden = (
        "minicore_runtime::tools::ToolRegistry",
        "pub use registry::ToolRegistry",
        "tools.build()",
    )
    return [
        f"README.md: stale public Tool facade documentation: {snippet}"
        for snippet in forbidden
        if snippet in text
    ]


def check_current_status() -> list[str]:
    forbidden = (
        "MiniCoreRuntime",
        "RuntimeQuery",
        "CommandRequest",
        "Wire V1",
        "AgentRevisionRef",
        "ToolExecutionPlan",
        "ToolStartGate",
        "SessionFileMutationQueue",
        "WorkspaceAuthority",
        "PrepareUnload",
        "SecurityInvalidation",
        "RuntimeDependencyProbe",
        "Steer",
        "FollowUp",
        "Fork",
        "src/agent_session_lifecycle.rs",
        "src/compaction.rs",
        "src/conversation_storage.rs",
        "src/durable_state.rs",
        "src/http_transport.rs",
        "src/live_conversation.rs",
        "src/model_gateway.rs",
        "src/runtime_interface.rs",
        "src/runtime_task.rs",
        "src/session_execution.rs",
        "src/session_ingress.rs",
        "src/session_residency.rs",
        "src/session_transcript.rs",
        "src/skills.rs",
        "src/tools.rs",
        "src/turn_execution_context.rs",
        "src/turn_item_interaction.rs",
        "P8 public switch pending",
        "P8 switch/deletion remains deferred",
        "v0.1 baseline",
        "current v0.1",
        "v0.1 MVP",
        "仓库仍无`Cargo.toml`",
        "下一实现入口：创建Rust crate",
        "当前入口是创建Rust crate",
        "生产实现仍待启动",
    )
    errors: list[str] = []
    for path in current_authority_files():
        if not path.exists():
            errors.append(f"missing current authority file: {path.relative_to(ROOT)}")
            continue
        text = path.read_text(encoding="utf-8")
        if path == ROOT / "docs/migration-v0.1-v0.2.md":
            continue
        for phrase in forbidden:
            if phrase in text:
                errors.append(
                    f"{path.relative_to(ROOT)}: forbidden current-authority text {phrase}"
                )
    return errors


def self_test() -> None:
    mapping = load_mapping(ROOT / "scripts/acceptance_v03.json")
    rendered = render_acceptance(mapping)
    unrelated = rendered.replace(
        "`full_command_mailbox_cannot_block_root_shutdown`",
        "`create_on_already_initialized_log_fails_closes_and_leaves_no_task_owner`",
        1,
    )
    assert acceptance_markdown_errors(mapping, unrelated)
    assert attributed_test("#[test]\nfn exact_test() {}\n", "exact_test")
    assert attributed_test(
        '#[tokio::test(flavor = "current_thread")]\nasync fn async_test() {}\n',
        "async_test",
    )
    assert attributed_test(
        "#[cfg(test)]\nmod tests {\n    #[test]\n    fn nested_test() {}\n}\n",
        "nested_test",
    )
    assert not attributed_test(
        "macro_rules! fake_brace {\n    () => {\n        #[test]\n"
        "        fn macro_brace_test() {}\n    };\n}\n",
        "macro_brace_test",
    )
    assert not attributed_test(
        "macro_rules! fake_paren (\n    #[test]\n    fn macro_paren_test() {}\n);\n",
        "macro_paren_test",
    )
    assert not attributed_test(
        "macro_rules! fake_bracket [\n    #[test]\n    fn macro_bracket_test() {}\n];\n",
        "macro_bracket_test",
    )
    assert not attributed_test(
        "fn ordinary() {\n    #[test]\n    fn nested_function_test() {}\n}\n",
        "nested_function_test",
    )
    assert not attributed_test(
        "struct Evidence;\nimpl Evidence {\n    #[test]\n"
        "    fn impl_test() {}\n}\n",
        "impl_test",
    )
    assert not attributed_test(
        "trait Evidence {\n    #[test]\n    fn trait_test() {}\n}\n",
        "trait_test",
    )
    assert not attributed_test(
        "#[cfg(any())]\nconst DISABLED: () = {\n    #[test]\n"
        "    fn const_test() {}\n};\n",
        "const_test",
    )
    assert attributed_test("#![cfg(test)]\n#[test]\nfn file_test() {}\n", "file_test")
    assert mask_rust("#![cfg(test)]\n#[test]\nfn file_test() {}\n").startswith("#![")
    crate_prefixes = (
        "#!/usr/bin/env tool\n",
        "\ufeff",
        "\ufeff#!/usr/bin/env tool\n",
        "#!/usr/bin/env tool(arg)[x]{y}\n",
    )
    for prefix in crate_prefixes:
        assert not attributed_test(
            prefix + "#![cfg(any())]\n#[test]\nfn prefixed_test() {}\n",
            "prefixed_test",
        )
        assert attributed_test(
            prefix + "#![cfg(test)]\n#[test]\nfn prefixed_test() {}\n",
            "prefixed_test",
        )
    assert not attributed_test(
        "mod tests {\n#!/usr/bin/env tool\n#![cfg(test)]\n"
        "#[test]\nfn inline_shebang_test() {}\n}\n",
        "inline_shebang_test",
    )
    assert attributed_test(
        "#[cfg(\n    test\n)]\n#[test]\nfn multiline_test_cfg() {}\n",
        "multiline_test_cfg",
    )
    assert attributed_test(
        '#[tokio::test(\n    flavor = "current_thread"\n)]\nasync fn multiline_test_attr() {}\n',
        "multiline_test_attr",
    )
    assert not attributed_test("fn exact_test() {}\n", "exact_test")
    assert not attributed_test(
        "/* #[test]\nfn commented_test() {} */\n",
        "commented_test",
    )
    assert not attributed_test(
        'const FAKE: &str = r#"#[test]\nfn raw_test() {}"#;\n',
        "raw_test",
    )
    assert not attributed_test(
        "#[cfg(any())]\n#[test]\nfn disabled_test() {}\n",
        "disabled_test",
    )
    assert not attributed_test(
        "#[cfg(any())]\nmod tests {\n    #[test]\n    fn nested_disabled() {}\n}\n",
        "nested_disabled",
    )
    assert not attributed_test(
        "mod tests {\n    #![cfg(any())]\n    #[test]\n    fn inline_inner_false() {}\n}\n",
        "inline_inner_false",
    )
    assert not attributed_test(
        "mod tests {\n    #![cfg(\n        any()\n    )]\n    #[test]\n"
        "    fn multiline_inline_inner_false() {}\n}\n",
        "multiline_inline_inner_false",
    )
    assert not attributed_test(
        "mod tests {\n    #![cfg_attr(\n        test,\n        allow(dead_code)\n    )]\n"
        "    #[test]\n    fn inline_inner_cfg_attr() {}\n}\n",
        "inline_inner_cfg_attr",
    )
    assert attributed_test(
        "mod tests {\n    #![cfg(\n        test\n    )]\n    #[test]\n"
        "    fn inline_inner_test_cfg() {}\n}\n",
        "inline_inner_test_cfg",
    )
    assert not attributed_test(
        "#[cfg(FALSE)]\n#[test]\nfn false_test() {}\n",
        "false_test",
    )
    assert not attributed_test(
        "#[cfg(\n    any()\n)]\n#[test]\nfn multiline_false_test() {}\n",
        "multiline_false_test",
    )
    assert not attributed_test(
        "#[cfg(\n    any()\n)]\nmod tests {\n    #[test]\n    fn multiline_inline_false() {}\n}\n",
        "multiline_inline_false",
    )
    assert not attributed_test(
        "#![cfg(\n    any()\n)]\n#[test]\nfn multiline_inner_false() {}\n",
        "multiline_inner_false",
    )
    assert not attributed_test(
        "#[cfg_attr(\n    test,\n    ignore\n)]\n#[test]\nfn multiline_cfg_attr() {}\n",
        "multiline_cfg_attr",
    )
    assert not attributed_test(
        "#[test]\n#[ignore]\nfn ignored_test() {}\n",
        "ignored_test",
    )
    assert not attributed_test(
        '#[test]\n#[ignore = "external"]\nfn ignored_reason_test() {}\n',
        "ignored_reason_test",
    )
    assert not authority_inventory_errors(
        {"README.md"},
        {"README.md"},
        {"implementation-input.md"},
    )
    assert not authority_inventory_errors(
        {"README.md", "implementation-input.md"},
        {"README.md"},
        {"implementation-input.md"},
    )
    assert authority_inventory_errors(
        {"README.md", "docs/extra-current.md"},
        {"README.md"},
        set(),
    )
    default_targets = tomllib.loads("[package]\nname = 'fixture'\n")
    assert cargo_lib_evidence_error(default_targets) is None
    assert cargo_test_target_error(default_targets, "tests/direct.rs") is None
    assert cargo_example_target_error(default_targets, README_EXAMPLE) is None
    assert cargo_lib_evidence_error(
        tomllib.loads("[package]\nname = 'fixture'\n[lib]\ntest = false\n")
    ) is not None
    assert cargo_lib_evidence_error(
        tomllib.loads("[package]\nname = 'fixture'\nautolib = false\n")
    ) is not None
    assert cargo_lib_evidence_error(
        tomllib.loads(
            "[package]\nname = 'fixture'\n[lib]\npath = 'src/alternate.rs'\n"
        )
    ) is not None
    assert cargo_lib_evidence_error(
        tomllib.loads(
            "[package]\nname = 'fixture'\nautolib = false\n"
            "[lib]\npath = 'src/lib.rs'\n"
        )
    ) is None
    assert cargo_lib_evidence_error(
        tomllib.loads(
            "[package]\nname = 'fixture'\n[lib]\npath = './src/lib.rs'\n"
        )
    ) is None
    assert cargo_test_target_error(
        tomllib.loads("[package]\nname = 'fixture'\nautotests = false\n"),
        "tests/direct.rs",
    ) is not None
    assert cargo_example_target_error(
        tomllib.loads("[package]\nname = 'fixture'\nautoexamples = false\n"),
        README_EXAMPLE,
    ) is not None
    disabled_test_target = tomllib.loads(
        "[package]\nname = 'fixture'\n"
        "[[test]]\nname = 'direct'\npath = 'tests/direct.rs'\ntest = false\n"
    )
    assert cargo_test_target_error(disabled_test_target, "tests/direct.rs") is not None
    harnessless_test_target = tomllib.loads(
        "[package]\nname = 'fixture'\n"
        "[[test]]\nname = 'direct'\npath = 'tests/direct.rs'\nharness = false\n"
    )
    assert cargo_test_target_error(harnessless_test_target, "tests/direct.rs") is not None
    required_test_target = tomllib.loads(
        "[package]\nname = 'fixture'\n"
        "[[test]]\nname = 'direct'\npath = 'tests/direct.rs'\n"
        "required-features = ['optional']\n"
    )
    assert cargo_test_target_error(required_test_target, "tests/direct.rs") is not None
    required_example_target = tomllib.loads(
        "[package]\nname = 'fixture'\n"
        "[[example]]\nname = 'session_runtime_lifecycle'\n"
        "path = 'examples/session_runtime_lifecycle.rs'\n"
        "required-features = ['optional']\n"
    )
    assert cargo_example_target_error(required_example_target, README_EXAMPLE) is not None
    explicit_targets_manifest = tomllib.loads(
        "[package]\nname = 'fixture'\nautotests = false\nautoexamples = false\n"
        "[[test]]\nname = 'direct'\npath = './tests/direct.rs'\n"
        "[[example]]\nname = 'session_runtime_lifecycle'\n"
        "path = './examples/session_runtime_lifecycle.rs'\ntest = false\n"
    )
    assert cargo_test_target_error(explicit_targets_manifest, "tests/direct.rs") is None
    assert cargo_example_target_error(explicit_targets_manifest, README_EXAMPLE) is None
    with tempfile.TemporaryDirectory(prefix="minicore-docs-self-test-") as directory:
        root = Path(directory)
        stale = root / "unlisted-current.md"
        stale.write_text("# Unlisted\n\nP8 remains pending\n", encoding="utf-8")
        assert stale_p8_status_errors([stale], root)
        missing_library_root = root / "missing-library"
        missing_library_root.mkdir()
        explicit_library = tomllib.loads(
            "[package]\nname = 'fixture'\n[lib]\npath = 'src/lib.rs'\n"
        )
        assert cargo_lib_evidence_error(explicit_library, missing_library_root) is not None
        assert cargo_lib_evidence_error(default_targets, missing_library_root) is not None
        (root / "src").mkdir()
        (root / "tests/disabled_evidence").mkdir(parents=True)
        (root / "src/lib.rs").write_text(
            '#[path =\n    "reachable.rs"\n]\nmod reachable;\n'
            "#[cfg(any())]\nmod disabled;\n"
            "#[cfg(\n    any()\n)]\nmod multiline_disabled;\n"
            "mod inner_disabled;\n"
            "macro_rules! fake_brace {\n    () => {\n        mod macro_brace;\n    };\n}\n"
            "macro_rules! fake_paren (\n    mod macro_paren;\n);\n"
            "macro_rules! fake_bracket [\n    mod macro_bracket;\n];\n"
            "#[cfg(any())]\nmod disabled_inline {\n    mod child;\n}\n"
            "#[cfg(test)]\nmod enabled_inline {\n    mod child;\n}\n"
            "#[path = \"./actual\"]\nmod alias {\n    mod child;\n}\n"
            "mod foo;\n"
            "#[path = \"outer_actual\"]\nmod outer_alias {\n"
            "    #[path = \"inner_actual\"]\n    mod inner_alias {\n"
            "        mod child;\n    }\n}\n"
            "#[path = \"owned_actual\"]\nmod owned_alias {\n"
            "    #[path = \"renamed.rs\"]\n    mod child;\n}\n",
            encoding="utf-8",
        )
        (root / "src/reachable.rs").write_text(
            "#[test]\nfn reachable_test() {}\n",
            encoding="utf-8",
        )
        (root / "src/disabled.rs").write_text(
            "#[test]\nfn disabled_evidence() {}\n",
            encoding="utf-8",
        )
        (root / "src/multiline_disabled.rs").write_text(
            "#[test]\nfn multiline_disabled_evidence() {}\n",
            encoding="utf-8",
        )
        (root / "src/inner_disabled.rs").write_text(
            "#![cfg(\n    any()\n)]\n#[test]\nfn inner_disabled_evidence() {}\n",
            encoding="utf-8",
        )
        for name in ("macro_brace", "macro_paren", "macro_bracket"):
            (root / f"src/{name}.rs").write_text(
                "#[test]\nfn fake_module_test() {}\n",
                encoding="utf-8",
            )
        (root / "src/disabled_inline").mkdir()
        (root / "src/disabled_inline/child.rs").write_text(
            "#[test]\nfn disabled_inline_child() {}\n",
            encoding="utf-8",
        )
        (root / "src/enabled_inline").mkdir()
        (root / "src/enabled_inline/child.rs").write_text(
            "#[test]\nfn enabled_inline_child() {}\n",
            encoding="utf-8",
        )
        (root / "src/actual").mkdir()
        (root / "src/actual/child.rs").write_text(
            "#[test]\nfn overridden_inline_child() {}\n",
            encoding="utf-8",
        )
        (root / "src/alias").mkdir()
        (root / "src/alias/child.rs").write_text(
            "#[test]\nfn wrong_alias_child() {}\n",
            encoding="utf-8",
        )
        (root / "src/foo.rs").write_text(
            "#[path = \"actual\"]\nmod alias {\n    mod child;\n}\n",
            encoding="utf-8",
        )
        (root / "src/foo/actual").mkdir(parents=True)
        (root / "src/foo/actual/child.rs").write_text(
            "#[test]\nfn non_mod_overridden_child() {}\n",
            encoding="utf-8",
        )
        (root / "src/foo/alias").mkdir()
        (root / "src/foo/alias/child.rs").write_text(
            "#[test]\nfn wrong_non_mod_alias_child() {}\n",
            encoding="utf-8",
        )
        (root / "src/outer_actual/inner_actual").mkdir(parents=True)
        (root / "src/outer_actual/inner_actual/child.rs").write_text(
            "#[test]\nfn nested_overridden_child() {}\n",
            encoding="utf-8",
        )
        (root / "src/owned_actual").mkdir()
        (root / "src/owned_actual/renamed.rs").write_text(
            "#[test]\nfn own_path_under_override() {}\n",
            encoding="utf-8",
        )
        (root / "tests/direct.rs").write_text(
            "#[test]\nfn direct_test() {}\n",
            encoding="utf-8",
        )
        (root / "tests/direct_disabled.rs").write_text(
            "#[cfg(\n    any()\n)]\n#[test]\nfn direct_disabled() {}\n",
            encoding="utf-8",
        )
        (root / "tests/disabled_evidence/fake.rs").write_text(
            "#[test]\nfn nested_test() {}\n",
            encoding="utf-8",
        )
        reachable, reachability_errors = resolve_test_reachable_sources(root)
        assert not reachability_errors, reachability_errors
        assert "src/reachable.rs" in reachable
        assert "src/disabled.rs" not in reachable
        assert "src/multiline_disabled.rs" not in reachable
        assert "src/inner_disabled.rs" not in reachable
        assert "src/macro_brace.rs" not in reachable
        assert "src/macro_paren.rs" not in reachable
        assert "src/macro_bracket.rs" not in reachable
        assert "src/disabled_inline/child.rs" not in reachable
        assert "src/enabled_inline/child.rs" in reachable
        assert "src/actual/child.rs" in reachable
        assert "src/alias/child.rs" not in reachable
        assert "src/foo.rs" in reachable
        assert "src/foo/actual/child.rs" in reachable
        assert "src/foo/alias/child.rs" not in reachable
        assert "src/outer_actual/inner_actual/child.rs" in reachable
        assert "src/owned_actual/renamed.rs" in reachable
        assert rust_evidence_error(
            root, "src/reachable.rs", "reachable_test", reachable
        ) is None
        assert rust_evidence_error(
            root, "src/disabled.rs", "disabled_evidence", reachable
        ) is not None
        assert rust_evidence_error(
            root,
            "src/multiline_disabled.rs",
            "multiline_disabled_evidence",
            reachable,
        ) is not None
        assert rust_evidence_error(
            root,
            "src/inner_disabled.rs",
            "inner_disabled_evidence",
            reachable,
        ) is not None
        assert rust_evidence_error(
            root, "tests/direct.rs", "direct_test", reachable
        ) is None
        assert rust_evidence_error(
            root,
            "tests/direct_disabled.rs",
            "direct_disabled",
            reachable,
        ) is not None
        assert rust_evidence_error(
            root,
            "tests/disabled_evidence/fake.rs",
            "nested_test",
            reachable,
        ) is not None
        invalid_inline_paths = {
            "absolute-path": "#[path = \"/absolute\"]\nmod alias {}\n",
            "escaping-path": "#[path = \"../outside\"]\nmod alias {}\n",
            "duplicate-path": (
                "#[path = \"one\"]\n#[path = \"two\"]\nmod alias {}\n"
            ),
        }
        for fixture, source in invalid_inline_paths.items():
            invalid_root = root / fixture
            (invalid_root / "src").mkdir(parents=True)
            (invalid_root / "src/lib.rs").write_text(source, encoding="utf-8")
            _, invalid_errors = resolve_test_reachable_sources(invalid_root)
            assert invalid_errors, fixture
        disabled_root = root / "disabled-root"
        (disabled_root / "src").mkdir(parents=True)
        (disabled_root / "src/lib.rs").write_text(
            "#![cfg(any())]\nmod child;\n",
            encoding="utf-8",
        )
        (disabled_root / "src/child.rs").write_text(
            "#[test]\nfn child_test() {}\n",
            encoding="utf-8",
        )
        disabled_reachable, disabled_errors = resolve_test_reachable_sources(disabled_root)
        assert not disabled_errors, disabled_errors
        assert not disabled_reachable
        enabled_root = root / "enabled-root"
        (enabled_root / "src").mkdir(parents=True)
        (enabled_root / "src/lib.rs").write_text(
            "#![cfg(\n    test\n)]\nmod child;\n",
            encoding="utf-8",
        )
        (enabled_root / "src/child.rs").write_text(
            "#[test]\nfn child_test() {}\n",
            encoding="utf-8",
        )
        enabled_reachable, enabled_errors = resolve_test_reachable_sources(enabled_root)
        assert not enabled_errors, enabled_errors
        assert enabled_reachable == {"src/lib.rs", "src/child.rs"}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        print(
            "documentation checker self-test passed: mapping/inventory mutations, balanced "
            "attributes, module scope, cfg/ignore filtering, reachability, and Cargo target policy"
        )
        return 0
    errors: list[str] = []
    for path in markdown_files():
        errors.extend(check_markdown(path))
    errors.extend(check_adr_index())
    errors.extend(check_authority_inventory())
    errors.extend(check_migration_status())
    errors.extend(check_readme_example())
    errors.extend(check_readme_runtime_example())
    errors.extend(check_cargo_lifecycle_target())
    errors.extend(check_tool_surface_docs())
    errors.extend(check_p8_documents())
    errors.extend(check_acceptance_matrix())
    errors.extend(check_release_document())
    errors.extend(check_p8_status())
    errors.extend(check_current_status())

    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1

    print("current Markdown, P8 contracts/acceptance/release, ADR index, and status checks passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
