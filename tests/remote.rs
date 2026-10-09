#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "reef-remote-cli-{}-{nonce:x}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn worker(root: &PathBuf, job: &str, action: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["remote-worker", "--root"])
        .arg(root)
        .args(["--job", job, "--action", action])
        .output()
        .unwrap()
}

fn repository(fixture: &Fixture) -> PathBuf {
    let repo = fixture.0.join("repo");
    fs::create_dir(&repo).unwrap();
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success()
    );
    fs::write(repo.join("file"), "source\n").unwrap();
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["add", "file"])
            .status()
            .unwrap()
            .success()
    );
    repo
}

fn policy_file(
    fixture: &Fixture,
    command: &[&str],
    fallback: bool,
    identity_file: &PathBuf,
) -> PathBuf {
    let known_hosts = fixture.0.join("known_hosts");
    fs::write(&known_hosts, "worker.example ssh-ed25519 AAAA\n").unwrap();
    fs::set_permissions(&known_hosts, fs::Permissions::from_mode(0o600)).unwrap();
    let policy = fixture.0.join("policy.json");
    let body = serde_json::json!({
        "enabled": true,
        "target": "worker@example.test",
        "identity_file": identity_file,
        "known_hosts_file": known_hosts,
        "worker_root": fixture.0.join("worker"),
        "worker_binary": env!("CARGO_BIN_EXE_reef"),
        "repository": "repo",
        "toolchain": "rustc",
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "allowed_commands": [{"identity": "job", "argv": command}],
        "local_fallback": fallback
    });
    fs::write(&policy, serde_json::to_vec(&body).unwrap()).unwrap();
    fs::set_permissions(&policy, fs::Permissions::from_mode(0o600)).unwrap();
    policy
}

#[test]
fn disabled_policy_refuses_remote_submission_without_a_connection() {
    let fixture = Fixture::new();
    let policy = fixture.0.join("policy.json");
    fs::write(&policy, r#"{"enabled":false,"target":"worker@example.test","identity_file":"/missing","known_hosts_file":"/missing","worker_root":"/missing","repository":"repo","toolchain":"rustc","os":"linux","arch":"x86_64","allowed_commands":[]}"#).unwrap();
    fs::set_permissions(&policy, fs::Permissions::from_mode(0o600)).unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["remote", "--config"])
        .arg(&policy)
        .args([
            "--repo",
            "/missing",
            "--approve-snapshot",
            "--",
            "/bin/true",
        ])
        .output()
        .unwrap();

    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("remote offload is disabled"));
}

