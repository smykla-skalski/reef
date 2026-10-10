# Reef

Resource-aware scheduling for coding agents.

Coding agents can start builds, linters, tests, language servers, and containers at the same time. Each process behaves correctly in isolation, but their combined load can exhaust memory, trigger swap, saturate the CPU, and make the workstation unusable.

Reef will coordinate that work across agents and terminals. When pressure rises, it will reduce sail: preserve capacity for interactive work, queue expensive commands, explain the load, and send overflow work to remote runners.

## Status

Reef is under active development. The CLI reports current CPU, memory, and swap usage.

On macOS, install the published CLI from the project Homebrew tap:

```console
brew install smykla-skalski/tap/reef
reef --version
```

Inspect current resource usage with:

```console
reef status
```

Run a command with inherited input and output, and save a measurement by default:

```console
reef run --category test -- cargo test --locked
```

To use a custom JSON Lines record path or skip persistence for one command:

```console
reef run --category build --identity project-build --record measurements.jsonl -- cargo build
reef run --no-record -- cargo metadata
```

`reef run`, `reef schedule`, and supported coding-agent commands save private measurements in `~/.local/state/reef/history` by default. Reef keeps at most seven days and 100 MiB there, pruning only files stamped with that history directory's private Reef identity when a new measurement arrives. `--record` writes only to the chosen custom path; `--no-record` disables persistence for that invocation. These two options cannot be combined. Reef prints the measurement to standard error and returns the child's exit code even if storage fails, with a warning on standard error. A record contains a category, validated safe label, status, Unix start and end timestamps, wall time, CPU time, and peak resident memory; it never contains command arguments or environment variables. Commands started through `reef agents launch` also carry a validated Codex, Claude, or OpenCode kind so the report can group measured build, lint, and test load by agent. The `tree_*` fields combine the direct child's kernel usage with 20 ms process-tree samples. `tree_usage_complete` is always `false` because descendants that start and exit between samples can be missed. The history directory is mode `0700` and its files are mode `0600` on Unix. Reef skips persistence to public custom files and invalid paths while still running the command.

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

The scheduler saves private lifecycle events in its state directory. Events contain request IDs, random server-session IDs, timestamps, outcomes, and queue wait durations by one reason at a time; they contain no command arguments, environment values, paths, or workload labels. On each new event, Reef prunes event files older than seven days or beyond 100 MiB. A write failure warns without changing command output or status.

Scheduling reserves estimated capacity before a command starts. It does not enforce actual CPU or memory consumption on macOS. On Linux with cgroup v2 and a systemd user manager, add hard limits to a scheduled command:

```sh
reef schedule --cpu 2 --memory-mib 4096 --limit-cpu-percent 200 --limit-memory-mib 4096 --limit-tasks 128 --limit-io-read /dev/nvme0n1=10485760 -- cargo build
```

The CPU limit is a percentage of one CPU, so `200` allows two CPUs. Memory uses MiB. Task limits count workload threads; Reef reserves one extra cgroup task for its measurement helper. I/O limits specify a block device or file path and bytes per second; use `--limit-io-write` for write bandwidth. Btrfs directory paths are unsupported because one filesystem can span multiple devices; a block-device path caps only that device. I/O enforcement also requires the systemd user manager to delegate the cgroup `io` controller; Reef refuses to start the command if the kernel limit was not applied. Reef launches a transient systemd user scope for a command with any hard limit. Its descendants stay in that scope, and cancellation stops the scope. The measurement includes configured boundaries and cgroup event counters for CPU throttling, memory limit hits, and task limit hits. The kernel exposes no I/O limit-hit counter, so I/O boundaries are recorded without an event count. Reef removes its scope on completion and cancels stale Reef-owned scopes on the next limited run after a crash. If cgroups or the systemd user manager are unavailable, an explicit limit fails before the command starts. Commands without hard limits retain admission-only behavior on supported Unix platforms.

Estimate commands from `reef run` measurements and leave headroom for interactive apps. The daemon holds job state only in memory and never receives command arguments or environment variables.

