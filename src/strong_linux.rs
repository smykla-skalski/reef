//! Experimental Linux owner proof. Public strong mode remains fail-closed.

use serde::{Deserialize, Serialize};
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const START_TIMEOUT: Duration = Duration::from_secs(90);
const PODMAN_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Deserialize, Serialize)]
struct Start {
    image: String,
    command: Vec<String>,
}

pub fn probe(image: &str, command: &[String]) -> io::Result<u8> {
    if nix::unistd::Uid::current().is_root() {
        return Err(io::Error::other(
            "strong owner requires an unprivileged user",
        ));
    }
    validate_image(image)?;
    if command.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty command"));
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| io::Error::other("XDG_RUNTIME_DIR is required"))?;
    let runtime = Path::new(&runtime);
    let metadata = fs::symlink_metadata(runtime)?;
    if !metadata.is_dir()
        || metadata.uid() != nix::unistd::Uid::current().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::other("unsafe user runtime directory"));
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let name = format!("reef-strong-{}-{nonce}", std::process::id());
    let state_dir = runtime.join(&name);
    DirBuilder::new().mode(0o700).create(&state_dir)?;
    let listener = UnixListener::bind(state_dir.join("owner.sock")).inspect_err(|_| {
        cleanup_state_dir(&state_dir);
    })?;

    let executable = std::env::current_exe().inspect_err(|_| {
        cleanup_state_dir(&state_dir);
    })?;
    let output = Command::new("systemd-run")
        .args([
            "--user",
            "--collect",
            "--property=Restart=always",
            "--property=RestartPreventExitStatus=0",
            "--property=RestartSec=1s",
            "--property=StartLimitIntervalSec=0",
            "--property=KillMode=mixed",
            "--property=TimeoutStopSec=2s",
        ])
        .arg(format!("--unit={name}"))
        .arg("--")
        .arg(executable)
        .arg("strong-owner")
        .arg("--state-dir")
        .arg(&state_dir)
        .arg("--name")
        .arg(&name)
        .output()
        .inspect_err(|_| {
            cleanup_state_dir(&state_dir);
        })?;
    if !output.status.success() {
        cleanup_state_dir(&state_dir);
        return Err(io::Error::other(format!(
            "cannot start independent owner: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    let outcome = probe_connected(&listener, image, command, &name);
    if outcome.is_err() {
        // The launcher is still alive here. If the service was explicitly
        // stopped, systemd will not restart it. Stop any remaining owner
        // first, so it cannot create a container after our absence check.
        quiesce_owner(&name);
        cleanup_until_absent(&name);
        cleanup_state_dir(&state_dir);
    }
    outcome
}

fn quiesce_owner(name: &str) {
    let unit = format!("{name}.service");
    loop {
        let mut stop = Command::new("systemctl");
        stop.args(["--user", "stop", &unit]);
        if matches!(bounded_status(&mut stop, PODMAN_TIMEOUT), Ok(status) if status.success()) {
            return;
        }

        // A collected transient unit can no longer start work. Keep retrying
        // if the user manager itself is unavailable or reports another state.
        let mut show = Command::new("systemctl");
        show.args([
            "--user",
            "show",
            "--property=LoadState,ActiveState",
            "--value",
            &unit,
        ]);
        if matches!(bounded_output(&mut show, PODMAN_TIMEOUT, 128), Ok(output) if output == b"not-found\ninactive\n")
        {
            return;
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn probe_connected(
    listener: &UnixListener,
    image: &str,
    command: &[String],
    name: &str,
) -> io::Result<u8> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + START_TIMEOUT;
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error),
        }
    };
    stream.set_read_timeout(Some(START_TIMEOUT))?;
    serde_json::to_writer(
        &mut stream,
        &Start {
            image: image.to_owned(),
            command: command.to_vec(),
        },
    )?;
    stream.write_all(b"\n")?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line != "READY\n" {
        return Err(io::Error::other(
            "strong owner did not acknowledge command start",
        ));
    }
    println!("READY {name}");
    io::stdout().flush()?;
    line.clear();
    reader.get_mut().set_read_timeout(None)?;
    reader.read_line(&mut line)?;
    if line == "DONE\n" {
        Ok(0)
    } else {
        Err(io::Error::other(
            "strong owner exited without verified cleanup",
        ))
    }
}

pub fn owner(state_dir: &Path, name: &str) -> io::Result<()> {
    validate_name_path(state_dir, name)?;
    if !state_dir.exists() {
        cleanup_until_absent(name);
        return Ok(());
    }
    validate_state_dir(state_dir)?;
    let stopping = Arc::new(AtomicBool::new(false));
    for signal in [
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGHUP,
    ] {
        signal_hook::flag::register(signal, Arc::clone(&stopping))?;
    }
    let marker = state_dir.join("attempted");
    if OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
        .is_err()
    {
        // A restarted owner may only recover; it must never rerun a command.
        cleanup_until_absent(name);
        cleanup_state_dir(state_dir);
        return Ok(());
    }

    let outcome = run_owner(state_dir, name, &stopping);
    cleanup_until_absent(name);
    if let Ok(mut stream) = outcome {
        let _ = stream.write_all(b"DONE\n");
    }
    cleanup_state_dir(state_dir);
    Ok(())
}

fn cleanup_state_dir(state_dir: &Path) {
    let _ = fs::remove_file(state_dir.join("attempted"));
    let _ = fs::remove_file(state_dir.join("owner.sock"));
    let _ = fs::remove_dir(state_dir);
}

fn run_owner(state_dir: &Path, name: &str, stopping: &AtomicBool) -> io::Result<UnixStream> {
    ensure_active(stopping)?;
    let mut stream = UnixStream::connect(state_dir.join("owner.sock"))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut request = String::new();
    BufReader::new(stream.try_clone()?)
        .take(16_385)
        .read_line(&mut request)?;
    if request.len() > 16_384 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "request too large",
        ));
    }
    let start: Start = serde_json::from_str(&request)?;
    validate_image(&start.image)?;
    if start.command.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty command"));
    }
    ensure_active(stopping)?;
    if podman_status(&["image", "exists", &start.image])?.code() != Some(0) {
        return Err(io::Error::other("container image is not available locally"));
    }
    ensure_active(stopping)?;
    reject_default_mount_files()?;

    let mut create = Command::new("podman");
    create.args([
        "create",
        "--pull=never",
        "--network=none",
        "--userns=auto",
        "--pid=private",
        "--ipc=private",
        "--uts=private",
        "--cgroupns=private",
        "--cap-drop=all",
        "--security-opt=no-new-privileges",
        "--read-only",
        "--http-proxy=false",
        "--tmpfs=/tmp:rw,nosuid,nodev,size=64m",
        "--tmpfs=/run:rw,nosuid,nodev,size=8m",
        "--pids-limit=64",
        "--memory=512m",
        "--cpus=1",
    ]);
    create
        .arg(format!("--name={name}"))
        .arg(&start.image)
        .args(&start.command);
    if !bounded_status(&mut create, PODMAN_TIMEOUT)?.success() {
        return Err(io::Error::other("podman create failed"));
    }
    ensure_active(stopping)?;
    reject_effective_mounts(name)?;
    reject_default_mount_files()?;
    ensure_active(stopping)?;
    if !podman_status(&["start", name])?.success() {
        return Err(io::Error::other("podman start failed"));
    }
    ensure_active(stopping)?;
    stream.write_all(b"READY\n")?;
    stream.set_read_timeout(Some(Duration::from_millis(100)))?;

    let mut waiter = Command::new("podman")
        .args(["wait", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    loop {
        ensure_active(stopping)?;
        let mut byte = [0_u8];
        match stream.read(&mut byte) {
            Ok(0) => {
                let _ = waiter.kill();
                let _ = waiter.wait();
                return Err(io::Error::other("launcher exited"));
            }
            Ok(_) => return Err(io::Error::other("unexpected launcher data")),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => return Err(error),
        }
        if waiter.try_wait()?.is_some() {
            return Ok(stream);
        }
    }
}

fn ensure_active(stopping: &AtomicBool) -> io::Result<()> {
    if stopping.load(Ordering::Relaxed) {
        Err(io::Error::other("owner was stopped"))
    } else {
        Ok(())
    }
}

fn reject_effective_mounts(name: &str) -> io::Result<()> {
    let mut inspect = Command::new("podman");
    inspect.args(["inspect", "--format", "{{json .Mounts}}", name]);
    let output = bounded_output(&mut inspect, PODMAN_TIMEOUT, 16_384)?;
    let mounts: serde_json::Value = serde_json::from_slice(&output)?;
    if mounts.as_array().is_none_or(|mounts| !mounts.is_empty()) {
        return Err(io::Error::other(
            "effective container mounts are not allowed",
        ));
    }
    Ok(())
}

fn reject_default_mount_files() -> io::Result<()> {
    let mut paths = vec![
        PathBuf::from("/usr/share/containers/mounts.conf"),
        PathBuf::from("/etc/containers/mounts.conf"),
    ];
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".config")))
        .ok_or_else(|| io::Error::other("cannot resolve Podman user configuration"))?;
    paths.push(config_home.join("containers/mounts.conf"));
    for path in paths {
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if contents
            .lines()
            .any(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
        {
            return Err(io::Error::other(
                "Podman default mount configuration is not allowed",
            ));
        }
    }
    Ok(())
}

