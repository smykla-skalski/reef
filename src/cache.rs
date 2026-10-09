use fs2::FileExt;
use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::{AccessFlags, Pid, access};
use serde::{Deserialize, Serialize};
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use signal_hook::iterator::Signals;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use wait4::Wait4;

use crate::cache_impact::{self, Kind};

const MIB: u64 = 1_048_576;
const MAX_CAPTURE: usize = 16 * 1024 * 1024;
const POLL: Duration = Duration::from_millis(50);

#[derive(Clone, Copy)]
pub struct RunOptions<'a> {
    pub command: &'a [String],
    pub inputs: &'a [PathBuf],
    pub ttl_seconds: u64,
    pub max_storage_mib: u64,
    pub cache_dir: Option<&'a Path>,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    version: u8,
    created_unix_seconds: u64,
    ttl_seconds: u64,
    stdout_hash: String,
    stderr_hash: String,
    stdout_len: u64,
    stderr_len: u64,
}

struct Output {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    complete: bool,
    code: u8,
    interrupted: bool,
    wall_ms: u128,
    cpu_ms: u128,
}

struct CachedOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

pub(crate) fn cache_path(override_path: Option<&Path>) -> io::Result<PathBuf> {
    if let Some(path) = override_path {
        return Ok(path.to_path_buf());
    }
    let home = std::env::var_os("HOME").ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "HOME is not set; pass --cache-dir")
    })?;
    Ok(PathBuf::from(home).join(".local/state/reef/cache"))
}

fn private_dir(path: &Path) -> io::Result<()> {
    if !path.exists() {
        fs::create_dir_all(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.mode() & 0o077 != 0
        || metadata.uid() != nix::unistd::Uid::current().as_raw()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cache directory must be private and owned by the current user",
        ));
    }
    Ok(())
}

fn open_lock(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.mode() & 0o077 != 0
        || metadata.uid() != nix::unistd::Uid::current().as_raw()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cache lock must be a private owned file",
        ));
    }
    Ok(file)
}

fn frame(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn hash_file(hasher: &mut blake3::Hasher, path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        frame(hasher, path.as_os_str().as_bytes());
        let target = fs::read_link(path)?;
        frame(hasher, target.as_os_str().as_bytes());
        match fs::canonicalize(path) {
            Ok(resolved) => hash_file(hasher, &resolved)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => frame(hasher, b"broken"),
            Err(error) => return Err(error),
        }
        return Ok(());
    }
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("cache input is not a regular file: {}", path.display()),
        ));
    }
    frame(hasher, path.as_os_str().as_bytes());
    frame(hasher, &metadata.mode().to_le_bytes());
    frame(hasher, &metadata.len().to_le_bytes());
    let mut file = File::open(path)?;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(())
}

fn command_path(command: &str, cwd: &Path) -> io::Result<PathBuf> {
    let path = Path::new(command);
    if path.components().count() > 1 {
        return fs::canonicalize(cwd.join(path));
    }
    let variable = std::env::var_os("PATH").unwrap_or_default();
    for directory in std::env::split_paths(&variable) {
        let candidate = directory.join(command);
        if candidate.is_file() && access(&candidate, AccessFlags::X_OK).is_ok() {
            return fs::canonicalize(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("cache command executable not found: {command}"),
    ))
}

fn toolchain(hasher: &mut blake3::Hasher, command: &str) -> io::Result<()> {
    let tools: &[(&str, &[&str])] = match command {
        "cargo" | "rustc" => &[
            ("cargo", &["--version"]),
            ("rustc", &["--version", "--verbose"]),
        ],
        "go" => &[("go", &["version"])],
        "golangci-lint" => &[("golangci-lint", &["version"])],
        "mise" => &[("mise", &["--version"])],
        _ => &[],
    };
    for (name, args) in tools {
        let result = Command::new(name).args(*args).output()?;
        if !result.status.success() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("cannot fingerprint toolchain: {name}"),
            ));
        }
        frame(hasher, name.as_bytes());
        frame(hasher, &result.stdout);
        frame(hasher, &result.stderr);
    }
    Ok(())
}