The server also samples live CPU and memory usage every second. New jobs wait during high pressure (90% CPU or memory), until both metrics stay below recovery thresholds (70% CPU and 80% memory) for five seconds. A stale or unavailable sample also holds admission. For each new job, Reef preserves one idle logical CPU on machines with more than two cores and 10% of physical memory by default; it never stops a running job because pressure rose. The admission check counts active reservations alongside sampled usage, which can hold more work than necessary when a running job already contributes to that sample. Use `--cpu-reserve`, `--memory-reserve-mib`, `--cpu-high-percent`, `--cpu-recover-percent`, `--memory-high-percent`, `--memory-recover-percent`, and `--recovery-seconds` on `reef serve` to tune the policy. Reserve values must be below machine capacity and each recovery threshold must be below its high threshold. `--no-pressure` disables live admission checks when needed for isolated testing. At critical pressure (98% CPU or 97% memory), the server logs a notice with the safe labels of tracked workloads and points to `reef queue`. Notices repeat at most once per minute.

### Shadow admission replay

Use `reef shadow` to inspect how one hypothetical job would fare at each recorded host sample, without starting the scheduler or delaying any agent:

```console
reef shadow --cpu 1 --memory-mib 1024
reef shadow --cpu 2 --memory-mib 4096 --format json
reef shadow --from 2026-10-09T09:00:00Z --to 2026-10-09T17:00:00Z --cpu-high-percent 85
```

The replay uses the scheduler's default static budget and pressure threshold/reserve settings unless you override them. It counts sampled points where a hypothetical new request would pass, fail, or lack enough data, plus overlap with registered agent kinds. It does **not** reconstruct real command arrivals, running reservations, FIFO, queue time, recovery dwell between samples, or workload savings. Host observations are often one minute apart; counts are not durations. An agent overlapping a blocked sample did not necessarily cause that pressure. Compare policy settings on the same range before enabling `reef serve`, then validate with real scheduler outcomes once traffic is routed through it.

## Coding-agent commands (Unix)

Start the scheduler, then launch a local Codex or Claude Code CLI session through Reef:

```console
reef agents launch codex
reef agents launch claude -- --model sonnet
```

Reef prepends a private directory of command shims to `PATH` for that session. The agent's normal command approval runs before shell command resolution. A supported heavy tool invocation then enters the shared scheduler automatically; light subcommands use the original executable. No agent settings, hooks, shell profiles, or global `PATH` are changed. Closing the agent session removes the modified environment. Use `--state-dir` before `--` to select a scheduler state directory.

The initial shims cover `go build|install|run|test|vet`, `cargo build|check|install|test|bench|clippy` (also after `+toolchain` and global flags), `golangci-lint run`, `mise run` targets named `build`, `test`, `lint`, or `check`, and all `make` recipes except help/version requests. Unknown leading Cargo flags and unrecognized Make options are scheduled conservatively instead of bypassing the budget. Makefile content from stdin is passed through untouched. Builds, tests, and linters are tagged by category. The scheduler's safe identity combines the agent name, a session token, and a hash of the current worktree; Reef does not send or store the original command text or worktree path in scheduler state. Queue messages show the request ID, position, and reason for waiting. Defaults are one CPU and 1024 MiB per command.

Nested shimmed tools launched by an admitted command run under its existing reservation and are included in the outer command's measurement, not saved as separate commands. An explicit nested `reef schedule` still records its own measurement. If the scheduler is unavailable, a heavy command fails with a clear error; it does not run outside the budget. Absolute executable paths, scripts that replace `PATH`, commands inside remote or cloud agent sessions, and tools outside the shim list bypass automatic routing. For those, invoke `reef schedule --category ... -- command` explicitly. Agent configurations that filter `PATH` or Reef's session variables also prevent automatic routing.

## Passive agent observation

`reef agents observe codex|claude|opencode` registers the calling agent process with the running observer. It does not start the scheduler, wrap commands, change `PATH`, or delay the agent's work. For a host application that starts an agent directly, pass its process ID with `--pid`; Reef checks the process start time so PID reuse cannot claim old data. The observer attributes sampled CPU, resident memory, and process I/O to the nearest registered agent ancestor. It also groups agent descendant processes into sanitized command families such as `go test`, `cargo build`, `golangci-lint`, `git`, and `other`. CPU, memory, and I/O by family are sampled estimates, not exact executions or proof that an agent caused host pressure. Sub-second or detached processes can be missed. OpenCode can host multiple sessions in one process; those sessions share one process-level total.

