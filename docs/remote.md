# Experimental remote offload

Reef can submit an explicitly approved build, lint, or test command to a trusted persistent SSH worker. This path is disabled by default. The local simulation in [ADR 0001](adr/0001-remote-execution.md) did not measure a real SSH host; do not enable general use until the ADR's real-host validation and cost gates pass.

## Worker setup

- Use a dedicated unprivileged SSH account and install the same Reef version and required toolchain on the worker.
- Create a private worker root, for example `/srv/reef-worker`, owned by that account with mode `0700`. Give it no source checkout or write-capable repository token.
- Pin the worker's verified host key in a dedicated local `known_hosts` file containing one entry. Use a dedicated private SSH identity. Both local files must be regular files with mode `0600` or stricter.
- The worker must have `rsync`, `rustc`, and the allowlisted command tools installed. Reef invokes its internal `remote-worker` command over SSH.

Copy [the example policy](../examples/remote-policy.json) to a private file, set mode `0600`, and fill in the worker target, paths, repository identity, platform, toolchain version, and exact allowed argument vectors. Keep `enabled` set to `false` until the real-host gate is complete. A policy with `enabled: false` refuses uploads and worker connections.

## Submission and recovery

Use an absolute executable path in each allowlisted argument vector.

```console
reef remote --config ~/.config/reef/remote.json --repo /path/to/worktree --approve-snapshot --category build --identity project-build -- /home/reef/.cargo/bin/cargo build --locked
reef remote --config ~/.config/reef/remote.json --status-job JOB_ID
reef remote --config ~/.config/reef/remote.json --cancel-job JOB_ID
reef remote --config ~/.config/reef/remote.json --cleanup-job JOB_ID
```

The exact command and identity must be in `allowed_commands`. Reef copies the current contents of tracked files and only untracked files named by repeated `--include-untracked PATH` flags. Tracked deletions remain deleted in the fresh remote job directory. It rejects sensitive or generated paths, symlinks, special files, snapshots over 10,000 files or 100 MiB, and files that change during copying. Reef prints the selected path list and total bytes, then transfers the source only when `--approve-snapshot` is present. Review the policy and selected paths before invoking the command; the worker operator can read the uploaded source and any command output.

Each job has a private directory under the configured worker root. Reef verifies SHA-256 hashes and the worker platform/toolchain before launch, streams command output to the terminal, preserves the command's exit status, and removes the job directory after a confirmed terminal state. If SSH disconnects after launch, Reef reports an unknown state and the job ID. Query or cancel that ID after the worker returns. Reef never automatically starts a second copy after launch. Cancellation requires the worker to confirm process termination. If cleanup fails, Reef reports that the remote job directory remains.

With `local_fallback: true`, a prepare, transfer, or verification failure requests a fresh local reservation from `reef serve`. Failures after launch never trigger local fallback. The remote path does not consume a local scheduler reservation, but snapshotting and transfer still use local CPU and memory. General admission based on measured local queue time, worker capacity, transfer cost, and interactive reserve remains a validation gate in ADR 0001.

The worker partitions its persistent `CARGO_TARGET_DIR` by repository, platform, toolchain, exact command, and lockfile digest. It removes idle cache partitions after seven days and evicts oldest unlocked partitions above 4 GiB. It does not cache source snapshots or credentials. No remote command output is archived by Reef.

## Release gate

No real worker was accessed while implementing this feature. Before changing `enabled` to `true` for general use, run the ADR's approved disposable-host matrix: at least 20 samples per case, p50/p95 connection and end-to-end latency, transfer bytes, cache reuse, local CPU/memory reserve, cancellation residue, snapshot integrity, and billed cost. Enable only command classes that pass its 20% p95 benefit and safety thresholds, with an explicit spending cap.
