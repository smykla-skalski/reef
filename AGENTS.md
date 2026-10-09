# Agent instructions

## Product boundaries

- Keep scheduling and enforcement deterministic. Use models only for optional classification or recommendations.
- Preserve interactive workstation capacity before maximizing background throughput.
- Never persist complete command lines, environment variables, file contents, credentials, or tokens by default.
- Keep agent-specific behavior behind adapters. The core must work from the CLI without an agent integration.

## Development

- Use `mise` for the Rust toolchain and project tasks.
- Run `mise run check` before committing.
- Put platform-specific process controls behind platform modules.
- Add behavior-focused tests for every scheduling policy and state transition.
- Do not add linter exceptions. Fix the underlying warning.

## Scratch and build output

- Create one scratch root per task with `mktemp -d "${TMPDIR:-/tmp}/reef-<task>.XXXXXX"`.
- Set `CARGO_TARGET_DIR` to a directory inside that scratch root.
- Reuse that target directory for sequential checks of the same revision.
- Remove only the scratch root created by the current task.
- Check free space before build-heavy work and stop when less than 20 GB remains.