fn key(command: &[String], inputs: &[PathBuf]) -> io::Result<String> {
    let cwd = fs::canonicalize(std::env::current_dir()?)?;
    let root = git_root(&cwd)?;
    let mut hasher = blake3::Hasher::new();
    frame(&mut hasher, b"reef-output-cache-v1");
    frame(&mut hasher, std::env::consts::OS.as_bytes());
    frame(&mut hasher, std::env::consts::ARCH.as_bytes());
    frame(&mut hasher, root.as_os_str().as_bytes());
    frame(&mut hasher, cwd.as_os_str().as_bytes());
    let executable = command_path(&command[0], &cwd)?;
    hash_file(&mut hasher, &executable)?;
    for arg in command {
        frame(&mut hasher, arg.as_bytes());
    }
    let tool = Path::new(&command[0])
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    toolchain(&mut hasher, tool)?;
    let mut environment: Vec<_> = std::env::vars_os().collect();
    environment.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, value) in environment {
        frame(&mut hasher, name.as_bytes());
        frame(&mut hasher, value.as_bytes());
    }
    hash_repository(&mut hasher, &root)?;
    let mut external: Vec<_> = inputs
        .iter()
        .map(fs::canonicalize)
        .collect::<io::Result<_>>()?;
    external.sort();
    for path in external {
        hash_file(&mut hasher, &path)?;
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn hash_repository(hasher: &mut blake3::Hasher, root: &Path) -> io::Result<()> {
    frame(hasher, root.as_os_str().as_bytes());
    for args in [
        &["rev-parse", "--verify", "HEAD"][..],
        &["symbolic-ref", "-q", "HEAD"][..],
        &["ls-files", "-s", "-z"][..],
        &["config", "--list", "--local", "--null"][..],
    ] {
        let output = Command::new("git").args(args).current_dir(root).output()?;
        frame(hasher, &[u8::from(output.status.success())]);
        frame(hasher, &output.stdout);
    }
    let files = Command::new("git")
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .current_dir(root)
        .output()?;
    if !files.status.success() {
        return Err(io::Error::other("cannot list Git worktree inputs"));
    }
    let mut tracked: Vec<_> = files
        .stdout
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .map(|name| PathBuf::from(OsStr::from_bytes(name)))
        .collect();
    tracked.sort();
    for relative in tracked {
        let path = root.join(&relative);
        frame(hasher, relative.as_os_str().as_bytes());
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {
                let nested = fs::canonicalize(&path)?;
                if git_root(&nested).is_ok_and(|found| found == nested) {
                    hash_repository(hasher, &nested)?;
                } else {
                    hash_directory(hasher, &nested)?;
                }
            }
            Ok(_) => hash_file(hasher, &path)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                frame(hasher, b"deleted");
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn hash_directory(hasher: &mut blake3::Hasher, path: &Path) -> io::Result<()> {
    frame(hasher, path.as_os_str().as_bytes());
    let mut entries = fs::read_dir(path)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            hash_directory(hasher, &path)?;
        } else {
            hash_file(hasher, &path)?;
        }
    }
    Ok(())
}

fn git_root(cwd: &Path) -> io::Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()?;
    if !output.status.success() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cache run requires a Git worktree",
        ));
    }
    fs::canonicalize(String::from_utf8_lossy(&output.stdout).trim())
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn entry_path(root: &Path, key: &str) -> PathBuf {
    root.join("entries").join(key)
}

