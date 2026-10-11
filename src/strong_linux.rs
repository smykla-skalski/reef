//! Experimental Linux owner proof. Public strong mode remains fail-closed.

use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const START_TIMEOUT: Duration = Duration::from_secs(90);
const PODMAN_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
const HEX: &[u8; 16] = b"0123456789abcdef";

#[derive(Deserialize, Serialize)]
struct Start {
    image: String,
    command: Vec<String>,
}

#[derive(Deserialize, Serialize)]
struct PodmanStore {
    #[serde(rename = "graphRoot")]
    graph_root: std::path::PathBuf,
    #[serde(rename = "runRoot")]
    run_root: std::path::PathBuf,
    #[serde(rename = "graphDriverName")]
    driver: String,
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
    if runtime.to_str().is_none() {
        return Err(io::Error::other("user runtime path must be UTF-8"));
    }
    let data_home = podman_data_home()?;
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
    prepare_podman_home(&state_dir).inspect_err(|_| {
        let _ = cleanup_state_dir(&state_dir);
    })?;
    pin_podman_store(&state_dir).inspect_err(|_| {
        let _ = cleanup_state_dir(&state_dir);
    })?;
    write_owner_token(&state_dir).inspect_err(|_| {
        let _ = cleanup_state_dir(&state_dir);
    })?;
    let listener = UnixListener::bind(state_dir.join("owner.sock")).inspect_err(|_| {
        let _ = cleanup_state_dir(&state_dir);
    })?;

    let executable = std::env::current_exe().inspect_err(|_| {
        let _ = cleanup_state_dir(&state_dir);
    })?;
    let mut data_home_env = OsString::from("--setenv=XDG_DATA_HOME=");
    data_home_env.push(data_home.as_os_str());
    let mut runtime_env = OsString::from("--setenv=XDG_RUNTIME_DIR=");
    runtime_env.push(runtime.as_os_str());
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
        .arg(data_home_env)
        .arg(runtime_env)
        .arg("--")
        .arg(executable)
        .arg("strong-owner")
        .arg("--state-dir")
        .arg(&state_dir)
        .arg("--name")
        .arg(&name)
        .output()
        .inspect_err(|_| {
            let _ = cleanup_state_dir(&state_dir);
        })?;
    if !output.status.success() {
        let _ = cleanup_state_dir(&state_dir);
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
        cleanup_until_absent(&state_dir);
        cleanup_state_dir(&state_dir)?;
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
        // The owner can die after renaming verified state to its tombstone.
        // Recovery never resolves a container by the old name.
        cleanup_state_dir(state_dir)?;
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
        cleanup_until_absent(state_dir);
        cleanup_state_dir(state_dir)?;
        return Ok(());
    }

    let outcome = run_owner(state_dir, name, &stopping);
    cleanup_until_absent(state_dir);
    cleanup_state_dir(state_dir)?;
    if let Ok(mut stream) = outcome {
        let _ = stream.write_all(b"DONE\n");
    }
    Ok(())
}

fn cleanup_state_dir(state_dir: &Path) -> io::Result<()> {
    // A restarted owner must see either the complete state or no state. Move
    // it out of the service's known path before deleting individual entries.
    let tombstone = state_dir.with_extension("done");
    match fs::rename(state_dir, &tombstone) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    // Podman writes private cache entries beneath this task-owned tree. Do
    // not assume the directory contains only the files Reef created.
    match fs::remove_dir_all(tombstone) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn prepare_podman_home(state_dir: &Path) -> io::Result<()> {
    let home = state_dir.join("podman-home");
    let config = home.join(".config/containers");
    DirBuilder::new().mode(0o700).create(&home)?;
    DirBuilder::new().mode(0o700).create(home.join(".config"))?;
    DirBuilder::new().mode(0o700).create(&config)?;
    DirBuilder::new()
        .mode(0o700)
        .create(state_dir.join("hooks"))?;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(config.join("mounts.conf"))?;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(config.join("containers.conf"))?;
    Ok(())
}

fn pin_podman_store(state_dir: &Path) -> io::Result<()> {
    // Query with the launcher's real HOME before Podman is given its private
    // HOME. A system storage.conf may expand $HOME in rootless_storage_path.
    let mut info = Command::new("podman");
    info.env_remove("CONTAINERS_STORAGE_CONF")
        .env_remove("STORAGE_DRIVER")
        .env_remove("STORAGE_OPTS")
        .env_remove("CONTAINER_HOST")
        .env_remove("CONTAINER_CONNECTION")
        .arg("--remote=false")
        .args(["info", "--format", "{{json .Store}}"]);
    let output = bounded_output(&mut info, PODMAN_TIMEOUT, 65_536)?;
    let store: PodmanStore = serde_json::from_slice(&output)?;
    validate_store(&store)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(state_dir.join("podman-store.json"))?;
    serde_json::to_writer(&mut file, &store)?;
    file.sync_all()
}

fn validate_store(store: &PodmanStore) -> io::Result<()> {
    if !store.graph_root.is_absolute()
        || !store.run_root.is_absolute()
        || store.driver.is_empty()
        || !store
            .driver
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(io::Error::other("invalid Podman storage configuration"));
    }
    Ok(())
}

