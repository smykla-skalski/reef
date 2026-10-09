# Reef

Resource-aware scheduling for coding agents.

Coding agents can start builds, linters, tests, language servers, and containers at the same time. Each process behaves correctly in isolation, but their combined load can exhaust memory, trigger swap, saturate the CPU, and make the workstation unusable.

Reef will coordinate that work across agents and terminals. When pressure rises, it will reduce sail: preserve capacity for interactive work, queue expensive commands, explain the load, and send overflow work to remote runners.

## Status

Reef is under active development. The CLI reports current CPU, memory, and swap usage:

```console
reef status
```

Run a command with inherited input and output, and print a measurement to standard error:

```console
reef run --category test -- cargo test --locked
```

To append a JSON Lines record, provide a private record path and an optional safe label:

```console
reef run --category build --identity project-build --record measurements.jsonl -- cargo build
```

`reef run` returns the command's exit code, including when saving a measurement fails. Its measurement includes Unix start and end timestamps in milliseconds, wall time, CPU time, and peak resident memory. The `tree_*` fields combine the direct child's kernel usage with 20 ms process-tree samples. `tree_usage_complete` is always `false` because descendants that start and exit between samples can be missed. Records contain only the category, label, status, timestamps, and measurements. Reef never saves command arguments or environment variables. New record files are mode `0600`; Reef skips persistence to public files and invalid paths while still running the command.

## Shared scheduling (Unix)

Start one scheduler per workstation, ideally under your user service manager:

```console
reef serve --cpu 6 --memory-mib 12288 --max-running 4
```

The scheduler runs in the foreground. Without explicit limits its static budget reserves one logical CPU and 30% of physical memory for other work; `--max-running` defaults to the CPU budget. Its private Unix socket lives in `~/.local/state/reef/schedule/reef.sock`. Only one server can bind that path. Use `--state-dir` on every command to select another private directory.

Queue a measured command with its estimated peak resource cost:

```console
reef schedule --cpu 2 --memory-mib 4096 --category build --identity kuma-build -- cargo build
reef queue
reef cancel 3
```

`reef queue` prints request IDs, safe labels, estimates, and queued/running states as JSON. Admission is FIFO, with no backfill: a later small job cannot pass a larger one at the head of the queue. Zero or over-budget estimates fail immediately. The defaults per command are one CPU and 1024 MiB. A queued command can be cancelled by ID or with Ctrl-C. Cancellation, failure, client termination, and normal completion release its reservation. If the server disappears, queued commands fail and running clients stop their process group. A command never starts when the scheduler is unavailable. Scheduled children inherit `REEF_ADMITTED=1` so a nested Reef shim can run without requesting another reservation; this marker is a coordination hint, not a security boundary.

Scheduling reserves estimated capacity before a command starts. It does not enforce actual CPU or memory consumption on macOS; Linux containment is tracked separately. Estimate commands from `reef run` measurements and leave headroom for interactive apps. The daemon holds job state only in memory and never receives command arguments or environment variables.

The server also samples live CPU and memory usage every second. New jobs wait during high pressure (90% CPU or memory), until both metrics stay below recovery thresholds (70% CPU and 80% memory) for five seconds. A stale or unavailable sample also holds admission. For each new job, Reef preserves one idle logical CPU on machines with more than two cores and 10% of physical memory by default; it never stops a running job because pressure rose. The admission check counts active reservations alongside sampled usage, which can hold more work than necessary when a running job already contributes to that sample. Use `--cpu-reserve`, `--memory-reserve-mib`, `--cpu-high-percent`, `--cpu-recover-percent`, `--memory-high-percent`, `--memory-recover-percent`, and `--recovery-seconds` on `reef serve` to tune the policy. Reserve values must be below machine capacity and each recovery threshold must be below its high threshold. `--no-pressure` disables live admission checks when needed for isolated testing. At critical pressure (98% CPU or 97% memory), the server logs a notice with the safe labels of tracked workloads and points to `reef queue`. Notices repeat at most once per minute.

## Coding-agent commands (Unix)

Start the scheduler, then launch a local Codex or Claude Code CLI session through Reef:

```console
reef agents launch codex
reef agents launch claude -- --model sonnet
```

