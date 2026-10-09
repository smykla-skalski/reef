use crate::run::RecordMode;
use crate::schedule;
use clap::Args;
use fs2::FileExt;
use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use signal_hook::iterator::Signals;
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MAX_FILES: usize = 10_000;
const MAX_BYTES: u64 = 100 * 1024 * 1024;
const CACHE_TTL: Duration = Duration::from_hours(168);
const CACHE_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;

#[derive(Debug, Args)]
pub struct Options {
    /// Private JSON policy file. Remote use must also be enabled in that file.
    #[arg(long)]
    config: PathBuf,
    /// Repository worktree whose source will be uploaded.
    #[arg(long)]
    repo: Option<PathBuf>,
    /// Explicitly include an untracked path in this job.
    #[arg(long)]
    include_untracked: Vec<PathBuf>,
    /// Confirm the displayed snapshot path list and trust boundary.
    #[arg(long)]
    approve_snapshot: bool,
    /// Return a previously disconnected job's state.
    #[arg(long, conflicts_with_all = ["cancel_job", "cleanup_job"])]
    status_job: Option<String>,
    /// Stop a job and verify that its process ended.
    #[arg(long)]
    cancel_job: Option<String>,
    /// Remove a terminal job's source after reconnecting.
    #[arg(long, conflicts_with = "cancel_job")]
    cleanup_job: Option<String>,
    /// Estimates used only if a pre-launch failure falls back locally.
    #[arg(long, default_value_t = 1)]
    cpu: u32,
    #[arg(long, default_value_t = 1024)]
    memory_mib: u64,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long, default_value = "build")]
    category: String,
    #[arg(long, default_value = "command")]
    identity: String,
    #[arg(required_unless_present_any = ["status_job", "cancel_job", "cleanup_job"], trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Debug, Args)]
pub struct WorkerOptions {
    #[arg(long)]
    root: PathBuf,
    #[arg(long)]
    job: String,
    #[arg(long, value_parser = ["prepare", "verify", "execute", "status", "cancel", "cleanup"])]
    action: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    enabled: bool,
    target: String,
    identity_file: PathBuf,
    known_hosts_file: PathBuf,
    worker_root: PathBuf,
    #[serde(default = "default_worker_binary")]
    worker_binary: String,
    repository: String,
    toolchain: String,
    os: String,
    arch: String,
    allowed_commands: Vec<AllowedCommand>,
    #[serde(default)]
    local_fallback: bool,
}

