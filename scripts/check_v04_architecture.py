#!/usr/bin/env python3
"""v0.4 architecture gate for minicore-runtime.

Checks only real boundaries against an injectable root (stdlib only, no full
file allow-list):

  1. No session-era types survive in production source.
  2. No deleted module/directory/file resurrects.
  3. Exactly one production `tokio::spawn`, owned by `AgentLoop::start`
     (`src/agent_loop/mod.rs`). Test code (directories named `tests/`, files
     named `tests.rs`, and `#[cfg(test)] mod` blocks) is excluded, so no port
     isolation or driver code may add a spawn.
  4. No `tokio::sync::Mutex` / `parking_lot`.
  5. `ExecutionConfig` exposes no `set_*` mutators.
  6. Cargo (via `tomllib`, including renamed packages) and Cargo.lock carry no
     provider / workspace / storage dependencies.
  7. The public root re-exports no session-era API.
  8. Events stay optional and out of the correctness path: public/internal
     observation queues are unbounded and non-awaiting, `try_emit` returns unit,
     and the public mpsc receiver stays inside `agent_loop/event.rs`.
  9. Production source forbids `unsafe`.

`--self-test` runs a real mutation test in a temporary tree: it plants a
SessionRuntime name, an extra production spawn (plus a `#[cfg(test)]` spawn
that must be ignored), an async mutex, an `ExecutionConfig` setter, and a
forbidden Cargo dependency, then asserts each matching checker reports them,
and finally asserts the real tree is green after cleanup.
"""

from __future__ import annotations

import argparse
import re
import sys
import tempfile
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

FORBIDDEN_NAMES = [
    "SessionLog",
    "SessionManifest",
    "SessionRuntime",
    "SessionEnvironment",
    "SessionState",
    "SessionManager",
    "SessionStorage",
    "SessionRepository",
    "SessionTranscript",
    "SessionRecovery",
    "SessionOpener",
    "ConversationLedger",
    "ConversationStore",
    "ConversationStorage",
    "DurableState",
    "HookBus",
    "Supervisor",
    "Workspace",
    # Non-type resilience markers for the deleted durable stack.
    "JSONL",
    "Jsonl",
    "Transcript",
    "Degraded",
    "Recovery",
    "Durability",
]

FORBIDDEN_PATHS = [
    "src/session",
    "src/storage",
    "src/conversation",
    "src/agent",
    "src/compaction",
    "src/context",
    "src/config",
    "src/error/operations.rs",
    "src/bindings.rs",
    "src/prompt_provider.rs",
    "examples/session_runtime_lifecycle.rs",
]

FORBIDDEN_DEPENDENCIES = [
    "rig",
    "openai",
    "anthropic",
    "sqlx",
    "rusqlite",
    "postgres",
    "tokio-postgres",
    "sled",
    "rocksdb",
    "sqlite",
]

OWNER_SPAWN_FILE = "src/agent_loop/mod.rs"


def source_files(root: Path) -> list[Path]:
    return sorted((root / "src").rglob("*.rs"))


def is_test_path(relative: str) -> bool:
    return "/tests/" in relative or Path(relative).name == "tests.rs"


def production_lines(path: Path):
    """Yield (line_no, line), skipping `#[cfg(test)] mod NAME { ... }` blocks."""
    text = path.read_text(encoding="utf-8")
    depth = 0
    in_cfg_test = False
    pending_cfg_test = False
    for index, line in enumerate(text.splitlines(), start=1):
        stripped = line.strip()
        if in_cfg_test:
            depth += line.count("{") - line.count("}")
            if depth <= 0:
                in_cfg_test = False
                depth = 0
            continue
        if re.match(r"^#\[cfg\(test\)\]", stripped):
            pending_cfg_test = True
            continue
        if pending_cfg_test:
            if re.match(r"^mod \w+(\s*\{)?$", stripped):
                in_cfg_test = True
                depth = line.count("{")
            pending_cfg_test = False
            continue
        yield index, line


def production_spawn_hits(root: Path) -> list[str]:
    hits = []
    for path in source_files(root):
        relative = path.relative_to(root / "src").as_posix()
        if is_test_path(relative):
            continue
        for line_no, line in production_lines(path):
            if re.search(r"\btokio::spawn\b", line):
                hits.append(f"src/{relative}:{line_no}")
    return hits