Configure each agent once for ordinary new sessions:

- Run `reef agents setup` to detect Codex, Claude Code, and OpenCode from their CLIs or existing user configuration and configure each one. Use `reef agents setup --check` to verify configuration without changing it, or name one agent (for example, `reef agents setup opencode`). Rerun setup after a Reef upgrade. The command preserves unrelated hooks and plugins; it adopts the exact older Reef OpenCode plugin but refuses to replace other unmanaged files or symlinks. `--home` redirects all agent configuration to another user home; `--config-dir` overrides only OpenCode's directory.
- Reef installs an asynchronous [Codex SessionStart hook](integrations/codex/hooks.json) and [Claude Code SessionStart hook](integrations/claude/settings.fragment.json). Review and trust the Codex hook with `/hooks`; Codex skips untrusted hooks. On macOS, setup also configures Orca's separate Codex runtime home if that directory exists.
- Reef bundles the [OpenCode plugin](integrations/opencode/reef-observe.js) in its binary and installs it in OpenCode's global plugin directory. It registers the OpenCode process when it loads and does not wait for Reef. Set `REEF_BIN` if `reef` is not on the agent's `PATH` and is not at a standard macOS Homebrew path. Set `REEF_OBSERVE_STATE_DIR` only when the observer uses a custom state directory.

If a host such as Sail bypasses native session hooks, it can call `reef agents observe <agent> --pid <agent-process-id>` after launching its agent. Terminal wrappers use the normal agent hooks when they inherit the configured user home. Orca has a separate Codex runtime home, so install the Codex hook in its runtime `hooks.json` as well and verify it on the next session. Run `reef report --format json` and inspect `agents` and `passive_commands` to verify collection. Registrations contain only agent kind, PID, and process start time in the private observation directory; command samples contain fixed family/category labels and numeric process usage, never process names, arguments, environment variables, or prompts. Stale registrations are removed when the observer samples. Observation remains available when the scheduler is off.

## Output cache (Unix)

Explicitly opt in for a command that only reads inputs and writes stdout or stderr:

```console
reef cache run -- cargo metadata --no-deps --format-version 1
reef cache status
reef cache prune --max-storage-mib 256
```

The command must run in a Git worktree. Reef hashes its executable, arguments, working directory, operating system, architecture, environment, recognized toolchain versions, and the contents and modes of Git tracked and untracked nonignored files. Use `--input path` for any file the command reads outside that set, including ignored build inputs and files outside the worktree. A changed input invalidates the result. `--ttl-seconds` defaults to 3600; `--max-storage-mib` defaults to 256. Requests for the same key wait on one execution and replay its successful stdout and stderr. Failed, interrupted, oversized, or input-changing commands are not stored. Each stream is captured up to 16 MiB; larger output still streams but is not cached.

Cached commands receive closed standard input. They must be noninteractive and deterministic for their declared inputs. Reef does not restore files created by a command, so do not cache a build or any test with required file side effects. Commands that depend on network state, time, ignored files, or undeclared files are not safe to cache. Cache hits replay stdout and stderr without running the command; they do not create a new resource measurement or scheduler reservation. This initial cache path is standalone; placing `reef schedule` around it reserves capacity for every caller before a cache hit is known.

The cache lives in a private user directory at `~/.local/state/reef/cache`. `--cache-dir` selects another private directory outside the worktree. Cache metadata stores only opaque hashes, timestamps, and sizes; it does not store command arguments, environment variables, or input file contents. Output is stored verbatim and can contain secrets, so enable caching only for commands whose output is safe to retain locally. Reef evicts its own expired and least recently used entries without touching Cargo, Go, or other external tool caches. `reef cache status` reports its entry count and byte size.

Start a background recorder with no project configuration:

```console
reef observe start
```

