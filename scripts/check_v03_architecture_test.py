"""Fixture-based self-tests for check_v03_architecture.py (Python 3.11+)."""
from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Callable

if __package__:
    from .check_v03_architecture import (
        MAX_FILE,
        MAX_PORT,
        PORT_DECLARATIONS,
        PUBLIC_MODULES,
        PRIVATE_MODULES,
        REQUIRED_FILES,
        ROOT_EXPORTS,
        production_view,
        scan,
    )
else:
    from check_v03_architecture import (
        MAX_FILE,
        MAX_PORT,
        PORT_DECLARATIONS,
        PUBLIC_MODULES,
        PRIVATE_MODULES,
        REQUIRED_FILES,
        ROOT_EXPORTS,
        production_view,
        scan,
    )


def make_fixture(root: Path) -> None:
    (root / "src").mkdir(parents=True)
    (root / "Cargo.toml").write_text(
        "[package]\nname = \"fixture\"\n\n[dependencies]\nserde = \"1\"\n",
        encoding="utf-8",
    )
    for relative in REQUIRED_FILES:
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("pub(crate) const _V03_FIXTURE: () = ();\n", encoding="utf-8")
    modules = "\n".join(f"pub mod {name};" for name in sorted(PUBLIC_MODULES))
    private = "\n".join(f"mod {name};" for name in sorted(PRIVATE_MODULES))
    (root / "src/lib.rs").write_text(modules + "\n" + private + "\n", encoding="utf-8")
    exports = [f"pub use {owner}::{{{', '.join(sorted(symbols))}}};" for owner, symbols in ROOT_EXPORTS.items()]
    with (root / "src/lib.rs").open("a", encoding="utf-8") as handle:
        handle.write("\n".join(exports) + "\n")
    for relative, (kind, name) in PORT_DECLARATIONS.items():
        path = root / relative
        path.write_text(f"pub {kind} {name} {{}}\n", encoding="utf-8")
    (root / "src/model/driver.rs").write_text(
        "pub(crate) const _MODEL_DRIVER: () = ();\n", encoding="utf-8"
    )
    (root / "src/agent/tool_driver.rs").write_text(
        "pub(crate) const _TOOL_DRIVER: () = ();\n", encoding="utf-8"
    )
    (root / "src/agent/runner.rs").write_text(
        "pub(crate) const _TURN_RUNNER: () = ();\n", encoding="utf-8"
    )
    (root / "src/context/driver.rs").write_text(
        "pub(crate) const _CONTEXT_DRIVER: () = ();\n", encoding="utf-8"
    )
    (root / "src/compaction/driver.rs").write_text(
        "pub(crate) const _COMPACTION_DRIVER: () = ();\n", encoding="utf-8"
    )
    (root / "src/prompt/builder.rs").write_text(
        "pub(crate) const _PROMPT_BUILDER: () = ();\n", encoding="utf-8"
    )
    (root / "src/tools/set.rs").write_text("pub struct ToolSet {}\n", encoding="utf-8")
    (root / "src/bindings.rs").write_text(
        "pub struct SessionBindings {}\n", encoding="utf-8"
    )
    (root / "src/session/runtime.rs").write_text(
        "pub struct SessionRuntime {}\npub struct SessionRuntimeOptions {}\n",
        encoding="utf-8",
    )


def expect_failure(root: Path, needle: str) -> None:
    errors = scan(root)
    assert any(needle in error for error in errors), (needle, errors)


def check_compatibility_delegate(root: Path) -> None:
    checkout = root / "delegate-checkout"
    scripts = checkout / "scripts"
    scripts.mkdir(parents=True)
    wrapper = Path(__file__).with_name("check_architecture.py").read_text(encoding="utf-8")
    (scripts / "check_architecture.py").write_text(wrapper, encoding="utf-8")
    (scripts / "check_v03_architecture.py").write_text(
        "def main():\n    print('delegate-main')\n    return 0\n",
        encoding="utf-8",
    )
    outside = root / "delegate-outside"
    outside.mkdir()
    import_command = (
        "import scripts.check_architecture as delegate; "
        "import scripts.check_v03_architecture as authoritative; "
        "assert delegate.main is authoritative.main; "
        "print('delegate-import')"
    )
    cases = [
        (
            "direct checkout",
            [sys.executable, "scripts/check_architecture.py"],
            checkout,
            "delegate-main",
        ),
        (
            "direct arbitrary cwd",
            [sys.executable, str((scripts / "check_architecture.py").resolve())],
            outside,
            "delegate-main",
        ),
        (
            "package module",
            [sys.executable, "-m", "scripts.check_architecture"],
            checkout,
            "delegate-main",
        ),
        (
            "package import",
            [sys.executable, "-c", import_command],
            checkout,
            "delegate-import",
        ),
    ]
    environment = os.environ.copy()
    environment.pop("PYTHONPATH", None)
    for name, command, cwd, marker in cases:
        result = subprocess.run(
            command,
            cwd=cwd,
            env=environment,
            capture_output=True,
            text=True,
            check=False,
        )
        assert result.returncode == 0, (name, result.stdout, result.stderr)
        assert marker in result.stdout, (name, result.stdout, result.stderr)


