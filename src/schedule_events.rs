use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[cfg(unix)]
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::path::Path;

#[cfg(unix)]
use std::fs::OpenOptions;
#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(unix)]
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
const RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1000;
#[cfg(unix)]
const MAX_BYTES: u64 = 100 * 1024 * 1024;
#[cfg(unix)]
static NEXT_EVENT: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Wait {
    pub pressure: u128,
    pub capacity: u128,
    pub fifo: u128,
    pub running_limit: u128,
    pub admission: u128,
}

impl Wait {
    pub fn total_ms(&self) -> u128 {
        self.pressure + self.capacity + self.fifo + self.running_limit + self.admission
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Event {
    pub session: String,
    pub id: Option<u64>,
    pub kind: String,
    pub at_unix_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait: Option<Wait>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submission_to_finish_ms: Option<u128>,
}

#[derive(Default, Serialize)]
pub struct Summary {
    pub submitted: usize,
    pub admitted: usize,
    pub completed: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub rejected: usize,
    pub queue_wait_p50_ms: Option<u128>,
    pub queue_wait_p95_ms: Option<u128>,
    pub submission_to_finish_p50_ms: Option<u128>,
    pub submission_to_finish_p95_ms: Option<u128>,
    pub wait: Wait,
}

fn percentile(values: &mut [u128], p: usize) -> Option<u128> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let index = (values.len() * p).div_ceil(100).saturating_sub(1);
    values.get(index).copied()
}

pub fn summary(events: &[Event], from_ms: u64, to_ms: u64) -> Summary {
    let mut result = Summary::default();
    let mut requests = BTreeMap::new();
    for event in events {
        if event.kind == "submitted" && event.at_unix_ms >= from_ms && event.at_unix_ms < to_ms {
            result.submitted += 1;
            if let Some(id) = event.id {
                requests.insert((event.session.clone(), id), (false, false));
            }
        } else if event.kind == "rejected"
            && event.at_unix_ms >= from_ms
            && event.at_unix_ms < to_ms
        {
            result.rejected += 1;
        }
    }
    let mut waits = Vec::new();
    let mut finishes = Vec::new();
    for event in events {
        let Some(id) = event.id else { continue };
        let Some((admitted, terminal)) = requests.get_mut(&(event.session.clone(), id)) else {
            continue;
        };
        match event.kind.as_str() {
            "admitted" if !*admitted => {
                *admitted = true;
                result.admitted += 1;
                if let Some(wait) = &event.wait {
                    waits.push(wait.total_ms());
                    result.wait.pressure += wait.pressure;
                    result.wait.capacity += wait.capacity;
                    result.wait.fifo += wait.fifo;
                    result.wait.running_limit += wait.running_limit;
                    result.wait.admission += wait.admission;
                }
            }
            "completed" | "failed" | "cancelled" if !*terminal => {
                *terminal = true;
                match event.kind.as_str() {
                    "completed" => result.completed += 1,
                    "failed" => result.failed += 1,
                    _ => result.cancelled += 1,
                }
                if let Some(duration) = event.submission_to_finish_ms {
                    finishes.push(duration);
                }
                if !*admitted && let Some(wait) = &event.wait {
                    waits.push(wait.total_ms());
                    result.wait.pressure += wait.pressure;
                    result.wait.capacity += wait.capacity;
                    result.wait.fifo += wait.fifo;
                    result.wait.running_limit += wait.running_limit;
                    result.wait.admission += wait.admission;
                }
            }
            _ => {}
        }
    }
    result.queue_wait_p50_ms = percentile(&mut waits, 50);
    result.queue_wait_p95_ms = percentile(&mut waits, 95);
    result.submission_to_finish_p50_ms = percentile(&mut finishes, 50);
    result.submission_to_finish_p95_ms = percentile(&mut finishes, 95);
    result
}

fn event_name(name: &str) -> Option<u64> {
    let body = name.strip_prefix("event-")?.strip_suffix(".jsonl")?;
    let mut parts = body.split('-');
    let timestamp = parts.next()?.parse().ok()?;
    parts.next()?.parse::<u32>().ok()?;
    parts.next()?.parse::<u64>().ok()?;
    parts.next().is_none().then_some(timestamp)
}

pub fn read(dir: &Path) -> io::Result<Option<Vec<Event>>> {
    let directory = match fs::symlink_metadata(dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !directory.is_dir() || directory.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "scheduler event directory must be private",
        ));
    }
    #[cfg(unix)]
    if directory.uid() != nix::unistd::Uid::current().as_raw()
        || directory.permissions().mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "scheduler event directory must be private",
        ));
    }
    let marker = dir.join("events.version");
    let metadata = match fs::symlink_metadata(&marker) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "scheduler event marker is not a regular file",
        ));
    }
    #[cfg(unix)]
    if metadata.uid() != nix::unistd::Uid::current().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "scheduler event marker must be private",
        ));
    }
    if fs::read(&marker)? != b"1\n" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported scheduler event version",
        ));
    }
    let mut events = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_none_or(|name| event_name(name).is_none())
        {
            continue;
        }
        if !entry.file_type()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "scheduler event is not a regular file",
            ));
        }
        #[cfg(unix)]
        {
            let metadata = entry.metadata()?;
            if metadata.uid() != nix::unistd::Uid::current().as_raw()
                || metadata.permissions().mode() & 0o077 != 0
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "scheduler event must be private",
                ));
            }
        }
        let file = File::open(entry.path())?;
        for (line_number, line) in BufReader::new(file).lines().enumerate() {
            events.push(serde_json::from_str(&line?).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}:{}: {error}", entry.path().display(), line_number + 1),
                )
            })?);
        }
    }
    Ok(Some(events))
}