def check_forbidden_names(root: Path) -> list[str]:
    pattern = re.compile(r"|".join(re.escape(name) for name in FORBIDDEN_NAMES))
    problems = []
    for path in source_files(root):
        relative = path.relative_to(root / "src").as_posix()
        if is_test_path(relative):
            continue
        for line_no, line in enumerate(
            path.read_text(encoding="utf-8").splitlines(), start=1
        ):
            match = pattern.search(line)
            if match:
                problems.append(
                    f"src/{relative}:{line_no}: forbidden name {match.group(0)!r}: {line.strip()}"
                )
    return problems


def check_deleted_paths(root: Path) -> list[str]:
    return [
        f"forbidden path resurrected: {relative}"
        for relative in FORBIDDEN_PATHS
        if (root / relative).exists()
    ]


def check_single_spawn(root: Path) -> list[str]:
    hits = production_spawn_hits(root)
    if len(hits) != 1 or hits[0].split(":")[0] != OWNER_SPAWN_FILE:
        return [
            f"expected exactly one production `tokio::spawn` in {OWNER_SPAWN_FILE}, "
            f"found {hits}"
        ]
    return []


def check_no_async_mutex(root: Path) -> list[str]:
    problems = []
    for path in source_files(root):
        relative = path.relative_to(root / "src").as_posix()
        if is_test_path(relative):
            continue
        text = path.read_text(encoding="utf-8")
        if re.search(r"tokio::sync::Mutex|parking_lot", text):
            problems.append(f"src/{relative}: async mutex / parking_lot present")
    return problems


def check_execution_config(root: Path) -> list[str]:
    path = root / "src" / "execution.rs"
    if not path.exists():
        return [f"{path} missing"]
    problems = []
    text = path.read_text(encoding="utf-8")
    in_config = False
    for line_no, line in enumerate(text.splitlines(), start=1):
        if re.search(r"\bimpl ExecutionConfig\b", line):
            in_config = True
        if in_config and re.search(r"pub(?:\(crate\))?\s+fn set_\w+", line):
            problems.append(f"src/execution.rs:{line_no}: setter on ExecutionConfig")
    return problems


def _dependency_names(cargo_data: dict) -> list[str]:
    names = []
    for section in ("dependencies", "dev-dependencies", "build-dependencies"):
        table = cargo_data.get(section, {})
        if not isinstance(table, dict):
            continue
        for key, spec in table.items():
            if isinstance(spec, dict) and isinstance(spec.get("package"), str):
                names.append(spec["package"])
            else:
                names.append(key)
    return names


def check_cargo_dependencies(root: Path) -> list[str]:
    problems = []
    manifest = root / "Cargo.toml"
    if not manifest.exists():
        return [f"{manifest} missing"]
    with manifest.open("rb") as handle:
        data = tomllib.load(handle)
    for name in _dependency_names(data):
        if name in FORBIDDEN_DEPENDENCIES:
            problems.append(f"Cargo.toml depends on forbidden crate {name!r}")
    lock = root / "Cargo.lock"
    if lock.exists():
        with lock.open("rb") as handle:
            lock_data = tomllib.load(handle)
        for package in lock_data.get("package", []):
            name = package.get("name")
            if name in FORBIDDEN_DEPENDENCIES:
                problems.append(f"Cargo.lock packages forbidden crate {name!r}")
    return problems


def check_public_root(root: Path) -> list[str]:
    path = root / "src" / "lib.rs"
    if not path.exists():
        return [f"{path} missing"]
    text = path.read_text(encoding="utf-8")
    return [
        f"public root re-exports forbidden name {name!r}"
        for name in FORBIDDEN_NAMES
        if re.search(rf"\b{re.escape(name)}\b", text)
    ]


OBSERVATION_PATHS = [
    "src/agent_loop/event.rs",
    "src/agent_loop/runner/model.rs",
    "src/agent_loop/runner/tools.rs",
    "src/model/driver.rs",
]