fn default_worker_binary() -> String {
    "reef".into()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AllowedCommand {
    identity: String,
    argv: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct FileEntry {
    path: String,
    bytes: u64,
    sha256: String,
}

#[derive(Serialize, Deserialize)]
struct JobManifest {
    repository: String,
    toolchain: String,
    os: String,
    arch: String,
    command: Vec<String>,
    files: Vec<FileEntry>,
}

struct Snapshot {
    path: PathBuf,
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub fn submit(options: &Options) -> io::Result<u8> {
    let policy = load_policy(&options.config)?;
    if let Some(job) = options.status_job.as_deref() {
        let output = ssh(&policy, job, "status")?;
        io::stdout().write_all(&output.stdout)?;
        io::stderr().write_all(&output.stderr)?;
        return Ok(output.status.code().map_or(1, exit_code));
    }
    if let Some(job) = options.cancel_job.as_deref() {
        let output = ssh(&policy, job, "cancel")?;
        io::stdout().write_all(&output.stdout)?;
        io::stderr().write_all(&output.stderr)?;
        return Ok(output.status.code().map_or(1, exit_code));
    }
    if let Some(job) = options.cleanup_job.as_deref() {
        let output = ssh(&policy, job, "cleanup")?;
        io::stdout().write_all(&output.stdout)?;
        io::stderr().write_all(&output.stderr)?;
        return Ok(output.status.code().map_or(1, exit_code));
    }
    if !options.approve_snapshot {
        return Err(invalid("remote upload requires --approve-snapshot"));
    }
    if !matches!(options.category.as_str(), "build" | "lint" | "test") {
        return Err(invalid("remote work is limited to build, lint, and test"));
    }
    if !policy
        .allowed_commands
        .iter()
        .any(|allowed| allowed.identity == options.identity && allowed.argv == options.command)
        || options.command.is_empty()
        || !Path::new(&options.command[0]).is_absolute()
    {
        return Err(invalid(
            "command and identity are absent from the remote allowlist",
        ));
    }
    let repo = options
        .repo
        .as_deref()
        .ok_or_else(|| invalid("pass --repo"))?;
    let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP, SIGQUIT])?;
    let snapshot = make_snapshot(repo, &options.include_untracked, &policy, &options.command)?;
    if signals.pending().next().is_some() {
        return Ok(130);
    }
    let manifest: JobManifest =
        serde_json::from_reader(File::open(snapshot.path.join("job.json"))?)?;
    let total: u64 = manifest.files.iter().map(|file| file.bytes).sum();
    eprintln!(
        "reef: approved remote snapshot: {} files, {total} bytes",
        manifest.files.len()
    );
    for file in &manifest.files {
        eprintln!("  {}", file.path);
    }
    let job = format!("{:x}{:x}", std::process::id(), now_nanos());
    let mut attempted_remote = false;
    let prelaunch = (|| {
        validate_credentials(&policy)?;
        attempted_remote = true;
        run_prelaunch(
            ssh_command(&policy, &job, "prepare"),
            &mut signals,
            "remote prepare",
        )?;
        sync(&policy, &job, &snapshot.path, &mut signals)?;
        run_prelaunch(
            ssh_command(&policy, &job, "verify"),
            &mut signals,
            "remote verification",
        )
    })();
    if let Err(error) = prelaunch {
        eprintln!("reef: remote job {job} did not launch: {error}");
        if attempted_remote {
            let cleanup = ssh(&policy, &job, "cleanup");
            if !cleanup.as_ref().is_ok_and(|output| output.status.success()) {
                eprintln!("reef: remote cleanup unconfirmed for job {job}");
            }
        }
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(130);
        }
        if policy.local_fallback {
            eprintln!("reef: requesting fresh local scheduler admission");
            let limits = crate::contain::Limits::default();
            return schedule::schedule(schedule::RunOptions {
                command: &options.command,
                category: &options.category,
                identity: &options.identity,
                record: RecordMode::Default,
                cpu: options.cpu,
                memory_mib: options.memory_mib,
                state_dir: options.state_dir.as_deref(),
                limits: &limits,
            });
        }
        return Err(error);
    }
    execute_remote(&policy, &job)
}

fn execute_remote(policy: &Policy, job: &str) -> io::Result<u8> {
    eprintln!("reef: remote job {job} launched; use --status-job or --cancel-job to reconcile");
    let mut child = ssh_command(policy, job, "execute")
        .stdin(Stdio::null())
        .spawn()?;
    let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP, SIGQUIT])?;
    loop {
        if signals.pending().next().is_some() {
            let cancelled = ssh(policy, job, "cancel")?;
            if !cancelled.status.success() {
                return Err(io::Error::other(format!(
                    "remote cancellation unconfirmed for job {job}"
                )));
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while child.try_wait()?.is_none() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            if child.try_wait()?.is_none() {
                child.kill()?;
                let _ = child.wait();
            }
            eprintln!("reef: remote job {job} cancelled; process termination confirmed");
            return Ok(130);
        }
        if let Some(status) = child.try_wait()? {
            let output = ssh(policy, job, "status").map_err(|error| {
                io::Error::new(error.kind(), format!("remote job {job} state unknown after SSH exit {status}; reconcile by job ID: {error}"))
            })?;
            if !output.status.success() {
                return Err(io::Error::other(format!(
                    "remote job {job} state unknown after SSH exit {status}; status query failed"
                )));
            }
            let remote_state = String::from_utf8_lossy(&output.stdout);
            let code = status
                .code()
                .map_or(1, |code| u8::try_from(code).unwrap_or(1));
            if !matches!(
                remote_state.trim(),
                state if state == format!("succeeded {code}")
                    || state == format!("failed {code}")
                    || (state == "cancelled" && code == 130)
            ) {
                return Err(io::Error::other(format!(
                    "remote job {job} state unknown; worker reported {}",
                    remote_state.trim()
                )));
            }
            check(&ssh(policy, job, "cleanup")?, "remote cleanup")?;
            return Ok(code);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn load_policy(path: &Path) -> io::Result<Policy> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.mode() & 0o077 != 0 {
        return Err(invalid("remote policy must be a private regular file"));
    }
    let policy: Policy = serde_json::from_reader(File::open(path)?)?;
    if !policy.enabled {
        return Err(invalid("remote offload is disabled in the policy"));
    }
    if !policy
        .target
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"@._-".contains(&byte))
        || !policy.target.contains('@')
        || policy.worker_binary.is_empty()
        || !policy
            .worker_binary
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/._-".contains(&byte))
        || !policy.worker_root.is_absolute()
        || policy.repository.is_empty()
        || policy.toolchain.is_empty()
    {
        return Err(invalid("invalid SSH worker policy"));
    }
    Ok(policy)
}

fn validate_credentials(policy: &Policy) -> io::Result<()> {
    for (name, path) in [
        ("SSH identity", &policy.identity_file),
        ("pinned known-hosts", &policy.known_hosts_file),
    ] {
        let metadata = fs::symlink_metadata(path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("{name} file {} unavailable: {error}", path.display()),
            )
        })?;
        if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.mode() & 0o077 != 0
        {
            return Err(invalid(
                "SSH identity and pinned known-hosts files must be private regular files",
            ));
        }
    }
    let hosts = fs::read_to_string(&policy.known_hosts_file)?;
    if hosts.lines().filter(|line| !line.trim().is_empty()).count() != 1 {
        return Err(invalid(
            "known-hosts file must contain exactly one pinned host key",
        ));
    }
    Ok(())
}