#[cfg(unix)]
pub fn now_ms() -> io::Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_millis()
        .try_into()
        .map_err(io::Error::other)
}

#[cfg(unix)]
pub fn session() -> io::Result<String> {
    let mut bytes = [0_u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let mut session = String::with_capacity(32);
    for byte in bytes {
        write!(&mut session, "{byte:02x}").map_err(io::Error::other)?;
    }
    Ok(session)
}

#[cfg(unix)]
pub fn init(dir: &Path) -> io::Result<()> {
    let path = dir.join("events.version");
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(b"1\n")?;
            file.sync_data()?;
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(&path)?;
            if !metadata.is_file()
                || metadata.file_type().is_symlink()
                || metadata.uid() != nix::unistd::Uid::current().as_raw()
                || metadata.permissions().mode() & 0o077 != 0
                || fs::read(&path)? != b"1\n"
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "scheduler event marker must be private",
                ));
            }
        }
        Err(error) => return Err(error),
    }
    Ok(())
}

#[cfg(unix)]
pub fn save(dir: &Path, event: &Event) -> io::Result<()> {
    let timestamp = now_ms()?;
    let suffix = format!(
        "event-{timestamp}-{}-{}.jsonl",
        std::process::id(),
        NEXT_EVENT.fetch_add(1, Ordering::Relaxed)
    );
    let path = dir.join(&suffix);
    let temporary = dir.join(format!("pending-{suffix}.tmp"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(&temporary)?;
    let publish = (|| {
        serde_json::to_writer(&mut file, event)?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        fs::hard_link(&temporary, &path)
    })();
    drop(file);
    let cleanup = fs::remove_file(&temporary);
    publish?;
    cleanup?;
    prune(dir, timestamp)
}

#[cfg(unix)]
fn prune(dir: &Path, now: u64) -> io::Result<()> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let Some(timestamp) = entry.file_name().to_str().and_then(event_name) else {
            continue;
        };
        if !entry.file_type()?.is_file() {
            continue;
        }
        let metadata = entry.metadata()?;
        if metadata.uid() == nix::unistd::Uid::current().as_raw()
            && metadata.permissions().mode().trailing_zeros() >= 6
        {
            files.push((timestamp, entry.path(), metadata.len()));
        }
    }
    files.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));
    let mut bytes: u64 = files.iter().map(|item| item.2).sum();
    for (timestamp, path, size) in files {
        if timestamp < now.saturating_sub(RETENTION_MS) || bytes > MAX_BYTES {
            fs::remove_file(path)?;
            bytes = bytes.saturating_sub(size);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Event, Wait, summary};
    #[cfg(unix)]
    use super::{init, now_ms, read, save};
    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn event(
        session: &str,
        id: Option<u64>,
        kind: &str,
        at: u64,
        wait: Option<Wait>,
        elapsed: Option<u128>,
    ) -> Event {
        Event {
            session: session.into(),
            id,
            kind: kind.into(),
            at_unix_ms: at,
            wait,
            submission_to_finish_ms: elapsed,
        }
    }

    #[test]
    fn range_counts_only_submitted_requests_and_nonoverlapping_wait() {
        let events = vec![
            event("one", Some(1), "submitted", 100, None, None),
            event(
                "one",
                Some(1),
                "admitted",
                200,
                Some(Wait {
                    pressure: 10,
                    capacity: 20,
                    fifo: 30,
                    running_limit: 40,
                    admission: 5,
                }),
                None,
            ),
            event("one", Some(1), "completed", 300, None, Some(200)),
            event("two", Some(1), "submitted", 150, None, None),
            event(
                "two",
                Some(1),
                "cancelled",
                250,
                Some(Wait {
                    fifo: 50,
                    ..Wait::default()
                }),
                Some(100),
            ),
            event("one", None, "rejected", 160, None, None),
            event("one", Some(2), "submitted", 99, None, None),
            event("one", Some(2), "admitted", 110, Some(Wait::default()), None),
            event("one", Some(2), "completed", 120, None, Some(21)),
        ];
        let report = summary(&events, 100, 300);
        assert_eq!(report.submitted, 2);
        assert_eq!(report.admitted, 1);
        assert_eq!(report.completed, 1);
        assert_eq!(report.cancelled, 1);
        assert_eq!(report.rejected, 1);
        assert_eq!(report.wait.total_ms(), 155);
        assert_eq!(report.wait.fifo, 80);
        assert_eq!(report.queue_wait_p50_ms, Some(50));
        assert_eq!(report.queue_wait_p95_ms, Some(105));
        assert_eq!(report.submission_to_finish_p50_ms, Some(100));
        assert_eq!(report.submission_to_finish_p95_ms, Some(200));
    }

    #[cfg(unix)]
    #[test]
    fn event_history_is_private_and_prunes_only_old_owned_files() {
        let dir = std::env::temp_dir().join(format!(
            "reef-event-test-{}-{}",
            std::process::id(),
            now_ms().unwrap()
        ));
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(read(&dir).unwrap().is_none());
        init(&dir).unwrap();
        assert!(read(&dir).unwrap().unwrap().is_empty());
        let old = dir.join("event-1-1-1.jsonl");
        fs::write(&old, b"{}\n").unwrap();
        fs::set_permissions(&old, fs::Permissions::from_mode(0o600)).unwrap();
        let unrelated = dir.join("notes.txt");
        fs::write(&unrelated, b"keep").unwrap();
        save(
            &dir,
            &event("session", Some(1), "submitted", 100, None, None),
        )
        .unwrap();
        assert!(!old.exists());
        assert!(unrelated.exists());
        assert_eq!(read(&dir).unwrap().unwrap().len(), 1);
        fs::remove_dir_all(dir).unwrap();
    }
}
