use clap::{Args, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const INPUT_LIMIT: u64 = 1024 * 1024;
const STORE_LIMIT: u64 = 128 * 1024;
const RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const MAX_SNAPSHOTS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Ccache,
    Sccache,
    GoTest,
}

#[derive(Debug, Args)]
pub struct Options {
    #[command(subcommand)]
    command: Action,
}

#[derive(Debug, Subcommand)]
enum Action {
    /// Read statistics from an installed ccache or sccache with fixed arguments.
    Capture {
        #[arg(long, value_enum)]
        source: Source,
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Import explicitly supplied statistics or plain go test output; discard raw input.
    Import {
        #[arg(long, value_enum)]
        source: Source,
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub source: Source,
    pub collected_unix_ms: u64,
    pub method: Method,
    pub cumulative: bool,
    pub hits: Option<u64>,
    pub misses: Option<u64>,
    pub cached_test_results: Option<u64>,
    pub executed_test_results: Option<u64>,
    pub reset_timestamp_seconds: Option<u64>,
    pub counter_reset_or_source_change: bool,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    Capture,
    Import,
}

#[derive(Serialize)]
pub struct Evidence {
    pub snapshots: Vec<Snapshot>,
    pub unsupported_sources: [&'static str; 2],
    pub disclosure: &'static str,
}

impl Default for Evidence {
    fn default() -> Self {
        Self {
            snapshots: Vec::new(),
            unsupported_sources: ["go_build", "node"],
            disclosure: "Absent snapshots mean unavailable, not zero. Collection time is import/capture time. Compiler counters are cumulative snapshots, not interval totals; never sum them. Decreases or changed ccache reset times flag resets/source changes; other resets are undetectable. Use one state directory per configured cache. Go executed results are not cache misses. No savings or hits are inferred from speed or repeated commands.",
        }
    }
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "unsupported or malformed native cache evidence",
    )
}

fn number(value: Option<&serde_json::Value>) -> io::Result<u64> {
    value
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(invalid)
}

fn sum_counts(value: Option<&serde_json::Value>) -> io::Result<u64> {
    value
        .and_then(|value| value.get("counts"))
        .and_then(serde_json::Value::as_object)
        .ok_or_else(invalid)?
        .values()
        .try_fold(0u64, |sum, value| {
            sum.checked_add(number(Some(value))?).ok_or_else(invalid)
        })
}

fn parse(source: Source, input: &[u8], method: Method, now: u64) -> io::Result<Snapshot> {
    let mut snapshot = Snapshot {
        source,
        collected_unix_ms: now,
        method,
        cumulative: source != Source::GoTest,
        hits: None,
        misses: None,
        cached_test_results: None,
        executed_test_results: None,
        reset_timestamp_seconds: None,
        counter_reset_or_source_change: false,
    };
    match source {
        Source::Ccache => {
            let text = std::str::from_utf8(input).map_err(|_| invalid())?;
            let mut counters = std::collections::BTreeMap::new();
            for line in text.lines() {
                let mut parts = line.split_whitespace();
                let key = parts.next().ok_or_else(invalid)?;
                let value = parts
                    .next()
                    .ok_or_else(invalid)?
                    .parse::<u64>()
                    .map_err(|_| invalid())?;
                if parts.next().is_some() || counters.insert(key, value).is_some() {
                    return Err(invalid());
                }
            }
            let get = |key| counters.get(key).copied().ok_or_else(invalid);
            snapshot.hits = Some(
                get("direct_cache_hit")?
                    .checked_add(get("preprocessed_cache_hit")?)
                    .ok_or_else(invalid)?,
            );
            snapshot.misses = Some(get("cache_miss")?);
            snapshot.reset_timestamp_seconds = Some(get("stats_zeroed_timestamp")?);
        }
        Source::Sccache => {
            let value: serde_json::Value = serde_json::from_slice(input).map_err(|_| invalid())?;
            let stats = value.get("stats").ok_or_else(invalid)?;
            snapshot.hits = Some(sum_counts(stats.get("cache_hits"))?);
            snapshot.misses = Some(sum_counts(stats.get("cache_misses"))?);
        }
        Source::GoTest => {
            let text = std::str::from_utf8(input).map_err(|_| invalid())?;
            let (mut cached, mut executed) = (0u64, 0u64);
            for line in text.lines() {
                let fields: Vec<_> = line.split_whitespace().collect();
                if fields.first() != Some(&"ok") || fields.len() < 3 {
                    continue;
                }
                if fields[2] == "(cached)" {
                    cached += 1;
                } else if fields[2]
                    .strip_suffix('s')
                    .and_then(|value| value.parse::<f64>().ok())
                    .is_some_and(|value| value.is_finite() && value >= 0.0)
                {
                    executed += 1;
                }
            }
            if cached + executed == 0 {
                return Err(invalid());
            }
            snapshot.cached_test_results = Some(cached);
            snapshot.executed_test_results = Some(executed);
        }
    }
    Ok(snapshot)
}

fn bounded_read(reader: impl Read, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "native cache input exceeds byte limit",
        ));
    }
    Ok(bytes)
}

