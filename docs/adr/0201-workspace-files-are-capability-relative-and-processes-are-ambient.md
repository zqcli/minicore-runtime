# ADR 0201: Workspace Files Are Capability-Relative and Processes Are Ambient

状态：Accepted

日期：2026-08-20

## Context

Filesystem access and child-process access have different enforceable authority. Treating them as one generic tool permission would either weaken filesystem containment or overstate process isolation.

## Decision

`Workspace::open` captures one absolute configured root as a capability. `read_file`, `list_directory`, and `write_file` accept checked relative paths and operate through that captured root. They reject lexical escape, symlink escape, invalid final targets, non-UTF-8 data, and bound violations. Access mode is `ReadOnly` or `ReadWrite`, and write operations never create directories.

`run_command` accepts a structured executable, argument list, optional relative cwd, timeout, and environment map. The cwd is checked before spawn, then the direct child receives an ambient host path. `ProcessPolicy` controls enablement, program allowlist, inherited environment keys, timeout, and output bounds. The child environment is cleared before permitted values are added.

## Consequences

Filesystem tools can make a capability-relative containment claim. The process tool cannot claim an OS sandbox or process-tree sandbox; its contract is direct-child execution with bounded I/O and explicit host policy. Hosts must opt into process execution and must not interpret the cwd check as a child-process security boundary.

See [architecture](../architecture.md), [workspace ownership](../modules/README.md#workspace), and [`src/workspace/root.rs`](../../src/workspace/root.rs).
