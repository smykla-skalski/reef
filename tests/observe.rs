use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct Recorder(Child);

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn recorder(dir: &Path) -> Recorder {
    recorder_at_interval(dir, "1")
}

fn recorder_at_interval(dir: &Path, interval: &str) -> Recorder {
    Recorder(
        Command::new(env!("CARGO_BIN_EXE_reef"))
            .args([
                "observe",
                "run",
                "--interval-seconds",
                interval,
                "--state-dir",
            ])
            .arg(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    )
}

#[test]
fn recorder_compacts_existing_history_before_its_first_new_sample() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let now = u64::try_from(nonce / 1_000_000).unwrap();
    let at = now - 3 * 86_400_000;
    let dir = std::env::temp_dir().join(format!(
        "reef-observe-compact-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&dir).unwrap();
    let raw = dir.join(format!("samples-{at}-1.jsonl"));
    fs::write(
        &raw,
        format!(
            "{{\"at_unix_ms\":{at},\"working_ms\":1000,\"cpu_percent\":50.0,\"memory_used_bytes\":100,\"memory_total_bytes\":1000}}\n"
        ),
    )
    .unwrap();
    let mut running = recorder_at_interval(&dir, "60");

    assert!(
        wait_until(|| {
            !raw.exists()
                && fs::read_dir(&dir).is_ok_and(|entries| {
                    entries
                        .flatten()
                        .any(|entry| entry.file_name().to_string_lossy().starts_with("host-5m-"))
                })
        }),
        "{}",
        child_failure(&mut running)
    );
    let stopped = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["observe", "stop", "--state-dir"])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(stopped.status.success(), "{stopped:?}");
    assert!(wait_until(|| running.0.try_wait().unwrap().is_some()));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn recorder_folds_previous_hour_agent_ticks_before_enforcing_cap() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let now = u64::try_from(nonce / 1_000_000).unwrap();
    let at = now - 3 * 3_600_000;
    let dir = std::env::temp_dir().join(format!(
        "reef-agent-fold-startup-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&dir).unwrap();
    let raw = dir.join(format!("agent-samples-{at}-1.jsonl"));
    let rollup = dir.join(format!("agent-rollups-{at}-1.jsonl"));
    let line = format!(
        "{{\"at_unix_ms\":{at},\"working_ms\":1000,\"agents\":[{{\"kind\":\"codex\",\"root_pid\":1,\"process_count\":1,\"cpu_percent\":25.0,\"memory_bytes\":1000,\"read_bytes\":0,\"written_bytes\":0}}],\"commands\":[]}}\n"
    );
    fs::write(&raw, line.repeat(7_000)).unwrap();
    let mut running = Recorder(
        Command::new(env!("CARGO_BIN_EXE_reef"))
            .args([
                "observe",
                "run",
                "--interval-seconds",
                "60",
                "--max-storage-mib",
                "1",
                "--state-dir",
            ])
            .arg(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );

    assert!(
        wait_until(|| !raw.exists() && rollup.exists()),
        "{}",
        child_failure(&mut running)
    );
    let stopped = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["observe", "stop", "--state-dir"])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(stopped.status.success(), "{stopped:?}");
    assert!(wait_until(|| running.0.try_wait().unwrap().is_some()));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn fast_agent_tick_enforces_cap_before_first_host_snapshot() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let now = u64::try_from(nonce / 1_000_000).unwrap();
    let dir = std::env::temp_dir().join(format!(
        "reef-agent-fast-cap-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&dir).unwrap();
    let raw = dir.join(format!("samples-{now}-1.jsonl"));
    let mut sample = format!(
        "{{\"at_unix_ms\":{now},\"working_ms\":60000,\"cpu_percent\":50.0,\"memory_used_bytes\":1048576,\"memory_total_bytes\":2097152}}"
    );
    sample.extend(std::iter::repeat_n(' ', 1_047_000 - sample.len() - 1));
    sample.push('\n');
    fs::write(&raw, sample).unwrap();
    let marked = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["agents", "observe", "codex", "--pid"])
        .arg(std::process::id().to_string())
        .arg("--state-dir")
        .arg(&dir)
        .output()
        .unwrap();
    assert!(marked.status.success(), "{marked:?}");
    let mut running = Recorder(
        Command::new(env!("CARGO_BIN_EXE_reef"))
            .args([
                "observe",
                "run",
                "--interval-seconds",
                "60",
                "--max-storage-mib",
                "1",
                "--state-dir",
            ])
            .arg(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );

    assert!(
        wait_until(|| !raw.exists()),
        "{}",
        child_failure(&mut running)
    );
    let stopped = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["observe", "stop", "--state-dir"])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(stopped.status.success(), "{stopped:?}");
    assert!(wait_until(|| running.0.try_wait().unwrap().is_some()));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn short_registered_session_is_visible_before_first_host_snapshot() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("reef-short-agent-{}-{nonce}", std::process::id()));
    let marked = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["agents", "observe", "codex", "--pid"])
        .arg(std::process::id().to_string())
        .arg("--state-dir")
        .arg(&dir)
        .output()
        .unwrap();
    assert!(marked.status.success(), "{marked:?}");
    let mut running = recorder_at_interval(&dir, "60");

    assert!(
        wait_until(|| fs::read_dir(&dir).is_ok_and(|entries| {
            entries.flatten().any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("agent-samples-")
            })
        })),
        "{}",
        child_failure(&mut running)
    );
    let report = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["report", "--state-dir"])
        .arg(&dir)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert!(report.status.success(), "{report:?}");
    let json: serde_json::Value = serde_json::from_slice(&report.stdout).unwrap();
    assert_eq!(json["observation_count"], 0);
    assert_eq!(json["agents"][0]["kind"], "codex");
    assert_eq!(json["agents"][0]["active_samples"], 1);
    let stopped = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["observe", "stop", "--state-dir"])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(stopped.status.success());
    assert!(wait_until(|| running.0.try_wait().unwrap().is_some()));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn process_cost_is_finalized_when_the_registered_agent_exits() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "reef-agent-root-exit-{}-{nonce}",
        std::process::id()
    ));
    let mut running = recorder_at_interval(&dir, "60");
    let mut agent = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "agent_root_fixture_spawns_descendant",
            "--nocapture",
        ])
        .env("REEF_TEST_AGENT_ROOT", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let marked = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["agents", "observe", "codex", "--pid"])
        .arg(agent.id().to_string())
        .arg("--state-dir")
        .arg(&dir)
        .output()
        .unwrap();
    assert!(marked.status.success(), "{marked:?}");
    assert!(
        wait_until(|| {
            let report = Command::new(env!("CARGO_BIN_EXE_reef"))
                .args(["report", "--state-dir"])
                .arg(&dir)
                .args(["--format", "json"])
                .output()
                .unwrap();
            if !report.status.success() {
                return false;
            }
            let json: serde_json::Value = serde_json::from_slice(&report.stdout).unwrap();
            json["passive_commands"].as_array().is_some_and(|groups| {
                groups
                    .iter()
                    .any(|group| group["agent"] == "codex" && group["family"] == "git")
            })
        }),
        "{}",
        child_failure(&mut running)
    );
    drop(agent.stdin.take());
    assert!(agent.wait().unwrap().success());

    assert!(
        wait_until(|| {
            let report = Command::new(env!("CARGO_BIN_EXE_reef"))
                .args(["report", "--state-dir"])
                .arg(&dir)
                .args(["--format", "json"])
                .output()
                .unwrap();
            if !report.status.success() {
                return false;
            }
            let json: serde_json::Value = serde_json::from_slice(&report.stdout).unwrap();
            json["process_commands"].as_array().is_some_and(|groups| {
                groups.iter().any(|group| {
                    group["agent"] == "codex"
                        && group["family"] == "git"
                        && group["process_count"]
                            .as_f64()
                            .is_some_and(|count| count >= 1.0)
                })
            })
        }),
        "{}",
        child_failure(&mut running)
    );
    let stopped = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["observe", "stop", "--state-dir"])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(stopped.status.success(), "{stopped:?}");
    assert!(wait_until(|| running.0.try_wait().unwrap().is_some()));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn agent_root_fixture_spawns_descendant() {
    if std::env::var_os("REEF_TEST_AGENT_ROOT").is_none() {
        return;
    }
    let mut git = Command::new("git")
        .args(["hash-object", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input).unwrap();
    drop(git.stdin.take());
    assert!(git.wait().unwrap().success());
}

#[test]
fn registered_agent_descendant_command_appears_in_report() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("reef-agent-command-{}-{nonce}", std::process::id()));
    let mut agent = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "agent_root_fixture_spawns_descendant",
            "--nocapture",
        ])
        .env("REEF_TEST_AGENT_ROOT", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let marked = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["agents", "observe", "codex", "--pid"])
        .arg(agent.id().to_string())
        .arg("--state-dir")
        .arg(&dir)
        .output()
        .unwrap();
    assert!(marked.status.success(), "{marked:?}");
    let mut running = recorder(&dir);

    let observed = wait_until(|| {
        let report = Command::new(env!("CARGO_BIN_EXE_reef"))
            .args(["report", "--state-dir"])
            .arg(&dir)
            .args(["--format", "json"])
            .output()
            .unwrap();
        if !report.status.success() {
            return false;
        }
        let json: serde_json::Value = serde_json::from_slice(&report.stdout).unwrap();
        json["passive_commands"].as_array().is_some_and(|groups| {
            groups
                .iter()
                .any(|group| group["agent"] == "codex" && group["family"] == "git")
        })
    });
    drop(agent.stdin.take());
    assert!(agent.wait().unwrap().success());
    assert!(observed, "{}", child_failure(&mut running));
    assert!(
        wait_until(|| {
            let report = Command::new(env!("CARGO_BIN_EXE_reef"))
                .args(["report", "--state-dir"])
                .arg(&dir)
                .args(["--format", "json"])
                .output()
                .unwrap();
            if !report.status.success() {
                return false;
            }
            let json: serde_json::Value = serde_json::from_slice(&report.stdout).unwrap();
            json["process_commands"].as_array().is_some_and(|groups| {
                groups.iter().any(|group| {
                    group["agent"] == "codex"
                        && group["family"] == "git"
                        && group["executable"] == "git"
                        && group["process_count"]
                            .as_f64()
                            .is_some_and(|count| count >= 1.0)
                })
            })
        }),
        "{}",
        child_failure(&mut running)
    );
    let stopped = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["observe", "stop", "--state-dir"])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(stopped.status.success());
    assert!(wait_until(|| running.0.try_wait().unwrap().is_some()));
    fs::remove_dir_all(dir).unwrap();
}