fn cleanup_until_absent(name: &str) {
    loop {
        let mut remove = Command::new("podman");
        remove.args(["rm", "--force", "--time=0", name]);
        let _ = bounded_status(&mut remove, STOP_TIMEOUT);
        if matches!(podman_status(&["container", "exists", name]), Ok(status) if status.code() == Some(1))
        {
            return;
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn podman_status(args: &[&str]) -> io::Result<ExitStatus> {
    let mut command = Command::new("podman");
    command.args(args);
    bounded_status(&mut command, PODMAN_TIMEOUT)
}

fn bounded_status(command: &mut Command, timeout: Duration) -> io::Result<ExitStatus> {
    let mut child = command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    wait_bounded(&mut child, timeout)
}

fn bounded_output(command: &mut Command, timeout: Duration, max_bytes: u64) -> io::Result<Vec<u8>> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing command output"))?;
    let reader = thread::spawn(move || {
        let mut output = Vec::new();
        stdout.take(max_bytes + 1).read_to_end(&mut output)?;
        Ok::<_, io::Error>(output)
    });
    let status = wait_bounded(&mut child, timeout)?;
    let output = reader
        .join()
        .map_err(|_| io::Error::other("output reader failed"))??;
    if !status.success() || output.len() as u64 > max_bytes {
        return Err(io::Error::other("container inspection failed"));
    }
    Ok(output)
}

fn wait_bounded(child: &mut Child, timeout: Duration) -> io::Result<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "container runtime timed out",
            ));
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn validate_image(image: &str) -> io::Result<()> {
    if image.is_empty()
        || image.starts_with('-')
        || !image
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"./:_-@".contains(&byte))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid image reference",
        ));
    }
    Ok(())
}

fn validate_name_path(state_dir: &Path, name: &str) -> io::Result<()> {
    if !name.starts_with("reef-strong-")
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        || state_dir.file_name().is_none_or(|value| value != name)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid owner name",
        ));
    }
    Ok(())
}

fn validate_state_dir(state_dir: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(state_dir)?;
    if !metadata.is_dir()
        || metadata.uid() != nix::unistd::Uid::current().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid owner state",
        ));
    }
    Ok(())
}
