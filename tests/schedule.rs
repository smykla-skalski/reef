#![cfg(unix)]

use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    path: PathBuf,
    server: Child,
}

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "reef-schedule-test-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let server = reef()
            .args(["serve", "--cpu", "1", "--memory-mib", "1024", "--state-dir"])
            .arg(&path)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let fixture = Self { path, server };
        Self::wait_until(|| fixture.path.join("reef.sock").exists());
        fixture
    }

    fn wait_until(condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for scheduler state"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn schedule(&self, command: &[&str]) -> Command {
        let mut process = reef();
        process
            .arg("schedule")
            .arg("--state-dir")
            .arg(&self.path)
            .arg("--");
        process.args(command);
        process
    }

    fn jobs(&self) -> Vec<Value> {
        let output = reef()
            .arg("queue")
            .arg("--state-dir")
            .arg(&self.path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.server.kill();
        let _ = self.server.wait();
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn reef() -> Command {
    Command::new(env!("CARGO_BIN_EXE_reef"))
}

fn has_job(jobs: &[Value], state: &str) -> bool {
    jobs.iter().any(|job| job["state"] == state)
}

#[test]
fn unavailable_scheduler_does_not_start_command() {
    let path = std::env::temp_dir().join(format!("reef-missing-{}", std::process::id()));
    let marker = path.join("started");
    let output = reef()
        .arg("schedule")
        .arg("--state-dir")
        .arg(&path)
        .args(["--", "sh", "-c", "touch \"$REEF_MARKER\""])
        .env("REEF_MARKER", &marker)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!marker.exists());
    assert!(String::from_utf8_lossy(&output.stderr).contains("scheduler unavailable"));
}

#[test]
fn shared_budget_queues_exposes_and_cancels_a_request() {
    let fixture = Fixture::new();
    let ready = fixture.path.join("first-started");
    let second_marker = fixture.path.join("second-started");
    let mut first = fixture
        .schedule(&["sh", "-c", "touch \"$REEF_READY\"; sleep 30"])
        .env("REEF_READY", &ready)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    Fixture::wait_until(|| ready.exists());
    let second = fixture
        .schedule(&["sh", "-c", "touch \"$REEF_SECOND\""])
        .env("REEF_SECOND", &second_marker)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    Fixture::wait_until(|| has_job(&fixture.jobs(), "queued"));
    let jobs = fixture.jobs();
    assert!(has_job(&jobs, "running"));
    let queued_id = jobs.iter().find(|job| job["state"] == "queued").unwrap()["id"]
        .as_u64()
        .unwrap();
    let output = reef()
        .arg("cancel")
        .arg(queued_id.to_string())
        .arg("--state-dir")
        .arg(&fixture.path)
        .output()
        .unwrap();
    assert!(output.status.success());
    let second_output = second.wait_with_output().unwrap();
    assert_eq!(second_output.status.code(), Some(130));
    assert!(!second_marker.exists());
    first.kill().unwrap();
    first.wait().unwrap();
    Fixture::wait_until(|| fixture.jobs().is_empty());
    let third = fixture.schedule(&["sh", "-c", "exit 7"]).output().unwrap();
    assert_eq!(third.status.code(), Some(7));
}

#[test]
fn nested_scheduled_command_reuses_parent_admission() {
    let fixture = Fixture::new();
    let binary = env!("CARGO_BIN_EXE_reef");
    let output = fixture
        .schedule(&[
            binary,
            "schedule",
            "--state-dir",
            fixture.path.to_str().unwrap(),
            "--",
            "sh",
            "-c",
            "exit 9",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(9));
    assert_eq!(fixture.jobs().len(), 0);
}

#[test]
fn client_termination_releases_capacity() {
    let fixture = Fixture::new();
    let ready = fixture.path.join("started");
    let mut client = fixture
        .schedule(&["sh", "-c", "touch \"$REEF_READY\"; sleep 30"])
        .env("REEF_READY", &ready)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    Fixture::wait_until(|| ready.exists());
    client.kill().unwrap();
    client.wait().unwrap();
    Fixture::wait_until(|| fixture.jobs().is_empty());
    let output = fixture.schedule(&["sh", "-c", "exit 0"]).output().unwrap();
    assert!(output.status.success());
}

#[test]
fn cancelling_a_running_request_stops_its_process_group() {
    let fixture = Fixture::new();
    let ready = fixture.path.join("started");
    let client = fixture
        .schedule(&["sh", "-c", "touch \"$REEF_READY\"; sleep 30"])
        .env("REEF_READY", &ready)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    Fixture::wait_until(|| ready.exists());
    let id = fixture.jobs()[0]["id"].as_u64().unwrap();
    let output = reef()
        .arg("cancel")
        .arg(id.to_string())
        .arg("--state-dir")
        .arg(&fixture.path)
        .output()
        .unwrap();
    assert!(output.status.success());
    let output = client.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(130),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("\"status\":\"cancelled\""));
    Fixture::wait_until(|| fixture.jobs().is_empty());
}

#[test]
fn restarts_after_abrupt_server_exit() {
    let mut fixture = Fixture::new();
    fixture.server.kill().unwrap();
    fixture.server.wait().unwrap();
    fixture.server = reef()
        .args(["serve", "--cpu", "1", "--memory-mib", "1024", "--state-dir"])
        .arg(&fixture.path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    Fixture::wait_until(|| {
        reef()
            .arg("queue")
            .arg("--state-dir")
            .arg(&fixture.path)
            .output()
            .is_ok_and(|output| output.status.success())
    });
    assert_eq!(fixture.jobs().len(), 0);
}

#[test]
fn server_termination_stops_running_command() {
    let mut fixture = Fixture::new();
    let ready = fixture.path.join("started");
    let mut client = fixture
        .schedule(&["sh", "-c", "touch \"$REEF_READY\"; sleep 30"])
        .env("REEF_READY", &ready)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    Fixture::wait_until(|| ready.exists());
    fixture.server.kill().unwrap();
    fixture.server.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = client.try_wait().unwrap() {
            assert!(!status.success());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "client kept running after scheduler exit"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
