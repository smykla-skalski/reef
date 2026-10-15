#![cfg(unix)]

use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "reef-native-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
    fn import(&self, source: &str, input: &[u8]) -> Output {
        fs::write(self.0.join("input"), input).unwrap();
        Command::new(env!("CARGO_BIN_EXE_reef"))
            .args(["native-cache", "import", "--source", source, "--input"])
            .arg(self.0.join("input"))
            .arg("--state-dir")
            .arg(self.0.join("state"))
            .output()
            .unwrap()
    }
    fn report(&self, format: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_reef"))
            .args([
                "report",
                "--from",
                "2026-01-01T00:00:00Z",
                "--format",
                format,
                "--native-cache-state-dir",
            ])
            .arg(self.0.join("state"))
            .env("HOME", &self.0)
            .output()
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn imports_official_counter_shapes_without_retaining_private_input() {
    let fixture = Fixture::new();
    assert!(fixture.import("ccache", b"direct_cache_hit\t4\npreprocessed_cache_hit\t2\ncache_miss\t3\nstats_zeroed_timestamp\t123\n").status.success());
    assert!(fixture.import("sccache", br#"{"stats":{"cache_hits":{"counts":{"Rust":9},"adv_counts":{"rustc":9}},"cache_misses":{"counts":{"Rust":1}}},"cache_location":"/private/secret","basedirs":["/private/source"]}"#).status.success());
    assert!(fixture.import("go-test", b"private log TOKEN=secret\nok\tprivate/package\t(cached)\nok\tprivate/other\t0.125s\n? private/none [no test files]\n").status.success());

    let output = fixture.report("json");
    assert!(output.status.success(), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let snapshots = &value["native_cache"]["snapshots"];
    assert_eq!(snapshots[0]["hits"], 6);
    assert_eq!(snapshots[0]["misses"], 3);
    assert_eq!(snapshots[1]["hits"], 9);
    assert_eq!(snapshots[2]["cached_test_results"], 1);
    assert_eq!(snapshots[2]["executed_test_results"], 1);
    assert!(snapshots[2]["misses"].is_null());
    let stored = fs::read_to_string(fixture.0.join("state/snapshots.json")).unwrap();
    assert!(!stored.contains("private"));
    assert!(!stored.contains("secret"));
    assert_eq!(
        fs::metadata(fixture.0.join("state/snapshots.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(
        String::from_utf8(fixture.report("markdown").stdout)
            .unwrap()
            .contains("Go build and Node cache metrics are unsupported")
    );
    assert!(
        String::from_utf8(fixture.report("html").stdout)
            .unwrap()
            .contains("Native cache evidence")
    );
}

#[test]
fn decreased_cumulative_counters_flag_reset_without_summing_snapshots() {
    let fixture = Fixture::new();
    assert!(fixture.import("ccache", b"direct_cache_hit 10\npreprocessed_cache_hit 2\ncache_miss 3\nstats_zeroed_timestamp 0\n").status.success());
    assert!(fixture.import("ccache", b"direct_cache_hit 1\npreprocessed_cache_hit 0\ncache_miss 0\nstats_zeroed_timestamp 42\n").status.success());

    let value: Value = serde_json::from_slice(&fixture.report("json").stdout).unwrap();
    assert_eq!(value["native_cache"]["snapshots"][1]["hits"], 1);
    assert_eq!(
        value["native_cache"]["snapshots"][1]["counter_reset_or_source_change"],
        true
    );
}

#[test]
fn malformed_or_oversized_inputs_never_create_evidence() {
    let fixture = Fixture::new();
    for (name, source, bytes) in [
        ("missing counters", "ccache", b"cache_miss 0\n".to_vec()),
        ("negative", "ccache", b"direct_cache_hit -1\n".to_vec()),
        ("unknown json", "sccache", b"{}".to_vec()),
        ("overflowing counters", "ccache", b"direct_cache_hit 18446744073709551615\npreprocessed_cache_hit 1\ncache_miss 0\nstats_zeroed_timestamp 0\n".to_vec()),
        ("negative json", "sccache", br#"{"stats":{"cache_hits":{"counts":{"Rust":-1}},"cache_misses":{"counts":{}}}}"#.to_vec()),
        (
            "not go summary",
            "go-test",
            b"arbitrary (cached)\n".to_vec(),
        ),
        ("oversized", "go-test", vec![b'a'; 1_048_577]),
    ] {
        let output = fixture.import(source, &bytes);

        assert!(!output.status.success(), "{name}");
        assert!(!fixture.0.join("state/snapshots.json").exists(), "{name}");
    }
}

#[test]
fn expired_evidence_is_unavailable_and_stale_staging_is_reclaimed() {
    let fixture = Fixture::new();
    assert!(
        fixture
            .import("go-test", b"ok private/package (cached)\n")
            .status
            .success()
    );
    let path = fixture.0.join("state/snapshots.json");
    let mut stored: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    stored[0]["collected_unix_ms"] = Value::from(1);
    fs::write(&path, serde_json::to_vec(&stored).unwrap()).unwrap();
    let pending = fixture.0.join("state/snapshots.pending");
    fs::write(&pending, b"interrupted").unwrap();
    fs::set_permissions(&pending, fs::Permissions::from_mode(0o600)).unwrap();

    let report: Value = serde_json::from_slice(&fixture.report("json").stdout).unwrap();
    assert_eq!(
        report["native_cache"]["snapshots"]
            .as_array()
            .unwrap()
            .as_slice(),
        [] as [Value; 0]
    );
    assert!(
        fixture
            .import("go-test", b"ok private/package (cached)\n")
            .status
            .success()
    );
    let stored: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(stored.as_array().unwrap().len(), 1);
    assert!(!pending.exists());
}

#[test]
fn capture_runs_fixed_statistics_arguments_without_storing_stderr() {
    let fixture = Fixture::new();
    let executable = fixture.0.join("ccache");
    fs::write(&executable, b"#!/bin/sh\n[ \"$*\" = --print-stats ] || exit 2\nprintf 'secret /private/source\\n' >&2\nprintf 'direct_cache_hit\\t2\\npreprocessed_cache_hit\\t1\\ncache_miss\\t4\\nstats_zeroed_timestamp\\t0\\n'\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args([
            "native-cache",
            "capture",
            "--source",
            "ccache",
            "--state-dir",
        ])
        .arg(fixture.0.join("state"))
        .env("PATH", &fixture.0)
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stderr.as_slice(), b"");
    let stored = fs::read_to_string(fixture.0.join("state/snapshots.json")).unwrap();
    assert!(!stored.contains("secret"));
    let value: Value = serde_json::from_str(&stored).unwrap();
    assert_eq!(value[0]["method"], "capture");
    assert_eq!(value[0]["hits"], 3);
}

#[test]
fn capture_rejects_unbounded_or_unresponsive_tools_without_recording_evidence() {
    for (name, script) in [
        (
            "unresponsive",
            b"#!/bin/sh\nexec /bin/sleep 60\n".as_slice(),
        ),
        ("oversized", b"#!/bin/sh\nexec /usr/bin/yes\n".as_slice()),
    ] {
        let fixture = Fixture::new();
        let executable = fixture.0.join("ccache");
        fs::write(&executable, script).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();

        let started = std::time::Instant::now();
        let output = Command::new(env!("CARGO_BIN_EXE_reef"))
            .args([
                "native-cache",
                "capture",
                "--source",
                "ccache",
                "--state-dir",
            ])
            .arg(fixture.0.join("state"))
            .env("PATH", &fixture.0)
            .output()
            .unwrap();

        assert!(!output.status.success(), "{name}");
        assert!(!fixture.0.join("state").exists(), "{name}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "{name}"
        );
    }
}

#[test]
fn retention_caps_snapshots_and_missing_tools_do_not_record_zero() {
    let fixture = Fixture::new();
    for _ in 0..130 {
        assert!(
            fixture
                .import("go-test", b"ok private/package (cached)\n")
                .status
                .success()
        );
    }
    let value: Value = serde_json::from_slice(&fixture.report("json").stdout).unwrap();
    assert_eq!(
        value["native_cache"]["snapshots"].as_array().unwrap().len(),
        128
    );
    let output = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args([
            "native-cache",
            "capture",
            "--source",
            "ccache",
            "--state-dir",
        ])
        .arg(fixture.0.join("absent"))
        .env("PATH", "")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!fixture.0.join("absent").exists());
}
