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
            "reef-compare-test-{}-{}",
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

fn args() -> [&'static str; 11] {
    [
        "compare",
        "--baseline-from",
        "2026-01-01T00:00:00Z",
        "--baseline-to",
        "2026-01-01T01:00:00Z",
        "--comparison-from",
        "2026-01-01T01:00:00Z",
        "--comparison-to",
        "2026-01-01T02:00:00Z",
        "--swap-threshold",
        "90",
    ]
}

#[test]
fn comparison_normalizes_pressure_and_counts_completed_commands() {
    let fixture = Fixture::new();
    fs::write(
        fixture.0.join("samples-1.jsonl"),
        concat!(
            "{\"at_unix_ms\":1767227400000,\"working_ms\":1800000,\"cpu_percent\":95,\"memory_used_bytes\":900,\"memory_total_bytes\":1000,\"swap_used_bytes\":100,\"swap_total_bytes\":1000}\n",
            "{\"at_unix_ms\":1767229200000,\"working_ms\":1800000,\"cpu_percent\":10,\"memory_used_bytes\":100,\"memory_total_bytes\":1000,\"swap_used_bytes\":200,\"swap_total_bytes\":1000}\n",
            "{\"at_unix_ms\":1767231000000,\"working_ms\":1800000,\"cpu_percent\":10,\"memory_used_bytes\":100,\"memory_total_bytes\":1000,\"swap_used_bytes\":250,\"swap_total_bytes\":1000}\n",
            "{\"at_unix_ms\":1767232800000,\"working_ms\":1800000,\"cpu_percent\":10,\"memory_used_bytes\":100,\"memory_total_bytes\":1000,\"swap_used_bytes\":300,\"swap_total_bytes\":1000}\n",
        ),
    )
    .unwrap();
    let records = fixture.0.join("commands.jsonl");
    fs::write(
        &records,
        concat!(
            "{\"category\":\"build\",\"status\":\"success\",\"started_at_unix_ms\":1767226500000,\"ended_at_unix_ms\":1767228000000,\"wall_ms\":1500000,\"tree_cpu_ms\":500000,\"tree_peak_memory_bytes\":1,\"identity\":\"private-project\"}\n",
            "{\"category\":\"test\",\"status\":\"failed\",\"started_at_unix_ms\":1767230100000,\"ended_at_unix_ms\":1767231900000,\"wall_ms\":1800000,\"tree_cpu_ms\":1000000,\"tree_peak_memory_bytes\":1}\n",
        ),
    )
    .unwrap();
    let output = reef()
        .args(args())
        .arg("--state-dir")
        .arg(&fixture.0)
        .arg("--records")
        .arg(&records)
        .args(["--format", "json"])
        .env("HOME", &fixture.0)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let comparison: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(comparison["interpretation"], "observational_not_causal");
    assert_eq!(comparison["baseline"]["observed_working_ms"], 3_600_000);
    assert_eq!(
        comparison["baseline"]["pressure_ms_per_observed_hour"]["cpu"],
        1_800_000
    );
    assert_eq!(
        comparison["baseline"]["pressure_without_measured_command_overlap_percent"],
        50.0
    );
    assert_eq!(comparison["baseline"]["swap_growth_bytes"], 100);
    assert_eq!(comparison["baseline"]["completed_command_count"], 1);
    assert_eq!(
        comparison["baseline"]["command_categories"][0]["wall_ms"],
        1_500_000
    );
    assert_eq!(comparison["comparison"]["completed_command_count"], 1);
    assert_eq!(
        comparison["comparison"]["pressure_ms_per_observed_hour"]["cpu"],
        0
    );
    assert!(
        comparison["comparison"]["pressure_without_measured_command_overlap_percent"].is_null()
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private-project"));
}

#[test]
fn missing_data_stays_unavailable_and_markdown_does_not_claim_causation() {
    let fixture = Fixture::new();
    let output = reef()
        .args(args())
        .arg("--state-dir")
        .arg(&fixture.0)
        .args(["--format", "json"])
        .env("HOME", &fixture.0)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let comparison: Value = serde_json::from_slice(&output.stdout).unwrap();
    for period in ["baseline", "comparison"] {
        assert_eq!(comparison[period]["observation_count"], 0);
        assert!(comparison[period]["observed_working_ms"].is_null());
        assert!(comparison[period]["pressure_ms_per_observed_hour"]["cpu"].is_null());
        assert!(comparison[period]["completed_command_count"].is_null());
        assert!(comparison[period]["pressure_without_measured_command_overlap_percent"].is_null());
    }
    let markdown = reef()
        .args(args())
        .arg("--state-dir")
        .arg(&fixture.0)
        .env("HOME", &fixture.0)
        .output()
        .unwrap();
    assert!(markdown.status.success());
    let text = String::from_utf8_lossy(&markdown.stdout);
    assert!(text.contains("Observational difference only"));
    assert!(text.contains("unavailable"));

    fs::write(
        fixture.0.join("samples-1.jsonl"),
        "{\"at_unix_ms\":1767231000000,\"working_ms\":1800000,\"cpu_percent\":0,\"memory_used_bytes\":0,\"memory_total_bytes\":1000,\"swap_used_bytes\":0,\"swap_total_bytes\":1000}\n",
    )
    .unwrap();
    let records = fixture.0.join("empty.jsonl");
    fs::write(&records, "").unwrap();
    let output = reef()
        .args(args())
        .arg("--state-dir")
        .arg(&fixture.0)
        .arg("--records")
        .arg(&records)
        .args(["--format", "json"])
        .env("HOME", &fixture.0)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let comparison: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(comparison["baseline"]["pressure_ms_per_observed_hour"]["cpu"].is_null());
    assert_eq!(
        comparison["comparison"]["pressure_ms_per_observed_hour"]["cpu"],
        0
    );
    assert_eq!(comparison["baseline"]["completed_command_count"], 0);
    assert_eq!(comparison["comparison"]["completed_command_count"], 0);
}

#[test]
fn rejects_overlapping_periods_before_reading_data() {
    let fixture = Fixture::new();
    let output = reef()
        .args([
            "compare",
            "--baseline-from",
            "2026-01-01T00:00:00Z",
            "--baseline-to",
            "2026-01-01T01:01:00Z",
            "--comparison-from",
            "2026-01-01T01:00:00Z",
            "--comparison-to",
            "2026-01-01T02:00:00Z",
        ])
        .arg("--state-dir")
        .arg(&fixture.0)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(output.stdout, Vec::<u8>::new());
    assert!(String::from_utf8_lossy(&output.stderr).contains("non-overlapping"));
}