fn load_entry(path: &Path, requested_ttl: Option<u64>) -> io::Result<Option<CachedOutput>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "invalid cache entry",
        ));
    }
    let entry: Entry = match fs::read(path.join("entry.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    {
        Some(entry) => entry,
        None => return Ok(None),
    };
    if entry.version != 1
        || now_seconds().saturating_sub(entry.created_unix_seconds)
            >= entry.ttl_seconds.min(requested_ttl.unwrap_or(u64::MAX))
        || entry.stdout_len > MAX_CAPTURE as u64
        || entry.stderr_len > MAX_CAPTURE as u64
    {
        return Ok(None);
    }
    let stdout = match fs::read(path.join("stdout")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let stderr = match fs::read(path.join("stderr")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if stdout.len() as u64 != entry.stdout_len
        || stderr.len() as u64 != entry.stderr_len
        || blake3::hash(&stdout).to_hex().as_str() != entry.stdout_hash
        || blake3::hash(&stderr).to_hex().as_str() != entry.stderr_hash
    {
        return Ok(None);
    }
    Ok(Some(CachedOutput { stdout, stderr }))
}

fn private_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn publish(root: &Path, key: &str, ttl_seconds: u64, output: &Output) -> io::Result<()> {
    let staging = root
        .join("entries")
        .join(format!(".{key}.{}", std::process::id()));
    fs::create_dir(&staging)?;
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o700))?;
    let entry = Entry {
        version: 1,
        created_unix_seconds: now_seconds(),
        ttl_seconds,
        stdout_hash: blake3::hash(&output.stdout).to_hex().to_string(),
        stderr_hash: blake3::hash(&output.stderr).to_hex().to_string(),
        stdout_len: output.stdout.len() as u64,
        stderr_len: output.stderr.len() as u64,
    };
    let result = (|| {
        private_write(&staging.join("stdout"), &output.stdout)?;
        private_write(&staging.join("stderr"), &output.stderr)?;
        private_write(&staging.join("entry.json"), &serde_json::to_vec(&entry)?)?;
        fs::rename(&staging, entry_path(root, key))
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn copy_stream<R: Read, W: Write>(mut input: R, mut output: W) -> io::Result<(Vec<u8>, bool)> {
    let mut captured = Vec::new();
    let mut complete = true;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        output.write_all(&buffer[..count])?;
        if captured.len().saturating_add(count) <= MAX_CAPTURE {
            captured.extend_from_slice(&buffer[..count]);
        } else {
            complete = false;
        }
    }
    output.flush()?;
    Ok((captured, complete))
}

fn forward(pid: u32, signal: Signal) -> io::Result<()> {
    let pid = Pid::from_raw(i32::try_from(pid).map_err(io::Error::other)?);
    match killpg(pid, signal) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(error) => Err(io::Error::other(error)),
    }
}

fn group_exists(pid: u32) -> io::Result<bool> {
    let pid = Pid::from_raw(i32::try_from(pid).map_err(io::Error::other)?);
    match killpg(pid, None) {
        Ok(()) => Ok(true),
        Err(Errno::ESRCH) => Ok(false),
        Err(error) => Err(io::Error::other(error)),
    }
}

fn execute(command: &[String], signals: &mut Signals) -> io::Result<Output> {
    let start = Instant::now();
    let mut process = Command::new(&command[0]);
    process
        .args(&command[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = process.spawn()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let out_thread = thread::spawn(move || copy_stream(stdout, io::stdout().lock()));
    let err_thread = thread::spawn(move || copy_stream(stderr, io::stderr().lock()));
    let mut interrupted = false;
    let mut interrupted_at = None;
    let status = loop {
        for number in signals.pending() {
            if let Ok(signal) = Signal::try_from(number) {
                forward(child.id(), signal)?;
                interrupted = true;
                interrupted_at.get_or_insert_with(Instant::now);
            }
        }
        if let Some(result) = child.try_wait4()? {
            break result;
        }
        if interrupted_at.is_some_and(|at: Instant| at.elapsed() >= Duration::from_secs(2)) {
            forward(child.id(), Signal::SIGKILL)?;
        }
        thread::sleep(POLL);
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    while group_exists(child.id())? && Instant::now() < deadline {
        thread::sleep(POLL);
    }
    if group_exists(child.id())? {
        forward(child.id(), Signal::SIGKILL)?;
    }
    let (stdout, out_complete) = out_thread
        .join()
        .map_err(|_| io::Error::other("stdout reader panicked"))??;
    let (stderr, err_complete) = err_thread
        .join()
        .map_err(|_| io::Error::other("stderr reader panicked"))??;
    let code = status.status.code().map_or_else(
        || 128_u8.saturating_add(u8::try_from(status.status.signal().unwrap_or(1)).unwrap_or(1)),
        |code| u8::try_from(code).unwrap_or(1),
    );
    Ok(Output {
        stdout,
        stderr,
        complete: out_complete && err_complete,
        code,
        interrupted,
        wall_ms: start.elapsed().as_millis(),
        cpu_ms: (status.rusage.utime + status.rusage.stime).as_millis(),
    })
}

pub fn run(options: RunOptions<'_>) -> io::Result<u8> {
    let root = cache_path(options.cache_dir)?;
    private_dir(&root)?;
    let root = fs::canonicalize(root)?;
    let worktree = git_root(&std::env::current_dir()?)?;
    if root.starts_with(worktree) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cache directory must be outside the current worktree",
        ));
    }
    private_dir(&root.join("entries"))?;
    private_dir(&root.join("locks"))?;
    let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP, SIGQUIT])?;
    let mut attempts = 0;
    loop {
        let digest = key(options.command, options.inputs)?;
        let lock = open_lock(&root.join("locks").join(&digest))?;
        let mut waited = false;
        loop {
            match lock.try_lock_exclusive() {
                Ok(()) => break,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    waited = true;
                    if signals.pending().next().is_some() {
                        return Ok(130);
                    }
                    thread::sleep(POLL);
                }
                Err(error) => return Err(error),
            }
        }
        if digest != key(options.command, options.inputs)? {
            attempts += 1;
            if attempts >= 3 {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "cache inputs changed while waiting",
                ));
            }
            continue;
        }
        let path = entry_path(&root, &digest);
        if let Some(cached) = load_entry(&path, Some(options.ttl_seconds))? {
            io::stdout().write_all(&cached.stdout)?;
            io::stderr().write_all(&cached.stderr)?;
            eprintln!("reef: cache hit");
            cache_impact::record(
                &root,
                &digest,
                if waited { Kind::Shared } else { Kind::Hit },
                None,
            );
            let _ = OpenOptions::new()
                .read(true)
                .open(path.join("entry.json"))?
                .set_modified(SystemTime::now());
            return Ok(0);
        }
        let code = execute_and_cache(&root, &digest, options, &mut signals)?;
        drop(lock);
        if let Err(error) = prune(Some(&root), options.max_storage_mib) {
            eprintln!("reef: cannot prune cache: {error}");
        }
        return Ok(code);
    }
}