def self_test() -> None:
    with tempfile.TemporaryDirectory(prefix="minicore-v03-gate-") as directory:
        directory_path = Path(directory)
        check_compatibility_delegate(directory_path)
        base = directory_path / "good"
        make_fixture(base)
        assert not scan(base), scan(base)

        legitimate = directory_path / "provider" / "workspace" / "checkout"
        legitimate.parent.mkdir(parents=True)
        shutil.copytree(base, legitimate)
        (legitimate / "src/restore.rs").write_text("use my_std::fs;\n", encoding="utf-8")
        (legitimate / "src/procession.rs").write_text("struct Procession;\n", encoding="utf-8")
        (legitimate / "src/names.rs").write_text(
            "use not_reqwest::Client;\nuse crate::std::fs;\nfn internal() { crate::std::fs::read(); }\n",
            encoding="utf-8",
        )
        assert not scan(legitimate), scan(legitimate)

        cases: list[tuple[str, str, Callable[[Path], None]]] = [
            ("path", "forbidden production path", lambda root: (root / "src/runtime.rs").write_text("", encoding="utf-8")),
            ("adapter", "forbidden production storage implementation", lambda root: (root / "src/storage/conversation_jsonl.rs").write_text("", encoding="utf-8")),
            ("model-missing-driver-role", "missing required production role: model driver", lambda root: (root / "src/model/driver.rs").unlink()),
            ("agent-missing-tool-driver-role", "missing required production role: tool driver", lambda root: (root / "src/agent/tool_driver.rs").unlink()),
            ("agent-missing-turn-runner-role", "missing required production role: turn runner", lambda root: (root / "src/agent/runner.rs").unlink()),
            ("context-missing-driver-role", "missing required production role: context driver", lambda root: (root / "src/context/driver.rs").unlink()),
            ("compaction-missing-driver-role", "missing required production role: compaction driver", lambda root: (root / "src/compaction/driver.rs").unlink()),
            ("prompt-missing-builder-role", "missing required production role: prompt builder", lambda root: (root / "src/prompt/builder.rs").unlink()),
            ("tools-missing-toolset-role", "missing required production role: tools ToolSet", lambda root: (root / "src/tools/set.rs").unlink()),
            ("session-missing-runtime-role", "missing required production role: session runtime owner", lambda root: (root / "src/session/runtime.rs").unlink()),
            ("model-gateway-file", "forbidden production src/model path", lambda root: (root / "src/model/gateway.rs").write_text("pub(crate) const _MODEL_GATEWAY: () = ();\n", encoding="utf-8")),
            ("agent-extra-file", "forbidden production src/agent path", lambda root: (root / "src/agent/manager.rs").write_text("pub struct Manager;\n", encoding="utf-8")),
            ("model-extra-file", "forbidden production src/model path", lambda root: (root / "src/model/legacy.rs").write_text("pub struct Legacy;\n", encoding="utf-8")),
            ("model-driver-extra-production", "forbidden production src/model path", lambda root: (root / "src/model/driver/network.rs").write_text("pub struct Network;\n", encoding="utf-8")),
            ("model-empty-file", "forbidden production src/model path", lambda root: (root / "src/model/legacy.rs").write_text("", encoding="utf-8")),
            ("model-test-helper-production", "forbidden production src/model path", lambda root: (root / "src/model/test_helper.rs").write_text("#[cfg(test)]\npub struct FakeModel;\npub struct Production;\n", encoding="utf-8")),
            ("model-provider-file", "forbidden production src/model path", lambda root: (root / "src/model/provider.rs").write_text("pub struct Provider;\n", encoding="utf-8")),
            ("model-transport-file", "forbidden production src/model path", lambda root: (root / "src/model/transport.rs").write_text("pub struct Transport;\n", encoding="utf-8")),
            ("model-anthropic-dir", "forbidden production src/model path", lambda root: ((root / "src/model/anthropic").mkdir(), (root / "src/model/anthropic/client.rs").write_text("pub struct Client;\n", encoding="utf-8"))),
            ("model-open-ai-dir", "forbidden production src/model path", lambda root: ((root / "src/model/open_ai").mkdir(), (root / "src/model/open_ai/client.rs").write_text("pub struct Client;\n", encoding="utf-8"))),
            ("model-transport-dir", "forbidden production src/model path", lambda root: ((root / "src/model/transport").mkdir(), (root / "src/model/transport/client.rs").write_text("pub struct Client;\n", encoding="utf-8"))),
            ("model-provider-dir", "forbidden production src/model path", lambda root: ((root / "src/model/provider").mkdir(), (root / "src/model/provider/client.rs").write_text("pub struct Client;\n", encoding="utf-8"))),
            ("model-empty-dir", "forbidden production src/model path", lambda root: (root / "src/model/providers").mkdir()),
            ("tools-extra-file", "forbidden production src/tools path", lambda root: (root / "src/tools/legacy.rs").write_text("pub struct Legacy;\n", encoding="utf-8")),
            ("tools-empty-file", "forbidden production src/tools path", lambda root: (root / "src/tools/legacy.rs").write_text("", encoding="utf-8")),
            ("tools-test-helper-production", "forbidden production src/tools path", lambda root: (root / "src/tools/test_helper.rs").write_text("#[cfg(test)]\npub struct FakeTool;\npub struct Production;\n", encoding="utf-8")),
            ("tools-progress-alias", "forbidden production src/tools path", lambda root: (root / "src/tools/provider.rs").write_text("pub struct Provider;\n", encoding="utf-8")),
            ("tools-filesystem-dir", "forbidden production src/tools path", lambda root: ((root / "src/tools/filesystem").mkdir(), (root / "src/tools/filesystem/adapter.rs").write_text("pub struct Adapter;\n", encoding="utf-8"))),
            ("tools-process-dir", "forbidden production src/tools path", lambda root: ((root / "src/tools/process").mkdir(), (root / "src/tools/process/adapter.rs").write_text("pub struct Adapter;\n", encoding="utf-8"))),
            ("tools-provider-dir", "forbidden production src/tools path", lambda root: ((root / "src/tools/provider").mkdir(), (root / "src/tools/provider/adapter.rs").write_text("pub struct Adapter;\n", encoding="utf-8"))),
            ("tools-builtins-dir", "forbidden production src/tools path", lambda root: ((root / "src/tools/builtins").mkdir(), (root / "src/tools/builtins/adapter.rs").write_text("pub struct Adapter;\n", encoding="utf-8"))),
            ("tools-empty-dir", "forbidden production src/tools path", lambda root: (root / "src/tools/providers").mkdir()),
            ("symbol", "forbidden production symbol Runtime", lambda root: (root / "src/lib.rs").write_text("struct Runtime;\n", encoding="utf-8")),
            ("grouped-import", "forbidden production import", lambda root: (root / "src/lib.rs").write_text("use std :: { fs, path :: Path };\n", encoding="utf-8")),
            ("tokio-import", "forbidden production import", lambda root: (root / "src/lib.rs").write_text("use tokio :: { net :: { TcpStream as Socket } };\n", encoding="utf-8")),
            ("std-expression", "forbidden production import/token std::fs", lambda root: (root / "src/lib.rs").write_text("fn real() { std :: fs :: read(); }\n", encoding="utf-8")),
            ("absolute-std", "forbidden production import/token std::fs", lambda root: (root / "src/lib.rs").write_text("fn real() { ::std::fs::read(); ::std::env::var(\"X\"); }\n", encoding="utf-8")),
            ("absolute-tokio", "forbidden production import/token tokio::process", lambda root: (root / "src/lib.rs").write_text("fn real() { ::tokio::process::Command::new(\"x\"); }\n", encoding="utf-8")),
            ("dependency-string", "forbidden direct dependency reqwest", lambda root: (root / "Cargo.toml").write_text("[dependencies]\nreqwest = \"1\"\n", encoding="utf-8")),
            ("dependency-inline", "forbidden direct dependency reqwest", lambda root: (root / "Cargo.toml").write_text("[dependencies]\nhttp = { package = \"reqwest\", version = \"1\" }\n", encoding="utf-8")),
            ("dependency-subtable", "forbidden direct dependency reqwest", lambda root: (root / "Cargo.toml").write_text("[dependencies.reqwest]\nversion = \"1\"\n", encoding="utf-8")),
            ("dependency-dev", "forbidden direct dependency reqwest", lambda root: (root / "Cargo.toml").write_text("[dev-dependencies]\nreqwest = \"1\"\n", encoding="utf-8")),
            ("dependency-build", "forbidden direct dependency reqwest", lambda root: (root / "Cargo.toml").write_text("[build-dependencies]\nreqwest = \"1\"\n", encoding="utf-8")),
            ("dependency-dev-alias", "forbidden direct dependency reqwest", lambda root: (root / "Cargo.toml").write_text("[dev-dependencies]\nhttp = { package = \"reqwest\", version = \"1\" }\n", encoding="utf-8")),
            ("dependency-build-alias", "forbidden direct dependency reqwest", lambda root: (root / "Cargo.toml").write_text("[build-dependencies]\nhttp = { package = \"reqwest\", version = \"1\" }\n", encoding="utf-8")),
            ("dependency-workspace-key", "forbidden direct dependency cap-std", lambda root: (root / "Cargo.toml").write_text("[dependencies.cap-std]\nworkspace = true\n", encoding="utf-8")),
            ("dependency-workspace-package", "forbidden direct dependency fs4", lambda root: (root / "workspace.toml").write_text("", encoding="utf-8")),
            ("dependency-target", "forbidden direct dependency cap-primitives", lambda root: (root / "Cargo.toml").write_text("[target.'cfg(unix)'.dependencies]\ncap-primitives = { version = \"1\" }\n", encoding="utf-8")),
            ("dependency-target-dev", "forbidden direct dependency reqwest", lambda root: (root / "Cargo.toml").write_text("[target.'cfg(unix)'.dev-dependencies]\nreqwest = \"1\"\n", encoding="utf-8")),
            ("dependency-target-build", "forbidden direct dependency reqwest", lambda root: (root / "Cargo.toml").write_text("[target.'cfg(unix)'.build-dependencies]\nreqwest = \"1\"\n", encoding="utf-8")),
            ("dependency-target-dev-alias", "forbidden direct dependency reqwest", lambda root: (root / "Cargo.toml").write_text("[target.'cfg(unix)'.dev-dependencies]\nhttp = { package = \"reqwest\", version = \"1\" }\n", encoding="utf-8")),
            ("dependency-target-build-alias", "forbidden direct dependency reqwest", lambda root: (root / "Cargo.toml").write_text("[target.'cfg(unix)'.build-dependencies]\nhttp = { package = \"reqwest\", version = \"1\" }\n", encoding="utf-8")),
            ("dependency-alias-workspace", "forbidden direct dependency reqwest", lambda root: (root / "Cargo.toml").write_text("[dependencies]\nhttp = { workspace = true, package = \"reqwest\" }\n", encoding="utf-8")),
            ("dependency-workspace-dev", "forbidden direct dependency reqwest", lambda root: (root / "Cargo.toml").write_text("[workspace.dev-dependencies]\nreqwest = \"1\"\n", encoding="utf-8")),
            ("dependency-workspace-build", "forbidden direct dependency reqwest", lambda root: (root / "Cargo.toml").write_text("[workspace.build-dependencies]\nreqwest = \"1\"\n", encoding="utf-8")),
            ("dependency-workspace-target-dev", "forbidden direct dependency reqwest", lambda root: (root / "Cargo.toml").write_text("[workspace.target.'cfg(unix)'.dev-dependencies]\nreqwest = \"1\"\n", encoding="utf-8")),
            ("dependency-workspace-target-build", "forbidden direct dependency reqwest", lambda root: (root / "Cargo.toml").write_text("[workspace.target.'cfg(unix)'.build-dependencies]\nreqwest = \"1\"\n", encoding="utf-8")),
            ("port-missing", "typed Port declaration missing or wrong kind", lambda root: (root / "src/tools/tool.rs").write_text("pub(crate) const _TOOL: () = ();\n", encoding="utf-8")),
            ("policy-port-missing", "typed Port declaration missing or wrong kind", lambda root: (root / "src/tools/policy.rs").write_text("pub(crate) const _POLICY: () = ();\n", encoding="utf-8")),
            ("bindings-role-missing", "typed Port declaration missing or wrong kind", lambda root: (root / "src/bindings.rs").write_text("pub(crate) const _BINDINGS: () = ();\n", encoding="utf-8")),
            ("port-wrong-kind", "typed Port declaration missing or wrong kind", lambda root: (root / "src/tools/tool.rs").write_text("pub struct Tool {}\n", encoding="utf-8")),
            ("port-test-only", "typed Port declaration missing or wrong kind", lambda root: (root / "src/conversation/session_log.rs").write_text("#[cfg(test)]\npub trait SessionLog {}\n", encoding="utf-8")),
            ("port-comment-string", "typed Port declaration missing or wrong kind", lambda root: (root / "src/model/model.rs").write_text("// pub trait Model {}\nlet x = \"pub trait Model {}\";\n", encoding="utf-8")),
            ("port-session-direction", "Port dependency violation", lambda root: (root / "src/tools/tool.rs").write_text("use crate::session::State;\npub trait Tool {}\n", encoding="utf-8")),
            ("policy-port-session-direction", "Port dependency violation", lambda root: (root / "src/tools/policy.rs").write_text("use crate::session::State;\npub trait ToolPolicy {}\n", encoding="utf-8")),
            ("bindings-runtime-direction", "Port dependency violation", lambda root: (root / "src/bindings.rs").write_text("use crate::runtime::Inner;\npub struct SessionBindings {}\n", encoding="utf-8")),
            ("port-signature-direction", "Port dependency violation", lambda root: (root / "src/tools/tool.rs").write_text("pub trait Tool { fn state(&self) -> crate::session::State; }\n", encoding="utf-8")),
            ("port-nested-declaration", "typed Port declaration missing or wrong kind", lambda root: (root / "src/tools/tool.rs").write_text("mod nested { pub trait Tool {} }\n", encoding="utf-8")),
            ("port-agent-direction", "Port dependency violation", lambda root: (root / "src/context/provider.rs").write_text("use crate::agent::Runner;\npub trait ContextProvider {}\n", encoding="utf-8")),
            ("port-runtime-direction", "Port dependency violation", lambda root: (root / "src/conversation/session_log.rs").write_text("use crate::runtime::Inner;\npub trait SessionLog {}\n", encoding="utf-8")),
            ("final-legacy-observation", "forbidden production symbol SessionSnapshot", lambda root: (root / "src/session/state.rs").write_text("pub struct SessionState {}\npub struct SessionSnapshot;\n", encoding="utf-8")),
            ("storage-jsonl-dir", "forbidden production storage implementation", lambda root: ((root / "src/storage/jsonl").mkdir(), (root / "src/storage/jsonl/adapter.rs").write_text("", encoding="utf-8"))),
            ("storage-store-dir", "forbidden production storage implementation", lambda root: ((root / "src/storage/store").mkdir(), (root / "src/storage/store/adapter.rs").write_text("", encoding="utf-8"))),
            ("storage-conversation-jsonl-dir", "forbidden production storage implementation", lambda root: ((root / "src/storage/conversation_jsonl").mkdir(), (root / "src/storage/conversation_jsonl/adapter.rs").write_text("", encoding="utf-8"))),
            ("storage-empty-dir", "forbidden production storage implementation", lambda root: (root / "src/storage/jsonl").mkdir()),
            ("storage-adapters", "forbidden production storage implementation", lambda root: [
                (root / "src/storage" / name).write_text("pub(crate) const _ADAPTER: () = ();\n", encoding="utf-8")
                for name in ("sqlite.rs", "postgres.rs", "adapters.rs", "file_store.rs")
            ]),
            ("storage-helper-production", "forbidden production storage implementation", lambda root: (root / "src/storage/test_helper.rs").write_text("#[cfg(test)]\npub struct FakeLog;\npub struct Production;\n", encoding="utf-8")),
            ("source-directory", "non-regular Rust source path", lambda root: (root / "src/odd.rs").mkdir()),
            ("file-size", "production file exceeds", lambda root: (root / "src/lib.rs").write_text("x\n" * (MAX_FILE + 1), encoding="utf-8")),
            ("port-size", "public Port file exceeds", lambda root: (root / "src/model/model.rs").write_text("x\n" * (MAX_PORT + 1), encoding="utf-8")),
            ("total-size", "production Rust exceeds", lambda root: [
                (root / "src" / f"extra_{index}.rs").write_text("x\n" * MAX_FILE, encoding="utf-8") for index in range(17)
            ]),
            ("missing-agent", "missing required production file: src/agent/tool_driver.rs", lambda root: (root / "src/agent/tool_driver.rs").unlink()),
            ("missing-prompt", "missing required production file: src/prompt/builder.rs", lambda root: (root / "src/prompt/builder.rs").unlink()),
            ("required-directory", "required production path is a directory", lambda root: (root / "src/agent/tool_driver.rs").unlink() or (root / "src/agent/tool_driver.rs").mkdir()),
            ("invalid-utf8", "unreadable production source", lambda root: (root / "src/value.rs").write_bytes(b"pub(crate) const _BAD: &[u8] = b'" + bytes([255]) + b"';\n")),
            ("missing-export", "missing root export: value::BoundedText", lambda root: (root / "src/lib.rs").write_text("", encoding="utf-8")),
            ("alias", "root export alias is forbidden", lambda root: (root / "src/lib.rs").write_text("pub use value::BoundedText as Text;\n", encoding="utf-8")),
            ("glob", "root export glob is forbidden", lambda root: (root / "src/lib.rs").write_text("pub use value::*;\n", encoding="utf-8")),
            ("extra-export", "extra root export", lambda root: (root / "src/lib.rs").write_text("pub use value::{BoundedText, Extra};\n", encoding="utf-8")),
            ("duplicate-export", "duplicate root export", lambda root: (root / "src/lib.rs").write_text("pub use value::{BoundedText};\npub use value::BoundedText;\n", encoding="utf-8")),
            ("unsupported-export", "unsupported or extra root export", lambda root: (root / "src/lib.rs").write_text("pub use value;\n", encoding="utf-8")),
            ("extra-private-module", "unexpected private modules", lambda root: (root / "src/lib.rs").write_text("mod rogue;\n", encoding="utf-8")),
        ]
        for name, needle, mutate in cases:
            fixture = directory_path / name
            shutil.copytree(base, fixture)
            if name == "dependency-workspace-package":
                (fixture / "Cargo.toml").write_text("[workspace.dependencies]\nfs4 = \"1\"\n", encoding="utf-8")
            mutate(fixture)
            expect_failure(fixture, needle)

        metadata = directory_path / "metadata-dependency"
        shutil.copytree(base, metadata)
        (metadata / "Cargo.toml").write_text(
            "[dependencies]\nserde = \"1\"\n[package.metadata.dependencies]\nreqwest = \"1\"\n"
            "[workspace.metadata.dependencies]\ncap-std = \"1\"\n"
            "[target.'cfg(unix)'.package.metadata.dependencies]\nfs4 = \"1\"\n",
            encoding="utf-8",
        )
        assert not scan(metadata), scan(metadata)

        storage_test = directory_path / "storage-test-helper"
        shutil.copytree(base, storage_test)
        (storage_test / "src/storage/test_helper.rs").write_text(
            "#[cfg(test)]\npub struct FakeLog;\n\n// trailing test-only comment\n/* trailing block comment */\n",
            encoding="utf-8",
        )
        assert not scan(storage_test), scan(storage_test)

        canonical_test = directory_path / "canonical-test-helper"
        shutil.copytree(base, canonical_test)
        (canonical_test / "src/model/test_helper.rs").write_text(
            "#[cfg(test)]\npub struct FakeModel;\n\n// trailing model helper comment\n",
            encoding="utf-8",
        )
        (canonical_test / "src/tools/test_helper.rs").write_text(
            "#[cfg(test)]\npub struct FakeTool;\n\n/* trailing tools helper comment */\n",
            encoding="utf-8",
        )
        (canonical_test / "src/model/crate_helper.rs").write_text("#![cfg(test)]\npub struct CrateOnlyModel;\n", encoding="utf-8")
        (canonical_test / "src/model/mod.rs").write_text(
            "pub(crate) const _MODEL: () = ();\n#[cfg(test)]\nmod nested_helper;\n",
            encoding="utf-8",
        )
        (canonical_test / "src/model/nested_helper.rs").write_text("#[cfg(test)]\npub struct NestedModel;\n", encoding="utf-8")
        assert not scan(canonical_test), scan(canonical_test)

        cfg_directory_modules = directory_path / "cfg-directory-modules"
        shutil.copytree(base, cfg_directory_modules)
        (cfg_directory_modules / "src/model/mod.rs").write_text(
            "pub(crate) const _MODEL: () = ();\n#[cfg(test)]\nmod model_fixture;\n",
            encoding="utf-8",
        )
        (cfg_directory_modules / "src/model/model_fixture").mkdir()
        (cfg_directory_modules / "src/model/model_fixture/mod.rs").write_text(
            "pub struct Runtime;\n", encoding="utf-8"
        )
        (cfg_directory_modules / "src/tools/mod.rs").write_text(
            "pub(crate) const _TOOLS: () = ();\n#[cfg(test)]\nmod tools_fixture;\n",
            encoding="utf-8",
        )
        (cfg_directory_modules / "src/tools/tools_fixture").mkdir()
        (cfg_directory_modules / "src/tools/tools_fixture/mod.rs").write_text(
            "pub struct ToolRegistry;\n", encoding="utf-8"
        )
        (cfg_directory_modules / "src/storage/mod.rs").write_text(
            "pub(crate) const _STORAGE: () = ();\n#[cfg(test)]\nmod storage_fixture;\n",
            encoding="utf-8",
        )
        (cfg_directory_modules / "src/storage/storage_fixture").mkdir()
        (cfg_directory_modules / "src/storage/storage_fixture/mod.rs").write_text(
            "use std::fs;\npub struct Runtime;\n", encoding="utf-8"
        )
        expect_failure(cfg_directory_modules, "forbidden final source symbol")

        for variant in ("file", "directory"):
            root_cfg_modules = directory_path / f"root-cfg-{variant}"
            shutil.copytree(base, root_cfg_modules)
            with (root_cfg_modules / "src/lib.rs").open("a", encoding="utf-8") as handle:
                handle.write("#[cfg(test)]\nmod helper;\n")
            if variant == "file":
                (root_cfg_modules / "src/helper.rs").write_text("pub struct Runtime;\n", encoding="utf-8")
            else:
                (root_cfg_modules / "src/helper").mkdir()
                (root_cfg_modules / "src/helper/mod.rs").write_text("pub struct Workspace;\n", encoding="utf-8")
            expect_failure(root_cfg_modules, "forbidden final source symbol")

        non_test_module = directory_path / "non-test-module"
        shutil.copytree(base, non_test_module)
        (non_test_module / "src/model/mod.rs").write_text(
            "pub(crate) const _MODEL: () = ();\nmod model_fixture;\n",
            encoding="utf-8",
        )
        (non_test_module / "src/model/model_fixture").mkdir()
        (non_test_module / "src/model/model_fixture/mod.rs").write_text(
            "pub struct Runtime;\n", encoding="utf-8"
        )
        expect_failure(non_test_module, "forbidden production src/model path")

        optional = directory_path / "optional-files"
        shutil.copytree(base, optional)
        (optional / "src/model/request.rs").write_text("pub(crate) const _REQUEST: () = ();\n", encoding="utf-8")
        (optional / "src/tools/progress.rs").write_text("pub(crate) const _PROGRESS: () = ();\n", encoding="utf-8")
        assert not scan(optional), scan(optional)

        legacy_roles = directory_path / "legacy-role-files"
        shutil.copytree(base, legacy_roles)
        (legacy_roles / "src/model/legacy_gateway.rs").write_text("pub(crate) const _MODEL_GATEWAY: () = ();\n", encoding="utf-8")
        (legacy_roles / "src/tools/registry.rs").write_text("pub(crate) const _REGISTRY: () = ();\n", encoding="utf-8")
        expect_failure(legacy_roles, "forbidden final legacy path")

        transitional_policy = directory_path / "transitional-policy"
        shutil.copytree(base, transitional_policy)
        (transitional_policy / "src/tools/legacy_policy.rs").write_text(
            "pub(crate) trait LegacyToolPolicy {}\n", encoding="utf-8"
        )
        expect_failure(transitional_policy, "forbidden final legacy path")

        transitional_session = directory_path / "transitional-session"
        shutil.copytree(base, transitional_session)
        (transitional_session / "src/session/legacy_snapshot.rs").write_text(
            "pub(crate) struct SessionSnapshot;\n", encoding="utf-8"
        )
        expect_failure(transitional_session, "forbidden final legacy path")

        grouped = directory_path / "grouped-cycle"
        shutil.copytree(base, grouped)
        (grouped / "src/agent/tool_driver.rs").write_text("use crate::{conversation::Entry, value::Value};\n", encoding="utf-8")
        (grouped / "src/conversation/entry.rs").write_text("use crate::agent::Runner;\n", encoding="utf-8")
        expect_failure(grouped, "module cycle")

        qualified = directory_path / "qualified-cycle"
        shutil.copytree(base, qualified)
        (qualified / "src/agent/tool_driver.rs").write_text("fn edge() { let _: Option<crate::conversation::Entry> = None; }\n", encoding="utf-8")
        (qualified / "src/conversation/entry.rs").write_text("fn edge() { let _: Option<crate::agent::Runner> = None; }\n", encoding="utf-8")
        expect_failure(qualified, "module cycle")

        qualified_masked = directory_path / "qualified-masked"
        shutil.copytree(base, qualified_masked)
        (qualified_masked / "src/agent/tool_driver.rs").write_text("// crate::conversation::Entry\nlet _ = \"crate::conversation::Entry\";\n", encoding="utf-8")
        assert not scan(qualified_masked), scan(qualified_masked)

        super_valid = directory_path / "super-valid"
        shutil.copytree(base, super_valid)
        (super_valid / "src/session/actor.rs").write_text("use super::conversation::Entry;\n", encoding="utf-8")
        nested_worker = super_valid / "src/session/nested/worker.rs"
        nested_worker.parent.mkdir(parents=True)
        nested_worker.write_text("use super::super::conversation::Entry;\n", encoding="utf-8")
        assert not scan(super_valid), scan(super_valid)

        super_cycle = directory_path / "super-cycle"
        shutil.copytree(base, super_cycle)
        root_escape = super_cycle / "src/session/nested/worker.rs"
        root_escape.parent.mkdir(parents=True)
        root_escape.write_text("use super::super::super::conversation::Entry;\n", encoding="utf-8")
        (super_cycle / "src/conversation/entry.rs").write_text("use crate::session::State;\n", encoding="utf-8")
        expect_failure(super_cycle, "module cycle")

        cfg = directory_path / "cfg-test"
        shutil.copytree(base, cfg)
        test_file = cfg / "src/session/tests.rs"
        test_file.write_text("pub struct Runtime;\nuse std::fs;\n", encoding="utf-8")
        (cfg / "src/session/mod.rs").write_text("pub(crate) const _SESSION: () = ();\n#[cfg(test)]\nmod tests;\n", encoding="utf-8")
        cfg_source = cfg / "src/session/actor.rs"
        cfg_source.write_text(
            "pub(crate) const _ACTOR: () = ();\n"
            "/// test docs\n#[derive(Debug)]\n#[cfg(test)] fn hidden()\n{ struct Runtime; }\n"
            "#[cfg(\n all(test, unix)\n)]\nfn hidden_two() { struct Workspace; }\n"
            "#[cfg(any(test, all(test, unix)))]\nfn hidden_three() { struct SessionStore; }\n"
            + "#[cfg(test)]\nfn long_test() {\n" + ("let x = 1;\n" * (MAX_FILE + 10)) + "}\n",
            encoding="utf-8",
        )
        view, count, _ = production_view(cfg_source.read_text(encoding="utf-8"))
        assert "Runtime" not in view and count == 1, (view, count)
        expect_failure(cfg, "forbidden final source symbol")
        cfg_source.write_text("pub(crate) const _ACTOR: () = ();\n#[cfg(all(feature=\"test\", unix))]\nfn production() { struct Runtime; }\n", encoding="utf-8")
        expect_failure(cfg, "forbidden production symbol Runtime")
        for predicate in ("any(test, windows)", "not(test)", 'feature="test"'):
            cfg_source.write_text(f"pub(crate) const _ACTOR: () = ();\n#[cfg({predicate})]\nfn production() {{ struct Runtime; }}\n", encoding="utf-8")
            expect_failure(cfg, "forbidden production symbol Runtime")
        cfg_source.write_text("pub(crate) const _ACTOR: () = ();\n", encoding="utf-8")
        crate_test_file = cfg / "src/agent/fixture.rs"
        crate_test_file.write_text("#![cfg(all(test, unix))]\npub struct Workspace;\n", encoding="utf-8")
        expect_failure(cfg, "forbidden final source symbol")
        nested_file = cfg / "src/agent/nested.rs"
        nested_file.write_text("mod inner { #![cfg(test)] pub struct Workspace; }\npub struct Runtime;\n", encoding="utf-8")
        expect_failure(cfg, "forbidden production symbol Runtime")

        allowed_port = directory_path / "allowed-port"
        shutil.copytree(base, allowed_port)
        (allowed_port / "src/tools/tool.rs").write_text("use crate::{ids::ToolCallId, conversation::ConversationEntry};\npub trait Tool {}\n", encoding="utf-8")
        assert not scan(allowed_port), scan(allowed_port)

        masking = directory_path / "masking"
        shutil.copytree(base, masking)
        (masking / "src/value.rs").write_text(
            "/* Runtime std::fs */\nlet ordinary = \"Runtime /* std::fs */ \\\"\";\n"
            "let raw_plain = r\"Runtime std::fs // quote\";\nlet raw = r###\"Runtime std::fs // quote \"###;\n"
            "let byte_raw_plain = br\"Runtime reqwest\";\nlet byte_raw = br##\"Runtime reqwest /* comment */\"##;\n"
            "let character = 'R';\nfn borrowed<'a>(value: &'a str) -> &'a str { value }\n",
            encoding="utf-8",
        )
        assert not scan(masking), scan(masking)
        (masking / "src/value.rs").write_text("use tokio :: { net :: TcpStream };\n", encoding="utf-8")
        expect_failure(masking, "forbidden production import")
