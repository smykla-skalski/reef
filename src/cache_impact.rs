use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const MAX_BYTES: u64 = 10 * 1024 * 1024;
const MAX_EVENTS: usize = 2048;
static NEXT_EVENT: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Hit,
    Shared,
    Miss,
    Uncacheable,
}

#[derive(Serialize, Deserialize)]
struct Event {
    at_unix_ms: u64,
    key: String,
    kind: Kind,
    wall_ms: Option<u128>,
    cpu_ms: Option<u128>,
}

#[derive(Default, Serialize)]
pub struct Impact {
    pub available: bool,
    pub hits: usize,
    pub misses: usize,
    pub shared_executions: usize,
    pub failed_or_uncacheable: usize,
    pub estimated_reused_wall_ms: Option<u128>,
    pub estimated_reused_cpu_ms: Option<u128>,
    pub estimate_sample_count: usize,
    pub estimate_missing_sample_count: usize,
}

pub fn markdown(impact: &Impact) -> String {
    if !impact.available {
        return "\n## Cache reuse\n\nCache impact history unavailable.\n".to_owned();
    }
    let printable = |value: Option<u128>| {
        value.map_or_else(|| "unavailable".to_owned(), |value| value.to_string())
    };
    let mut out = String::from("\n## Cache reuse\n\n");
    let _ = write!(
        out,
        "- Hits: {}\n- Misses: {}\n- Shared executions: {} (subset of hits)\n- Failed or uncacheable attempts: {} (subset of misses)\n- Estimated reused wall: {} ms\n- Estimated reused CPU: {} ms\n- Valid cost samples: {}\n- Hits without a cost sample: {}\n",
        impact.hits,
        impact.misses,
        impact.shared_executions,
        impact.failed_or_uncacheable,
        printable(impact.estimated_reused_wall_ms),
        printable(impact.estimated_reused_cpu_ms),
        impact.estimate_sample_count,
        impact.estimate_missing_sample_count,
    );
    out.push_str("Reused time is estimated from successful executions with the same cache key; it is not measured time saved.\n");
    out
}

fn now_ms() -> io::Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_millis()
        .try_into()
        .map_err(io::Error::other)
}

fn private_dir(path: &Path, create: bool) -> io::Result<bool> {
    if create && !path.exists() {
        fs::DirBuilder::new().mode(0o700).create(path)?;
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if !create && error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != nix::unistd::Uid::current().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cache impact directory must be private and owned by the current user",
        ));
    }
    Ok(true)
}

fn event_files(dir: &Path) -> io::Result<Vec<(u64, PathBuf, u64)>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(parts) = name
            .to_str()
            .and_then(|name| name.strip_prefix("event-"))
            .and_then(|name| name.strip_suffix(".json"))
            .map(|name| name.split('-').collect::<Vec<_>>())
        else {
            continue;
        };
        if parts.len() != 3 {
            continue;
        }
        let (Ok(timestamp), Ok(_pid), Ok(_counter)) = (
            parts[0].parse::<u64>(),
            parts[1].parse::<u32>(),
            parts[2].parse::<u64>(),
        ) else {
            continue;
        };
        let metadata = fs::symlink_metadata(entry.path())?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.uid() != nix::unistd::Uid::current().as_raw()
            || metadata.mode() & 0o077 != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cache impact event must be a private regular file",
            ));
        }
        files.push((timestamp, entry.path(), metadata.len()));
    }
    files.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));
    Ok(files)
}

fn prune(dir: &Path, now: u64) -> io::Result<()> {
    let files = event_files(dir)?;
    let mut total: u64 = files.iter().map(|(_, _, len)| len).sum();
    let excess = files.len().saturating_sub(MAX_EVENTS);
    for (index, (timestamp, path, len)) in files.into_iter().enumerate() {
        if index < excess || timestamp < now.saturating_sub(RETENTION_MS) || total > MAX_BYTES {
            fs::remove_file(path)?;
            total = total.saturating_sub(len);
        }
    }
    Ok(())
}

