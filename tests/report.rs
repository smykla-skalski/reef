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
            "reef-report-test-{}-{}",
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

fn reef() -> Command {
    Command::new(env!("CARGO_BIN_EXE_reef"))
}

#[test]
fn reports_empty_range_with_unavailable_command_measurements() {
    let fixture = Fixture::new();
    let output = reef()
        .args([
            "report",
            "--from",
            "2026-01-01T00:00:00Z",
            "--to",
            "2026-01-02T00:00:00Z",
            "--state-dir",
        ])
        .arg(&fixture.0)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["empty"], true);
    assert_eq!(report["observation_count"], 0);
    assert_eq!(report["command_measurements_available"], false);
    assert!(report["categories"].is_null());
    assert!(report["pressure"]["any_above_ms"].is_null());
}

#[test]
fn reads_records_and_never_prints_private_identity() {
    let fixture = Fixture::new();
    let path = fixture.0.join("measurements.jsonl");
    fs::write(
        &path,
        "{\"category\":\"build\",\"identity\":\"private-project\",\"status\":\"success\",\"started_at_unix_ms\":1767225600000,\"ended_at_unix_ms\":1767225601000,\"wall_ms\":1000,\"tree_cpu_ms\":500,\"tree_peak_memory_bytes\":4096}\n",
    )
    .unwrap();
    let output = reef()
        .args([
            "report",
            "--from",
            "2026-01-01T00:00:00Z",
            "--to",
            "2026-01-02T00:00:00Z",
            "--state-dir",
        ])
        .arg(&fixture.0)
        .arg("--records")
        .arg(&path)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["command_count"], 1);
    assert_eq!(report["categories"][0]["category"], "build");
    assert_eq!(report["categories"][0]["cpu_ms"], 500);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private-project"));
}

#[test]
fn rejects_malformed_records_without_partial_report() {
    let fixture = Fixture::new();
    let path = fixture.0.join("measurements.jsonl");
    fs::write(&path, "{\"category\":\"build\"\n").unwrap();
    let output = reef()
        .args([
            "report",
            "--from",
            "2026-01-01T00:00:00Z",
            "--to",
            "2026-01-02T00:00:00Z",
            "--state-dir",
        ])
        .arg(&fixture.0)
        .arg("--records")
        .arg(&path)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(output.stdout, Vec::<u8>::new());
    assert!(String::from_utf8_lossy(&output.stderr).contains("measurements.jsonl:1"));
}

#[test]
fn repeated_record_path_does_not_count_a_command_twice() {
    let fixture = Fixture::new();
    let path = fixture.0.join("measurements.jsonl");
    fs::write(
        &path,
        "{\"category\":\"build\",\"status\":\"success\",\"started_at_unix_ms\":1767225600000,\"ended_at_unix_ms\":1767225601000,\"wall_ms\":1000,\"tree_cpu_ms\":500,\"tree_peak_memory_bytes\":4096}\n",
    )
    .unwrap();
    let output = reef()
        .args([
            "report",
            "--from",
            "2026-01-01T00:00:00Z",
            "--to",
            "2026-01-01T00:00:02Z",
            "--state-dir",
        ])
        .arg(&fixture.0)
        .arg("--records")
        .arg(&path)
        .arg("--records")
        .arg(&path)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["command_count"], 1);
    assert_eq!(report["categories"][0]["wall_ms"], 1000);
    assert_eq!(report["peak_command_concurrency"], 1);
}
