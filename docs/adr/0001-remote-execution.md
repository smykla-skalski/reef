# ADR 0001: Start remote execution with trusted persistent SSH workers

- Status: Accepted for design; remote execution remains unimplemented
- Date: 2026-10-09
- Issue: [#4](https://github.com/smykla-skalski/reef/issues/4)
- Follow-up implementation: [#11](https://github.com/smykla-skalski/reef/issues/11)

## Context and constraints

Reef protects the interactive workstation by admitting background work only within a deterministic resource budget. Remote work is useful only when its startup, transfer, and execution overhead is lower than waiting for local capacity, and when a command can run away from the workstation without changing its meaning. The CLI must remain usable without an agent integration or a remote worker.

The source of a job can be a dirty worktree. A Git revision alone omits modified tracked files and untracked files. Remote execution therefore needs a bounded, verifiable source snapshot, a compatible toolchain and platform, explicit handling of secrets, and cancellation that does not leave a build running after the user has stopped it. Arbitrary commands can have side effects; Reef cannot assume they are safe to retry or to run concurrently in two places.

This decision covers build, lint, and test jobs submitted from a local CLI. It does not add a public remote execution API or authorize remote infrastructure. The measurements below use an isolated local worker simulation. Actual SSH, provider startup, network, and cloud behavior remain validation gates for #11.

## Decision

Use an **explicitly configured, trusted persistent SSH worker** as the first remote execution model. Reef will create an isolated directory per job on that worker, transfer a manifest-defined source snapshot over SSH, execute a permitted command with a compatible toolchain, stream bounded status and output to the requesting CLI, and remove the job directory on completion. Keep dependency and compiler caches outside job directories, scoped to the worker and trust domain. Require the user to choose a remote target and approve the source snapshot policy before any transfer. Do not discover workers or upload source automatically.

The first implementation should support only a narrow allowlist of build, lint, and test commands with an explicit working directory and declared platform requirements. Reef's scheduler makes the admission decision from local pressure, worker capacity, predicted transfer and runtime cost, and the configured policy. A model may recommend an action but cannot decide admission or enforcement. If the predicted remote benefit is below the configured threshold, queue locally. Keep a reserved local CPU and memory budget for the interactive workstation during snapshotting and transfer.

This chooses the transport and trust model, not a public interface. #11 must pass the validation matrix below before enabling general use.

## Options considered

| Model | Dirty-worktree path | Cache and startup | Security and operating cost | Decision |
| --- | --- | --- | --- | --- |
| Trusted persistent SSH worker | Transfer an explicit snapshot from the local worktree; verify its manifest before execution | Incremental transfer and persistent tool/dependency caches; connection and job setup still cost time | Operator controls host and retention; persistent host increases cross-job exposure and incurs idle capacity cost | **Chosen for a narrow first version** because it supports local-first editing and repeated short jobs without a provider lifecycle per command |
| Managed development environment, such as GitHub Codespaces | Usually starts from a repository and branch; a dirty local tree still needs a separate upload path | Prebuilds reduce setup, but creation and resume are provider operations; stopped environments retain storage | Provider stores source and environment data; compute and storage are billed separately, and secret scope needs explicit configuration | Reject as the first execution backend: it is an interactive workspace lifecycle, not a small command-execution primitive |
| Disposable hosted or self-hosted runner | Requires a snapshot artifact or pushed commit; a Git revision loses dirty changes | Fresh environment and queue/provisioning on every job; external cache restore/write adds traffic | Stronger per-job isolation, but artifacts, logs, and caches persist outside Reef; per-minute and cache costs apply | Reject for interactive overflow jobs; retain as a later option for long, reproducible jobs |

VS Code documents `rsync` as a way to synchronize local and SSH-host files, while its Remote SSH extension does not itself synchronize local source. GitHub documents Codespaces prebuild and lifecycle costs, and recommends ephemeral self-hosted Actions runners for autoscaling because persistent runners retain cross-job risk. These are properties of the candidate services, not measured Reef performance. [VS Code Remote SSH](https://code.visualstudio.com/docs/remote/ssh), [Codespaces prebuilds](https://docs.github.com/en/codespaces/prebuilding-your-codespaces/about-github-codespaces-prebuilds), [Codespaces lifecycle](https://docs.github.com/en/codespaces/about-codespaces/understanding-the-codespace-lifecycle), [self-hosted runners](https://docs.github.com/en/actions/reference/runners/self-hosted-runners).

Docker Build Cloud is scoped to container image builds through BuildKit. Reef needs to run ordinary Rust build, lint, and test commands, so it is not a general execution backend for this decision. [Docker Build Cloud](https://docs.docker.com/build-cloud/).

## Prototype and measurements

On 2026-10-09, a macOS arm64 local simulation copied the 18 tracked repository files into a fresh source directory, appended a harmless comment to `src/main.rs`, and added an untracked text file. `rsync -a --delete` transferred that dirty snapshot to a separate local worker directory, excluding `.git` and `target`. SHA-256 comparisons confirmed that the modified and untracked files arrived intact. The worker used one scratch-local `CARGO_TARGET_DIR`; `mise run build`, `mise run lint`, and `mise run test` passed in cold, unchanged, and edited runs. The test command ran three tests each time. Timings are wall-clock observations from one run, not performance guarantees.

| Local simulation phase | Sync time | Regular files transferred | File bytes transferred | Bytes sent by rsync | Build | Lint | Test |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Initial dirty snapshot, cold build cache | 0.770 s | 19 | 22,087 | 23,607 | 24.236 s | 8.351 s | 3.003 s |
| Unchanged snapshot, warm cache | 0.094 s | 0 | 0 | 680 | 0.981 s | 2.275 s | 1.244 s |
| Second source and untracked edit, warm cache | 0.115 s | 2 | 482 | 1,252 | 1.839 s | 3.319 s | 3.555 s |

The setup copy took 0.378 s. The source snapshot was 22,087 bytes before the second edit. The local simulation used a shared filesystem and a preinstalled Rust toolchain; it did **not** measure SSH connection or authentication, network latency, bandwidth, remote provisioning, toolchain installation, remote cache download, provider startup, transfer charges, or cloud spend. Its observed provider spend was zero because no provider was used. The data establishes that dirty files can be transferred and that the Rust tasks can reuse a persistent worker cache; it does not establish that offloading improves end-to-end time on an actual worker. `rsync` reports protocol bytes for this local run, which are a transfer-volume proxy rather than a network bill. [rsync manual](https://rsync.samba.org/ftp/rsync/rsync.1).

For an actual worker, record cost per useful job as `(provisioned worker hours × worker rate) + (storage GB-hours × storage rate) + transfer and egress charges`, allocated over completed jobs, including idle time. GitHub currently lists Codespaces 2-core compute at $0.18 per active hour and storage at $0.07 per GB-month; these illustrate the separate cost dimensions and are not a quote for an SSH host. [GitHub Codespaces billing](https://docs.github.com/en/billing/concepts/product-billing/github-codespaces).

## Trust, security, and privacy model

1. **Trust boundary.** The local user explicitly trusts a named SSH host and its operator with the selected source snapshot and any output generated there. SSH authenticates the host using a pinned host key and the user using a dedicated, least-privilege credential. Do not silently accept a changed host key, use agent forwarding, or copy local credential stores.
2. **Snapshot policy.** Build a manifest from a stable local snapshot before admission. Include modified tracked files and only untracked paths explicitly selected for that job; exclude `.git`, build outputs, device files, sockets, symlinks escaping the root, and configured sensitive paths. Reject snapshots that change during packaging, exceed size or file-count limits, or contain unsupported file types. Display path names and total bytes for approval without persisting file contents. A path exclusion is a safeguard, not proof that approved source has no secret; the user owns the upload decision.
3. **Execution identity.** Use a dedicated unprivileged account and per-job directory with restrictive permissions. No inherited local environment, SSH agent socket, cloud metadata credential, write-capable repository token, or general home-directory access. Pass only an allowlisted environment assembled for the job. Toolchain and command identity are validated before launch. Treat build scripts, tests, and their output as untrusted code and data.
4. **Data retention.** Reef stores job IDs, categories, timings, byte counts, exit states, and a keyed manifest digest locally. It does not store complete command lines, environment variables, source content, credentials, tokens, or raw output by default. Stream output to the live caller with bounded buffering and no default log archive. Remove the remote job directory after terminal state; on interruption, sweep expired job directories. Persistent caches contain dependencies and build artifacts only and have a configured TTL and size cap. Never cache source snapshots, secret-bearing paths, or credentials.
5. **Cache trust.** Partition caches by worker trust domain, repository identity, OS/architecture, toolchain version, lockfile, build profile, and relevant compile flags. A cache miss must rebuild correctly. Treat restored cache entries as untrusted executable input; do not share writable caches across unrelated users or trust levels. GitHub's cache guidance explicitly warns against secrets in caches and cache poisoning. [GitHub dependency caching](https://docs.github.com/en/actions/reference/workflows-and-actions/dependency-caching).
6. **Artifacts and output.** Return only explicitly requested artifacts by verified path and digest, with size limits. Do not implicitly copy the remote workspace back into the local tree. Output may reveal secrets printed by a command, so the CLI must avoid persistent transcripts and clearly show that the chosen command can produce sensitive output. Redaction cannot be the sole security boundary.

## Failure behavior

The job state machine is deterministic: `queued → snapshotting → transferring → running → collecting → succeeded|failed|cancelled|unknown`. Every transition has a job ID and a timeout. A snapshot or transfer failure leaves the local worktree untouched, marks the attempt failed, and cleans the partial remote directory. A command's nonzero exit is a job failure, not a transport failure. Artifact verification failure never writes a partial local result.

Cancellation sends a signal to the remote process group, waits a bounded grace period, then requests a forceful termination through a platform-specific worker adapter; it also aborts transfer and cleanup work. The worker must report confirmed process termination before Reef marks `cancelled`. If SSH disconnects or acknowledgement is lost after launch, mark the job `unknown`, reconcile by job ID on reconnect, and never start a second copy automatically. A lost worker triggers an expiry-based cleanup on that worker when it returns.

Before launch, a failure can return the job to the local queue only after a fresh local admission decision. After launch, no automatic local retry or failover is allowed because tests and build scripts can have side effects. Explicit user retry creates a new job and snapshot. A remote failure must not consume the workstation's reserved interactive budget through an uncontrolled local fallback.

## Staged implementation and release gates for #11

1. **Snapshot and policy.** Implement manifest creation, path selection, sensitive-path exclusions, stable-snapshot checks, size limits, and digest verification in the CLI core. Add behavior tests for modified tracked files, selected untracked files, deletions, symlinks, changing files, and rejection of sensitive paths. No remote interface is exposed yet.
2. **Worker protocol.** Add an internal SSH adapter with pinned host keys, job IDs, explicit toolchain/platform capability checks, isolated directories, bounded output, and cleanup. Keep process control in OS-specific modules. Add tests for each state transition, transfer failure, nonzero exit, cancellation, disconnect, reconnect, and duplicate launch prevention.
3. **Scheduler integration.** Admit remote jobs only when worker capacity and measured predicted completion beat local queueing by a configured margin, while reserving interactive workstation capacity. Measure both pathways and expose the reason for the choice. Never use a model for enforcement.
4. **Real-host validation.** On an approved, disposable test host, run the same dirty snapshot and build/lint/test matrix on macOS arm64 and Linux x86_64 where supported. Test cold and warm cache, source edits and deletions, 1 MB and 100 MB snapshots, large untracked exclusions, slow and interrupted links, cancellation during transfer and execution, stale host key, unavailable worker, and a command that exits nonzero. Record at least 20 samples per case, with p50/p95 connection, transfer, execution, end-to-end latency, bytes, cache hit rate, peak local CPU/memory, cleanup residue, and actual billed compute/storage/egress. Compare to local queue wait plus execution under workstation load.
5. **Enablement threshold.** Enable offload for a command class only when its measured p95 end-to-end completion is at least 20% faster than waiting and running locally under the tested pressure profile, its p95 local snapshot/transfer CPU and memory stay inside the configured reserve, 100% of sampled snapshots and artifacts verify, cancellation leaves no running process, and cleanup leaves no job source after the configured TTL. Report cost per completed job and require an explicit spending cap. If any threshold fails, keep that command class local and revise the design.

## Consequences

Persistent SSH workers offer incremental transfer and cache reuse for repeated work. The price is an operator-managed trust boundary, idle capacity, cache isolation, cleanup, and careful handling of dirty source. The local prototype supports the snapshot and cache assumptions but leaves remote value unproven. #11 owns the real-host measurements and release decision; this ADR does not authorize production offload on its own.
