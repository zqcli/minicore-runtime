#!/usr/bin/env python3
"""v0.4 documentation gate for minicore-runtime.

Checks, against an injectable root (stdlib only, no LOC budgets):

  1. The current authority set exists and is complete.
  2. Every non-archive Markdown file is authority or the tracked
     non-authority implementation spec.
  3. The root README states the exact v0.4 positioning sentence first and
     covers the ten mandatory runtime properties.
  4. docs/README.md links every contract and core page (real parsed links).
  5. No stale session-era page is kept current (no removed contract/migration
     named as authority).
  6. The compile-gate examples exist and exercise AgentLoop,
     steer/update/cancel, and the MemoryAgent composition pattern.
  7. Real Markdown link validation: every inline and reference link in every
     non-archive Markdown file resolves to an existing target, and every
     `#fragment` matches a GitHub-style heading anchor (duplicate headings
     handled in the basic `foo`, `foo-1`, ... form).

`--self-test` runs a real mutation test in a temporary tree: it plants a
broken link, an unknown Markdown file, and a missing required marker, then
asserts the matching checkers report them, and finally asserts the real tree
is green after cleanup.
"""

from __future__ import annotations

import argparse
import re
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

AUTHORITY = [
    "README.md",
    "CONTEXT.md",
    "tests/README.md",
    "docs/README.md",
    "docs/architecture.md",
    "docs/modules/README.md",
    "docs/development-plan.md",
    "docs/adr/README.md",
    "docs/adr/0300-v0.4-agent-loop-reset.md",
    "docs/contracts/agent-loop.md",
    "docs/contracts/cancellation.md",
    "docs/contracts/event-stream.md",
    "docs/contracts/history.md",
    "docs/contracts/model.md",
    "docs/contracts/prompt.md",
    "docs/contracts/tool-policy-interaction.md",
    "docs/integration/host-boundary.md",
    "docs/migrations/v0.3-to-v0.4.md",
    "docs/acceptance-v0.4.md",
    "docs/release-v0.4.md",
]

NON_AUTHORITY_SPEC = ["minicore-runtime-v0.4-flex-agent-loop-reset-spec.md"]

POSITIONING = "MiniCore Runtime is a small Rust execution core for one live agent loop."

REQUIRED_PROPERTY_MARKERS = {
    "one-shot / exactly once": "exactly once",
    "host owns sessions": "Sessions are owned by the host",
    "host owns history": "History is passed in by the host",
    "host saves LoopReport": "LoopReport is saved by the host",
    "events are not authoritative": "Events are not authoritative",
    "tool side effects / host log not atomic": "not atomic with host logging",
    "update at next request": "next model request",
    "current batch snapshot": "snapshot of the request that produced it",
    "steer at next request": "next model request",
    "update does not keep alive": "does not keep the loop alive",
    "needs Tokio": "Tokio context",
}

# Removed pages that must never be cited as current authority.
STALE_AUTHORITY = [
    "session-runtime-lifecycle.md",
    "session-state.md",
    "session-log.md",
    "conversation.md",
    "extensions.md",
    "v0.2-to-v0.3",
]

CONTRACT_LINKS = {
    "agent-loop": "contracts/agent-loop.md",
    "model": "contracts/model.md",
    "event-stream": "contracts/event-stream.md",
    "cancellation": "contracts/cancellation.md",
    "tool-policy-interaction": "contracts/tool-policy-interaction.md",
    "history": "contracts/history.md",
    "prompt": "contracts/prompt.md",
}

CORE_LINKS = {
    "architecture": "architecture.md",
    "modules": "modules/README.md",
    "development-plan": "development-plan.md",
    "adr": "adr/README.md",
    "host-boundary": "integration/host-boundary.md",
    "migration": "migrations/v0.3-to-v0.4.md",
}


def markdown_files(root: Path) -> list[Path]:
    archive = root / "docs" / "archive"
    exclude = {"target", ".git"}
    return sorted(
        p
        for p in root.rglob("*.md")
        if not any(part in exclude for part in p.parts)
        and archive not in p.parents
        and p != archive
    )


def known_files(root: Path) -> set[Path]:
    return {(root / relative).resolve() for relative in [*AUTHORITY, *NON_AUTHORITY_SPEC]}