def check_events_best_effort(root: Path) -> list[str]:
    problems = []
    sources = {}
    for relative in OBSERVATION_PATHS:
        path = root / relative
        if not path.exists():
            problems.append(f"{relative} missing")
            continue
        text = "\n".join(line for _, line in production_lines(path))
        sources[relative] = text
        if re.search(r"mpsc::(?:channel|Sender|Receiver)\b|try_send|TrySendError|dropped_before", text):
            problems.append(f"{relative}: bounded or lost-on-full observation path")
        if re.search(r"\.send\([^;]*\.await", text):
            problems.append(f"{relative}: observation send must not await")
        if relative != "src/model/driver.rs" and "mpsc::unbounded_channel()" not in text:
            problems.append(f"{relative}: observation queue must be unbounded")
    event = sources.get("src/agent_loop/event.rs", "")
    for required in [
        "mpsc::UnboundedSender<LoopEventEnvelope>",
        "mpsc::UnboundedReceiver<LoopEventEnvelope>",
        "let _ = self.sender.send(LoopEventEnvelope { event });",
    ]:
        if required not in event:
            problems.append(f"event sink misses {required}")
    signature = re.search(r"(?:async\s+)?fn try_emit[^{]*", event)
    if not signature or "->" in signature.group(0) or "async" in signature.group(0):
        problems.append("try_emit must be synchronous and return unit")
    if "mpsc::UnboundedSender<ModelDriverProgress>" not in sources.get("src/model/driver.rs", ""):
        problems.append("model progress sender must be unbounded")
    for path in source_files(root):
        relative = path.relative_to(root / "src").as_posix()
        if relative == "agent_loop/event.rs" or is_test_path(relative):
            continue
        text = "\n".join(line for _, line in production_lines(path))
        if re.search(r"mpsc::(?:Unbounded)?Receiver<\s*LoopEventEnvelope\s*>", text):
            problems.append(f"loop event receiver must stay in event.rs (found in src/{relative})")
    return problems


def check_unsafe_forbidden(root: Path) -> list[str]:
    problems = []
    for path in source_files(root):
        relative = path.relative_to(root / "src").as_posix()
        if is_test_path(relative):
            continue
        text = re.sub(r'".*?"', "", path.read_text(encoding="utf-8"))
        for line_no, line in enumerate(text.splitlines(), start=1):
            if re.search(r"\bunsafe\b", line):
                problems.append(f"src/{relative}:{line_no}: unsafe in production source")
    return problems


ALL_CHECKS = [
    ("forbidden names", check_forbidden_names),
    ("deleted paths", check_deleted_paths),
    ("single production spawn", check_single_spawn),
    ("async mutex", check_no_async_mutex),
    ("ExecutionConfig purist", check_execution_config),
    ("cargo dependencies", check_cargo_dependencies),
    ("public root", check_public_root),
    ("best-effort events", check_events_best_effort),
    ("unsafe forbidden", check_unsafe_forbidden),
]


def run_checks(root: Path) -> list[str]:
    problems: list[str] = []
    for name, check in ALL_CHECKS:
        for problem in check(root):
            problems.append(f"[{name}] {problem}")
    return problems