fn read_podman_store(state_dir: &Path) -> io::Result<PodmanStore> {
    let bytes = fs::read(state_dir.join("podman-store.json"))?;
    if bytes.len() > 4096 {
        return Err(io::Error::other(
            "Podman storage configuration is too large",
        ));
    }
    let store: PodmanStore = serde_json::from_slice(&bytes)?;
    validate_store(&store)?;
    Ok(store)
}

fn write_owner_token(state_dir: &Path) -> io::Result<()> {
    let mut bytes = [0_u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let mut token = String::with_capacity(32);
    for byte in bytes {
        token.push(HEX[(byte >> 4) as usize] as char);
        token.push(HEX[(byte & 0x0f) as usize] as char);
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(state_dir.join("owner.token"))?;
    file.write_all(token.as_bytes())?;
    file.sync_all()
}

fn read_owner_token(state_dir: &Path) -> io::Result<Option<String>> {
    let bytes = match fs::read(state_dir.join("owner.token")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let token = std::str::from_utf8(&bytes).map_err(io::Error::other)?;
    if token.len() != 32 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(io::Error::other("invalid owner token"));
    }
    Ok(Some(token.to_owned()))
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
    if podman_status(state_dir, &["image", "exists", &start.image])?.code() != Some(0) {
        return Err(io::Error::other("container image is not available locally"));
    }
    ensure_active(stopping)?;
    let token =
        read_owner_token(state_dir)?.ok_or_else(|| io::Error::other("missing owner token"))?;

    let mut create = podman_command(state_dir)?;
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
        "--image-volume=ignore",
        "--http-proxy=false",
        "--tmpfs=/tmp:rw,nosuid,nodev,size=64m",
        "--tmpfs=/run:rw,nosuid,nodev,size=8m",
        "--pids-limit=64",
        "--memory=512m",
        "--cpus=1",
    ]);
    create
        .arg(format!("--name={name}"))
        .arg(format!("--label=io.reef.owner={name}"))
        .arg(format!("--label=io.reef.token={token}"))
        .arg(format!(
            "--cidfile={}",
            state_dir.join("container.cid").display()
        ))
        .arg(&start.image)
        .args(&start.command);
    if !bounded_status(&mut create, PODMAN_TIMEOUT)?.success() {
        return Err(io::Error::other("podman create failed"));
    }
    ensure_active(stopping)?;
    let id = read_container_id(state_dir)?
        .ok_or_else(|| io::Error::other("podman did not record the created container ID"))?;
    verify_container_label(state_dir, &id, name, &token)?;
    persist_container_id(state_dir, &id)?;
    reject_effective_mounts(state_dir, &id)?;
    ensure_active(stopping)?;
    if !podman_status(state_dir, &["start", &id])?.success() {
        return Err(io::Error::other("podman start failed"));
    }
    ensure_active(stopping)?;
    stream.write_all(b"READY\n")?;
    stream.set_read_timeout(Some(Duration::from_millis(100)))?;

    let mut waiter = podman_command(state_dir)?;
    let mut waiter = waiter
        .args(["wait", &id])
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

fn reject_effective_mounts(state_dir: &Path, id: &str) -> io::Result<()> {
    let mut inspect = podman_command(state_dir)?;
    inspect.args(["inspect", "--format", "{{json .Mounts}}", id]);
    let output = bounded_output(&mut inspect, PODMAN_TIMEOUT, 16_384)?;
    let mounts: serde_json::Value = serde_json::from_slice(&output)?;
    if mounts.as_array().is_none_or(|mounts| !mounts.is_empty()) {
        return Err(io::Error::other(
            "effective container mounts are not allowed",
        ));
    }
    Ok(())
}

fn cleanup_until_absent(state_dir: &Path) {
    let Some(name) = state_dir.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let id = loop {
        match recover_container_id(state_dir, name) {
            Ok(Some(id)) => break id,
            Ok(None) => return,
            Err(_) => thread::sleep(Duration::from_secs(1)),
        }
    };
    loop {
        match podman_status(state_dir, &["container", "exists", &id]) {
            Ok(status) if status.code() == Some(1) => return,
            Ok(status) if status.success() => {}
            _ => {
                thread::sleep(Duration::from_secs(1));
                continue;
            }
        }
        let Ok(mut remove) = podman_command(state_dir) else {
            thread::sleep(Duration::from_secs(1));
            continue;
        };
        remove.args(["rm", "--force", "--volumes", "--time=0", &id]);
        let _ = bounded_status(&mut remove, STOP_TIMEOUT);
        thread::sleep(Duration::from_secs(1));
    }
}

fn verify_container_label(state_dir: &Path, id: &str, name: &str, token: &str) -> io::Result<()> {
    let mut inspect = podman_command(state_dir)?;
    inspect.args([
        "container",
        "inspect",
        "--format",
        "{{index .Config.Labels \"io.reef.owner\"}}|{{index .Config.Labels \"io.reef.token\"}}",
        id,
    ]);
    let output = bounded_output(&mut inspect, PODMAN_TIMEOUT, 128)?;
    if output != format!("{name}|{token}\n").as_bytes() {
        return Err(io::Error::other("created container is not owned by Reef"));
    }
    Ok(())
}

fn recover_container_id(state_dir: &Path, name: &str) -> io::Result<Option<String>> {
    if let Some(id) = read_container_id(state_dir)? {
        return Ok(Some(id));
    }
    let Some(token) = read_owner_token(state_dir)? else {
        return Ok(None);
    };
    let exists = podman_status(state_dir, &["container", "exists", name])?;
    if exists.code() == Some(1) {
        return Ok(None);
    }
    if !exists.success() {
        return Err(io::Error::other("cannot verify container existence"));
    }
    let mut inspect = podman_command(state_dir)?;
    inspect.args([
        "container",
        "inspect",
        "--format",
        "{{.Id}}|{{index .Config.Labels \"io.reef.owner\"}}|{{index .Config.Labels \"io.reef.token\"}}",
        name,
    ]);
    let output = bounded_output(&mut inspect, PODMAN_TIMEOUT, 256)?;
    let output = std::str::from_utf8(&output).map_err(io::Error::other)?;
    let expected_suffix = format!("|{name}|{token}");
    let output = output.trim_end();
    let Some(id) = output.strip_suffix(&expected_suffix) else {
        return Ok(None);
    };
    if id.len() != 64 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(io::Error::other("invalid recovered container ID"));
    }
    persist_container_id(state_dir, id)?;
    Ok(Some(id.to_owned()))
}

