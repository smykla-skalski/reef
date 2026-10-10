#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    path: PathBuf,
    server: Option<Child>,
}

impl Fixture {
    fn new(serve: bool) -> Self {
        let path = std::env::temp_dir().join(format!(
            "reef-agents-test-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let bin = path.join("bin");
        fs::create_dir(&bin).unwrap();
        write_script(&bin.join("codex"), "#!/bin/sh\nexec \"$@\"\n");
        write_script(&bin.join("claude"), "#!/bin/sh\nexec \"$@\"\n");
        write_script(&bin.join("opencode"), "#!/bin/sh\nexec \"$@\"\n");
        write_script(
            &bin.join("go"),
            "#!/bin/sh\nprintf '%s|%s|%s\\n' \"$REEF_ADMITTED\" \"$REEF_AGENT_KIND\" \"$*\" >> \"$REEF_TEST_OUTPUT\"\nif [ \"$REEF_TEST_PAUSE\" = 1 ]; then touch \"$REEF_TEST_MARKER\"; sleep 2; fi\nif [ \"$REEF_TEST_NESTED_ENABLED\" = 1 ] && [ -z \"$REEF_TEST_NESTED\" ]; then REEF_TEST_NESTED=1 go test nested; fi\nexit 7\n",
        );
        for tool in ["cargo", "make"] {
            write_script(
                &bin.join(tool),
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$REEF_TEST_OUTPUT\"\nexit 7\n",
            );
        }
        let server = if serve {
            let child = reef()
                .args([
                    "serve",
                    "--cpu",
                    "1",
                    "--memory-mib",
                    "1024",
                    "--no-pressure",
                    "--state-dir",
                ])
                .arg(&path)
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap();
            wait_until(|| {
                reef()
                    .args(["queue", "--state-dir"])
                    .arg(&path)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|status| status.success())
            });
            Some(child)
        } else {
            None
        };
        Self { path, server }
    }

    fn launch(&self, agent: &str, args: &[&str]) -> Command {
        self.launch_mode(agent, args, false)
    }

    fn launch_mode(&self, agent: &str, args: &[&str], control: bool) -> Command {
        let mut command = reef();
        let path = std::env::join_paths(
            std::iter::once(self.path.join("bin"))
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        command
            .args(["agents", "launch", agent, "--state-dir"])
            .arg(&self.path);
        if control {
            command.arg("--control");
        }
        command
            .arg("--")
            .args(args)
            .env("PATH", path)
            .env("HOME", &self.path)
            .env("REEF_TEST_OUTPUT", self.path.join("output"));
        command
    }

    fn control_report(&self) -> serde_json::Value {
        let output = reef()
            .args(["report", "--format", "json", "--state-dir"])
            .arg(&self.path)
            .arg("--control-state-dir")
            .arg(self.path.join("control"))
            .env("HOME", &self.path)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(server) = self.server.as_mut() {
            let _ = server.kill();
            let _ = server.wait();
        }
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn reef() -> Command {
    Command::new(env!("CARGO_BIN_EXE_reef"))
}

fn write_script(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn wait_until(condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for scheduler");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn agents_route_heavy_commands_and_preserve_status() {
    for agent in ["codex", "claude", "opencode"] {
        let fixture = Fixture::new(true);
        let output = fixture
            .launch(agent, &["go", "test", "./..."])
            .env("REEF_TEST_NESTED_ENABLED", "1")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(7));
        let calls = fs::read_to_string(fixture.path.join("output")).unwrap();
        assert_eq!(
            calls,
            format!("1|{agent}|test ./...\n1|{agent}|test nested\n")
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("queued request"), "{stderr}");
        assert!(stderr.contains("position 1"), "{stderr}");
        assert!(stderr.contains("\"category\":\"test\""), "{stderr}");
        assert!(!stderr.contains("./..."), "{stderr}");
        let history = fixture.path.join(".local/state/reef/history");
        let records: Vec<_> = fs::read_dir(history)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "jsonl")
            })
            .collect();
        assert_eq!(records.len(), 1);
        let record: serde_json::Value =
            serde_json::from_slice(&fs::read(&records[0]).unwrap()).unwrap();
        assert_eq!(record["category"], "test");
        assert_eq!(record["status"], "failed");
        assert!(record["identity"].as_str().unwrap().starts_with(agent));
    }
}

#[test]
fn light_command_runs_without_scheduler() {
    let fixture = Fixture::new(false);
    let output = fixture.launch("codex", &["go", "env"]).output().unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(
        fs::read_to_string(fixture.path.join("output")).unwrap(),
        "|codex|env\n"
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("queued request"));
}

#[test]
fn unavailable_scheduler_never_starts_heavy_command() {
    let fixture = Fixture::new(false);
    for args in [
        vec!["go", "test"],
        vec!["go", "-C", "module", "test"],
        vec!["cargo", "--offline", "test"],
        vec!["cargo", "--color", "never", "test"],
        vec!["cargo", "-vv", "test", "--no-run", "--offline"],
        vec!["make", "-j8", "test"],
        vec!["make", "--no-print-directory", "-f", "/dev/stdin", "test"],
    ] {
        let output = fixture.launch("claude", &args).output().unwrap();
        assert!(!output.status.success(), "{args:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("scheduler unavailable"),
            "{args:?}: {output:?}"
        );
    }
    assert!(!fixture.path.join("output").exists());
}

#[test]
fn makefile_from_stdin_is_not_consumed_or_duplicated() {
    for agent in ["codex", "claude"] {
        let fixture = Fixture::new(true);
        let fake_make = fixture.path.join("bin/make");
        fs::remove_file(&fake_make).unwrap();
        std::os::unix::fs::symlink("/usr/bin/make", &fake_make).unwrap();
        let mut child = fixture
            .launch(
                agent,
                &["make", "--no-print-directory", "-f", "/dev/stdin", "test"],
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"test:\n\t@echo REEF_STDIN_OK\n")
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{agent}: {output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "REEF_STDIN_OK"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("queued request"), "{agent}: {stderr}");
    }
}

#[test]
fn simultaneous_agent_launches_share_private_shims() {
    let fixture = Fixture::new(false);
    let mut codex = fixture.launch("codex", &["go", "env"]);
    let mut claude = fixture.launch("claude", &["go", "env"]);
    let first = std::thread::spawn(move || codex.output().unwrap());
    let second = std::thread::spawn(move || claude.output().unwrap());
    for output in [first.join().unwrap(), second.join().unwrap()] {
        assert_eq!(output.status.code(), Some(7), "{output:?}");
    }
}

#[test]
fn missing_control_command_is_counted_as_unavailable() {
    for agent in ["codex", "claude", "opencode"] {
        let fixture = Fixture::new(false);
        fs::remove_file(fixture.path.join("bin/go")).unwrap();
        let output = fixture
            .launch_mode(agent, &["go", "test", "private-argument"], true)
            .env("PATH", fixture.path.join("bin"))
            .output()
            .unwrap();
        assert!(!output.status.success(), "{output:?}");
        assert!(!fixture.path.join("output").exists());
        let report = fixture.control_report();
        assert_eq!(report["control"]["available"], true);
        assert_eq!(report["control"]["attempted"], 1);
        assert_eq!(report["control"]["completed"], 0);
        assert_eq!(report["control"]["pending"], 0);
        assert_eq!(report["control"]["unavailable"], 1);
        assert_eq!(report["command_count"], 0);
        let events = fs::read_to_string(fixture.path.join("control/attempts.jsonl")).unwrap();
        assert_eq!(events.lines().count(), 2);
        assert!(events.contains("\"event\":\"assigned\""));
        assert!(events.contains("\"event\":\"unavailable\""));
        assert!(!events.contains("private-argument"));
    }
}

#[test]
fn control_counts_one_attempt_per_top_level_command_without_scheduler() {
    for agent in ["codex", "claude", "opencode"] {
        let fixture = Fixture::new(false);
        let ready = reef()
            .args(["agents", "launch", agent, "--control", "--state-dir"])
            .arg(&fixture.path)
            .env(
                "PATH",
                std::env::join_paths(
                    std::iter::once(fixture.path.join("bin"))
                        .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
                )
                .unwrap(),
            )
            .args(["--", "go", "env"])
            .env("HOME", &fixture.path)
            .env("REEF_TEST_OUTPUT", fixture.path.join("output"))
            .output()
            .unwrap();
        assert_eq!(ready.status.code(), Some(7));
        let empty = fixture.control_report();
        assert_eq!(empty["control"]["available"], true);
        assert_eq!(empty["control"]["attempted"], 0);
        let output = fixture
            .launch_mode(agent, &["go", "test", "private-argument"], true)
            .env("REEF_TEST_NESTED_ENABLED", "1")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(7), "{output:?}");
        assert!(!String::from_utf8_lossy(&output.stderr).contains("queued request"));
        let report = fixture.control_report();
        assert_eq!(report["control"]["attempted"], 1);
        assert_eq!(report["control"]["completed"], 1);
        assert_eq!(report["control"]["pending"], 0);
        assert_eq!(report["control"]["unavailable"], 0);
        assert_eq!(report["command_count"], 1);
        assert_eq!(report["agent_commands"][0]["agent"], agent);
        let events = fs::read_to_string(fixture.path.join("control/attempts.jsonl")).unwrap();
        let measurements =
            fs::read_to_string(fixture.path.join("control/measurements.jsonl")).unwrap();
        assert_eq!(events.lines().count(), 2);
        assert_eq!(measurements.lines().count(), 1);
        assert!(events.contains("\"assignment\":\"control\""));
        assert!(!events.contains("private-argument"));
        assert!(!measurements.contains("private-argument"));
        assert!(!events.contains(fixture.path.to_str().unwrap()));
        assert!(!measurements.contains(fixture.path.to_str().unwrap()));
        assert_eq!(
            fs::metadata(fixture.path.join("control/attempts.jsonl"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn control_clock_rollback_keeps_report_and_enrollment_usable() {
    let fixture = Fixture::new(false);
    let first = fixture
        .launch_mode("codex", &["go", "test"], true)
        .output()
        .unwrap();
    assert_eq!(first.status.code(), Some(7));

    let events_path = fixture.path.join("control/attempts.jsonl");
    let mut events: Vec<serde_json::Value> = fs::read_to_string(&events_path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let assigned_at = events[0]["at_unix_ms"].as_u64().unwrap();
    events[0]["at_unix_ms"] = (assigned_at + 300_000).into();
    fs::write(&events_path, format!("{}\n{}\n", events[0], events[1])).unwrap();

    let measurements_path = fixture.path.join("control/measurements.jsonl");
    let mut measurement: serde_json::Value =
        serde_json::from_slice(&fs::read(&measurements_path).unwrap()).unwrap();
    let started_at = measurement["started_at_unix_ms"].as_u64().unwrap();
    measurement["started_at_unix_ms"] = (started_at + 300_000).into();
    fs::write(&measurements_path, format!("{measurement}\n")).unwrap();

    let report = fixture.control_report();
    assert_eq!(report["control"]["available"], true);
    assert_eq!(report["control"]["attempted"], 1);
    assert_eq!(report["control"]["completed"], 1);
    assert_eq!(report["control"]["unavailable"], 0);
    assert_eq!(report["command_count"], 1);
    let explicit = reef()
        .args(["report", "--format", "json", "--state-dir"])
        .arg(&fixture.path)
        .arg("--control-state-dir")
        .arg(fixture.path.join("control"))
        .arg("--records")
        .arg(&measurements_path)
        .env("HOME", &fixture.path)
        .output()
        .unwrap();
    assert!(explicit.status.success(), "{explicit:?}");
    let explicit_report: serde_json::Value = serde_json::from_slice(&explicit.stdout).unwrap();
    assert_eq!(explicit_report["command_count"], 1);

    let second = fixture
        .launch_mode("codex", &["go", "test"], true)
        .output()
        .unwrap();
    assert_eq!(second.status.code(), Some(7), "{second:?}");
    assert_eq!(fixture.control_report()["control"]["completed"], 2);
}

#[test]
fn control_refuses_unwritable_telemetry_and_report_warns_on_missing_telemetry() {
    let fixture = Fixture::new(false);
    let missing = fixture.control_report();
    assert_eq!(missing["control"]["available"], false);
    assert!(missing["control"]["attempted"].is_null());
    assert!(
        missing["control"]["warning"]
            .as_str()
            .unwrap()
            .contains("missing")
    );
    let dir = fixture.path.join("control");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    let output = fixture
        .launch_mode("codex", &["go", "test"], true)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!fixture.path.join("output").exists());
    assert!(String::from_utf8_lossy(&output.stderr).contains("control enrollment refused"));
    assert_eq!(fixture.control_report()["control"]["available"], false);
}

#[test]
fn missing_control_measurements_warn_after_enrollment() {
    let fixture = Fixture::new(false);
    let output = fixture
        .launch_mode("codex", &["go", "test"], true)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7));
    fs::remove_file(fixture.path.join("control/measurements.jsonl")).unwrap();
    let report = fixture.control_report();
    assert_eq!(report["control"]["available"], false);
    assert!(report["control"]["attempted"].is_null());
    let before = fs::read_to_string(fixture.path.join("output")).unwrap();
    let output = fixture
        .launch_mode("codex", &["go", "test"], true)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("control enrollment refused"));
    assert_eq!(
        fs::read_to_string(fixture.path.join("output")).unwrap(),
        before
    );
}

#[test]
fn lost_control_measurement_is_unavailable_and_blocks_new_enrollment() {
    let fixture = Fixture::new(false);
    assert_eq!(
        fixture
            .launch_mode("codex", &["go", "test"], true)
            .output()
            .unwrap()
            .status
            .code(),
        Some(7)
    );
    fs::write(fixture.path.join("control/measurements.jsonl"), b"").unwrap();
    let report = fixture.control_report();
    assert_eq!(report["control"]["available"], true);
    assert_eq!(report["control"]["attempted"], 1);
    assert_eq!(report["control"]["completed"], 0);
    assert_eq!(report["control"]["pending"], 0);
    assert_eq!(report["control"]["unavailable"], 1);
    assert!(
        report["control"]["warning"]
            .as_str()
            .unwrap()
            .contains("missing")
    );
    let before = fs::read_to_string(fixture.path.join("output")).unwrap();
    let output = fixture
        .launch_mode("codex", &["go", "test"], true)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("control enrollment refused"));
    assert_eq!(
        fs::read_to_string(fixture.path.join("output")).unwrap(),
        before
    );
}

#[test]
fn malformed_control_measurement_blocks_enrollment_before_execution() {
    let fixture = Fixture::new(false);
    assert_eq!(
        fixture
            .launch_mode("codex", &["go", "env"], true)
            .output()
            .unwrap()
            .status
            .code(),
        Some(7)
    );
    fs::write(
        fixture.path.join("control/measurements.jsonl"),
        b"{\"identity\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}\n",
    )
    .unwrap();
    assert_eq!(fixture.control_report()["control"]["available"], false);
    let before = fs::read_to_string(fixture.path.join("output")).unwrap();
    let output = fixture
        .launch_mode("codex", &["go", "test"], true)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("control enrollment refused"));
    assert_eq!(
        fs::read_to_string(fixture.path.join("output")).unwrap(),
        before
    );
}