fn capture(source: Source) -> io::Result<Vec<u8>> {
    let (program, args): (&str, &[&str]) = match source {
        Source::Ccache => ("ccache", &["--print-stats"]),
        Source::Sccache => ("sccache", &["--show-stats", "--stats-format", "json"]),
        Source::GoTest => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Go test evidence requires explicit output import",
            ));
        }
    };
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                "native cache tool unavailable; no evidence captured",
            )
        })?;
    let result = (|| {
        let mut stdout = child.stdout.take().ok_or_else(invalid)?;
        let flags =
            nix::fcntl::fcntl(&stdout, nix::fcntl::FcntlArg::F_GETFL).map_err(io::Error::other)?;
        nix::fcntl::fcntl(
            &stdout,
            nix::fcntl::FcntlArg::F_SETFL(
                nix::fcntl::OFlag::from_bits_truncate(flags) | nix::fcntl::OFlag::O_NONBLOCK,
            ),
        )
        .map_err(io::Error::other)?;
        let start = Instant::now();
        let mut output = Vec::new();
        let mut buffer = [0; 8192];
        loop {
            match stdout.read(&mut buffer) {
                Ok(0) => {
                    if let Some(status) = child.try_wait()? {
                        return if status.success() {
                            Ok(output)
                        } else {
                            Err(io::Error::other(
                                "native cache statistics command failed; no evidence captured",
                            ))
                        };
                    }
                }
                Ok(count) => {
                    output.extend_from_slice(&buffer[..count]);
                    if output.len() as u64 > INPUT_LIMIT {
                        return Err(invalid());
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
            if start.elapsed() > Duration::from_secs(5) {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "native cache capture timed out",
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    let _ = child.wait();
    result
}

fn directory(override_path: Option<&Path>) -> io::Result<PathBuf> {
    override_path.map_or_else(
        || {
            std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join(".local/state/reef/native-cache"))
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "HOME unavailable; pass --state-dir",
                    )
                })
        },
        |path| Ok(path.to_owned()),
    )
}

fn private(metadata: &fs::Metadata) -> io::Result<()> {
    if metadata.uid() != nix::unistd::Uid::current().as_raw() || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "native cache state must be private and owned by the current user",
        ));
    }
    Ok(())
}

fn state_dir(path: &Path, create: bool) -> io::Result<bool> {
    if create {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(invalid());
    }
    private(&metadata)?;
    Ok(true)
}

fn open_private(path: &Path, write: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(write)
        .create(write)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(invalid());
    }
    private(&metadata)?;
    Ok(file)
}

fn load(dir: &Path) -> io::Result<Vec<Snapshot>> {
    let file = match open_private(&dir.join("snapshots.json"), false) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let snapshots: Vec<Snapshot> =
        serde_json::from_slice(&bounded_read(file, STORE_LIMIT)?).map_err(|_| invalid())?;
    if snapshots.len() > MAX_SNAPSHOTS || snapshots.iter().any(|snapshot| !valid_snapshot(snapshot))
    {
        return Err(invalid());
    }
    Ok(snapshots)
}

fn valid_snapshot(snapshot: &Snapshot) -> bool {
    if snapshot.source == Source::GoTest {
        !snapshot.cumulative
            && snapshot.hits.is_none()
            && snapshot.misses.is_none()
            && snapshot.reset_timestamp_seconds.is_none()
            && snapshot.cached_test_results.is_some()
            && snapshot.executed_test_results.is_some()
            && !snapshot.counter_reset_or_source_change
    } else {
        snapshot.cumulative
            && snapshot.hits.is_some()
            && snapshot.misses.is_some()
            && snapshot.cached_test_results.is_none()
            && snapshot.executed_test_results.is_none()
            && (snapshot.reset_timestamp_seconds.is_some() == (snapshot.source == Source::Ccache))
    }
}