fn ssh_command(policy: &Policy, job: &str, action: &str) -> Command {
    let mut command = Command::new("ssh");
    command.process_group(0);
    command.args([
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        "ForwardAgent=no",
        "-o",
        "IdentityAgent=none",
        "-o",
        "IdentitiesOnly=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "GlobalKnownHostsFile=/dev/null",
        "-o",
        "PasswordAuthentication=no",
        "-o",
        "KbdInteractiveAuthentication=no",
        "-o",
        "ControlMaster=no",
        "-i",
    ]);
    command.arg(&policy.identity_file);
    command.arg("-o").arg(format!(
        "UserKnownHostsFile={}",
        policy.known_hosts_file.display()
    ));
    command.arg("--").arg(&policy.target);
    command.arg(format!(
        "{} remote-worker --root {} --job {} --action {}",
        shell_quote(&policy.worker_binary),
        shell_quote(&policy.worker_root.to_string_lossy()),
        shell_quote(job),
        shell_quote(action),
    ));
    command
}

fn ssh(policy: &Policy, job: &str, action: &str) -> io::Result<std::process::Output> {
    validate_job_id(job)?;
    ssh_command(policy, job, action).output()
}

fn sync(policy: &Policy, job: &str, snapshot: &Path, signals: &mut Signals) -> io::Result<()> {
    let source_remote = format!(
        "{}:{}/jobs/{job}/source/",
        policy.target,
        policy.worker_root.display()
    );
    sync_path(
        policy,
        &format!("{}/", snapshot.join("source").display()),
        &source_remote,
        signals,
    )?;
    let manifest_remote = format!(
        "{}:{}/jobs/{job}/job.json",
        policy.target,
        policy.worker_root.display()
    );
    sync_path(
        policy,
        &snapshot.join("job.json").to_string_lossy(),
        &manifest_remote,
        signals,
    )
}

fn sync_path(
    policy: &Policy,
    source: &str,
    destination: &str,
    signals: &mut Signals,
) -> io::Result<()> {
    let mut command = Command::new("rsync");
    command.process_group(0);
    command.args(["--archive", "--delete", "--checksum", "--protect-args"]);
    command.arg("-e");
    command.arg(format!(
        "ssh -o BatchMode=yes -o StrictHostKeyChecking=yes -o ForwardAgent=no -o IdentityAgent=none -o IdentitiesOnly=yes -o GlobalKnownHostsFile=/dev/null -o PasswordAuthentication=no -o KbdInteractiveAuthentication=no -i {} -o UserKnownHostsFile={}",
        shell_quote(&policy.identity_file.to_string_lossy()),
        shell_quote(&policy.known_hosts_file.to_string_lossy())
    ));
    command.arg("--");
    command.arg(source);
    command.arg(destination);
    run_prelaunch(command, signals, "snapshot transfer")
}