def _strip_fences(text: str) -> list[str]:
    """Return code-free lines (fenced blocks and inline code spans removed)."""
    lines = text.split("\n")
    out = []
    in_fence = False
    for line in lines:
        if line.strip().startswith("```") or line.strip().startswith("~~~"):
            in_fence = not in_fence
            continue
        out.append(re.sub(r"`[^`]*`", "", line) if not in_fence else "")
    return out


def extract_links(text: str) -> list[tuple[str, int]]:
    """Collect (target, line_no) for inline and reference links."""
    lines = _strip_fences(text)
    references: dict[str, str] = {}
    for index, line in enumerate(lines, start=1):
        match = re.match(r"^\s*\[([^\]]+)\]:\s*(\S+)", line)
        if match:
            references[match.group(1)] = match.group(2)
    links: list[tuple[str, int]] = []
    for index, line in enumerate(lines, start=1):
        for match in re.finditer(r"\[[^\]!][^\]]*?\]\(([^)\s]+)(?:\s+[\"'][^\"']*[\"'])?\)", line):
            links.append((match.group(1), index))
        for match in re.finditer(r"\[[^\]!][^\]]*?\]\[\s*([^\]]*)\s*\]", line):
            ref = match.group(1)
            if ref in references:
                links.append((references[ref], index))
        for match in re.finditer(r"\[([^\]!][^\]]*?)\]\[\s*\]", line):
            if match.group(1) in references:
                links.append((references[match.group(1)], index))
    return links


def slugify_heading(heading: str) -> str:
    kept = [ch for ch in heading.lower() if ch.isalnum() or ch in (" ", "-")]
    return "".join(kept).replace(" ", "-")


def heading_anchors(path: Path) -> set[str]:
    anchors: set[str] = set()
    counts: dict[str, int] = {}
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return anchors
    for line in text.splitlines():
        match = re.match(r"^(#{1,6})\s+(.*)$", line)
        if not match:
            continue
        base = slugify_heading(match.group(2))
        count = counts.get(base, -1) + 1
        counts[base] = count
        anchors.add(base if count == 0 else f"{base}-{count}")
    return anchors


def check_links(root: Path) -> list[str]:
    """Every inline/reference link resolves; `#fragment`s match heading anchors."""
    problems: list[str] = []
    for source in markdown_files(root):
        text = source.read_text(encoding="utf-8")
        for target, line_no in extract_links(text):
            if target.startswith(("http://", "https://", "mailto:")):
                continue
            path_part, _, fragment = target.partition("#")
            if not path_part and not fragment:
                continue
            if not path_part:
                resolved = source
            else:
                resolved = (source.parent / path_part).resolve()
                if not resolved.exists():
                    problems.append(
                        f"{source}:{line_no}: broken link target {target!r} does not exist"
                    )
                    continue
            if fragment:
                if resolved.is_file() and resolved.suffix == ".md":
                    if fragment not in heading_anchors(resolved):
                        problems.append(
                            f"{source}:{line_no}: link {target!r} has no heading anchor"
                            f" #{fragment} in {resolved.name}"
                        )
    return problems


def check_authority_exists(root: Path) -> list[str]:
    return [f"missing authority file: {relative}" for relative in AUTHORITY if not (root / relative).exists()]


def check_markdown_inventory(root: Path) -> list[str]:
    known = known_files(root)
    return [
        f"unlisted current Markdown: {path.relative_to(root)}"
        for path in markdown_files(root)
        if path.resolve() not in known
    ]


def check_root_readme(root: Path) -> list[str]:
    path = root / "README.md"
    if not path.exists():
        return ["README.md missing"]
    text = path.read_text(encoding="utf-8")
    problems = []
    preamble = text.split("\n## ", 1)[0].replace("\n", " ")
    if POSITIONING not in preamble:
        problems.append("README first paragraph lacks the v0.4 positioning sentence")
    plain = text.replace("`", "")
    for name, marker in REQUIRED_PROPERTY_MARKERS.items():
        if marker not in plain:
            problems.append(f"README missing required property {name!r} ({marker!r})")
    return problems


def check_docs_index(root: Path) -> list[str]:
    path = root / "docs" / "README.md"
    if not path.exists():
        return ["docs/README.md missing"]
    text = path.read_text(encoding="utf-8")
    targets = {target.partition("#")[0] for target, _ in extract_links(text)}
    problems = []
    for name, link in {**CONTRACT_LINKS, **CORE_LINKS}.items():
        if link not in targets:
            problems.append(f"docs/README does not link {name!r} ({link})")
    if "non-authority" not in text or "v0.4-flex-agent-loop-reset-spec" not in text:
        problems.append("docs/README must classify the spec file as non-authority")
    return problems