#[test]
fn restrictive_umask_allows_snapshot_before_missing_credentials() {
    let fixture = Fixture::new();
    let repo = repository(&fixture);
    fs::create_dir(repo.join("nested")).unwrap();
    fs::write(repo.join("nested/file"), "nested source\n").unwrap();
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["add", "nested/file"])
            .status()
            .unwrap()
            .success()
    );
    let identity = fixture.0.join("missing-identity");
    let policy = policy_file(&fixture, &["/usr/bin/true"], false, &identity);

    let result = Command::new("/bin/sh")
        .args(["-c", "umask 0700; exec \"$@\"", "sh"])
        .arg(env!("CARGO_BIN_EXE_reef"))
        .args(["remote", "--config"])
        .arg(&policy)
        .args(["--repo"])
        .arg(&repo)
        .args([
            "--approve-snapshot",
            "--identity",
            "job",
            "--",
            "/usr/bin/true",
        ])
        .output()
        .unwrap();

    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("SSH identity file"),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn local_worker_cancel_confirms_termination_and_prevents_cleanup_while_running() {
    let fixture = Fixture::new();
    let root = fixture.0.join("worker");
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(worker(&root, "abc123", "prepare").status.success());
    let manifest = format!(
        "{{\"repository\":\"repo\",\"toolchain\":\"rustc\",\"os\":\"{}\",\"arch\":\"{}\",\"command\":[\"/bin/sh\",\"-c\",\"sleep 5\"],\"files\":[]}}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    fs::write(root.join("jobs/abc123/job.json"), manifest).unwrap();
    assert!(worker(&root, "abc123", "verify").status.success());
    let mut executing = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["remote-worker", "--root"])
        .arg(&root)
        .args(["--job", "abc123", "--action", "execute"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let status_path = root.join("jobs/abc123/status");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !status_path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(status_path.exists());

    let premature_cleanup = worker(&root, "abc123", "cleanup");
    let cancelled = worker(&root, "abc123", "cancel");
    let exit = executing.wait().unwrap();

    assert!(!premature_cleanup.status.success());
    assert!(
        cancelled.status.success(),
        "{}",
        String::from_utf8_lossy(&cancelled.stderr)
    );
    assert_eq!(exit.code(), Some(130));
    assert_eq!(fs::read_to_string(status_path).unwrap(), "cancelled\n");
    assert!(worker(&root, "abc123", "cleanup").status.success());
    assert!(!root.join("jobs/abc123").exists());
}

#[test]
fn cache_setup_failure_keeps_remote_job_cleanable() {
    let fixture = Fixture::new();
    let root = fixture.0.join("worker");
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(worker(&root, "abc123", "prepare").status.success());
    let manifest = format!(
        "{{\"repository\":\"repo\",\"toolchain\":\"rustc\",\"os\":\"{}\",\"arch\":\"{}\",\"command\":[\"/bin/true\"],\"files\":[]}}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    fs::write(root.join("jobs/abc123/job.json"), manifest).unwrap();
    fs::write(root.join("cache"), "not a directory").unwrap();

    let execute = worker(&root, "abc123", "execute");

    assert!(!execute.status.success());
    assert_eq!(
        fs::read_to_string(root.join("jobs/abc123/status")).unwrap(),
        "failed 126\n"
    );
    assert!(worker(&root, "abc123", "cleanup").status.success());
    assert!(!root.join("jobs/abc123").exists());
}

#[test]
fn missing_identity_uses_fresh_local_scheduler_admission() {
    let fixture = Fixture::new();
    let repo = repository(&fixture);
    let identity = fixture.0.join("missing-identity");
    let policy = policy_file(&fixture, &["/usr/bin/true"], true, &identity);
    let state = PathBuf::from(format!(
        "/tmp/reef-sched-{}-{}",
        std::process::id(),
        NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut server = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args([
            "serve",
            "--no-pressure",
            "--cpu",
            "1",
            "--memory-mib",
            "1024",
            "--state-dir",
        ])
        .arg(&state)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !state.join("reef.sock").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let result = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["remote", "--config"])
        .arg(&policy)
        .arg("--repo")
        .arg(&repo)
        .arg("--state-dir")
        .arg(&state)
        .args([
            "--approve-snapshot",
            "--identity",
            "job",
            "--",
            "/usr/bin/true",
        ])
        .env("HOME", &fixture.0)
        .output()
        .unwrap();
    server.kill().unwrap();
    let _ = server.wait();
    fs::remove_dir_all(&state).unwrap();

    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stderr).contains("SSH identity file"));
    assert!(String::from_utf8_lossy(&result.stderr).contains("fresh local scheduler admission"));
    let history = fixture.0.join(".local/state/reef/history");
    let record = fs::read_dir(history)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let saved: serde_json::Value = serde_json::from_slice(&fs::read(record).unwrap()).unwrap();
    assert_eq!(saved["identity"], "job");
    assert_eq!(saved["status"], "success");
}

#[test]
fn local_transport_preserves_remote_exit_255_and_cleans_source() {
    let fixture = Fixture::new();
    let repo = repository(&fixture);
    let root = fixture.0.join("worker");
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let identity = fixture.0.join("identity");
    fs::write(&identity, "test key").unwrap();
    fs::set_permissions(&identity, fs::Permissions::from_mode(0o600)).unwrap();
    let policy = policy_file(&fixture, &["/bin/sh", "-c", "exit 255"], false, &identity);
    let ssh = fixture.0.join("ssh");
    fs::write(
        &ssh,
        "#!/bin/sh\nfor argument do remote=$argument; done\nexec /bin/sh -c \"$remote\"\n",
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let rsync = fixture.0.join("rsync");
    fs::write(&rsync, "#!/bin/sh\nfor argument do previous=${last-}; last=$argument; done\ndestination=${last#*:}\ncase $previous in\n  */) /bin/mkdir -p \"$destination\"; /bin/cp -R \"$previous.\" \"$destination\";;\n  *) /bin/cp \"$previous\" \"$destination\";;\nesac\n").unwrap();
    fs::set_permissions(&rsync, fs::Permissions::from_mode(0o700)).unwrap();
    let path = format!("{}:{}", fixture.0.display(), std::env::var("PATH").unwrap());

    let result = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["remote", "--config"])
        .arg(&policy)
        .arg("--repo")
        .arg(&repo)
        .args([
            "--approve-snapshot",
            "--identity",
            "job",
            "--",
            "/bin/sh",
            "-c",
            "exit 255",
        ])
        .env("PATH", path)
        .output()
        .unwrap();

    assert_eq!(
        result.status.code(),
        Some(255),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(fs::read_dir(root.join("jobs")).unwrap().count(), 0);
}