fn child_failure(recorder: &mut Recorder) -> String {
    let status = recorder.0.try_wait().unwrap();
    let mut stderr = String::new();
    if status.is_some() {
        recorder
            .0
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
    }
    format!("status: {status:?}; stderr: {stderr}")
}

fn wait_until(mut ready: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if ready() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn recorder_recovers_after_abrupt_exit_without_running_twice() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "reef-observe-restart-{}-{nonce}",
        std::process::id()
    ));
    let mut first = recorder(&dir);
    let lock = dir.join("recorder.pid");
    assert!(
        wait_until(|| fs::read_to_string(&lock).is_ok_and(|text| !text.is_empty())),
        "{}",
        child_failure(&mut first)
    );

    let mut duplicate = recorder(&dir);
    assert!(wait_until(|| duplicate.0.try_wait().unwrap().is_some()));
    assert!(!duplicate.0.wait().unwrap().success());

    first.0.kill().unwrap();
    first.0.wait().unwrap();
    assert_ne!(fs::read_to_string(&lock).unwrap(), "");

    let mut restarted = recorder(&dir);
    assert!(wait_until(|| {
        fs::read_to_string(&lock)
            .is_ok_and(|text| text.starts_with(&format!("{} ", restarted.0.id())))
    }));
    assert!(wait_until(|| {
        fs::read_dir(&dir).unwrap().flatten().any(|entry| {
            entry.file_name().to_string_lossy().starts_with("samples-")
                && entry.metadata().unwrap().len() > 0
        })
    }));

    let report = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["report", "--state-dir"])
        .arg(&dir)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert!(report.status.success());
    let json: serde_json::Value = serde_json::from_slice(&report.stdout).unwrap();
    assert!(json["observation_count"].as_u64().unwrap() > 0);

    let stop = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["observe", "stop", "--state-dir"])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(
        stop.status.success(),
        "{}",
        String::from_utf8_lossy(&stop.stderr)
    );
    assert!(wait_until(|| restarted.0.try_wait().unwrap().is_some()));
    assert!(restarted.0.wait().unwrap().success());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn recorder_reconciles_completed_span_before_startup_compaction() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let now = u64::try_from(nonce / 1_000_000).unwrap();
    let at = now - 15 * 86_400_000;
    let dir = std::env::temp_dir().join(format!(
        "reef-process-recovery-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&dir).unwrap();
    let raw = dir.join(format!("process-spans-{at}-1.jsonl"));
    let common = serde_json::json!({
        "agent": "codex",
        "parent_pid": 1,
        "parent_start_time": 1,
        "executable": "go",
        "subcommand": "test",
        "family": "go test",
        "category": "test",
        "workspace_kind": "worktree",
        "first_seen_unix_ms": at - 1_000,
        "last_seen_unix_ms": at,
        "observed_working_ms": 1_000,
        "sample_count": 1,
        "cpu_core_ms_estimate": 500.0,
        "peak_cpu_percent": 50.0,
        "peak_memory_bytes": 100
    });
    let mut completed = common.clone();
    completed["pid"] = serde_json::json!(2_000_000_000_u32);
    completed["start_time"] = serde_json::json!(10);
    completed["read_bytes"] = serde_json::json!(0);
    completed["written_bytes"] = serde_json::json!(0);
    completed["coverage"] = serde_json::json!("sampled");
    fs::write(&raw, format!("{completed}\n")).unwrap();
    let mut active = common;
    active["key"] = serde_json::json!({"pid": 2_000_000_000_u32, "start_time": 10});
    active["first_total_read_bytes"] = serde_json::json!(10);
    active["last_total_read_bytes"] = serde_json::json!(10);
    active["first_total_written_bytes"] = serde_json::json!(0);
    active["last_total_written_bytes"] = serde_json::json!(0);
    fs::write(
        dir.join("process-active.json"),
        serde_json::to_vec(&vec![active]).unwrap(),
    )
    .unwrap();

    let mut running = recorder(&dir);
    assert!(
        wait_until(|| {
            !raw.exists()
                && fs::read_dir(&dir).is_ok_and(|entries| {
                    entries
                        .flatten()
                        .any(|entry| entry.file_name().to_string_lossy().starts_with("samples-"))
                })
        }),
        "{}",
        child_failure(&mut running)
    );
    let stop = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["observe", "stop", "--state-dir"])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(stop.status.success(), "{stop:?}");
    assert!(wait_until(|| running.0.try_wait().unwrap().is_some()));
    let report = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["report", "--state-dir"])
        .arg(&dir)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert!(report.status.success(), "{report:?}");
    let json: serde_json::Value = serde_json::from_slice(&report.stdout).unwrap();
    assert_eq!(
        json["process_commands"][0]["process_count"].as_f64(),
        Some(1.0)
    );
    assert_eq!(
        json["process_commands"][0]["cpu_core_ms_estimate"].as_f64(),
        Some(500.0)
    );
    fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn recorder_reloads_after_its_invocation_symlink_changes() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("reef-reload-{}-{nonce}", std::process::id()));
    fs::create_dir(&dir).unwrap();
    let invocation = dir.join("reef");
    symlink(env!("CARGO_BIN_EXE_reef"), &invocation).unwrap();
    let replacement = dir.join("replacement");
    let marker = dir.join("reloaded");
    fs::write(
        &replacement,
        "#!/bin/sh\n: > \"$REEF_RELOAD_MARKER\"\nexec \"$REEF_RELOAD_BINARY\" \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&replacement, fs::Permissions::from_mode(0o700)).unwrap();

    let mut running = Recorder(
        Command::new(&invocation)
            .args(["observe", "run", "--interval-seconds", "1", "--state-dir"])
            .arg(&dir)
            .env("REEF_RELOAD_MARKER", &marker)
            .env("REEF_RELOAD_BINARY", env!("CARGO_BIN_EXE_reef"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    assert!(
        wait_until(|| fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .any(|entry| { entry.file_name().to_string_lossy().starts_with("samples-") })),
        "{}",
        child_failure(&mut running)
    );
    let next = dir.join("reef-next");
    symlink(&replacement, &next).unwrap();
    fs::rename(&next, &invocation).unwrap();

    assert!(
        wait_until(|| marker.exists()),
        "{}",
        child_failure(&mut running)
    );
    let stopped = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["observe", "stop", "--state-dir"])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(stopped.status.success(), "{stopped:?}");
    assert!(wait_until(|| running.0.try_wait().unwrap().is_some()));
    assert!(running.0.wait().unwrap().success());
    fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn recorder_launched_through_env_reloads_after_link_changes() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("reef-env-reload-{}-{nonce}", std::process::id()));
    fs::create_dir(&dir).unwrap();
    let invocation = dir.join("reef");
    symlink(env!("CARGO_BIN_EXE_reef"), &invocation).unwrap();
    let replacement = dir.join("replacement");
    let marker = dir.join("reloaded");
    fs::write(
        &replacement,
        "#!/bin/sh\n: > \"$REEF_RELOAD_MARKER\"\nexec \"$REEF_RELOAD_BINARY\" \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&replacement, fs::Permissions::from_mode(0o700)).unwrap();
    let existing_path = std::env::var_os("PATH").unwrap();
    let path = std::env::join_paths(
        std::iter::once(dir.clone()).chain(std::env::split_paths(&existing_path)),
    )
    .unwrap();

    let mut running = Recorder(
        Command::new("/usr/bin/env")
            .arg("reef")
            .args(["observe", "run", "--interval-seconds", "1", "--state-dir"])
            .arg(&dir)
            .env("PATH", path)
            .env("REEF_RELOAD_MARKER", &marker)
            .env("REEF_RELOAD_BINARY", env!("CARGO_BIN_EXE_reef"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    assert!(
        wait_until(|| fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .any(|entry| { entry.file_name().to_string_lossy().starts_with("samples-") })),
        "{}",
        child_failure(&mut running)
    );
    let next = dir.join("reef-next");
    symlink(&replacement, &next).unwrap();
    fs::rename(&next, &invocation).unwrap();

    assert!(
        wait_until(|| marker.exists()),
        "{}",
        child_failure(&mut running)
    );
    let stopped = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["observe", "stop", "--state-dir"])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(stopped.status.success(), "{stopped:?}");
    assert!(wait_until(|| running.0.try_wait().unwrap().is_some()));
    assert!(running.0.wait().unwrap().success());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn registered_agent_is_reported_without_scheduling_its_process() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("reef-passive-agent-{}-{nonce}", std::process::id()));
    let marked = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["agents", "observe", "codex", "--pid"])
        .arg(std::process::id().to_string())
        .arg("--state-dir")
        .arg(&dir)
        .output()
        .unwrap();
    assert!(
        marked.status.success(),
        "{}",
        String::from_utf8_lossy(&marked.stderr)
    );
    let mut running = recorder(&dir);
    assert!(
        wait_until(|| {
            fs::read_dir(&dir).unwrap().flatten().any(|entry| {
                entry.file_name().to_string_lossy().starts_with("samples-")
                    && entry.metadata().unwrap().len() > 0
            })
        }),
        "{}",
        child_failure(&mut running)
    );
    let report = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["report", "--state-dir"])
        .arg(&dir)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert!(report.status.success());
    let json: serde_json::Value = serde_json::from_slice(&report.stdout).unwrap();
    assert_eq!(json["agents"][0]["kind"], "codex");
    assert!(json["agents"][0]["peak_processes"].as_u64().unwrap() >= 1);
    let stopped = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["observe", "stop", "--state-dir"])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(stopped.status.success());
    assert!(wait_until(|| running.0.try_wait().unwrap().is_some()));
    fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn agent_hook_finds_its_cli_ancestor_without_a_pid_argument() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("reef-agent-hook-{}-{nonce}", std::process::id()));
    fs::create_dir(&dir).unwrap();
    let script = dir.join("codex");
    std::os::unix::fs::symlink("/bin/sh", &script).unwrap();
    let output = Command::new(&script)
        .arg("-c")
        .arg("\"$1\" agents observe codex --state-dir \"$2\"; sleep 1")
        .arg("codex")
        .arg(env!("CARGO_BIN_EXE_reef"))
        .arg(&dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let registration = fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .find(|entry| entry.file_name().to_string_lossy().starts_with("agent-"))
        .unwrap();
    let json: serde_json::Value =
        serde_json::from_slice(&fs::read(registration.path()).unwrap()).unwrap();
    assert_eq!(json["kind"], "codex");
    fs::remove_dir_all(dir).unwrap();
}
