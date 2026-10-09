#![cfg(unix)]

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "reef-run-test-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn record(&self) -> PathBuf {
        self.0.join("measurements.jsonl")
    }

    fn history(&self) -> PathBuf {
        self.0.join(".local/state/reef/history")
    }

    fn history_records(&self) -> Vec<PathBuf> {
        fs::read_dir(self.history())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "jsonl")
            })
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn reef() -> Command {
    Command::new(env!("CARGO_BIN_EXE_reef"))
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

#[test]
fn default_run_saves_private_measurement_without_command_text() {
    let fixture = Fixture::new();
    let output = reef()
        .args([
            "run",
            "--category",
            "test",
            "--identity",
            "safe-job",
            "--",
            "sh",
            "-c",
            "printf 'child-output\\n'; exit 7",
        ])
        .env("HOME", &fixture.0)
        .env("PRIVATE_TEST_SECRET", "do-not-save-me")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(7));
    assert_eq!(output.stdout, b"child-output\n");
    let paths = fixture.history_records();
    assert_eq!(paths.len(), 1);
    let path = &paths[0];
    let saved = fs::read_to_string(path).unwrap();
    let record: Value = serde_json::from_str(saved.trim()).unwrap();
    assert_eq!(record["category"], "test");
    assert_eq!(record["identity"], "safe-job");
    assert_eq!(record["status"], "failed");
    assert_eq!(record["exit_code"], 7);
    assert!(record["started_at_unix_ms"].as_u64().is_some());
    assert!(record["ended_at_unix_ms"].as_u64().is_some());
    assert!(record["tree_cpu_ms"].as_u64().is_some());
    assert!(!saved.contains("child-output"));
    assert!(!saved.contains("do-not-save-me"));
    assert!(!saved.contains("printf"));
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(fixture.history())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}

#[test]
fn no_record_disables_default_history_for_one_run() {
    let fixture = Fixture::new();
    let output = reef()
        .args(["run", "--no-record", "--", "sh", "-c", "exit 7"])
        .env("HOME", &fixture.0)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(7));
    assert!(!fixture.history().exists());
}

#[test]
fn custom_record_does_not_duplicate_into_default_history() {
    let fixture = Fixture::new();
    let output = reef()
        .args(["run", "--record"])
        .arg(fixture.record())
        .args(["--", "sh", "-c", "exit 7"])
        .env("HOME", &fixture.0)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(7));
    assert!(fixture.record().exists());
    assert!(!fixture.history().exists());
}

#[test]
fn history_write_failure_warns_without_changing_child_result() {
    let fixture = Fixture::new();
    let home = fixture.0.join("not-a-directory");
    fs::write(&home, "occupied").unwrap();
    let output = reef()
        .args(["run", "--", "sh", "-c", "printf 'child-output\\n'; exit 7"])
        .env("HOME", home)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(7));
    assert_eq!(output.stdout, b"child-output\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot save measurement"));
}

#[test]
fn symlinked_history_is_rejected_without_writing_outside_reef() {
    let fixture = Fixture::new();
    let parent = fixture.0.join(".local/state/reef");
    let outside = fixture.0.join("outside");
    fs::create_dir_all(&parent).unwrap();
    fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, parent.join("history")).unwrap();
    let output = reef()
        .args(["run", "--", "sh", "-c", "exit 7"])
        .env("HOME", &fixture.0)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(7));
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot save measurement"));
    assert_eq!(fs::read_dir(outside).unwrap().count(), 0);
}

#[test]
fn pruning_keeps_user_file_that_resembles_history() {
    let fixture = Fixture::new();
    let first = reef()
        .args(["run", "--", "/usr/bin/true"])
        .env("HOME", &fixture.0)
        .output()
        .unwrap();
    assert!(first.status.success());
    let user_file = fixture.history().join("command-1-1-1.jsonl");
    fs::write(&user_file, "personal data").unwrap();
    fs::set_permissions(&user_file, fs::Permissions::from_mode(0o600)).unwrap();

    let second = reef()
        .args(["run", "--", "/usr/bin/true"])
        .env("HOME", &fixture.0)
        .output()
        .unwrap();

    assert!(second.status.success());
    assert_eq!(fs::read(&user_file).unwrap(), b"personal data");
    assert_eq!(fixture.history_records().len(), 3);
}

