#![cfg(unix)]

use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("reef-shadow-test-{}", std::process::id()));
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
fn shadow_help_builds_without_argument_group_collision() {
    let output = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["shadow", "--help"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("--cpu-high-percent"));
}

#[test]
fn shadow_reads_private_samples_without_starting_scheduler() {
    let fixture = Fixture::new();
    fs::write(
        fixture.0.join("samples-test.jsonl"),
        "{\"at_unix_ms\":1767225601000,\"working_ms\":1000,\"cpu_percent\":10.0,\"memory_used_bytes\":4294967296,\"memory_total_bytes\":17179869184}\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args([
            "shadow",
            "--from",
            "2026-01-01T00:00:00Z",
            "--to",
            "2026-01-01T00:00:03Z",
            "--state-dir",
        ])
        .arg(&fixture.0)
        .args(["--cpu-cores", "8", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let replay: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(replay["all_samples"]["samples"], 1);
    assert_eq!(replay["all_samples"]["would_admit"], 1);
    assert!(
        replay["interpretation"]
            .as_str()
            .unwrap()
            .contains("No command interception")
    );
}
