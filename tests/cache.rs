#![cfg(unix)]

use serde_json::Value;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    repo: PathBuf,
    cache: PathBuf,
    counter: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "reef-cache-test-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let repo = root.join("repo");
        let cache = root.join("cache");
        let counter = root.join("counter");
        fs::create_dir_all(&repo).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
        fs::write(repo.join("input.txt"), "first").unwrap();
        assert!(
            Command::new("git")
                .args(["add", "input.txt"])
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
        Self {
            root,
            repo,
            cache,
            counter,
        }
    }

    fn run(&self, script: &str) -> Output {
        self.command(script).output().unwrap()
    }

    fn command(&self, script: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_reef"));
        command
            .current_dir(&self.repo)
            .env("REEF_TEST_COUNTER", &self.counter)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .args(["cache", "run", "--cache-dir"])
            .arg(&self.cache)
            .args(["--", "sh", "-c", script]);
        command
    }

    fn impact(&self) -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_reef"))
            .args([
                "report",
                "--from",
                "2020-01-01T00:00:00Z",
                "--to",
                "2100-01-01T00:00:00Z",
                "--state-dir",
            ])
            .arg(self.root.join("observe"))
            .arg("--cache-dir")
            .arg(&self.cache)
            .args(["--format", "json"])
            .env("HOME", &self.root)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["cache"].clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

const COUNT: &str = "n=$(cat \"$REEF_TEST_COUNTER\" 2>/dev/null || printf 0); n=$((n+1)); printf '%s' \"$n\" > \"$REEF_TEST_COUNTER\"; printf 'count:%s\\n' \"$n\"";

#[test]
fn repeats_a_success_without_running_the_command_again() {
    let fixture = Fixture::new();

    let first = fixture.run(COUNT);
    let second = fixture.run(COUNT);

    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(first.stdout, b"count:1\n");
    assert_eq!(second.stdout, b"count:1\n");
    assert_eq!(fs::read_to_string(&fixture.counter).unwrap(), "1");
    assert!(String::from_utf8_lossy(&first.stderr).contains("cache miss"));
    assert!(String::from_utf8_lossy(&second.stderr).contains("cache hit"));
}

#[test]
fn reports_key_based_reuse_without_recording_command_text() {
    let fixture = Fixture::new();
    assert_eq!(fixture.impact()["available"], false);
    let first = fixture.run(COUNT);
    let second = fixture.run(COUNT);
    assert!(first.status.success());
    assert!(second.status.success());
    let impact = fixture.impact();
    assert_eq!(impact["available"], true);
    assert_eq!(impact["hits"], 1);
    assert_eq!(impact["misses"], 1);
    assert_eq!(impact["estimate_sample_count"], 1);
    assert_eq!(impact["estimate_missing_sample_count"], 0);
    assert!(impact["estimated_reused_wall_ms"].as_u64().is_some());
    assert!(impact["estimated_reused_cpu_ms"].as_u64().is_some());
    for entry in fs::read_dir(fixture.cache.join("impact")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|value| value == "json") {
            let contents = fs::read_to_string(&path).unwrap();
            assert!(!contents.contains(COUNT));
            assert!(!contents.contains("REEF_TEST_COUNTER"));
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}

#[test]
fn missing_cost_sample_is_unavailable_not_zero_savings() {
    let fixture = Fixture::new();
    fixture.run(COUNT);
    let impact_dir = fixture.cache.join("impact");
    for entry in fs::read_dir(&impact_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|value| value == "json") {
            fs::remove_file(path).unwrap();
        }
    }
    fixture.run(COUNT);
    let impact = fixture.impact();
    assert_eq!(impact["hits"], 1);
    assert_eq!(impact["estimate_sample_count"], 0);
    assert_eq!(impact["estimate_missing_sample_count"], 1);
    assert!(impact["estimated_reused_wall_ms"].is_null());
    assert!(impact["estimated_reused_cpu_ms"].is_null());
}

