# Reef

Resource-aware scheduling for coding agents.

Coding agents can start builds, linters, tests, language servers, and containers at the same time. Each process behaves correctly in isolation, but their combined load can exhaust memory, trigger swap, saturate the CPU, and make the workstation unusable.

Reef will coordinate that work across agents and terminals. When pressure rises, it will reduce sail: preserve capacity for interactive work, queue expensive commands, explain the load, and send overflow work to remote runners.

## Status

Reef is under active development. The CLI reports current CPU, memory, and swap usage:

```console
reef status
```

Start a background recorder with no project configuration:

```console
reef observe start
```

It samples every 30 seconds, keeps seven days of observations, and limits storage to 100 MiB. Samples are JSON Lines files in `~/.local/state/reef/observe`. The directory is private to your user account on Unix. Each sample includes system CPU, memory, swap, root disk capacity, and numeric process usage with parent PIDs for tree analysis. Metrics that are unavailable are `null`. Samples include `working_ms`, capped at the configured interval so time spent asleep does not accumulate as working time. Reef stores no process names, command lines, environment variables, file contents, credentials, or tokens.

Change collection limits when starting the recorder:

```console
reef observe start --interval-seconds 60 --retention-days 14 --max-storage-mib 200
reef observe stop
```

Use `reef observe run` to keep the recorder in the foreground. All three commands accept `--state-dir` for an alternate location; pass it to `stop` as well when using a custom directory. Stopping takes effect at the next interval. If the recorder exits unexpectedly and leaves `recorder.pid`, remove that stale file before restarting.

## Planned capabilities

- Attribute resource cost to builds, linters, tests, agents, and containers
- Apply one global concurrency and resource budget across independent agents
- Reserve CPU and memory for the developer's interactive applications
- Reuse compatible results and persistent build caches
- Offload work when local capacity is insufficient
- Produce reports for workstation sizing and workflow tuning

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
