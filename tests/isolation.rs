#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "reef-isolation-test-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn strong_isolation_never_starts_a_command_without_a_backend() {
    let fixture = Fixture::new();

    for subcommand in ["run", "schedule"] {
        let marker = fixture.0.join(subcommand);
        let output = Command::new(env!("CARGO_BIN_EXE_reef"))
            .arg(subcommand)
            .args(["--isolation", "strong", "--no-record", "--", "sh", "-c"])
            .arg("touch \"$REEF_MARKER\"")
            .env("REEF_MARKER", &marker)
            .env("HOME", &fixture.0)
            .output()
            .unwrap();

        assert!(!output.status.success(), "{subcommand} started a command");
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("strong isolation is not available; command was not started"),
            "{subcommand}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!marker.exists(), "{subcommand} started a command");
    }
}

#[test]
fn explicit_host_isolation_keeps_existing_run_behavior() {
    let output = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args([
            "run",
            "--isolation",
            "host",
            "--no-record",
            "--",
            "sh",
            "-c",
            "exit 7",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(7));
}
