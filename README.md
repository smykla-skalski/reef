# Reef

Resource-aware scheduling for coding agents.

Coding agents can start builds, linters, tests, language servers, and containers at the same time. Each process behaves correctly in isolation, but their combined load can exhaust memory, trigger swap, saturate the CPU, and make the workstation unusable.

Reef will coordinate that work across agents and terminals. When pressure rises, it will reduce sail: preserve capacity for interactive work, queue expensive commands, explain the load, and send overflow work to remote runners.

## Status

Reef is under active development. The initial CLI reports current CPU, memory, and swap usage:

```console
reef status
```

## Planned capabilities

- Record system and process-tree resource usage over several working days
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
