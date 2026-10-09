use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use serde::Serialize;
#[cfg(unix)]
use std::fs::OpenOptions;
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(unix)]
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
const RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1000;
#[cfg(unix)]
const MAX_BYTES: u64 = 100 * 1024 * 1024;

#[cfg(unix)]
static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

pub fn default_dir() -> io::Result<PathBuf> {
    #[cfg(windows)]
    let home = env::var_os("HOME").or_else(|| env::var_os("USERPROFILE"));
    #[cfg(not(windows))]
    let home = env::var_os("HOME");
    let home = home.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    Ok(PathBuf::from(home).join(".local/state/reef/history"))
}

fn owned_file_name(name: &str) -> Option<u64> {
    let body = name.strip_prefix("command-")?.strip_suffix(".jsonl")?;
    let mut parts = body.split('-');
    let timestamp = parts.next()?.parse::<u64>().ok()?;
    parts.next()?.parse::<u32>().ok()?;
    parts.next()?.parse::<u64>().ok()?;
    parts.next().is_none().then_some(timestamp)
}

fn private_dir(path: &Path, create: bool) -> io::Result<bool> {
    if create {
        #[cfg(unix)]
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
        #[cfg(not(unix))]
        fs::create_dir_all(path)?;
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if !create && error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "history directory must be a private directory, not a symlink",
        ));
    }
    #[cfg(unix)]
    if metadata.uid() != nix::unistd::Uid::current().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "history directory must be owned by this user and private",
        ));
    }
    Ok(true)
}

fn owned_files(dir: &Path) -> io::Result<Vec<(u64, PathBuf, u64)>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let Some(timestamp) = entry.file_name().to_str().and_then(owned_file_name) else {
            continue;
        };
        if !entry.file_type()?.is_file() {
            continue;
        }
        let metadata = entry.metadata()?;
        #[cfg(unix)]
        if metadata.uid() != nix::unistd::Uid::current().as_raw()
            || metadata.permissions().mode() & 0o077 != 0
        {
            continue;
        }
        files.push((timestamp, entry.path(), metadata.len()));
    }
    files.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));
    Ok(files)
}

pub fn paths() -> io::Result<Option<Vec<PathBuf>>> {
    let dir = default_dir()?;
    if !private_dir(&dir, false)? {
        return Ok(None);
    }
    private_dir(dir.parent().expect("history has a parent"), false)?;
    Ok(Some(
        owned_files(&dir)?
            .into_iter()
            .map(|(_, path, _)| path)
            .collect(),
    ))
}

#[cfg(unix)]
fn now_ms() -> io::Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_millis()
        .try_into()
        .map_err(io::Error::other)
}

#[cfg(unix)]
fn prune(dir: &Path, now: u64) -> io::Result<()> {
    let cutoff = now.saturating_sub(RETENTION_MS);
    let files = owned_files(dir)?;
    let mut total: u64 = files.iter().map(|(_, _, size)| size).sum();
    for (timestamp, path, size) in files {
        if timestamp < cutoff || total > MAX_BYTES {
            fs::remove_file(path)?;
            total = total.saturating_sub(size);
        }
    }
    Ok(())
}

#[cfg(unix)]
pub fn save<T: Serialize>(record: &T) -> io::Result<()> {
    let dir = default_dir()?;
    private_dir(&dir, true)?;
    private_dir(dir.parent().expect("history has a parent"), false)?;
    let now = now_ms()?;
    let suffix = format!(
        "{now}-{}-{}",
        std::process::id(),
        NEXT_FILE.fetch_add(1, Ordering::Relaxed)
    );
    let temporary = dir.join(format!("pending-{suffix}.tmp"));
    let final_path = dir.join(format!("command-{suffix}.jsonl"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let write_result = (|| {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        file.write_all(&line)?;
        file.sync_data()?;
        fs::hard_link(&temporary, &final_path)?;
        Ok::<(), io::Error>(())
    })();
    drop(file);
    let cleanup_result = fs::remove_file(&temporary);
    write_result?;
    cleanup_result?;
    prune(&dir, now)
}

#[cfg(all(test, unix))]
mod tests {
    use super::{MAX_BYTES, RETENTION_MS, prune};
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

    fn fixture() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "reef-history-unit-{}-{}",
            std::process::id(),
            NEXT_TEST.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn old_history_is_removed_without_touching_unrelated_files() {
        let dir = fixture();
        let old = dir.join("command-1-1-1.jsonl");
        let unrelated = dir.join("personal-notes.txt");
        let symlink_path = dir.join("command-1-1-2.jsonl");
        fs::write(&old, "old").unwrap();
        fs::set_permissions(&old, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&unrelated, "keep").unwrap();
        symlink(&unrelated, &symlink_path).unwrap();

        prune(&dir, RETENTION_MS + 2).unwrap();

        assert!(!old.exists());
        assert!(unrelated.exists());
        assert!(symlink_path.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn size_limit_removes_oldest_history_only() {
        let dir = fixture();
        let older = dir.join("command-1000-1-1.jsonl");
        let newer = dir.join("command-1001-1-1.jsonl");
        fs::write(&older, "").unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&older)
            .unwrap()
            .set_len(MAX_BYTES)
            .unwrap();
        fs::set_permissions(&older, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&newer, "new").unwrap();
        fs::set_permissions(&newer, fs::Permissions::from_mode(0o600)).unwrap();

        prune(&dir, 1001).unwrap();

        assert!(!older.exists());
        assert_eq!(fs::read(&newer).unwrap(), b"new");
        fs::remove_dir_all(dir).unwrap();
    }
}
