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
        .env("HOME", &fixture.0)
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["empty"], true);
    assert_eq!(report["observation_count"], 0);
    assert_eq!(report["command_measurements_available"], false);
    assert!(report["scheduler"].is_null());
    assert!(report["categories"].is_null());
    assert!(report["pressure"]["any_above_ms"].is_null());
}

#[test]
fn html_report_contains_host_charts_and_sanitized_agent_cost() {
    let fixture = Fixture::new();
    fs::write(
        fixture.0.join("samples-test.jsonl"),
        concat!(
            "{\"at_unix_ms\":1767225601000,\"working_ms\":1000,\"cpu_percent\":95.0,\"memory_used_bytes\":900,\"memory_total_bytes\":1000,\"swap_used_bytes\":1073741824,\"swap_total_bytes\":2147483648}\n",
            "{\"at_unix_ms\":1767225602000,\"working_ms\":1000,\"cpu_percent\":50.0,\"memory_used_bytes\":500,\"memory_total_bytes\":1000,\"swap_used_bytes\":536870912,\"swap_total_bytes\":2147483648}\n",
        ),
    )
    .unwrap();
    fs::write(
        fixture.0.join("agent-samples-test.jsonl"),
        "{\"at_unix_ms\":1767225601000,\"working_ms\":1000,\"agents\":[],\"commands\":[{\"kind\":\"codex\",\"family\":\"<script>alert(1)</script>\",\"category\":\"other\",\"process_count\":1,\"cpu_percent\":50.0,\"memory_bytes\":1024,\"read_bytes\":10,\"written_bytes\":20}]}\n",
    )
    .unwrap();
    let output = reef()
        .args([
            "report",
            "--from",
            "2026-01-01T00:00:00Z",
            "--to",
            "2026-01-01T00:00:03Z",
            "--state-dir",
        ])
        .arg(&fixture.0)
        .args(["--format", "html"])
        .env("HOME", &fixture.0)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let page = String::from_utf8(output.stdout).unwrap();
    assert!(page.starts_with("<!doctype html>"));
    assert!(page.contains("<svg"));
    assert!(page.contains("CPU usage"));
    assert!(page.contains("Memory usage"));
    assert!(page.contains("Swap used"));
    assert!(page.contains("Agent command cost"));
    assert!(page.contains("codex"));
    assert!(!page.contains("<script>alert(1)</script>"));
    assert!(!page.contains("alert(1)"));
    assert!(!page.contains("https://"));
}

