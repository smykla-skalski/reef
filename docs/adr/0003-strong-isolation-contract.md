# ADR 0003: Stage strong isolation behind a fail-closed command contract

- Status: Implementation gate; no strong backend is enabled
- Date: 2026-10-11
- Issues: [#68](https://github.com/smykla-skalski/reef/issues/68), [#66](https://github.com/smykla-skalski/reef/issues/66)

## Decision

`reef run` and `reef schedule` accept `--isolation host|strong`. `host` preserves their existing behavior. Until a platform backend passes the gates below, `strong` rejects the request before spawning or scheduling a command. No failure path may silently retry on the host. The command and its environment are not stored in the rejection.

Strong mode owns a single command's process boundary, not a whole agent session. Observation and shadow replay remain independent of it. The first canary may select read-only source workloads whose outputs are disposable or explicitly exported. Source-writing commands remain in host mode until an isolated workspace write-back protocol is proven.

## Platform boundaries

- macOS: one native VM per command, as selected by [ADR 0002](0002-macos-native-vm-containment.md). The host owner starts before guest work, receives an acknowledged VM identity, watches a close-on-exec launcher-liveness channel, stops the VM on launcher death, and confirms driver exit before releasing the scheduler lease. The VM has no live host mount or reusable host credentials.
- Linux: evaluate a single-use rootless container with private PID, user, mount, network, and cgroup namespaces, no host user-service bus or host sockets, and a dedicated cgroup v2 subtree. The host owner kills the entire subtree and verifies it has no live processes. A shared guest with a per-command cgroup remains rejected: a command can ask the reachable user manager to start a sibling unit. Native Linux tests must prove the sealed container denies that escape before enabling the backend. A per-command VM is the fallback if the host lacks the required isolation or the proof fails.
- Windows: retain passive behavior; `strong` remains unsupported.

## Workspace behavior

The initial canary copies a bounded source snapshot into the isolated environment and does not expose a writable host directory. It does not copy changes back by default. Commands that edit source or require build artifacts on the host are ineligible. A later write-capable mode needs a bounded change manifest covering tracked, untracked, ignored, binary, symlink, mode, rename, and delete operations; conflict checks against the baseline; and application only after verified teardown. An ambiguous result preserves a recovery bundle instead of overwriting concurrent host changes.

## Enablement gates

1. Preflight the backend, guest/image, workspace size, privileges, resource headroom, and owner recovery path before scheduler admission. An unsupported preflight has no command side effect.
2. Kill the Reef launcher before and after command-start acknowledgment. Verify automatic teardown of detached children and sibling services, including repeated stop failures and owner restart. Do not release a lease while the boundary can still execute.
3. Preserve normal exit, cancellation, signal forwarding, terminal input/output, measurement, and no-automatic-retry behavior on the real CLI.
4. On native Linux, attack the container through host D-Bus, host sockets, host networking, nested cgroups, and runtime controls. On macOS, repeat native VM driver-exit checks with real build, test, and lint commands.
5. Measure cold/warm start latency, peak host memory, incremental disk, residue, and command outcomes across repeated runs. Set an admission budget before canary exposure.
6. Start with a bounded read-only canary and compare measured control/treatment outcomes. Roll back new treatment on containment, latency, or data-coverage failures.

The guarantee covers work inside the selected guest or container. Work delegated to an allowed external service is outside it. No backend can claim full descendant containment while it can reach an uncontrolled host service that executes on its behalf.
