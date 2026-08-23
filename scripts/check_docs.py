#!/usr/bin/env python3
from __future__ import annotations

import re
import sys
import unicodedata
from collections import Counter
from pathlib import Path
from urllib.parse import unquote

ROOT = Path(__file__).resolve().parent.parent
SKIP_PARTS = {".git", "target"}
INLINE_LINK_RE = re.compile(r"!?\[[^\]]*\]\(([^)]+)\)")
REFERENCE_LINK_RE = re.compile(
    r'^\s*\[[^\]]+\]:\s*(?:<([^>]+)>|([^\s]+))', re.MULTILINE
)
FENCE_RE = re.compile(r"^[ \t]*(`{3,}|~{3,})", re.MULTILINE)
HEADING_RE = re.compile(r"^#{1,6}\s+(.+?)\s*#*\s*$", re.MULTILINE)
HTML_ID_RE = re.compile(r'<(?:a|span)\s+(?:[^>]*?\s)?(?:id|name)=["\']([^"\']+)["\']', re.IGNORECASE)


def relative_parts(path: Path) -> tuple[str, ...]:
    return path.relative_to(ROOT).parts


def markdown_files() -> list[Path]:
    current: list[Path] = []
    for path in ROOT.rglob("*.md"):
        parts = relative_parts(path)
        if SKIP_PARTS.intersection(parts) or "archive" in parts:
            continue
        current.append(path)
    # The pre-reset tree is intentionally stale. Existing v2 ADR/review prose is
    # also historical and remains unchanged; fixture Markdown remains checked.
    archived_v2 = [
        path
        for path in (ROOT / "docs/archive/v2").rglob("*.md")
        if "pre-reset" not in relative_parts(path)
    ]
    return sorted({*current, *archived_v2})


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
        ROOT / "docs/architecture.md",
        ROOT / "docs/development-plan.md",
        ROOT / "docs/migration-v0.1-v0.2.md",
        ROOT / "docs/release-v0.2-core-reset.md",
        ROOT / "docs/modules/README.md",
        ROOT / "docs/adr/README.md",
        *sorted((ROOT / "docs/formats").glob("*.md")),
        *sorted((ROOT / "docs/adr").glob("020[0-3]-*.md")),
    ]


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
    example = re.search(r"```rust,no_run\n(.*?)\n```", text, re.DOTALL)
    if example is None:
        return ["README.md: missing primary rust,no_run ToolSet example"]
    source = example.group(1)
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
        "Archive",
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


def main() -> int:
    errors: list[str] = []
    for path in markdown_files():
        errors.extend(check_markdown(path))
    errors.extend(check_adr_index())
    errors.extend(check_migration_status())
    errors.extend(check_readme_example())
    errors.extend(check_tool_surface_docs())
    errors.extend(check_current_status())

    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1

    print("current/archive-v2 Markdown, ADR index, and status checks passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