#[test]
fn failed_execution_is_counted_but_not_used_as_a_cost_sample() {
    let fixture = Fixture::new();
    let first = fixture.run("exit 7");
    assert_eq!(first.status.code(), Some(7));
    let impact = fixture.impact();
    assert_eq!(impact["misses"], 1);
    assert_eq!(impact["failed_or_uncacheable"], 1);
    assert_eq!(impact["hits"], 0);
    assert!(impact["estimated_reused_wall_ms"].is_null());
    assert!(impact["estimated_reused_cpu_ms"].is_null());
}

#[test]
fn concurrent_first_time_cache_keys_keep_every_miss_event() {
    let fixture = Fixture::new();
    let start = std::sync::Arc::new(std::sync::Barrier::new(32));
    let work: Vec<_> = (0..32)
        .map(|number| {
            let start = start.clone();
            let repo = fixture.repo.clone();
            let cache = fixture.cache.clone();
            std::thread::spawn(move || {
                start.wait();
                Command::new(env!("CARGO_BIN_EXE_reef"))
                    .current_dir(repo)
                    .args(["cache", "run", "--cache-dir"])
                    .arg(cache)
                    .args(["--", "printf"])
                    .arg(format!("{number}"))
                    .output()
                    .unwrap()
            })
        })
        .collect();

    for worker in work {
        let output = worker.join().unwrap();
        assert!(output.status.success(), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("cannot save cache impact event"),
            "{stderr}"
        );
        assert!(!stderr.contains("cannot prune cache"), "{stderr}");
    }
    let impact = fixture.impact();
    assert_eq!(impact["misses"], 32);
    assert_eq!(impact["hits"], 0);
    assert!(impact["estimated_reused_wall_ms"].is_null());
}

#[test]
fn status_ignores_unpublished_entries() {
    let fixture = Fixture::new();
    let first = fixture.run("printf ok");
    assert!(first.status.success());
    let pending = fixture
        .cache
        .join("entries")
        .join(format!(".{}.123", "a".repeat(64)));
    fs::create_dir(&pending).unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["cache", "status", "--cache-dir"])
        .arg(&fixture.cache)
        .output()
        .unwrap();

    assert!(status.status.success());
    let value: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(value["entries"], 1);
}

