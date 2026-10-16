# External-work admission protocol v1

Applications can reserve scheduler capacity for work they own, including browser
tabs and agent turns, without launching a command through Reef.

Run `reef admission-protocol` (optionally `--state-dir PATH`) before connecting.
It probes the running scheduler and prints one JSON object with `version: 1` and
an absolute `socket` path. A missing, unavailable or incompatible scheduler exits
unsuccessfully. Fail closed when discovery fails; never infer support from the
Reef executable version or fall back to unscheduled work.

Use the returned socket path unchanged. Discovery preserves absolute aliases so
clients can use short socket addresses on systems with Unix socket path limits.

Connect to that private Unix socket and exchange newline-delimited UTF-8 JSON.
Each connection owns one lease. Submit:

```json
{"op":"acquire","cpu":1,"memory_mib":1024,"identity":"sail-agent"}
```

CPU and memory estimates must be positive and fit the scheduler's total budget.
Identity is 1–64 lowercase ASCII letters, digits, dots, underscores or hyphens.
An optional positive `max_wait_ms` bounds queue time. Messages are limited to
4096 bytes. Treat unknown or malformed replies as an unavailable scheduler.

The scheduler sends `queued` with `id`, `position` and `reason`, followed by zero
or more `waiting` updates with the same fields. Start work only after `granted`
with the matching `id`. An `error` carries a `message`; `cancelled` means the
request was cancelled. Display queue reasons supplied by Reef.

Close the connection to cancel queued work. After admission, retain the
connection until work finishes, then write `done success\n`, `done failed\n` or
`done cancelled\n` and close it. Closing an admitted connection also releases
capacity, recording an interrupted lease. External clients must never send a
`started` process identifier: Reef does not own their process lifecycle.

Pressure changes affect queued requests only. An external cancellation or
scheduler disconnect does not terminate application-owned running work. The
application may finish that work; new work must rediscover a live scheduler and
acquire a fresh lease. Never retry an admitted request as new work.

Protocol v1 preserves these messages and semantics. Future incompatible
protocols require separate discovery. The socket is private to the current user;
clients must protect the discovered path and never expose it to other users.
