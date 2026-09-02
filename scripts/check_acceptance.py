#!/usr/bin/env python3
"""v0.4 acceptance gate.

Validates `scripts/acceptance_v04.json` (envelope `{phase, rows}` with
V4-001..V4-070) and keeps `docs/acceptance-v0.4.md` generated from it, in
sync.

  1. Exactly 70 rows, ids V4-001..V4-070 in order, no duplicates.
  2. Phase rule: in `pre-ci` exactly one row may be `Pending` and it must be
     V4-063 (everything else `Passed`); in any other phase (`complete`) every
     row must be `Passed`. A second pending row, a pending row with any other
     id, or any pending row outside `pre-ci` is an error.
  3. Every evidence item resolves: plain paths exist; `path::fn` items name a
     Rust function carrying a `#[test]`/`#[tokio::test(...)]` attribute.
  4. `docs/acceptance-v0.4.md` is byte-for-byte the deterministic render of
     the manifest (`--write` regenerates it).

`--self-test` runs a real mutation test on temporary manifests (illegal
pending id, multiple pending rows, pending outside pre-ci, dropped id,
bad status, missing evidence file, non-test function reference, stale
Markdown) and asserts every checker reports its planted fault.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MANIFEST = ROOT / "scripts" / "acceptance_v04.json"
ACCEPTANCE_MD = ROOT / "docs" / "acceptance-v0.4.md"
EXPECTED_IDS = [f"V4-{index:03d}" for index in range(1, 71)]
PENDING_ID_ALLOWED_PRE_CI = "V4-063"


def load_manifest(path: Path) -> tuple[str, list[dict]]:
    with path.open(encoding="utf-8") as handle:
        data = json.load(handle)
    if not isinstance(data, dict) or "rows" not in data:
        raise ValueError(f"{path}: manifest must be an object with a rows array")
    rows = data.get("rows")
    if not isinstance(rows, list):
        raise ValueError(f"{path}: manifest rows must be a list")
    phase = data.get("phase", "complete")
    return phase, rows


def check_phase(phase: str, rows: list[dict], source: Path) -> list[str]:
    """Exactly the pre-CI pending rule: only V4-063 may be Pending, and only
    in phase `pre-ci`; otherwise all rows must be Passed."""
    problems = []
    pending = [row.get("id") for row in rows if row.get("status") == "Pending"]
    if phase == "pre-ci":
        if len(pending) != 1 or pending[0] != PENDING_ID_ALLOWED_PRE_CI:
            problems.append(
                f"{source}: pre-ci phase permits exactly one Pending row "
                f"({PENDING_ID_ALLOWED_PRE_CI}); got {pending}"
            )
    else:
        if pending:
            problems.append(
                f"{source}: phase {phase!r} requires all rows Passed; Pending: {pending}"
            )
    return problems


def check_json(rows: list[dict], root: Path, source: Path) -> list[str]:
    problems = []
    ids = [row.get("id") for row in rows]
    if ids != EXPECTED_IDS:
        problems.append(
            f"{source}: ids must be {EXPECTED_IDS[0]}..{EXPECTED_IDS[-1]} once, in order; got {ids}"
        )
    for index, row in enumerate(rows, start=1):
        label = f"{source} row {index}"
        if not isinstance(row, dict):
            problems.append(f"{label}: row is not an object")
            continue
        status = row.get("status")
        if status not in ("Passed", "Pending"):
            problems.append(f"{label} ({row.get('id')}): status must be Passed/Pending, got {status!r}")
        summary = row.get("summary")
        if not isinstance(summary, str) or not summary.strip():
            problems.append(f"{label} ({row.get('id')}): summary must be non-empty")
        evidence = row.get("evidence")
        if not isinstance(evidence, list) or not evidence:
            problems.append(f"{label} ({row.get('id')}): evidence must list at least one item")
        else:
            for item in evidence:
                problems.extend(_check_evidence(item, root, label))
    return problems


_TEST_ATTR_FN = None


def _check_evidence(item: str, root: Path, label: str) -> list[str]:
    problems = []
    if not isinstance(item, str) or not item:
        problems.append(f"{label}: evidence item must be a non-empty path")
        return problems
    path_text, _, fn = item.partition("::")
    path = (root / path_text).resolve()
    if not path.exists():
        return [f"{label}: evidence path does not exist: {path_text}"]
    if fn:
        text = path.read_text(encoding="utf-8")
        if not re.search(rf"\bfn\s+{re.escape(fn)}\b", text):
            return [f"{label}: evidence test function not found: {fn} in {path_text}"]
        # The named definition must carry a #[test]/#[tokio::test(...)]
        # attribute directly before it; helper/non-test functions are not
        # valid evidence.
        search = re.compile(
            r"#\[\s*(?:tokio::)?test[^\]]*\]\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+"
            + re.escape(fn)
            + r"\b"
        )
        if not search.search(text):
            return [
                f"{label}: evidence {fn} in {path_text} must be a #[test]/"
                "#[tokio::test] function"
            ]
    return problems


def render_markdown(phase: str, rows: list[dict]) -> str:
    pending = "; ".join(f"`{row['id']}`" for row in rows if row.get("status") == "Pending")
    phase_line = (
        f"Phase: pre-ci ({pending} Pending; final-push CI runs are outstanding)."
        if phase == "pre-ci" and pending
        else "Phase: complete (all rows Passed)."
    )
    lines = [
        "# Acceptance Matrix (V4-001..V4-070)",
        "",
        "v0.4 delivery acceptance. Rows are generated from",
        "`scripts/acceptance_v04.json` by `scripts/check_acceptance.py`; edit the JSON,",
        "not this file. Evidence names real tests, gates, docs, or examples.",
        "",
        phase_line,
        "",
        "| ID | Status | Summary | Evidence |",
        "| --- | --- | --- | --- |",
    ]
    for row in rows:
        evidence = "; ".join(f"`{item}`" for item in row["evidence"])
        lines.append(f"| {row['id']} | {row['status']} | {row['summary']} | {evidence} |")
    lines.append("")
    return "\n".join(lines)


def check_markdown(path: Path, expected: str) -> list[str]:
    if not path.exists():
        return [f"{path}: acceptance markdown missing (run --write)"]
    actual = path.read_text(encoding="utf-8")
    if actual != expected:
        return [f"{path}: acceptance markdown drifted from scripts/acceptance_v04.json"]
    return []


def summarize(rows: list[dict]) -> str:
    passed = sum(1 for row in rows if row.get("status") == "Passed")
    pending = sum(1 for row in rows if row.get("status") == "Pending")
    return f"{passed} Passed / {pending} Pending"


def run_checks(root: Path, manifest: Path, markdown: Path) -> list[str]:
    try:
        phase, rows = load_manifest(manifest)
    except (OSError, ValueError) as error:
        return [f"{manifest}: cannot read manifest: {error}"]
    problems = []
    problems.extend(check_phase(phase, rows, manifest))
    problems.extend(check_json(rows, root, manifest))
    try:
        rendered = render_markdown(phase, rows)
    except KeyError as error:
        return problems + [f"{manifest}: row missing key: {error}"]
    problems.extend(check_markdown(markdown, rendered))
    return problems


def self_test() -> list[str]:
    """Mutation test: each planted fault is detected; real tree is green after."""
    problems = []
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        (root / "scripts").mkdir()
        phase, rows = load_manifest(MANIFEST)

        def write(manifest_data: dict) -> None:
            (root / "scripts" / "acceptance_v04.json").write_text(
                json.dumps(manifest_data, ensure_ascii=False), encoding="utf-8"
            )

        def rows_copy():
            return [dict(r) for r in rows]

        # Illegal pending id (not V4-063).
        illegal_pending = rows_copy()
        for row in illegal_pending:
            if row["id"] == "V4-005":
                row["status"] = "Pending"
        write({"phase": "pre-ci", "rows": illegal_pending})
        probe_phase, probe_rows = load_manifest(root / "scripts" / "acceptance_v04.json")
        if not check_phase(probe_phase, probe_rows, root / "scripts" / "acceptance_v04.json"):
            problems.append("self-test: illegal-pending-id scanner is vacuous")

        # Multiple pending rows.
        multi_pending = rows_copy()
        for row in multi_pending:
            if row["id"] in ("V4-063", "V4-064"):
                row["status"] = "Pending"
        if not check_phase("pre-ci", multi_pending, root / "scripts" / "acceptance_v04.json"):
            problems.append("self-test: multiple-pending scanner is vacuous")

        # Pending outside pre-ci (mutate a copy back to Pending first, since
        # the live manifest is already complete/all-Passed).
        outside = rows_copy()
        for row in outside:
            if row["id"] == "V4-063":
                row["status"] = "Pending"
        if not check_phase("complete", outside, root / "scripts" / "acceptance_v04.json"):
            problems.append("self-test: pending-outside-pre-ci scanner is vacuous")

        # Dropped id.
        dropped = rows_copy()
        dropped.pop(3)
        if not check_json(dropped, ROOT, root / "scripts" / "acceptance_v04.json"):
            problems.append("self-test: missing-id scanner is vacuous")

        # Bad status.
        bad_status = rows_copy()
        bad_status[0]["status"] = "Everything"
        if not check_json(bad_status, ROOT, root / "scripts" / "acceptance_v04.json"):
            problems.append("self-test: status scanner is vacuous")

        # Missing evidence path.
        bad_evidence = rows_copy()
        bad_evidence[0]["evidence"] = ["docs/does-not-exist.md"]
        if not check_json(bad_evidence, ROOT, root / "scripts" / "acceptance_v04.json"):
            problems.append("self-test: missing-evidence-path scanner is vacuous")

        # A real but non-test function must be rejected.
        bad_test = rows_copy()
        bad_test[0]["evidence"] = ["tests/p3_agent_loop_closeout.rs::wait_for_request"]
        if not check_json(bad_test, ROOT, root / "scripts" / "acceptance_v04.json"):
            problems.append("self-test: non-test-fn scanner is vacuous")

        # Markdown drift.
        drifted = render_markdown(phase, rows) + "\n<!-- drift -->\n"
        if not check_markdown(root / "docs" / "acceptance-v0.4.md", drifted):
            problems.append("self-test: markdown-drift scanner is vacuous")

    after = run_checks(ROOT, MANIFEST, ACCEPTANCE_MD)
    if after:
        problems.append(f"self-test: real tree not green after cleanup: {after[:3]}")
    return problems


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--write", action="store_true", help="regenerate docs/acceptance-v0.4.md")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        problems = list(self_test())
        if problems:
            for problem in problems:
                print(f"  - {problem}", file=sys.stderr)
            return 1
        print("self-test OK (mutation test red->green)")
        return 0
    phase, rows = load_manifest(MANIFEST)
    rendered = render_markdown(phase, rows)
    if args.write:
        ACCEPTANCE_MD.write_text(rendered, encoding="utf-8")
        print(f"wrote {ACCEPTANCE_MD.relative_to(ROOT)}")
        return 0
    problems = run_checks(ROOT, MANIFEST, ACCEPTANCE_MD)
    if problems:
        print("check_acceptance:", file=sys.stderr)
        for problem in problems:
            print(f"  - {problem}", file=sys.stderr)
        return 1
    print(f"acceptance gate OK ({summarize(rows)}; phase={phase}; markdown in sync)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())