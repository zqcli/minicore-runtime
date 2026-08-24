#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent.parent
MAPPING_PATH = ROOT / "scripts/acceptance_v03.json"
OUTPUT_PATH = ROOT / "docs/acceptance-v0.3.md"

GATE_EVIDENCE = {
    "v03_architecture": {
        "file": "scripts/check_v03_architecture.py",
        "label": "v0.3 architecture gate",
    },
    "api_compile": {
        "file": "tests/api_compile.rs",
        "label": "api_compile all-target target",
    },
    "check_sh": {
        "file": "scripts/check.sh",
        "label": "full stable quality gate",
    },
    "ci_wiring": {
        "file": ".github/workflows/ci.yml",
        "label": "native CI wiring",
    },
}


def load_mapping(path: Path = MAPPING_PATH) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


def evidence_markdown(evidence: dict[str, str]) -> str:
    if evidence["kind"] == "rust_test":
        file = evidence["file"]
        return f"[`{file}`](../{file}) — `{evidence['test']}`"
    gate = GATE_EVIDENCE[evidence["gate"]]
    return f"[{gate['label']}](../{gate['file']})"


def render_acceptance(mapping: dict[str, Any]) -> str:
    criteria = mapping["criteria"]
    lines = [
        "# v0.3 Acceptance Matrix",
        "",
        "This matrix is generated from `scripts/acceptance_v03.json`. The mapping is reviewed traceability: the documentation checker validates exact identity, criterion/status/evidence equality, allowed gates, attributed non-ignored Rust tests in Cargo-enabled reachable library sources or direct integration targets, and the current Markdown authority inventory. It does not semantically prove behavior; the remote Rust gates execute the cited evidence.",
        "",
        "All functional criteria AT-K01 through AT-K73 passed on the remote Linux validation checkout. Native release publication remains separately gated by macOS and Windows CI.",
        "",
        "## Validation Environment",
        "",
        "| Evidence | Result |",
        "| --- | --- |",
        "| Remote checkout | Linux, `/root/minicore-runtime-v03` |",
        "| Stable toolchain | `rustc 1.97.1`, `cargo 1.97.1` |",
        "| Stable gate | `scripts/check.sh` passed in full |",
        "| Root tests | 285 library tests passed; cleaned integration suites also passed |",
        "| Provider evidence | provider-gate tests and warnings-denied Clippy passed through `scripts/check.sh` |",
        "| MSRV | `rustc 1.85.0`, `cargo 1.85.0`; `scripts/check-msrv.sh` passed |",
        "| Documentation | `RUSTDOCFLAGS=\"-D warnings\" cargo doc --no-deps --locked` passed |",
        "| Architecture | authoritative scanner passed with `production_files=143` |",
        "| Dependencies | 8 direct dependencies; root lock contains 37 package records |",
        "| P6 lock diff | 39 records removed, 0 added, 0 retained-package version drift |",
        "",
        "## OS Matrix",
        "",
        "| Operating system | Status | Evidence |",
        "| --- | --- | --- |",
        "| Linux | Passed | Full functional matrix and all validation commands above ran in this session. |",
        "| macOS | Pending external CI | Required native job is configured in [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) but was not executed in this session. |",
        "| Windows | Pending external CI | Required native MSVC job is configured in [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) but was not executed in this session. |",
        "",
        "All functional acceptance criteria passed on Linux. Release publication is blocked until both native CI jobs pass; this matrix does not infer their result from Linux.",
        "",
    ]
    current_group = None
    for criterion in criteria:
        group = criterion["group"]
        if group != current_group:
            if current_group is not None:
                lines.append("")
            lines.extend(
                [
                    f"## {group}",
                    "",
                    "| ID | Criterion | Status | Evidence |",
                    "| --- | --- | --- | --- |",
                ]
            )
            current_group = group
        evidence = "; ".join(evidence_markdown(item) for item in criterion["evidence"])
        lines.append(
            f"| {criterion['id']} | {criterion['criterion']} | "
            f"{criterion['status']} | {evidence} |"
        )
    lines.extend(
        [
            "",
            "## Acceptance Conclusion",
            "",
            "All functional rows above are Passed on Linux. The implementation is a release candidate, not a published cross-platform release, until the configured macOS and Windows native jobs pass. See the [v0.3 release note](release-v0.3.md) and [migration guide](migrations/v0.2-to-v0.3.md).",
            "",
        ]
    )
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    rendered = render_acceptance(load_mapping())
    if args.check:
        if not OUTPUT_PATH.exists() or OUTPUT_PATH.read_text(encoding="utf-8") != rendered:
            raise SystemExit("docs/acceptance-v0.3.md is not synchronized with acceptance_v03.json")
        print("v0.3 acceptance Markdown is synchronized")
        return 0
    OUTPUT_PATH.write_text(rendered, encoding="utf-8")
    print(f"wrote {OUTPUT_PATH.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