Reef prepends a private directory of command shims to `PATH` for that session. The agent's normal command approval runs before shell command resolution. A supported heavy tool invocation then enters the shared scheduler automatically; light subcommands use the original executable. No agent settings, hooks, shell profiles, or global `PATH` are changed. Closing the agent session removes the modified environment. Use `--state-dir` before `--` to select a scheduler state directory.

The initial shims cover `go build|install|run|test|vet`, `cargo build|check|install|test|bench|clippy` (also after `+toolchain`), `golangci-lint run`, and `mise run` or `make` targets named `build`, `test`, `lint`, or `check`. Builds, tests, and linters are tagged by category. The scheduler's safe identity combines the agent name, a session token, and a hash of the current worktree; Reef does not send or store the original command text or worktree path in scheduler state. Queue messages show the request ID, position, and reason for waiting. Defaults are one CPU and 1024 MiB per command.

Nested tools launched by an admitted command run under its existing reservation. If the scheduler is unavailable, a heavy command fails with a clear error; it does not run outside the budget. Absolute executable paths, scripts that replace `PATH`, commands inside remote or cloud agent sessions, and tools outside the shim list bypass automatic routing. For those, invoke `reef schedule --category ... -- command` explicitly. Agent configurations that filter `PATH` or Reef's session variables also prevent automatic routing.

Start a background recorder with no project configuration:

```console
reef observe start
```

It samples every 30 seconds, keeps up to seven days of observations, and limits storage to 100 MiB. High process counts can reach the storage limit sooner. Samples are JSON Lines files in `~/.local/state/reef/observe`. The directory is private to your user account on Unix. Each sample includes system CPU, memory, swap, root disk capacity, and numeric process usage with parent PIDs for tree analysis. Metrics that are unavailable are `null`; zero process I/O deltas are also `null` because a failed system query is indistinguishable from no I/O. Samples include `working_ms`, capped at the configured interval so time spent asleep does not accumulate as working time. Reef stores no process names, command lines, environment variables, file contents, credentials, or tokens.

Change collection limits when starting the recorder:

```console
reef observe start --interval-seconds 60 --retention-days 14 --max-storage-mib 200
reef observe stop
```

Use `reef observe run` to keep the recorder in the foreground. All three commands accept `--state-dir` for an alternate location; pass it to `stop` as well when using a custom directory. Stopping takes effect at the next interval. If the recorder exits unexpectedly and leaves `recorder.pid`, remove that stale file before restarting.

Generate a workload report from observations and one or more command record files:

```console
reef report --records measurements.jsonl
reef report --from 2026-10-08T00:00:00Z --to 2026-10-09T00:00:00Z --records measurements.jsonl --format json
```

The default range is the last 24 hours. Its start is inclusive and its end is exclusive. Markdown is the default format. Without `--records`, command measurements are shown as unavailable; Reef does not infer command categories from anonymous system samples. Command totals include each whole command whose execution overlaps the range. The wall and CPU percentages are shares of measured commands, not shares of machine capacity. Concurrent command wall times can add up to more than the elapsed range. The pressure timeline lists categories active during a pressured sample; overlap alone does not establish cause. Thresholds default to 90% CPU, 90% memory, and 1% swap and can be changed with `--cpu-threshold`, `--memory-threshold`, and `--swap-threshold`. Missing metrics remain unavailable rather than becoming zero. Agent, container, and interactive categories require later instrumentation; `reef run` currently offers build, lint, test, and other.

## Planned capabilities

- Attribute resource cost to agents and containers
- Reuse compatible results and persistent build caches
- Offload work when local capacity is insufficient

## Design principles

- Keep admission and scheduling deterministic
- Treat agent integrations as adapters around one shared scheduler
- Store categories and measurements without retaining secrets from command arguments
- Prefer delaying work over killing an expensive task after it starts
- Use platform controls for enforcement: admission control on macOS and cgroups on Linux
- Stay useful as a standalone CLI without requiring a specific coding agent

## Development

Install the toolchain and run all checks:

```console
mise install
mise run check
```

Build and run Reef:

```console
mise run build
cargo run -- status
```

## License

[MIT](LICENSE)
