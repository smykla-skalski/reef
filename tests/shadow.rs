#![cfg(unix)]

use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "reef-shadow-test-{}-{}",
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

#[test]
fn shadow_rejects_coarse_history_as_admission_input() {
    let fixture = Fixture::new();
    fs::write(
        fixture.0.join("host-5m-1767225600000-1.jsonl"),
        "{\"bucket_start_unix_ms\":1767225600000,\"resolution_ms\":300000,\"source_samples\":2,\"observed_working_ms\":120000,\"cpu_percent\":{\"weighted_sum\":6000000.0,\"weight_ms\":120000,\"peak\":95.0},\"memory_used_bytes\":{\"weighted_sum\":515396075520000,\"weight_ms\":120000,\"peak\":4294967296},\"memory_total_bytes\":{\"weighted_sum\":2061584302080000,\"weight_ms\":120000,\"peak\":17179869184}}\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args([
            "shadow",
            "--from",
            "2026-01-01T00:00:00Z",
            "--to",
            "2026-01-01T00:05:01Z",
            "--state-dir",
        ])
        .arg(&fixture.0)
        .args(["--cpu-cores", "8", "--format", "json"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("coarse history is not exact admission input")
    );
}