#[test]
fn orphan_control_measurement_blocks_enrollment() {
    let fixture = Fixture::new(false);
    assert_eq!(
        fixture
            .launch_mode("codex", &["go", "test"], true)
            .output()
            .unwrap()
            .status
            .code(),
        Some(7)
    );
    fs::write(fixture.path.join("control/attempts.jsonl"), b"").unwrap();
    assert_eq!(fixture.control_report()["control"]["available"], false);
    let before = fs::read_to_string(fixture.path.join("output")).unwrap();
    let output = fixture
        .launch_mode("codex", &["go", "test"], true)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("control enrollment refused"));
    assert_eq!(
        fs::read_to_string(fixture.path.join("output")).unwrap(),
        before
    );
}

#[test]
fn repeated_control_commands_receive_distinct_attempt_ids() {
    let fixture = Fixture::new(false);
    for _ in 0..2 {
        let output = fixture
            .launch_mode("codex", &["go", "test"], true)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(7));
    }
    let report = fixture.control_report();
    assert_eq!(report["control"]["attempted"], 2);
    assert_eq!(report["control"]["completed"], 2);
    let events = fs::read_to_string(fixture.path.join("control/attempts.jsonl")).unwrap();
    let identities: std::collections::BTreeSet<_> = events
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|record| record["event"] == "assigned")
        .map(|record| record["attempt_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(identities.len(), 2);
    assert!(identities.iter().all(|id| id.len() == 32));
}

#[test]
fn running_control_attempt_is_pending_until_its_measurement_finishes() {
    let fixture = Fixture::new(false);
    let marker = fixture.path.join("started");
    let child = fixture
        .launch_mode("codex", &["go", "test"], true)
        .env("REEF_TEST_PAUSE", "1")
        .env("REEF_TEST_MARKER", &marker)
        .spawn()
        .unwrap();
    wait_until(|| marker.exists());
    let pending = fixture.control_report();
    assert_eq!(pending["control"]["attempted"], 1);
    assert_eq!(pending["control"]["pending"], 1);
    assert_eq!(pending["control"]["completed"], 0);
    assert_eq!(child.wait_with_output().unwrap().status.code(), Some(7));
    let completed = fixture.control_report();
    assert_eq!(completed["control"]["pending"], 0);
    assert_eq!(completed["control"]["completed"], 1);
}

#[test]
fn mismatched_control_measurement_blocks_report_and_active_session() {
    for field in ["category", "agent_kind"] {
        let fixture = Fixture::new(false);
        let first_done = fixture.path.join("first-done");
        let proceed = fixture.path.join("proceed");
        let child = fixture
            .launch_mode(
                "codex",
                &[
                    "sh",
                    "-c",
                    "go test; touch \"$REEF_TEST_FIRST_DONE\"; while [ ! -f \"$REEF_TEST_PROCEED\" ]; do sleep 0.02; done; go test",
                ],
                true,
            )
            .env("REEF_TEST_FIRST_DONE", &first_done)
            .env("REEF_TEST_PROCEED", &proceed)
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        wait_until(|| first_done.exists());
        let measurements = fixture.path.join("control/measurements.jsonl");
        let mut record: serde_json::Value =
            serde_json::from_slice(&fs::read(&measurements).unwrap()).unwrap();
        record[field] = serde_json::Value::String(if field == "category" {
            "lint".to_owned()
        } else {
            "claude".to_owned()
        });
        fs::write(&measurements, format!("{record}\n")).unwrap();
        assert_eq!(fixture.control_report()["control"]["available"], false);
        fs::write(&proceed, b"").unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(!output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("control enrollment refused"),
            "{output:?}"
        );
        assert_eq!(
            fs::read_to_string(fixture.path.join("output"))
                .unwrap()
                .lines()
                .count(),
            1
        );
    }
}

#[test]
fn mismatched_control_terminal_event_blocks_enrollment() {
    let fixture = Fixture::new(false);
    assert_eq!(
        fixture
            .launch_mode("codex", &["go", "test"], true)
            .output()
            .unwrap()
            .status
            .code(),
        Some(7)
    );
    let events = fixture.path.join("control/attempts.jsonl");
    let mut records: Vec<serde_json::Value> = fs::read_to_string(&events)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    records[1]["category"] = serde_json::Value::String("lint".to_owned());
    let mut tampered = String::new();
    for record in &records {
        tampered.push_str(&record.to_string());
        tampered.push('\n');
    }
    fs::write(&events, tampered).unwrap();
    assert_eq!(fixture.control_report()["control"]["available"], false);
    let before = fs::read_to_string(fixture.path.join("output")).unwrap();
    let output = fixture
        .launch_mode("codex", &["go", "test"], true)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("control enrollment refused"));
    assert_eq!(
        fs::read_to_string(fixture.path.join("output")).unwrap(),
        before
    );
}