It samples the host every 30 seconds by default, keeps up to seven days of observations, and limits storage to 100 MiB. Registered agents also get compact one-second process-tree and command-family samples, so brief sessions can appear before the first host sample. At each hour boundary, Reef folds older agent samples into per-minute CPU, memory, I/O, active-time, and command-family aggregates and removes their raw one-second history. Queries crossing a folded minute include that whole minute. The storage limit and retention policy cover host samples, recent agent samples, and rollups together; high process counts can reach the limit sooner. Samples are JSON Lines files in `~/.local/state/reef/observe`. The directory is private to your user account on Unix. Each host sample includes system CPU, memory, swap, root disk capacity, numeric process usage with parent PIDs for tree analysis, and load attributed to registered agent processes. Metrics that are unavailable are `null`; zero process I/O deltas are also `null` because a failed system query is indistinguishable from no I/O. Samples include `working_ms`, capped at the configured interval so time spent asleep does not accumulate as working time. Reef stores no process names, command lines, environment variables, file contents, credentials, or tokens.

Change collection limits when starting the recorder:

```console
reef observe start --interval-seconds 60 --retention-days 14 --max-storage-mib 200
reef observe stop
```

Use `reef observe run` to keep the recorder in the foreground. All three commands accept `--state-dir` for an alternate location; pass it to `stop` as well when using a custom directory. Stopping takes effect within one second. A crashed recorder releases its lock, so the next run reuses its state files without manual cleanup. Do not delete `recorder.pid` or `recorder.lock` while a recorder is running.

On Unix, a recorder launched through a stable command path such as Homebrew's `reef` link reloads itself after that link points to a new executable. It finishes the current host sample first, then replaces its process at the next host interval. The replacement keeps the same PID and process start time, so use the executable mapped by the process to verify its version. This does not download upgrades. A recorder launched from a version-specific binary path stays on that version.

Generate a workload report from observations and default command history; add custom record files with `--records`:

```console
reef report
reef report --records measurements.jsonl
reef report --timeline
reef report --from 2026-10-08T00:00:00Z --to 2026-10-09T00:00:00Z --records measurements.jsonl --format json
reef report --schedule-state-dir /private/reef-schedule --format json
reef report --cache-dir /private/path/to/cache --format json
reef report --format html --output ./reef-report.html --open
```

The default range is the last 24 hours. Its start is inclusive and its end is exclusive. Markdown is the default format. The default report summarizes pressure duration and measured command or sampled agent overlap without printing individual timestamps. Use `--timeline` to include per-sample events in Markdown or JSON when investigating a specific period. `--records` adds custom files to default history without double-counting a repeated path. If no history exists, command measurements are unavailable; if history exists but no command overlaps the range, the measured count is zero. Measured command overlap uses actual intersections with pressure intervals and counts concurrent commands once in the overall total; per-category totals can overlap. Agent overlap is a sampled process-tree estimate. Neither kind of overlap establishes which activity caused host pressure. Reef reports pressure without an overlapping measured command separately instead of assigning it to an agent. Command totals include each whole command whose execution overlaps the range. The wall and CPU percentages are shares of measured commands, not shares of machine capacity. Concurrent command wall times can add up to more than the elapsed range. `--state-dir` selects the observation directory, not the default command history directory. Thresholds default to 90% CPU, 90% memory, and 1% swap and can be changed with `--cpu-threshold`, `--memory-threshold`, and `--swap-threshold`. Missing metrics remain unavailable rather than becoming zero. The passive command-family table gives sampled CPU core-ms, peak RSS, process I/O, and active time for each registered agent without wrapping its commands; it is not an exact command count or a causal allocation of host pressure. The separate measured-command table requires `reef run`, `reef schedule`, or the opt-in `reef agents launch` shims. Container and interactive categories require later instrumentation.

Markdown shows memory and I/O in binary units (KiB, MiB, GiB) and counts host observations that contain active agents. JSON keeps exact byte values and separate counts for one-second agent ticks and compacted rollups; those counts can be zero while host observations still contain agent load.

The activity-adjusted agent comparison divides sampled CPU and I/O by each agent kind's observed active time. Its activity mix divides classified build, test, lint, and other descendant-command CPU by all classified descendant-command CPU for that agent. It is not a share of host CPU or a causal cost allocation. Peak RSS remains an absolute peak. The comparison is available in Markdown, JSON, and HTML.