#[test]
fn event_write_failure_warns_without_changing_cached_output() {
    let fixture = Fixture::new();
    let first = fixture.run(COUNT);
    assert!(first.status.success());
    fs::set_permissions(
        fixture.cache.join("impact"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    let second = fixture.run(COUNT);

    assert!(second.status.success());
    assert_eq!(second.stdout, first.stdout);
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(stderr.contains("cache hit"));
    assert!(stderr.contains("cannot save cache impact event"));
}

#[test]
fn invalidates_when_tracked_input_changes() {
    let fixture = Fixture::new();
    fixture.run(COUNT);
    fs::write(fixture.repo.join("input.txt"), "second").unwrap();

    let second = fixture.run(COUNT);

    assert_eq!(second.stdout, b"count:2\n");
    assert!(String::from_utf8_lossy(&second.stderr).contains("cache miss"));
}

#[test]
fn invalidates_when_declared_external_input_changes() {
    let fixture = Fixture::new();
    let external = fixture.root.join("external.txt");
    fs::write(&external, "first").unwrap();
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_reef"))
            .current_dir(&fixture.repo)
            .env("REEF_TEST_COUNTER", &fixture.counter)
            .args(["cache", "run", "--cache-dir"])
            .arg(&fixture.cache)
            .arg("--input")
            .arg(&external)
            .args(["--", "sh", "-c", COUNT])
            .output()
            .unwrap()
    };

    assert_eq!(run().stdout, b"count:1\n");
    fs::write(&external, "second").unwrap();
    assert_eq!(run().stdout, b"count:2\n");
}

#[test]
fn invalidates_when_environment_changes() {
    let fixture = Fixture::new();
    let first = fixture
        .command(COUNT)
        .env("REEF_TEST_MODE", "first")
        .output()
        .unwrap();
    let second = fixture
        .command(COUNT)
        .env("REEF_TEST_MODE", "second")
        .output()
        .unwrap();

    assert_eq!(first.stdout, b"count:1\n");
    assert_eq!(second.stdout, b"count:2\n");
}

#[test]
fn invalidates_when_git_head_changes_without_file_changes() {
    let fixture = Fixture::new();
    let switch = |branch| {
        assert!(
            Command::new("git")
                .args(["symbolic-ref", "HEAD", branch])
                .current_dir(&fixture.repo)
                .status()
                .unwrap()
                .success()
        );
    };
    switch("refs/heads/first");
    let first = fixture.run("git symbolic-ref HEAD");
    switch("refs/heads/second");

    let second = fixture.run("git symbolic-ref HEAD");

    assert_eq!(first.stdout, b"refs/heads/first\n");
    assert_eq!(second.stdout, b"refs/heads/second\n");
    assert!(String::from_utf8_lossy(&second.stderr).contains("cache miss"));
}

#[test]
fn invalidates_when_broken_symlink_target_changes() {
    let fixture = Fixture::new();
    let link = fixture.repo.join("link");
    symlink("missing-first", &link).unwrap();
    assert!(
        Command::new("git")
            .args(["add", "link"])
            .current_dir(&fixture.repo)
            .status()
            .unwrap()
            .success()
    );
    let first = fixture.run("readlink link");
    fs::remove_file(&link).unwrap();
    symlink("missing-second", &link).unwrap();

    let second = fixture.run("readlink link");

    assert_eq!(first.stdout, b"missing-first\n");
    assert_eq!(second.stdout, b"missing-second\n");
}

#[test]
fn accepts_a_tracked_symlink_to_a_regular_file() {
    let fixture = Fixture::new();
    symlink("input.txt", fixture.repo.join("link.txt")).unwrap();
    assert!(
        Command::new("git")
            .args(["add", "link.txt"])
            .current_dir(&fixture.repo)
            .status()
            .unwrap()
            .success()
    );

    let output = fixture.run("cat link.txt");

    assert_eq!(output.stdout, b"first");
    assert!(output.status.success());
}

#[test]
fn runs_with_a_populated_submodule_and_invalidates_on_change() {
    let fixture = Fixture::new();
    let module = fixture.repo.join("module");
    fs::create_dir(&module).unwrap();
    fs::write(module.join("value.txt"), "first").unwrap();
    assert!(
        Command::new("git")
            .args([
                "update-index",
                "--add",
                "--cacheinfo",
                "160000,1111111111111111111111111111111111111111,module",
            ])
            .current_dir(&fixture.repo)
            .status()
            .unwrap()
            .success()
    );
    let first = fixture.run("cat module/value.txt");
    fs::write(module.join("value.txt"), "second").unwrap();

    let second = fixture.run("cat module/value.txt");

    assert_eq!(first.stdout, b"first");
    assert_eq!(second.stdout, b"second");
    assert!(first.status.success());
    assert!(second.status.success());
}

#[test]
fn hashes_the_executable_the_operating_system_runs() {
    let fixture = Fixture::new();
    let first_dir = fixture.root.join("first-path");
    let second_dir = fixture.root.join("second-path");
    fs::create_dir(&first_dir).unwrap();
    fs::create_dir(&second_dir).unwrap();
    fs::write(first_dir.join("probe-tool"), "not executable").unwrap();
    let tool = second_dir.join("probe-tool");
    fs::write(&tool, "#!/bin/sh\nprintf first").unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}:/usr/bin:/bin",
        first_dir.display(),
        second_dir.display()
    );
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_reef"))
            .current_dir(&fixture.repo)
            .env("PATH", &path)
            .args(["cache", "run", "--cache-dir"])
            .arg(&fixture.cache)
            .args(["--", "probe-tool"])
            .output()
            .unwrap()
    };
    let first = run();
    fs::write(&tool, "#!/bin/sh\nprintf second").unwrap();

    let second = run();

    assert_eq!(first.stdout, b"first");
    assert_eq!(second.stdout, b"second");
}