fn run_prelaunch(mut command: Command, signals: &mut Signals, stage: &str) -> io::Result<()> {
    let mut child = command
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    loop {
        if signals.pending().next().is_some() {
            stop_group(child.id())?;
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                format!("{stage} cancelled"),
            ));
        }
        if let Some(status) = child.try_wait()? {
            return if status.success() {
                Ok(())
            } else {
                Err(io::Error::other(format!(
                    "{stage} failed with status {status}"
                )))
            };
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn check(output: &std::process::Output, stage: &str) -> io::Result<()> {
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{stage} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

fn exit_code(code: i32) -> u8 {
    u8::try_from(code).unwrap_or(1)
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn validate_job_id(job: &str) -> io::Result<()> {
    if job.is_empty() || job.len() > 64 || !job.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Err(invalid("invalid remote job ID"))
    } else {
        Ok(())
    }
}

fn safe_path(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(invalid(
            "snapshot path must be relative and remain inside the worktree",
        ));
    }
    for part in path.components() {
        let name = part.as_os_str().to_string_lossy().to_ascii_lowercase();
        if matches!(
            name.as_str(),
            ".git"
                | ".ssh"
                | ".aws"
                | "target"
                | "node_modules"
                | ".env"
                | "credentials"
                | "secrets"
                | "id_rsa"
        ) || Path::new(&name).extension().is_some_and(|extension| {
            extension.eq_ignore_ascii_case("pem") || extension.eq_ignore_ascii_case("key")
        }) || name.starts_with(".env.")
        {
            return Err(invalid("snapshot includes a sensitive or generated path"));
        }
    }
    Ok(())
}

fn git_paths(repo: &Path, args: &[&str]) -> io::Result<BTreeSet<PathBuf>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()?;
    check(&output, "git file listing")?;
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| PathBuf::from(std::ffi::OsStr::from_bytes(path)))
        .collect())
}

fn reject_symlink_ancestors(root: &Path, relative: &Path) -> io::Result<()> {
    let mut ancestor = root.to_path_buf();
    for component in relative.components() {
        ancestor.push(component.as_os_str());
        if fs::symlink_metadata(&ancestor).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(invalid("snapshot contains a symlink"));
        }
    }
    Ok(())
}

fn make_snapshot(
    repo: &Path,
    included: &[PathBuf],
    policy: &Policy,
    command: &[String],
) -> io::Result<Snapshot> {
    let root = fs::canonicalize(repo)?;
    let top = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["rev-parse", "--show-toplevel"])
        .output()?;
    check(&top, "git root")?;
    if fs::canonicalize(String::from_utf8_lossy(&top.stdout).trim())? != root {
        return Err(invalid("--repo must name the worktree root"));
    }
    let mut paths = git_paths(&root, &["ls-files", "--cached", "-z"])?;
    let untracked = git_paths(&root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    for path in included {
        if !untracked.contains(path) {
            return Err(invalid(
                "selected untracked path is not a single Git-untracked file",
            ));
        }
        paths.insert(path.clone());
    }
    if paths.len() > MAX_FILES {
        return Err(invalid("snapshot exceeds file-count limit"));
    }
    let candidate = std::env::temp_dir().join(format!(
        "reef-snapshot-{:x}{:x}",
        std::process::id(),
        now_nanos()
    ));
    let path = create_private_snapshot_dir(&candidate)?;
    let snapshot = Snapshot { path };
    let source_root = snapshot.path.join("source");
    create_private_snapshot_subdir(&source_root)?;
    let mut files = Vec::new();
    let mut total = 0_u64;
    for relative in paths {
        safe_path(&relative)?;
        reject_symlink_ancestors(&root, &relative)?;
        let source = root.join(&relative);
        let metadata = match fs::symlink_metadata(&source) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(invalid(
                "snapshot contains a symlink or unsupported file type",
            ));
        }
        total = total
            .checked_add(metadata.len())
            .ok_or_else(|| invalid("snapshot size overflow"))?;
        if total > MAX_BYTES {
            return Err(invalid("snapshot exceeds 100 MiB"));
        }
        let destination = source_root.join(&relative);
        create_private_snapshot_parents(&source_root, &relative)?;
        let mut input = File::open(&source)?;
        let mut output = File::create(&destination)?;
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 8192];
        loop {
            let count = input.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
            output.write_all(&buffer[..count])?;
        }
        fs::set_permissions(&destination, metadata.permissions())?;
        let after = fs::symlink_metadata(&source)?;
        if after.len() != metadata.len()
            || after.mtime() != metadata.mtime()
            || after.mtime_nsec() != metadata.mtime_nsec()
        {
            return Err(invalid("source changed during snapshot"));
        }
        files.push(FileEntry {
            path: relative
                .to_str()
                .ok_or_else(|| invalid("non-UTF-8 snapshot path"))?
                .into(),
            bytes: metadata.len(),
            sha256: hex_digest(hasher.finalize()),
        });
    }
    let manifest = JobManifest {
        repository: policy.repository.clone(),
        toolchain: policy.toolchain.clone(),
        os: policy.os.clone(),
        arch: policy.arch.clone(),
        command: command.to_vec(),
        files,
    };
    let manifest_path = snapshot.path.join("job.json");
    serde_json::to_writer(File::create(&manifest_path)?, &manifest)?;
    fs::set_permissions(&manifest_path, fs::Permissions::from_mode(0o600))?;
    Ok(snapshot)
}

