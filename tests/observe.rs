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
    Recorder(
        Command::new(env!("CARGO_BIN_EXE_reef"))
            .args(["observe", "run", "--interval-seconds", "1", "--state-dir"])
            .arg(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    )
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