`--format html` prints a self-contained report to stdout. Add `--output path.html` to save it as a new file, then `--open` to launch that file in the default browser. Reef refuses to overwrite an existing file. On Unix, saved files have mode `0600`. A headless Linux host can generate the file, but `--open` requires a configured browser. The page works offline and includes sampled CPU, memory, swap, and root-disk usage time series, privacy-safe memory-consumer family charts, a command-family cost breakdown, and report summary cards. Family charts share a scale and show the five largest families by mean RSS; the table lists all families. RSS can count shared pages more than once and does not add up to host used memory. Charts preserve sampled peaks while bounding the number of plotted points; gaps are not connected. They show observations, not causal attribution or continuous monitoring. Add `--timeline` only when you need a table of individual pressure intervals. HTML output does not load scripts, fonts, or chart assets from the network. Treat the saved page as private workstation data and remove it when no longer needed.

Compare a baseline with a later, non-overlapping period:

```console
reef compare --baseline-from 2026-10-07T09:00:00Z --baseline-to 2026-10-07T17:00:00Z --comparison-from 2026-10-08T09:00:00Z --comparison-to 2026-10-08T17:00:00Z
reef compare --baseline-from 2026-10-07T09:00:00Z --baseline-to 2026-10-07T17:00:00Z --comparison-from 2026-10-08T09:00:00Z --comparison-to 2026-10-08T17:00:00Z --format json
```

`reef compare` reads the same private observations and command history as `reef report`, including optional `--state-dir` and repeated `--records` files. It reports pressure milliseconds per observed working hour so gaps and sleeping time do not dilute a period. Commands count in the period where they finish, including failed and cancelled commands; their whole wall and CPU durations appear by category. Swap growth is the difference between the first and last available swap samples within a period. The no-overlap share measures pressured time without a concurrent recorded command, not unobserved or uninstrumented work. Missing observations or history remain unavailable, and zero-pressure periods have no defined no-overlap percentage. The comparison is observational; it does not show that Reef caused a change.

The scheduler section counts requests submitted in the selected range, their observed outcomes, rejected requests, queue-wait p50/p95, and submission-to-finish p50/p95. Queue time is partitioned into pressure, capacity, FIFO, running-limit, and admission/sampling-unavailable time; no interval is counted twice. It is unavailable until a scheduler creates event history; a running scheduler with no requests reports zero counts. `--schedule-state-dir` selects a nondefault scheduler directory. These are observed interventions, not proof that Reef prevented machine pressure.

On Unix, `reef report` also shows cache hits, misses, shared executions, and failed or uncacheable attempts. Pass `--cache-dir` when the cache uses a custom directory. Reused wall and child-process CPU time are estimates from successful executions of the same cache key, not measured savings. The report gives the number of hits with and without a valid cost sample; with no valid sample, the estimate is unavailable. Cache event files contain only opaque cache keys, outcomes, timestamps, and cost measurements. They are private, capped at 2,048 events or 10 MiB, and events older than seven days are pruned on the next cache operation. Cache hits still create no command measurement or scheduler reservation.

## Planned capabilities

- Attribute resource cost to agents and containers
- Restore build artifacts before allowing build cache hits
- Offload work when local capacity is insufficient

An opt-in trusted SSH worker path is implemented behind a disabled-by-default policy. Its setup, recovery behavior, and unmeasured real-host release gate are documented in [experimental remote offload](docs/remote.md).

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

## Release

Run the **Prepare release** GitHub Actions workflow on `main` and choose `patch`, `minor`, or `major`. It verifies the current published version, creates a version-bump branch with a GitHub-signed commit, and prints an **Open the version-bump PR** link in the run summary. Open that PR, review its branch checks, and merge it through the normal review process. The changed `.release-version` marker then starts the four-platform release workflow, which creates the unsigned `vX.Y.Z` tag and publishes archives and checksums only after its builds and tests pass. After publication, Reef asks the Homebrew tap to verify the release and open a formula PR. The tap update remains subject to review. Ordinary merges do not bump or publish versions.

## License

[MIT](LICENSE)