fn save(dir: &Path, mut snapshot: Snapshot) -> io::Result<()> {
    state_dir(dir, true)?;
    let lock = open_private(&dir.join("state.lock"), true)?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let mut snapshots = load(dir)?;
    snapshots.retain(|value| {
        value.collected_unix_ms >= snapshot.collected_unix_ms.saturating_sub(RETENTION_MS)
    });
    if snapshot.cumulative
        && let Some(previous) = snapshots
            .iter()
            .rev()
            .find(|value| value.source == snapshot.source)
    {
        snapshot.counter_reset_or_source_change = snapshot.hits < previous.hits
            || snapshot.misses < previous.misses
            || snapshot.reset_timestamp_seconds != previous.reset_timestamp_seconds;
    }
    snapshots.push(snapshot);
    let excess = snapshots.len().saturating_sub(MAX_SNAPSHOTS);
    snapshots.drain(..excess);
    let bytes = serde_json::to_vec(&snapshots)?;
    if bytes.len() as u64 > STORE_LIMIT {
        return Err(invalid());
    }
    let pending = dir.join("snapshots.pending");
    match open_private(&pending, false) {
        Ok(file) => {
            drop(file);
            fs::remove_file(&pending)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&pending)?;
    let result = (|| {
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&pending, dir.join("snapshots.json"))
    })();
    if result.is_err() {
        let _ = fs::remove_file(pending);
    }
    result
}

pub fn run(options: &Options) -> io::Result<()> {
    let (source, state, bytes, method) = match &options.command {
        Action::Capture { source, state_dir } => {
            (*source, state_dir, capture(*source)?, Method::Capture)
        }
        Action::Import {
            source,
            input,
            state_dir,
        } => {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
                .open(input)?;
            if !file.metadata()?.is_file() {
                return Err(invalid());
            }
            (
                *source,
                state_dir,
                bounded_read(file, INPUT_LIMIT)?,
                Method::Import,
            )
        }
    };
    let now = u64::try_from(chrono::Utc::now().timestamp_millis()).map_err(|_| invalid())?;
    let snapshot = parse(source, &bytes, method, now)?;
    save(&directory(state.as_deref())?, snapshot)?;
    println!("Native cache evidence stored; raw input discarded.");
    Ok(())
}

pub fn report(state: Option<&Path>, from: u64, to: u64) -> io::Result<Evidence> {
    let dir = match directory(state) {
        Ok(dir) => dir,
        Err(error) if state.is_none() && error.kind() == io::ErrorKind::NotFound => {
            return Ok(Evidence::default());
        }
        Err(error) => return Err(error),
    };
    if !state_dir(&dir, false)? {
        return Ok(Evidence::default());
    }
    let lock = match open_private(&dir.join("state.lock"), false) {
        Ok(lock) => lock,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Evidence::default()),
        Err(error) => return Err(error),
    };
    fs2::FileExt::lock_shared(&lock)?;
    let oldest = u64::try_from(chrono::Utc::now().timestamp_millis())
        .unwrap_or_default()
        .saturating_sub(RETENTION_MS);
    Ok(Evidence {
        snapshots: load(&dir)?
            .into_iter()
            .filter(|value| {
                value.collected_unix_ms >= from.max(oldest) && value.collected_unix_ms < to
            })
            .collect(),
        ..Evidence::default()
    })
}

pub fn text(evidence: &Evidence) -> String {
    let mut out = String::from("Native cache evidence\n\n");
    for source in [Source::Ccache, Source::Sccache, Source::GoTest] {
        let name = match source {
            Source::Ccache => "ccache",
            Source::Sccache => "sccache",
            Source::GoTest => "Go test results",
        };
        let selected: Vec<_> = evidence
            .snapshots
            .iter()
            .filter(|snapshot| snapshot.source == source)
            .collect();
        if selected.is_empty() {
            let _ = writeln!(
                out,
                "- {name}: unavailable (not collected, expired, outside range, or tool absent)."
            );
        }
        for snapshot in selected {
            let method = match snapshot.method {
                Method::Capture => "capture",
                Method::Import => "import",
            };
            let _ = write!(
                out,
                "- {name}: {method}, collected Unix ms {}; ",
                snapshot.collected_unix_ms
            );
            if snapshot.cumulative {
                let _ = writeln!(
                    out,
                    "cumulative hits {}, misses {}; counter reset or source change: {}.",
                    snapshot.hits.unwrap_or_default(),
                    snapshot.misses.unwrap_or_default(),
                    snapshot.counter_reset_or_source_change
                );
            } else {
                let _ = writeln!(
                    out,
                    "cached package results {}, executed successful package results {} (not cache misses).",
                    snapshot.cached_test_results.unwrap_or_default(),
                    snapshot.executed_test_results.unwrap_or_default()
                );
            }
        }
    }
    out.push_str("\nCounters are snapshots, never added or attributed to this report interval. Counter decreases or ccache reset timestamps flag resets/source changes; other resets are undetectable. Imports are user-supplied; collection time is import/capture time, not execution time. Use one state directory per configured compiler cache. Go build and Node cache metrics are unsupported. No hit/miss or time savings are inferred from repeated commands or speed. Reef output-cache statistics are separate.\n");
    out
}