#[test]
fn html_output_is_private_and_never_overwrites_an_existing_file() {
    let fixture = Fixture::new();
    let path = fixture.0.join("report.html");
    let mut command = reef();
    command.args(["report", "--state-dir"]).arg(&fixture.0);
    command.args(["--format", "html", "--output"]).arg(&path);
    command.env("HOME", &fixture.0);
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        path.to_str().unwrap()
    );
    let original = fs::read(&path).unwrap();
    assert!(original.starts_with(b"<!doctype html>"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let second = reef()
        .args(["report", "--state-dir"])
        .arg(&fixture.0)
        .args(["--format", "html", "--output"])
        .arg(&path)
        .env("HOME", &fixture.0)
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert_eq!(fs::read(&path).unwrap(), original);
}

#[test]
fn browser_opening_requires_an_html_output_file() {
    let fixture = Fixture::new();
    let without_output = reef()
        .args(["report", "--format", "html", "--open"])
        .env("HOME", &fixture.0)
        .output()
        .unwrap();
    assert!(!without_output.status.success());
    let wrong_format = reef()
        .args(["report", "--output"])
        .arg(fixture.0.join("report.html"))
        .env("HOME", &fixture.0)
        .output()
        .unwrap();
    assert!(!wrong_format.status.success());
    assert!(!fixture.0.join("report.html").exists());
}

#[test]
fn timeline_requires_explicit_flag_in_both_formats() {
    let fixture = Fixture::new();
    fs::write(
        fixture.0.join("samples-test.jsonl"),
        "{\"at_unix_ms\":1767225601000,\"working_ms\":1000,\"cpu_percent\":95.0,\"memory_used_bytes\":900,\"memory_total_bytes\":1000,\"swap_used_bytes\":10,\"swap_total_bytes\":1000}\n",
    )
    .unwrap();
    let args = [
        "report",
        "--from",
        "2026-01-01T00:00:00Z",
        "--to",
        "2026-01-01T00:00:02Z",
        "--state-dir",
    ];
    let overview = reef()
        .args(args)
        .arg(&fixture.0)
        .env("HOME", &fixture.0)
        .output()
        .unwrap();
    assert!(overview.status.success(), "{overview:?}");
    let overview = String::from_utf8(overview.stdout).unwrap();
    assert!(overview.contains("## Pressure"));
    assert!(!overview.contains("## Pressure timeline"));
    assert!(!overview.contains("1767225600000..1767225601000"));

    let detailed = reef()
        .args(args)
        .arg(&fixture.0)
        .arg("--timeline")
        .env("HOME", &fixture.0)
        .output()
        .unwrap();
    assert!(detailed.status.success(), "{detailed:?}");
    assert!(String::from_utf8_lossy(&detailed.stdout).contains("## Pressure timeline"));

    let json = reef()
        .args(args)
        .arg(&fixture.0)
        .args(["--format", "json"])
        .env("HOME", &fixture.0)
        .output()
        .unwrap();
    assert!(json.status.success(), "{json:?}");
    let json: Value = serde_json::from_slice(&json.stdout).unwrap();
    assert!(json.get("timeline").is_none());
    assert_eq!(
        json["pressure_overlap"]["no_measured_command_ms"],
        Value::Null
    );

    let detailed_json = reef()
        .args(args)
        .arg(&fixture.0)
        .args(["--format", "json", "--timeline"])
        .env("HOME", &fixture.0)
        .output()
        .unwrap();
    assert!(detailed_json.status.success(), "{detailed_json:?}");
    let detailed_json: Value = serde_json::from_slice(&detailed_json.stdout).unwrap();
    assert_eq!(detailed_json["timeline"].as_array().unwrap().len(), 1);
}

#[test]
fn overview_shows_agent_host_samples_and_readable_memory() {
    let fixture = Fixture::new();
    fs::write(
        fixture.0.join("samples-test.jsonl"),
        concat!(
            "{\"at_unix_ms\":1767225601000,\"working_ms\":1000,\"memory_used_bytes\":3221225472,\"swap_used_bytes\":2147483648,\"agents\":[{\"kind\":\"codex\",\"root_pid\":42,\"process_count\":1,\"memory_bytes\":1073741824,\"read_bytes\":2147483648,\"written_bytes\":2048}]}\n",
            "{\"at_unix_ms\":1767225602000,\"working_ms\":1000,\"memory_used_bytes\":4294967296,\"swap_used_bytes\":1073741824,\"agents\":[]}\n",
        ),
    )
    .unwrap();
    let records = fixture.0.join("measurements.jsonl");
    fs::write(&records, "{\"category\":\"build\",\"status\":\"success\",\"started_at_unix_ms\":1767225600000,\"ended_at_unix_ms\":1767225601000,\"wall_ms\":1000,\"tree_cpu_ms\":500,\"tree_peak_memory_bytes\":3145728}\n").unwrap();
    let args = [
        "report",
        "--from",
        "2026-01-01T00:00:00Z",
        "--to",
        "2026-01-01T00:00:03Z",
        "--state-dir",
    ];
    let markdown = reef()
        .args(args)
        .arg(&fixture.0)
        .arg("--records")
        .arg(&records)
        .env("HOME", &fixture.0)
        .output()
        .unwrap();
    assert!(markdown.status.success(), "{markdown:?}");
    let markdown = String::from_utf8(markdown.stdout).unwrap();
    assert!(markdown.contains("Host observations with agents: 1"));
    assert!(!markdown.contains("Agent observations:"));
    assert!(!markdown.contains("Agent rollups:"));
    assert!(markdown.contains("Peak host memory: 4.0 GiB"));
    assert!(markdown.contains("Swap growth: -1.0 GiB"));
    assert!(markdown.contains("| build | 1 | 0 | 1000 | 100.0 | 500 | 100.0 | 3.0 MiB |"));
    assert!(
        markdown.contains("| codex | 1 | 0 | 0.0 | 1.0 GiB | 2.0 GiB | 2.0 KiB | 1 | 1 |"),
        "{markdown}"
    );

    let json = reef()
        .args(args)
        .arg(&fixture.0)
        .arg("--records")
        .arg(&records)
        .args(["--format", "json"])
        .env("HOME", &fixture.0)
        .output()
        .unwrap();
    assert!(json.status.success(), "{json:?}");
    let json: Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(json["host_observations_with_agents"], 1);
    assert_eq!(json["agent_observation_count"], 0);
    assert_eq!(json["agent_rollup_count"], 0);
    assert_eq!(json["peak_host_memory_bytes"], 4_294_967_296_u64);
    assert_eq!(json["swap_growth_bytes"], -1_073_741_824_i64);
    assert_eq!(json["categories"][0]["peak_memory_bytes"], 3_145_728);
}

#[cfg(windows)]
#[test]
fn windows_reports_missing_scheduler_history_as_unavailable() {
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
        .arg("--schedule-state-dir")
        .arg(fixture.0.join("never-started"))
        .args(["--format", "json"])
        .env("HOME", &fixture.0)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report["scheduler"].is_null());
}

