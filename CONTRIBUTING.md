# Contributing to Reef

## Prerequisites

- [mise](https://mise.jdx.dev/) for toolchain management
- Rust stable with Clippy and rustfmt

## Setup

```console
mise install
mise run check
```

## Development workflow

- Create a focused branch from `main`
- Add tests for observable behavior
- Run `mise run check` before opening a pull request
- Keep commits signed and use `type(scope): description`
- Keep commit titles at 50 characters or fewer

## Pull requests

- Explain the problem in `Motivation`
- Describe behavior and technical decisions in `Implementation information`
- Keep each pull request focused on one deliverable
- Resolve every review comment and failing check before merge

By contributing, you agree that your contributions are licensed under the MIT License.
