# Selective enforcement trial

This is an operator checklist, not authorization to enable treatment. Reef currently measures opt-in control attempts; it does not expose a treatment canary. Keep public strong isolation fail-closed until [#68](https://github.com/smykla-skalski/reef/issues/68) and [#69](https://github.com/smykla-skalski/reef/issues/69) meet their acceptance gates on macOS and Linux.

## Before exposure

- Verify each selected command is eligible for a read-only source snapshot. Do not enroll source-writing commands, commands requiring host build artifacts, or commands containing host credentials.
- Prove launcher-death cleanup on the exact release candidate before and after command-start acknowledgment, including detached children and sibling services. Verify the boundary has stopped before the scheduler lease is released.
- Confirm the control and treatment telemetry stores are private, writable, readable, and complete. A missing, malformed, contradictory, or retention-pruned attempt blocks promotion; it is not a zero-cost or successful attempt.
- Record the configured treatment percentage and actual assigned and measured counts for every agent and build/test/lint category. Host CPU, memory, swap, and disk trends are machine-level context, not causal arm effects.
- Set a bounded pre-admission wait and an admission budget from repeated native build, test, and lint runs, including cold and warm startup, peak host memory, incremental disk, cleanup residue, terminal I/O, and command outcomes.

## Stages

1. Run 0% control smoke checks for Codex, Claude Code, and OpenCode. Verify every eligible top-level attempt gets one opaque ID, command outcome, and cost record without storing its arguments or environment.
2. Run 100% treatment smoke checks on disposable read-only workloads. Exercise missing scheduler, timeout, cancellation, off switch, signal handling, and launcher death. Do not proceed if a command can start twice or outlive its boundary.
3. Run 5%, then 25%, then 50% treatment. Hold each stage for at least 24 hours and 50 mature treatment attempts, including at least 10 each of build, test, and lint across the three agents. Count actual treatment assignments, not the configured percentage.

## Promotion and rollback

- Promote only with complete mature outcomes, at least 99% measured cost coverage, no scheduler-induced command failure or telemetry-integrity error, treatment p95 queue wait below 10 seconds, and a healthy one-minute command-launch heartbeat.
- Keep pending attempts distinct from failed attempts until they mature. If a stage has insufficient data, extend it; do not infer success from a quiet period.
- If any gate fails, switch off new treatment immediately. Identify queued treatment attempts for explicit cancellation. Leave already-running work under its existing owner and do not retry an uncertain or started command.
- Do not compare simultaneous machine-wide swap, CPU, or disk pressure as if mixed control and treatment arms were independent hosts. Use arm-specific command outcomes and measured costs for the canary decision.
