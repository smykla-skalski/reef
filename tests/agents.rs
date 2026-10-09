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
        write_script(
            &bin.join("go"),
            "#!/bin/sh\nprintf '%s|%s|%s\\n' \"$REEF_ADMITTED\" \"$REEF_AGENT_KIND\" \"$*\" >> \"$REEF_TEST_OUTPUT\"\nif [ \"$REEF_TEST_NESTED_ENABLED\" = 1 ] && [ -z \"$REEF_TEST_NESTED\" ]; then REEF_TEST_NESTED=1 go test nested; fi\nexit 7\n",
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
            wait_until(|| path.join("reef.sock").exists());
            Some(child)
        } else {
            None
        };
        Self { path, server }
    }

    fn launch(&self, agent: &str, args: &[&str]) -> Command {
        let mut command = reef();
        let path = std::env::join_paths(
            std::iter::once(self.path.join("bin"))
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        command
            .args(["agents", "launch", agent, "--state-dir"])
            .arg(&self.path)
            .arg("--")
            .args(args)
            .env("PATH", path)
            .env("HOME", &self.path)
            .env("REEF_TEST_OUTPUT", self.path.join("output"));
        command
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
fn codex_and_claude_route_heavy_commands_and_preserve_status() {
    for agent in ["codex", "claude"] {
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
        let records: Vec<_> = fs::read_dir(history).unwrap().collect();
        assert_eq!(records.len(), 1);
        let record: serde_json::Value =
            serde_json::from_slice(&fs::read(records[0].as_ref().unwrap().path()).unwrap())
                .unwrap();
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