#[test]
fn rejects_a_cache_directory_inside_the_worktree_from_a_subdirectory() {
    let fixture = Fixture::new();
    let sub = fixture.repo.join("sub");
    fs::create_dir(&sub).unwrap();
    let internal_cache = fixture.repo.join("cache");

    let output = Command::new(env!("CARGO_BIN_EXE_reef"))
        .current_dir(sub)
        .args(["cache", "run", "--cache-dir"])
        .arg(internal_cache)
        .args(["--", "printf", "ok"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("outside the current worktree"));
}

#[test]
fn failed_commands_are_never_reused() {
    let fixture = Fixture::new();
    let script = format!("{COUNT}; exit 7");

    let first = fixture.run(&script);
    let second = fixture.run(&script);

    assert_eq!(first.status.code(), Some(7));
    assert_eq!(second.status.code(), Some(7));
    assert_eq!(first.stdout, b"count:1\n");
    assert_eq!(second.stdout, b"count:2\n");
}

#[test]
fn concurrent_equivalent_requests_share_one_execution() {
    let fixture = Fixture::new();
    let script = format!("{COUNT}; sleep 1");

    let first = fixture.command(&script).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let second = fixture.command(&script).spawn().unwrap();
    let first = first.wait_with_output().unwrap();
    let second = second.wait_with_output().unwrap();

    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(first.stdout, b"count:1\n");
    assert_eq!(second.stdout, b"count:1\n");
    assert_eq!(fs::read_to_string(&fixture.counter).unwrap(), "1");
    let impact = fixture.impact();
    assert_eq!(impact["hits"], 1);
    assert_eq!(impact["shared_executions"], 1);
    assert_eq!(impact["misses"], 1);
}

#[test]
fn shorter_requested_lifetime_expires_an_old_entry() {
    let fixture = Fixture::new();
    fixture.run(COUNT);
    std::thread::sleep(Duration::from_secs(1));
    let output = Command::new(env!("CARGO_BIN_EXE_reef"))
        .current_dir(&fixture.repo)
        .env("REEF_TEST_COUNTER", &fixture.counter)
        .args(["cache", "run", "--ttl-seconds", "1", "--cache-dir"])
        .arg(&fixture.cache)
        .args(["--", "sh", "-c", COUNT])
        .output()
        .unwrap();

    assert_eq!(output.stdout, b"count:2\n");
}

#[test]
fn stores_only_private_entries_and_reports_owned_size() {
    let fixture = Fixture::new();
    fixture.run(COUNT);

    let report = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["cache", "status", "--cache-dir"])
        .arg(&fixture.cache)
        .output()
        .unwrap();
    let value: Value = serde_json::from_slice(&report.stdout).unwrap();
    assert_eq!(value["entries"], 1);
    assert!(value["bytes"].as_u64().unwrap() > 0);
    assert_eq!(
        fs::metadata(&fixture.cache).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let entry = fs::read_dir(fixture.cache.join("entries"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(
        fs::metadata(&entry).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(entry.join("stdout"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(
        !fs::read_to_string(entry.join("entry.json"))
            .unwrap()
            .contains(COUNT)
    );
}

#[test]
fn corrupted_output_is_discarded_and_recomputed() {
    let fixture = Fixture::new();
    fixture.run(COUNT);
    let entry = fs::read_dir(fixture.cache.join("entries"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::write(entry.join("stdout"), "corrupt").unwrap();

    let output = fixture.run(COUNT);

    assert_eq!(output.stdout, b"count:2\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("cache miss"));
}

#[test]
fn evicts_older_entries_when_size_limit_is_reached() {
    let fixture = Fixture::new();
    let run = |script| {
        Command::new(env!("CARGO_BIN_EXE_reef"))
            .current_dir(&fixture.repo)
            .args(["cache", "run", "--max-storage-mib", "1", "--cache-dir"])
            .arg(&fixture.cache)
            .args(["--", "sh", "-c", script])
            .output()
            .unwrap()
    };
    let first = run("head -c 700000 /dev/zero");
    let second = run("printf x; head -c 700000 /dev/zero");

    assert!(first.status.success());
    assert!(second.status.success());
    let report = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["cache", "status", "--cache-dir"])
        .arg(&fixture.cache)
        .output()
        .unwrap();
    let value: Value = serde_json::from_slice(&report.stdout).unwrap();
    assert_eq!(value["entries"], 1);
}
