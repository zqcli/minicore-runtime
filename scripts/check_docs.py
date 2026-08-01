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
    archived_v2 = list((ROOT / "docs/archive/v2").rglob("*.md"))
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


def check_current_status() -> list[str]:
    paths = [
        ROOT / "README.md",
        ROOT / "CONTEXT.md",
        ROOT / "docs/README.md",
        ROOT / "docs/architecture.md",
        ROOT / "docs/development-plan.md",
        ROOT / "docs/migration/v1-to-v2.md",
        ROOT / "docs/review/v2-design-review-4.md",
        *sorted((ROOT / "docs/modules").glob("*.md")),
    ]
    forbidden = (
        "仓库仍无`Cargo.toml`",
        "下一实现入口：创建Rust crate",
        "当前入口是创建Rust crate",
        "生产实现仍待启动",
    )
    errors: list[str] = []
    for path in paths:
        text = path.read_text(encoding="utf-8")
        for phrase in forbidden:
            if phrase in text:
                errors.append(f"{path.relative_to(ROOT)}: stale status phrase {phrase}")
    return errors


def main() -> int:
    errors: list[str] = []
    for path in markdown_files():
        errors.extend(check_markdown(path))
    errors.extend(check_adr_index())
    errors.extend(check_current_status())

    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1

    print("current/archive-v2 Markdown, ADR index, and status checks passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
