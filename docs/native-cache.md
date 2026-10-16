# Native cache evidence

Reef keeps native compiler and test-cache evidence separate from its own output cache and repeated-command estimates. Instrumentation is optional and never changes compiler settings or resets counters.

## Capture installed tools

```sh
reef native-cache capture --source ccache
reef native-cache capture --source sccache
reef report --format html --output report.html --open
```

- ccache: fixed `ccache --print-stats` arguments; machine-readable tab-separated counters (`direct_cache_hit`, `preprocessed_cache_hit`, `cache_miss`, `stats_zeroed_timestamp`). Human-readable `--show-stats` output is unsupported.
- sccache: fixed `sccache --show-stats --stats-format json` arguments; `stats.cache_hits.counts` and `stats.cache_misses.counts` are summed across languages. Advanced counts are overlapping and excluded. Other metadata, including cache locations, is discarded.
- Commands run without a shell, with closed stdin and discarded stderr. Capture stops after five seconds or 1 MiB. Missing tools, unsuccessful commands, and unsupported formats fail without creating an observation. No tools are installed automatically.
- sccache reports server counters; an inactive server can return empty counters. A native zero is a tool observation, not proof that cache instrumentation covered a workload.

## Explicit imports

```sh
reef native-cache import --source ccache --input ccache-stats.txt
reef native-cache import --source sccache --input sccache-stats.json
reef native-cache import --source go-test --input go-test-output.txt
```

- Supply plain `go test` output yourself. Reef does not run tests or enable diagnostics. Only successful package summaries (`ok PACKAGE (cached)` or `ok PACKAGE 0.123s`) contribute counts. Package names, logs, and timing values are discarded. JSON test output is unsupported.
- `(cached)` is direct evidence of a reused successful package test result. Executed successful package results are not called cache misses: cacheability is unknown. Failed packages and packages without tests are excluded.
- Input is limited to a regular, non-symlink file of at most 1 MiB. Malformed statistics fail without retaining raw input. Go output without recognized summaries is rejected.
- Import time is the collection time, not the original execution time. Imports are user-supplied assertions; Reef cannot authenticate their origin.

## Storage and interpretation

- Default private state: `~/.local/state/reef/native-cache`. Use `--state-dir` for capture/import and matching `reef report --native-cache-state-dir` to override. Use one state directory per configured compiler cache; switching caches is not reliably detectable.
- Only source, capture/import method, collection time, selected counts, ccache reset time, and reset/source-change flag are stored. No raw output, paths, command arguments, environment, or source file contents are retained.
- State is capped at 128 snapshots and 128 KiB, plus one bounded 128 KiB staging file during atomic replacement. A stale staging file is reclaimed under the write lock. Snapshots older than seven days are removed on the next successful capture/import and excluded from reports even before that cleanup. Files are private and must be owned by the current user.
- Compiler statistics are cumulative snapshots, never added together or attributed to the selected report interval. Decreasing counters or a changed ccache reset timestamp flag a reset/source change. Resets followed by enough new activity are undetectable for sccache. No deltas or savings are calculated.
- JSON exposes `native_cache.snapshots`; Markdown and HTML include a separate Native cache evidence section. Report ranges select collection times. Missing, expired, or out-of-range evidence remains unavailable rather than zero.
- Go build-cache and Node cache hit/miss metrics are unsupported. Repeated commands and shorter execution times do not prove a native cache hit.

## Format references

- [ccache manual: machine-readable statistics](https://ccache.dev/manual/latest.html#_options_for_scripting_or_debugging)
- [ccache statistics fields and output](https://github.com/ccache/ccache/blob/master/src/ccache/core/statistics.cpp)
- [sccache server statistics and per-language counts](https://github.com/mozilla/sccache/blob/main/src/server.rs)
- [sccache statistics command](https://github.com/mozilla/sccache/blob/main/src/commands.rs)
- [Go test result caching and summary semantics](https://pkg.go.dev/cmd/go#hdr-Test_packages)