fn execute_and_cache(
    root: &Path,
    digest: &str,
    options: RunOptions<'_>,
    signals: &mut Signals,
) -> io::Result<u8> {
    let path = entry_path(root, digest);
    if path.exists() {
        fs::remove_dir_all(&path)?;
    }
    eprintln!("reef: cache miss");
    let output = match execute(options.command, signals) {
        Ok(output) => output,
        Err(error) => {
            cache_impact::record(root, digest, Kind::Uncacheable, None);
            return Err(error);
        }
    };
    let mut reusable = false;
    if output.code == 0 && !output.interrupted && output.complete {
        match key(options.command, options.inputs) {
            Ok(after) if digest == after => {
                match publish(root, digest, options.ttl_seconds, &output) {
                    Ok(()) => reusable = true,
                    Err(error) => eprintln!("reef: cannot save cache entry: {error}"),
                }
            }
            Ok(_) => {
                eprintln!("reef: cache inputs changed during execution; result not stored");
            }
            Err(error) => eprintln!("reef: cannot recheck cache inputs: {error}"),
        }
    }
    cache_impact::record(
        root,
        digest,
        if reusable {
            Kind::Miss
        } else {
            Kind::Uncacheable
        },
        Some((output.wall_ms, output.cpu_ms)),
    );
    Ok(output.code)
}

fn entries(root: &Path) -> io::Result<Vec<(PathBuf, u64, SystemTime)>> {
    let mut found = Vec::new();
    for item in fs::read_dir(root.join("entries"))? {
        let item = item?;
        let path = item.path();
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let bytes = ["entry.json", "stdout", "stderr"]
            .into_iter()
            .filter_map(|name| fs::metadata(path.join(name)).ok().map(|m| m.len()))
            .sum();
        let used_at = fs::metadata(path.join("entry.json"))
            .and_then(|entry| entry.modified())
            .unwrap_or(UNIX_EPOCH);
        found.push((path, bytes, used_at));
    }
    Ok(found)
}

pub fn status(cache_dir: Option<&Path>) -> io::Result<()> {
    let root = cache_path(cache_dir)?;
    private_dir(&root)?;
    private_dir(&root.join("entries"))?;
    let found = entries(&root)?;
    let bytes: u64 = found.iter().map(|item| item.1).sum();
    println!("{{\"entries\":{},\"bytes\":{bytes}}}", found.len());
    Ok(())
}

pub fn prune(cache_dir: Option<&Path>, max_storage_mib: u64) -> io::Result<()> {
    let root = cache_path(cache_dir)?;
    private_dir(&root)?;
    private_dir(&root.join("entries"))?;
    private_dir(&root.join("locks"))?;
    let global = open_lock(&root.join("prune.lock"))?;
    global.lock_exclusive()?;
    let mut found = entries(&root)?;
    found.sort_by_key(|item| item.2);
    let mut size: u64 = found.iter().map(|item| item.1).sum();
    let limit = max_storage_mib.saturating_mul(MIB);
    for (path, bytes, _) in found {
        let Some(name) = path.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        if name.len() != 64 || !name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        let lock = open_lock(&root.join("locks").join(name))?;
        if lock.try_lock_exclusive().is_err() {
            continue;
        }
        let expired = load_entry(&path, None)?.is_none();
        if expired || size > limit {
            fs::remove_dir_all(&path)?;
            size = size.saturating_sub(bytes);
        }
    }
    Ok(())
}