fn read_container_id(state_dir: &Path) -> io::Result<Option<String>> {
    for filename in ["owned.id", "container.cid"] {
        let contents = match fs::read(state_dir.join(filename)) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let id = std::str::from_utf8(&contents)
            .map_err(io::Error::other)?
            .trim();
        if id.len() != 64 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(io::Error::other("invalid recorded container ID"));
        }
        return Ok(Some(id.to_owned()));
    }
    Ok(None)
}

fn persist_container_id(state_dir: &Path, id: &str) -> io::Result<()> {
    let pending = state_dir.join("owned.id.pending");
    if pending.exists() {
        // Recovery may find a partial write from the previous owner.
        fs::remove_file(&pending)?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&pending)?;
    file.write_all(id.as_bytes())?;
    file.sync_all()?;
    fs::rename(pending, state_dir.join("owned.id"))
}

fn podman_status(state_dir: &Path, args: &[&str]) -> io::Result<ExitStatus> {
    let mut command = podman_command(state_dir)?;
    command.args(args);
    bounded_status(&mut command, PODMAN_TIMEOUT)
}

fn podman_command(state_dir: &Path) -> io::Result<Command> {
    let data_home = podman_data_home()?;
    let store = read_podman_store(state_dir)?;
    let home = state_dir.join("podman-home");
    let runtime = state_dir
        .parent()
        .ok_or_else(|| io::Error::other("invalid owner state path"))?;
    let mut command = Command::new("podman");
    command
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", data_home)
        .env("XDG_RUNTIME_DIR", runtime)
        .env(
            "CONTAINERS_CONF",
            home.join(".config/containers/containers.conf"),
        )
        .env_remove("CONTAINERS_CONF_OVERRIDE")
        .env_remove("CONTAINERS_STORAGE_CONF")
        .env_remove("STORAGE_DRIVER")
        .env_remove("STORAGE_OPTS")
        .env_remove("CONTAINER_HOST")
        .env_remove("CONTAINER_CONNECTION")
        .arg("--remote=false")
        .arg(format!("--root={}", store.graph_root.display()))
        .arg(format!("--runroot={}", store.run_root.display()))
        .arg(format!("--storage-driver={}", store.driver))
        .arg(format!("--hooks-dir={}", state_dir.join("hooks").display()));
    Ok(command)
}

fn podman_data_home() -> io::Result<std::path::PathBuf> {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|home| Path::new(&home).join(".local/share").into_os_string())
        })
        .ok_or_else(|| io::Error::other("cannot resolve Podman image store"))?;
    let data_home = std::path::PathBuf::from(data_home);
    if !data_home.is_absolute() {
        return Err(io::Error::other("Podman image store path must be absolute"));
    }
    if data_home.to_str().is_none() {
        return Err(io::Error::other("Podman image store path must be UTF-8"));
    }
    Ok(data_home)
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