#[test]
fn default_history_records_cancelled_command() {
    let fixture = Fixture::new();
    let ready = fixture.0.join("ready-default");
    let mut child = reef()
        .args(["run", "--", "sh", "-c", "touch \"$REEF_READY\"; sleep 30"])
        .env("HOME", &fixture.0)
        .env("REEF_READY", &ready)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "command never started");
        std::thread::sleep(Duration::from_millis(10));
    }

    kill(
        Pid::from_raw(i32::try_from(child.id()).unwrap()),
        Signal::SIGTERM,
    )
    .unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(143));
    let path = fixture.history_records().remove(0);
    let record: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(record["status"], "cancelled");
    assert_eq!(record["signal"], 15);
}

#[test]
fn recording_modes_cannot_be_combined() {
    let fixture = Fixture::new();
    let marker = fixture.0.join("not-started");
    let output = reef()
        .args(["run", "--record"])
        .arg(fixture.record())
        .args(["--no-record", "--", "sh", "-c", "touch \"$REEF_MARKER\""])
        .env("REEF_MARKER", &marker)
        .env("HOME", &fixture.0)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(!marker.exists());
    assert!(!fixture.history().exists());
}

#[test]
fn preserves_streams_exit_status_and_private_record() {
    let fixture = Fixture::new();
    let record = fixture.record();
    let mut process = reef();
    process.args([
        "run",
        "--category",
        "test",
        "--identity",
        "sample-1",
        "--record",
    ]);
    process.arg(&record).args([
        "--",
        "sh",
        "-c",
        "read line; printf 'out:%s\\n' \"$line\"; printf 'err\\n' >&2; exit 7",
    ]);
    process
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let before = unix_ms();
    let mut child = process.spawn().unwrap();
    child.stdin.take().unwrap().write_all(b"hello\n").unwrap();
    let output = child.wait_with_output().unwrap();
    let after = unix_ms();
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(output.stdout, b"out:hello\n");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.starts_with("err\nreef: {"));
    let saved = fs::read_to_string(&record).unwrap();
    let result: Value = serde_json::from_str(saved.trim()).unwrap();
    assert_eq!(result["category"], "test");
    assert_eq!(result["identity"], "sample-1");
    assert_eq!(result["status"], "failed");
    assert_eq!(result["exit_code"], 7);
    assert!(result["wall_ms"].as_u64().is_some());
    let started = u128::from(result["started_at_unix_ms"].as_u64().unwrap());
    let ended = u128::from(result["ended_at_unix_ms"].as_u64().unwrap());
    assert!(before <= started && started <= ended && ended <= after);
    assert_eq!(result["tree_usage_complete"], false);
    assert!(result["peak_memory_bytes"].as_u64().unwrap() > 0);
    assert!(!saved.contains("hello"));
    assert!(!saved.contains("read line"));
    assert_eq!(
        fs::metadata(record).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn skips_public_record_but_runs_command() {
    let fixture = Fixture::new();
    let record = fixture.record();
    fs::write(&record, "").unwrap();
    fs::set_permissions(&record, fs::Permissions::from_mode(0o644)).unwrap();
    let marker = fixture.0.join("started");
    let output = reef()
        .args(["run", "--record"])
        .arg(&record)
        .args(["--", "sh", "-c"])
        .arg(format!("touch {}; exit 7", marker.display()))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert!(marker.exists());
    assert_eq!(fs::read(&record).unwrap(), b"");
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot open measurement record"));
}

#[test]
fn runs_command_when_record_directory_is_missing() {
    let fixture = Fixture::new();
    let record = fixture.0.join("missing").join("measurements.jsonl");
    let output = reef()
        .args(["run", "--record"])
        .arg(&record)
        .args(["--", "sh", "-c", "printf 'started\\n'; exit 7"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(output.stdout, b"started\n");
    assert!(!record.exists());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot open measurement record"));
}

#[test]
fn skips_pipe_record_without_waiting_for_a_reader() {
    let fixture = Fixture::new();
    let record = fixture.record();
    assert!(
        Command::new("mkfifo")
            .arg(&record)
            .status()
            .unwrap()
            .success()
    );
    let mut child = reef()
        .args(["run", "--record"])
        .arg(&record)
        .args(["--", "sh", "-c", "exit 7"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("Reef waited for a pipe reader");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status.code(), Some(7));
}

#[test]
fn record_write_failure_does_not_replace_command_status() {
    let fixture = Fixture::new();
    let output = Command::new("sh")
        .arg("-c")
        .arg("ulimit -f 0; trap '' XFSZ; exec \"$REEF_BIN\" run --record \"$REEF_RECORD\" -- sh -c 'exit 7'")
        .env("REEF_BIN", env!("CARGO_BIN_EXE_reef"))
        .env("REEF_RECORD", fixture.record())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot save measurement"));
}

#[test]
fn records_a_command_that_cannot_start() {
    let fixture = Fixture::new();
    let record = fixture.record();
    let output = reef()
        .args(["run", "--record"])
        .arg(&record)
        .args(["--", "reef-command-that-does-not-exist"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(127));
    let result: Value = serde_json::from_str(fs::read_to_string(record).unwrap().trim()).unwrap();
    assert_eq!(result["status"], "failed");
    assert_eq!(result["exit_code"], 127);
    assert_eq!(result["cpu_ms"], 0);
}

#[test]
fn records_a_signal_failure_as_failed() {
    let fixture = Fixture::new();
    let record = fixture.record();
    let output = reef()
        .args(["run", "--record"])
        .arg(&record)
        .args(["--", "sh", "-c", "kill -PIPE $$"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(141));
    let result: Value = serde_json::from_str(fs::read_to_string(record).unwrap().trim()).unwrap();
    assert_eq!(result["status"], "failed");
    assert_eq!(result["signal"], 13);
}

#[test]
fn forwards_interrupt_and_saves_cancelled_measurement() {
    let fixture = Fixture::new();
    let record = fixture.record();
    let ready = fixture.0.join("ready");
    let script = format!("touch {}; sleep 30", ready.display());
    let mut child = reef()
        .args(["run", "--record"])
        .arg(&record)
        .args(["--", "sh", "-c", &script])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "command never started");
        std::thread::sleep(Duration::from_millis(10));
    }
    kill(
        Pid::from_raw(i32::try_from(child.id()).unwrap()),
        Signal::SIGTERM,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    while fs::read_to_string(&record).unwrap().trim().is_empty() {
        assert!(
            Instant::now() < deadline,
            "cancelled measurement was not saved"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let status = child.wait().unwrap();
    assert!(!status.success());
    let result: Value = serde_json::from_str(fs::read_to_string(record).unwrap().trim()).unwrap();
    assert_eq!(result["status"], "cancelled");
    assert_eq!(result["signal"], 15);
}

#[test]
fn forwards_later_interrupt_after_the_first_is_ignored() {
    let fixture = Fixture::new();
    let record = fixture.record();
    let ready = fixture.0.join("ready");
    let mut child = reef()
        .args(["run", "--record"])
        .arg(&record)
        .args([
            "--",
            "sh",
            "-c",
            "trap '' INT; trap 'exit 42' TERM; touch \"$REEF_READY\"; while :; do sleep 1; done",
        ])
        .env("REEF_READY", &ready)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "command never started");
        std::thread::sleep(Duration::from_millis(10));
    }
    let pid = Pid::from_raw(i32::try_from(child.id()).unwrap());
    kill(pid, Signal::SIGINT).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    kill(pid, Signal::SIGTERM).unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(42));
    let result: Value = serde_json::from_str(fs::read_to_string(record).unwrap().trim()).unwrap();
    assert_eq!(result["status"], "cancelled");
    assert_eq!(result["signal"], 15);
}

#[test]
fn cancellation_stops_descendants_that_ignore_the_forwarded_signal() {
    let fixture = Fixture::new();
    let record = fixture.record();
    let pid_file = fixture.0.join("descendant.pid");
    let mut child = reef()
        .args(["run", "--record"])
        .arg(&record)
        .args([
            "--",
            "sh",
            "-c",
            "sh -c 'trap \"\" TERM; echo $$ > \"$REEF_PIDFILE\"; exec sleep 30' & wait",
        ])
        .env("REEF_PIDFILE", &pid_file)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let descendant = loop {
        if let Ok(pid) = fs::read_to_string(&pid_file)
            && let Ok(pid) = pid.trim().parse::<i32>()
        {
            break pid;
        }
        assert!(Instant::now() < deadline, "descendant never started");
        std::thread::sleep(Duration::from_millis(10));
    };
    kill(
        Pid::from_raw(i32::try_from(child.id()).unwrap()),
        Signal::SIGTERM,
    )
    .unwrap();
    let status = child.wait().unwrap();
    let alive = kill(Pid::from_raw(descendant), None).is_ok();
    if alive {
        let _ = kill(Pid::from_raw(descendant), Signal::SIGKILL);
    }
    assert_eq!(status.code(), Some(143));
    assert!(!alive, "descendant survived cancellation");
    let result: Value = serde_json::from_str(fs::read_to_string(record).unwrap().trim()).unwrap();
    assert_eq!(result["status"], "cancelled");
}

#[test]
fn concurrent_runs_append_complete_records() {
    let fixture = Fixture::new();
    let record = fixture.record();
    let children: Vec<_> = (0..24)
        .map(|number| {
            reef()
                .args(["run", "--identity", &format!("job-{number}"), "--record"])
                .arg(&record)
                .args(["--", "sh", "-c", ":"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    let saved = fs::read_to_string(record).unwrap();
    let records: Vec<Value> = saved
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records.len(), 24);
}

#[test]
fn concurrent_default_history_contains_complete_records() {
    let fixture = Fixture::new();
    let children: Vec<_> = (0..16)
        .map(|number| {
            reef()
                .args(["run", "--identity", &format!("job-{number}"), "--"])
                .args(["sh", "-c", ":"])
                .env("HOME", &fixture.0)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }

    let records: Vec<_> = fixture
        .history_records()
        .into_iter()
        .map(|entry| {
            let bytes = fs::read(entry).unwrap();
            serde_json::from_slice::<Value>(&bytes).unwrap()
        })
        .collect();
    assert_eq!(records.len(), 16);
    assert!(records.iter().all(|record| record["status"] == "success"));
}

#[test]
fn forwards_quit_and_records_cancellation() {
    let fixture = Fixture::new();
    let record = fixture.record();
    let ready = fixture.0.join("ready");
    let mut child = reef()
        .args(["run", "--record"])
        .arg(&record)
        .args([
            "--",
            "sh",
            "-c",
            "echo ready > \"$REEF_READY\"; trap 'exit 3' QUIT; while :; do :; done",
        ])
        .env("REEF_READY", &ready)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "command never started");
        std::thread::sleep(Duration::from_millis(10));
    }
    kill(
        Pid::from_raw(i32::try_from(child.id()).unwrap()),
        Signal::SIGQUIT,
    )
    .unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(3));
    let result: Value = serde_json::from_str(fs::read_to_string(record).unwrap().trim()).unwrap();
    assert_eq!(result["status"], "cancelled");
    assert_eq!(result["signal"], 3);
}