#[test]
fn explicit_records_work_without_a_home_directory() {
    let fixture = Fixture::new();
    let path = fixture.0.join("measurements.jsonl");
    fs::write(&path, "{\"category\":\"build\",\"status\":\"success\",\"started_at_unix_ms\":1767225600000,\"ended_at_unix_ms\":1767225601000,\"wall_ms\":1000,\"tree_cpu_ms\":500,\"tree_peak_memory_bytes\":4096}\n").unwrap();
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
        .env_remove("HOME")
        .env_remove("USERPROFILE")
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["command_count"], 1);
    assert_eq!(report["categories"][0]["category"], "build");
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
        .env("HOME", &fixture.0)
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
        .env("HOME", &fixture.0)
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
        .env("HOME", &fixture.0)
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["command_count"], 1);
    assert_eq!(report["categories"][0]["wall_ms"], 1000);
    assert_eq!(report["peak_command_concurrency"], 1);
}

#[cfg(unix)]
#[test]
fn report_reads_default_history_without_records_flag() {
    let fixture = Fixture::new();
    let run = reef()
        .args([
            "run",
            "--category",
            "build",
            "--identity",
            "private-project",
            "--",
            "sh",
            "-c",
            "exit 7",
        ])
        .env("HOME", &fixture.0)
        .env("REEF_AGENT_KIND", "codex")
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(7));

    let output = reef()
        .args([
            "report",
            "--from",
            "2020-01-01T00:00:00Z",
            "--to",
            "2100-01-01T00:00:00Z",
            "--state-dir",
        ])
        .arg(&fixture.0)
        .args(["--format", "json"])
        .env("HOME", &fixture.0)
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["command_measurements_available"], true);
    assert_eq!(report["command_count"], 1);
    assert_eq!(report["categories"][0]["category"], "build");
    assert_eq!(report["agent_commands"][0]["agent"], "codex");
    assert_eq!(report["agent_commands"][0]["category"], "build");
    assert_eq!(report["categories"][0]["failed_or_cancelled"], 1);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private-project"));
}