def check_no_stale_authority(root: Path) -> list[str]:
    problems = []
    for relative in AUTHORITY:
        path = root / relative
        if not path.exists():
            continue
        text = path.read_text(encoding="utf-8")
        for marker in STALE_AUTHORITY:
            if marker in text:
                problems.append(f"{relative}: stale authority marker {marker!r}")
    return problems


def check_examples(root: Path) -> list[str]:
    problems = []
    agent_loop = root / "examples" / "agent_loop.rs"
    memory_agent = root / "examples" / "memory_agent.rs"
    if not agent_loop.exists():
        problems.append("examples/agent_loop.rs missing")
    else:
        text = agent_loop.read_text(encoding="utf-8")
        for marker in ["AgentLoop::start", ".join()", ".steer(", ".update(", ".cancel("]:
            if marker not in text:
                problems.append(f"examples/agent_loop.rs missing {marker!r}")
    if not memory_agent.exists():
        problems.append("examples/memory_agent.rs missing")
    else:
        text = memory_agent.read_text(encoding="utf-8")
        if "struct MemoryAgent" not in text or "AgentLoop::start" not in text:
            problems.append("examples/memory_agent.rs must define MemoryAgent over AgentLoop::start")
    return problems


ALL_CHECKS = [
    ("authority exists", check_authority_exists),
    ("markdown inventory", check_markdown_inventory),
    ("root README", check_root_readme),
    ("docs index", check_docs_index),
    ("stale authority", check_no_stale_authority),
    ("examples", check_examples),
    ("links", check_links),
]


def run_checks(root: Path) -> list[str]:
    problems: list[str] = []
    for name, check in ALL_CHECKS:
        for problem in check(root):
            problems.append(f"[{name}] {problem}")
    return problems


def self_test() -> list[str]:
    """Real mutation test: broken link, unknown md, missing marker -> red;
    after cleanup the real tree must be green."""
    problems: list[str] = []
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        (root / "docs" / "archive").mkdir(parents=True)
        # Archived Markdown is exempt from inventory and link checking.
        (root / "docs" / "archive" / "old.md").write_text(
            "# old\n\n[x](vanished.md) on an archived page\n", encoding="utf-8"
        )
        (root / "docs" / "a.md").write_text(
            "# Heading\n\nbroken [x](missing.md) and [frag](#nope) and [ref][r]\n\n[r]: other.md\n",
            encoding="utf-8",
        )
        (root / "docs" / "other.md").write_text("# Other\n", encoding="utf-8")
        (root / "README.md").write_text("# MiniCore Runtime\n\nno positioning here\n", encoding="utf-8")
        (root / "stray.md").write_text("# stray\n", encoding="utf-8")

        link_hits = check_links(root)
        if not any("missing.md" in hit for hit in link_hits):
            problems.append("self-test: broken-link scanner is vacuous")
        if not any("#nope" in hit or "no heading anchor" in hit for hit in link_hits):
            problems.append("self-test: heading-anchor scanner is vacuous")
        if any("archive" in hit for hit in link_hits):
            problems.append("self-test: archive pages must be exempt from link checks")
        inventory_hits = check_markdown_inventory(root)
        if not any("stray.md" in hit for hit in inventory_hits):
            problems.append("self-test: markdown-inventory scanner is vacuous")
        if any("archive" in hit for hit in inventory_hits):
            problems.append("self-test: archive dir must be exempt from the inventory")
        if not check_root_readme(root):
            problems.append("self-test: README required-marker scanner is vacuous")

    after = run_checks(ROOT)
    if after:
        problems.append(f"self-test: real tree not green after cleanup: {after[:3]}")
    return problems


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    problems = list(self_test()) if args.self_test else run_checks(ROOT)
    if problems:
        print("check_docs:", file=sys.stderr)
        for problem in problems:
            print(f"  - {problem}", file=sys.stderr)
        return 1
    mode = "self-test OK (mutation test red->green)" if args.self_test else "docs gate OK"
    print(f"{mode} ({len(ALL_CHECKS)} check families)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())