fn save(root: &Path, event: &Event) -> io::Result<()> {
    let dir = root.join("impact");
    private_dir(&dir, true)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(dir.join("events.lock"))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let suffix = format!(
        "{}-{}-{}",
        event.at_unix_ms,
        std::process::id(),
        NEXT_EVENT.fetch_add(1, Ordering::Relaxed)
    );
    let pending = dir.join(format!("pending-{suffix}.tmp"));
    let final_path = dir.join(format!("event-{suffix}.json"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&pending)?;
    let result = (|| {
        file.write_all(&serde_json::to_vec(event)?)?;
        file.sync_data()?;
        fs::hard_link(&pending, &final_path)?;
        Ok::<(), io::Error>(())
    })();
    drop(file);
    let cleanup = fs::remove_file(&pending);
    result?;
    cleanup?;
    prune(&dir, event.at_unix_ms)
}

pub fn record(root: &Path, key: &str, kind: Kind, cost: Option<(u128, u128)>) {
    let result = now_ms().and_then(|at_unix_ms| {
        let event = Event {
            at_unix_ms,
            key: key.to_owned(),
            kind,
            wall_ms: cost.map(|value| value.0),
            cpu_ms: cost.map(|value| value.1),
        };
        save(root, &event)
    });
    if let Err(error) = result {
        eprintln!("reef: cannot save cache impact event: {error}");
    }
}

pub fn report(cache_dir: Option<&Path>, from_ms: u128, to_ms: u128) -> io::Result<Impact> {
    let root = match crate::cache::cache_path(cache_dir) {
        Ok(root) => root,
        Err(error) if cache_dir.is_none() && error.kind() == io::ErrorKind::NotFound => {
            return Ok(Impact::default());
        }
        Err(error) => return Err(error),
    };
    if !private_dir(&root, false)? {
        return Ok(Impact::default());
    }
    let dir = root.join("impact");
    if !private_dir(&dir, false)? {
        return Ok(Impact::default());
    }
    let mut events = Vec::new();
    for (_, path, _) in event_files(&dir)? {
        let mut bytes = Vec::new();
        File::open(path)?.take(4096).read_to_end(&mut bytes)?;
        events.push(serde_json::from_slice::<Event>(&bytes)?);
    }
    events.sort_by_key(|event| (event.at_unix_ms, !matches!(event.kind, Kind::Miss)));
    let mut samples = HashMap::<String, (u128, u128)>::new();
    let mut impact = Impact {
        available: true,
        ..Impact::default()
    };
    for event in events {
        if u128::from(event.at_unix_ms) >= from_ms && u128::from(event.at_unix_ms) < to_ms {
            match event.kind {
                Kind::Hit | Kind::Shared => {
                    impact.hits += 1;
                    if matches!(event.kind, Kind::Shared) {
                        impact.shared_executions += 1;
                    }
                    if let Some(&(wall, cpu)) = samples.get(&event.key) {
                        *impact.estimated_reused_wall_ms.get_or_insert(0) += wall;
                        *impact.estimated_reused_cpu_ms.get_or_insert(0) += cpu;
                        impact.estimate_sample_count += 1;
                    } else {
                        impact.estimate_missing_sample_count += 1;
                    }
                }
                Kind::Miss => impact.misses += 1,
                Kind::Uncacheable => {
                    impact.misses += 1;
                    impact.failed_or_uncacheable += 1;
                }
            }
        }
        if matches!(event.kind, Kind::Miss)
            && let Some(cost) = event.wall_ms.zip(event.cpu_ms)
        {
            samples.insert(event.key, cost);
        }
    }
    if impact.hits == 0 {
        impact.estimated_reused_wall_ms = Some(0);
        impact.estimated_reused_cpu_ms = Some(0);
    }
    Ok(impact)
}