def self_test() -> list[str]:
    """Real mutation test: plant violations, assert each checker goes red,
    and prove `#[cfg(test)]` spawns are excluded; green after cleanup."""
    problems: list[str] = []
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        src = root / "src"
        src.mkdir()
        (src / "lib.rs").write_text("pub use SessionLog;\n", encoding="utf-8")
        (src / "mod.rs").write_text(
            "fn run() { tokio::spawn(async {}); }\n", encoding="utf-8"
        )
        (src / "session.rs").write_text("struct SessionRuntime;\n", encoding="utf-8")
        (src / "execution.rs").write_text(
            "impl ExecutionConfig { pub fn set_foo(&mut self) {} pub(crate) fn set_bar(&mut self) {} }\n",
            encoding="utf-8",
        )
        (src / "mutex.rs").write_text(
            "fn m() { let _ = tokio::sync::Mutex::new(0); let _ = parking_lot::Mutex::new(0); }\n",
            encoding="utf-8",
        )
        (root / "Cargo.toml").write_text("[dependencies]\nrig = \"0.1\"\n", encoding="utf-8")

        agent_loop = src / "agent_loop"
        agent_loop.mkdir()
        (agent_loop / "mod.rs").write_text(
            "pub fn start() { tokio::spawn(async {}); }\n", encoding="utf-8"
        )
        # Event receiver must stay in event.rs; leak it and a non-unit try_emit.
        (agent_loop / "event.rs").write_text(
            "fn try_emit(&mut self, event: LoopEvent) -> bool { true }\n"
            "struct Sink { rx: mpsc::Receiver<LoopEventEnvelope> }\n",
            encoding="utf-8",
        )
        # A deleted-path marker must trip the scanner too.
        (src / "bindings.rs").write_text("// old alias file\n", encoding="utf-8")
        # A #[cfg(test)] spawn that production scanning must ignore.
        (src / "portcall.rs").write_text(
            "#[cfg(test)]\nmod tests {\n    fn t() { tokio::spawn(async {}); }\n}\n",
            encoding="utf-8",
        )

        if not check_forbidden_names(root):
            problems.append("self-test: forbidden-name scanner is vacuous")
        spawn_hits = production_spawn_hits(root)
        if "src/portcall.rs:" in " ".join(spawn_hits):
            problems.append("self-test: #[cfg(test)] spawn leaked into production hits")
        if not check_single_spawn(root):
            problems.append("self-test: single-production-spawn scanner is vacuous")
        if not check_no_async_mutex(root):
            problems.append("self-test: async-mutex scanner is vacuous")
        if not check_execution_config(root):
            problems.append("self-test: ExecutionConfig setter scanner is vacuous")
        if not check_cargo_dependencies(root):
            problems.append("self-test: Cargo dependency scanner is vacuous")
        if not check_public_root(root):
            problems.append("self-test: public-root scanner is vacuous")
        if not check_deleted_paths(root):
            problems.append("self-test: deleted-path scanner is vacuous")
        if not check_events_best_effort(root):
            problems.append("self-test: event best-effort scanner is vacuous")

        for relative in OBSERVATION_PATHS:
            target = root / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text((ROOT / relative).read_text(encoding="utf-8"), encoding="utf-8")
        if check_events_best_effort(root):
            problems.append("self-test: valid observation paths were rejected")
        mutations = [
            ("src/agent_loop/event.rs", "mpsc::unbounded_channel()", "mpsc::channel(1)"),
            ("src/agent_loop/event.rs", "fn try_emit(&mut self, event: LoopEvent)",
             "fn try_emit(&mut self, event: LoopEvent) -> bool"),
            ("src/agent_loop/event.rs", "self.sender.send(LoopEventEnvelope { event });",
             "self.sender.send(LoopEventEnvelope { event }).await;"),
            ("src/agent_loop/runner/model.rs", "mpsc::unbounded_channel()", "mpsc::channel(1)"),
            ("src/agent_loop/runner/tools.rs", "mpsc::unbounded_channel()", "mpsc::channel(1)"),
            ("src/model/driver.rs", "progress.send(progress_event)", "progress.try_send(progress_event)"),
            ("src/model/driver.rs", "mpsc::UnboundedSender<ModelDriverProgress>",
             "mpsc::Sender<ModelDriverProgress>"),
        ]
        for relative, old, new in mutations:
            target = root / relative
            original = target.read_text(encoding="utf-8")
            if old not in original:
                problems.append(f"self-test: missing observation mutation target: {relative}: {old}")
                continue
            target.write_text(original.replace(old, new), encoding="utf-8")
            if not check_events_best_effort(root):
                problems.append(f"self-test: observation mutation escaped: {relative}: {new}")
            target.write_text(original, encoding="utf-8")
        (src / "receiver_leak.rs").write_text(
            "struct Leaked { rx: mpsc::UnboundedReceiver<LoopEventEnvelope> }\n", encoding="utf-8"
        )
        if not check_events_best_effort(root):
            problems.append("self-test: public event receiver leak escaped")

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
        print("check_v04_architecture:", file=sys.stderr)
        for problem in problems:
            print(f"  - {problem}", file=sys.stderr)
        return 1
    mode = (
        "self-test OK (mutation test red->green)"
        if args.self_test
        else "v0.4 architecture gate OK"
    )
    print(f"{mode} ({len(ALL_CHECKS)} check families)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())