fn create_private_snapshot_subdir(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new().mode(0o700).create(path)?;
    if let Err(error) = fs::set_permissions(path, fs::Permissions::from_mode(0o700)) {
        let _ = fs::remove_dir(path);
        return Err(error);
    }
    Ok(())
}

fn create_private_snapshot_parents(source_root: &Path, relative: &Path) -> io::Result<()> {
    let mut directory = source_root.to_path_buf();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            directory.push(component.as_os_str());
            match create_private_snapshot_subdir(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

fn create_private_snapshot_dir(candidate: &Path) -> io::Result<PathBuf> {
    for attempt in 0..128 {
        let path = if attempt == 0 {
            candidate.to_path_buf()
        } else {
            candidate.with_file_name(format!(
                "{}-{attempt:x}",
                candidate
                    .file_name()
                    .expect("snapshot name")
                    .to_string_lossy()
            ))
        };
        match create_private_snapshot_subdir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "cannot reserve a unique snapshot directory",
    ))
}

pub fn worker(options: &WorkerOptions) -> io::Result<u8> {
    validate_job_id(&options.job)?;
    if !options.root.is_absolute() {
        return Err(invalid("worker root must be absolute"));
    }
    let root = &options.root;
    private_worker_root(root)?;
    let jobs = root.join("jobs");
    private_child_dir(&jobs)?;
    let job = jobs.join(&options.job);
    match options.action.as_str() {
        "prepare" => {
            fs::DirBuilder::new().mode(0o700).create(&job)?;
            fs::DirBuilder::new()
                .mode(0o700)
                .create(job.join("source"))?;
            Ok(0)
        }
        "verify" => {
            verify(&job)?;
            Ok(0)
        }
        "execute" => execute(&job, root),
        "status" => {
            println!("{}", fs::read_to_string(job.join("status"))?.trim());
            Ok(0)
        }
        "cancel" => cancel(&job),
        "cleanup" => {
            if fs::read_to_string(job.join("status"))
                .is_ok_and(|status| matches!(status.trim(), "running" | "preparing"))
            {
                return Err(invalid("cannot clean an active remote job"));
            }
            fs::remove_dir_all(job)?;
            Ok(0)
        }
        _ => Err(invalid("invalid worker action")),
    }
}

fn private_worker_root(root: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(root)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.mode() & 0o077 != 0
        || metadata.uid() != nix::unistd::Uid::current().as_raw()
    {
        return Err(invalid(
            "worker root must be a private directory owned by the worker",
        ));
    }
    Ok(())
}

fn private_child_dir(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new().mode(0o700).create(path)?;
        }
        Err(error) => return Err(error),
    }
    private_worker_root(path)
}

fn verify(job: &Path) -> io::Result<JobManifest> {
    let manifest: JobManifest = serde_json::from_reader(File::open(job.join("job.json"))?)?;
    if manifest.os != std::env::consts::OS || manifest.arch != std::env::consts::ARCH {
        return Err(invalid("worker platform does not match job requirement"));
    }
    let toolchain = Command::new("rustc").arg("--version").output()?;
    check(&toolchain, "worker toolchain")?;
    if !String::from_utf8_lossy(&toolchain.stdout).contains(&manifest.toolchain) {
        return Err(invalid("worker toolchain does not match job requirement"));
    }
    if manifest.command.is_empty()
        || manifest.files.len() > MAX_FILES
        || manifest
            .files
            .iter()
            .try_fold(0_u64, |total, file| total.checked_add(file.bytes))
            .is_none_or(|total| total > MAX_BYTES)
    {
        return Err(invalid("worker snapshot exceeds limits"));
    }
    for entry in &manifest.files {
        safe_path(Path::new(&entry.path))?;
        let path = job.join("source").join(&entry.path);
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() != entry.bytes
        {
            return Err(invalid("worker snapshot file differs from manifest"));
        }
        let digest = Sha256::digest(fs::read(path)?);
        if hex_digest(digest) != entry.sha256 {
            return Err(invalid("worker snapshot digest mismatch"));
        }
    }
    Ok(manifest)
}

fn execute(job: &Path, root: &Path) -> io::Result<u8> {
    private_worker_root(job)?;
    private_worker_root(&job.join("source"))?;
    let manifest = verify(job)?;
    let marker = job.join("status");
    let mut marker_file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
        .map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                invalid("duplicate remote launch rejected")
            } else {
                error
            }
        })?;
    marker_file.write_all(b"preparing\n")?;
    let (cache, cache_lock, mut child) = match start_job(job, root, &manifest) {
        Ok(started) => started,
        Err(error) => {
            fs::write(&marker, "failed 126\n")?;
            return Err(error);
        }
    };
    if let Err(error) = fs::write(&marker, "running\n") {
        let _ = stop_group(child.id());
        let _ = child.wait();
        return Err(error);
    }
    let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP, SIGQUIT])?;
    loop {
        if signals.pending().next().is_some() {
            stop_group(child.id())?;
            let _ = child.wait();
            fs::write(&marker, "cancelled\n")?;
            fs::write(cache.join(".last_used"), b"")?;
            drop(cache_lock);
            prune_caches(root, Path::new(""))?;
            return Ok(130);
        }
        if let Some(status) = child.try_wait()? {
            let code = status
                .code()
                .map_or(1, |value| u8::try_from(value).unwrap_or(1));
            let cancelled = job.join("cancel-requested").exists();
            let final_state = if cancelled {
                "cancelled\n".to_owned()
            } else if status.success() {
                format!("succeeded {code}\n")
            } else {
                format!("failed {code}\n")
            };
            fs::write(&marker, final_state)?;
            fs::write(cache.join(".last_used"), b"")?;
            drop(cache_lock);
            prune_caches(root, Path::new(""))?;
            return Ok(if cancelled { 130 } else { code });
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn start_job(
    job: &Path,
    root: &Path,
    manifest: &JobManifest,
) -> io::Result<(PathBuf, File, std::process::Child)> {
    let cache = cache_path(root, manifest)?;
    prune_caches(root, &cache)?;
    private_child_dir(&cache)?;
    let cache_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(cache.join(".lock"))?;
    cache_lock.lock_exclusive()?;
    let mut child = Command::new(&manifest.command[0]);
    child.args(&manifest.command[1..]);
    child.current_dir(job.join("source"));
    child.env_clear();
    child.env("PATH", "/usr/local/bin:/usr/bin:/bin");
    let account = nix::unistd::User::from_uid(nix::unistd::Uid::current())?
        .ok_or_else(|| invalid("worker account is unavailable"))?;
    child.env("HOME", account.dir);
    child.env("CARGO_TARGET_DIR", &cache);
    child.env("REEF_ADMITTED", "1");
    child.process_group(0);
    let mut child = child.spawn()?;
    if let Err(error) = fs::write(job.join("pid"), child.id().to_string()) {
        let _ = stop_group(child.id());
        let _ = child.wait();
        return Err(error);
    }
    Ok((cache, cache_lock, child))
}

fn cache_path(root: &Path, manifest: &JobManifest) -> io::Result<PathBuf> {
    let mut hasher = Sha256::new();
    hasher.update(manifest.repository.as_bytes());
    hasher.update([0]);
    hasher.update(manifest.toolchain.as_bytes());
    hasher.update([0]);
    hasher.update(manifest.os.as_bytes());
    hasher.update([0]);
    hasher.update(manifest.arch.as_bytes());
    hasher.update([0]);
    hasher.update(serde_json::to_vec(&manifest.command)?);
    for file in &manifest.files {
        if matches!(
            Path::new(&file.path)
                .file_name()
                .and_then(|name| name.to_str()),
            Some("Cargo.lock" | "go.sum" | "package-lock.json" | "pnpm-lock.yaml")
        ) {
            hasher.update(file.path.as_bytes());
            hasher.update(file.sha256.as_bytes());
        }
    }
    Ok(root.join("cache").join(hex_digest(hasher.finalize())))
}

fn hex_digest(digest: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = digest.as_ref();
    let mut hex = String::with_capacity(bytes.len().saturating_mul(2));
    for &byte in bytes {
        hex.push(char::from(HEX[usize::from(byte >> 4)]));
        hex.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    hex
}

fn cache_size(path: &Path) -> io::Result<u64> {
    let mut size = 0_u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() {
            size = size.saturating_add(cache_size(&entry.path())?);
        } else if metadata.is_file() {
            size = size.saturating_add(metadata.len());
        }
    }
    Ok(size)
}

fn prune_caches(root: &Path, active: &Path) -> io::Result<()> {
    let cache_root = root.join("cache");
    private_child_dir(&cache_root)?;
    let mut entries = Vec::new();
    let mut total = 0_u64;
    for entry in fs::read_dir(&cache_root)? {
        let path = entry?.path();
        if path == active || !fs::symlink_metadata(&path)?.is_dir() {
            continue;
        }
        let size = cache_size(&path)?;
        total = total.saturating_add(size);
        let modified = fs::metadata(path.join(".last_used"))
            .or_else(|_| fs::metadata(&path))?
            .modified()?;
        entries.push((path, size, modified));
    }
    entries.sort_by_key(|(_, _, modified)| *modified);
    for (path, size, modified) in entries {
        if modified.elapsed().unwrap_or_default() < CACHE_TTL && total <= CACHE_MAX_BYTES {
            continue;
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(path.join(".lock"));
        if let Ok(lock) = lock
            && lock.try_lock_exclusive().is_ok()
        {
            fs::remove_dir_all(&path)?;
            total = total.saturating_sub(size);
        }
    }
    Ok(())
}

fn cancel(job: &Path) -> io::Result<u8> {
    let mut status = fs::read_to_string(job.join("status"))?;
    if status.trim() == "preparing" {
        fs::write(job.join("cancel-requested"), b"")?;
        for _ in 0..200 {
            std::thread::sleep(Duration::from_millis(50));
            status = fs::read_to_string(job.join("status"))?;
            if status.trim() != "preparing" {
                break;
            }
        }
        if status.trim() == "preparing" {
            return Err(io::Error::other("remote preparation state unknown"));
        }
    }
    if status.trim() == "running" {
        fs::write(job.join("cancel-requested"), b"")?;
        let mut pid_file = fs::read_to_string(job.join("pid"));
        for _ in 0..40 {
            if pid_file.is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
            pid_file = fs::read_to_string(job.join("pid"));
        }
        let pid: u32 = pid_file?.trim().parse().map_err(io::Error::other)?;
        stop_group(pid)?;
        for _ in 0..40 {
            if fs::read_to_string(job.join("status"))?.trim() != "running" {
                println!("remote process terminated; cleanup pending");
                return Ok(0);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        return Err(io::Error::other("remote process termination unconfirmed"));
    }
    println!("{}", status.trim());
    Ok(0)
}

fn stop_group(pid: u32) -> io::Result<()> {
    let pid = Pid::from_raw(i32::try_from(pid).map_err(io::Error::other)?);
    match killpg(pid, Signal::SIGKILL) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(error) => Err(io::Error::other(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AllowedCommand, Policy, WorkerOptions, create_private_snapshot_dir, hex_digest,
        make_snapshot, worker,
    };
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "reef-remote-test-{:x}{:x}{:x}",
                std::process::id(),
                super::now_nanos(),
                NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }

        fn repo(&self) -> PathBuf {
            let repo = self.0.join("repo");
            fs::create_dir(&repo).unwrap();
            assert!(
                Command::new("git")
                    .arg("init")
                    .arg(&repo)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
            repo
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn policy() -> Policy {
        Policy {
            enabled: true,
            target: "worker@example.test".into(),
            identity_file: PathBuf::from("unused"),
            known_hosts_file: PathBuf::from("unused"),
            worker_root: PathBuf::from("/unused"),
            worker_binary: "reef".into(),
            repository: "test-repo".into(),
            toolchain: "rustc".into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            allowed_commands: vec![AllowedCommand {
                identity: "test".into(),
                argv: vec!["/bin/sh".into(), "-c".into(), "exit 7".into()],
            }],
            local_fallback: false,
        }
    }

    fn add(repo: &Path, path: &str) {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(["add", "-f", "--", path])
                .output()
                .unwrap()
                .status
                .success()
        );
    }

    #[test]
    fn hex_digest_uses_lowercase_zero_padded_bytes() {
        assert_eq!(hex_digest([0, 15, 16, 255]), "000f10ff");
    }

    #[test]
    fn snapshot_directory_collision_preserves_existing_content() {
        let fixture = Fixture::new();
        let candidate = fixture.0.join("snapshot");
        fs::create_dir(&candidate).unwrap();
        fs::write(candidate.join("existing.txt"), "keep").unwrap();
        fs::create_dir(fixture.0.join("snapshot-1")).unwrap();

        let snapshot = create_private_snapshot_dir(&candidate).unwrap();

        assert_eq!(snapshot, fixture.0.join("snapshot-2"));
        assert_eq!(
            fs::read_to_string(candidate.join("existing.txt")).unwrap(),
            "keep"
        );
        assert_eq!(
            fs::metadata(snapshot).unwrap().permissions().mode() & 0o077,
            0
        );
    }

    #[test]
    fn dirty_snapshot_contains_selected_files_and_deletions() {
        let fixture = Fixture::new();
        let repo = fixture.repo();
        fs::write(repo.join("tracked.txt"), "before").unwrap();
        fs::write(repo.join("deleted.txt"), "old").unwrap();
        add(&repo, "tracked.txt");
        add(&repo, "deleted.txt");
        fs::write(repo.join("tracked.txt"), "after").unwrap();
        fs::remove_file(repo.join("deleted.txt")).unwrap();
        fs::write(repo.join("selected.txt"), "chosen").unwrap();
        fs::write(repo.join("ignored.txt"), "excluded").unwrap();

        let snapshot = make_snapshot(
            &repo,
            &[PathBuf::from("selected.txt")],
            &policy(),
            &["sh".into()],
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(snapshot.path.join("source/tracked.txt")).unwrap(),
            "after"
        );
        assert_eq!(
            fs::read_to_string(snapshot.path.join("source/selected.txt")).unwrap(),
            "chosen"
        );
        assert!(!snapshot.path.join("source/deleted.txt").exists());
        assert!(!snapshot.path.join("source/ignored.txt").exists());
    }

    #[test]
    fn snapshot_rejects_tracked_sensitive_path() {
        let fixture = Fixture::new();
        let repo = fixture.repo();
        fs::write(repo.join(".env"), "secret").unwrap();
        add(&repo, ".env");

        let result = make_snapshot(&repo, &[], &policy(), &["sh".into()]);

        assert!(result.is_err());
    }

    #[test]
    fn snapshot_rejects_symlink_that_escapes_worktree() {
        let fixture = Fixture::new();
        let repo = fixture.repo();
        symlink(&fixture.0, repo.join("outside")).unwrap();
        add(&repo, "outside");

        let result = make_snapshot(&repo, &[], &policy(), &["sh".into()]);

        assert!(result.is_err());
    }

    #[test]
    fn tracked_job_json_remains_source_content() {
        let fixture = Fixture::new();
        let repo = fixture.repo();
        fs::write(repo.join("job.json"), "project data").unwrap();
        add(&repo, "job.json");

        let snapshot = make_snapshot(&repo, &[], &policy(), &["/bin/true".into()]).unwrap();

        assert_eq!(
            fs::read_to_string(snapshot.path.join("source/job.json")).unwrap(),
            "project data"
        );
        assert!(snapshot.path.join("job.json").exists());
    }

    #[test]
    fn executable_file_stays_executable_in_snapshot() {
        let fixture = Fixture::new();
        let repo = fixture.repo();
        let script = repo.join("build.sh");
        fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        add(&repo, "build.sh");

        let snapshot = make_snapshot(&repo, &[], &policy(), &["/bin/sh".into()]).unwrap();

        assert_ne!(
            fs::metadata(snapshot.path.join("source/build.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
    }

    #[test]
    fn worker_verifies_dirty_snapshot_and_preserves_failure_status() {
        let fixture = Fixture::new();
        let repo = fixture.repo();
        fs::write(repo.join("tracked.txt"), "source").unwrap();
        add(&repo, "tracked.txt");
        let command = vec!["/bin/sh".into(), "-c".into(), "exit 7".into()];
        let snapshot = make_snapshot(&repo, &[], &policy(), &command).unwrap();
        let root = fixture.0.join("worker");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let options = |action: &str| WorkerOptions {
            root: root.clone(),
            job: "abc123".into(),
            action: action.into(),
        };
        assert_eq!(worker(&options("prepare")).unwrap(), 0);
        fs::remove_dir(root.join("jobs/abc123/source")).unwrap();
        fs::rename(
            snapshot.path.join("source"),
            root.join("jobs/abc123/source"),
        )
        .unwrap();
        fs::rename(
            snapshot.path.join("job.json"),
            root.join("jobs/abc123/job.json"),
        )
        .unwrap();

        assert_eq!(worker(&options("verify")).unwrap(), 0);
        assert_eq!(worker(&options("execute")).unwrap(), 7);
        assert_eq!(
            fs::read_to_string(root.join("jobs/abc123/status")).unwrap(),
            "failed 7\n"
        );
        assert!(worker(&options("execute")).is_err());
        assert_eq!(worker(&options("cleanup")).unwrap(), 0);
        assert!(!root.join("jobs/abc123").exists());
    }
}
