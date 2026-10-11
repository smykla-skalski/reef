//! Experimental Linux owner proof. Public strong mode remains fail-closed.

use nix::fcntl::{FcntlArg, FdFlag, Flock, FlockArg, OFlag, fcntl};
use serde::{Deserialize, Serialize};
use signal_hook::iterator::Signals;
use std::ffi::{OsStr, OsString};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
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
const OWNER_LOCK_TIMEOUT: Duration = Duration::from_secs(90);
const CONTROL_TICK: Duration = Duration::from_millis(250);
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(500);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(10);
const CANCEL_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const SIGNAL_GRACE: Duration = Duration::from_secs(2);
const STORE_MAX_BYTES: usize = 65_536;
const HEX: &[u8; 16] = b"0123456789abcdef";

#[derive(Deserialize, Serialize)]
struct Start {
    image: String,
    command: Vec<String>,
    workspace_snapshot: bool,
}

struct ProbeListeners {
    control: UnixListener,
    input: UnixListener,
    output: UnixListener,
    error: UnixListener,
}

#[derive(Deserialize, Serialize)]
struct Usage {
    wall_ms: u128,
    observed_cpu_ms: Option<u64>,
    observed_peak_memory_bytes: Option<u64>,
    observed_io_read_bytes: Option<u64>,
    observed_io_write_bytes: Option<u64>,
}

#[derive(Deserialize, Serialize)]
struct Done {
    code: u8,
    usage: Usage,
}

#[derive(Default)]
struct UsageSample {
    cpu_usec: Option<u64>,
    peak_memory_bytes: Option<u64>,
    io_read_bytes: Option<u64>,
    io_write_bytes: Option<u64>,
}

struct RunOutcome {
    stream: UnixStream,
    done: Done,
}

enum StoreOutput {
    Done(Vec<u8>),
    Cancelled(i32),
}

enum ProbeCompletion {
    Finished(u8),
    Cancelled(i32),
}

struct Monitored {
    done: Done,
    force_stop: bool,
}

#[derive(Deserialize, Serialize)]
struct PodmanStore {
    #[serde(rename = "graphRoot")]
    graph_root: std::path::PathBuf,
    #[serde(rename = "runRoot")]
    run_root: std::path::PathBuf,
    #[serde(rename = "graphDriverName")]
    driver: String,
    #[serde(rename = "graphOptions", default)]
    graph_options: serde_json::Value,
    #[serde(rename = "configFile", default)]
    config_file: std::path::PathBuf,
}

pub fn probe(
    image: &str,
    workspace: Option<&Path>,
    approve_snapshot: bool,
    metrics: bool,
    command: &[String],
) -> io::Result<u8> {
    validate_probe_request(image, workspace, approve_snapshot, command)?;
    let mut signals = Signals::new([
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
        signal_hook::consts::SIGQUIT,
    ])?;
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
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(state_dir.join("owner.lock"))
        .inspect_err(|_| {
            let _ = cleanup_state_dir(&state_dir);
        })?;
    prepare_podman_home(&state_dir).inspect_err(|_| {
        let _ = cleanup_state_dir(&state_dir);
    })?;
    match pin_podman_store(&state_dir, &mut signals) {
        Ok(None) => {}
        Ok(Some(signal)) => {
            cleanup_state_dir(&state_dir)?;
            return Ok(signal_exit_code(signal));
        }
        Err(error) => {
            cleanup_state_dir(&state_dir)?;
            return Err(error);
        }
    }
    write_owner_token(&state_dir).inspect_err(|_| {
        let _ = cleanup_state_dir(&state_dir);
    })?;
    let listeners = bind_probe_listeners(&state_dir).inspect_err(|_| {
        let _ = cleanup_state_dir(&state_dir);
    })?;

    let executable = std::env::current_exe().inspect_err(|_| {
        let _ = cleanup_state_dir(&state_dir);
    })?;
    if let Some(signal) = signals.pending().next() {
        cleanup_state_dir(&state_dir)?;
        return Ok(signal_exit_code(signal));
    }
    match start_owner_service(
        &state_dir,
        &name,
        &data_home,
        runtime,
        &executable,
        &mut signals,
    ) {
        Ok(None) => {}
        Ok(Some(signal)) => {
            cleanup_failed_probe(&state_dir, &name)?;
            return Ok(signal_exit_code(signal));
        }
        Err(error) => {
            cleanup_failed_probe(&state_dir, &name)?;
            return Err(error);
        }
    }

    let outcome = probe_connected(
        &listeners,
        &state_dir,
        image,
        workspace,
        metrics,
        command,
        &mut signals,
    );
    if !matches!(outcome, Ok(ProbeCompletion::Finished(_))) {
        cleanup_failed_probe(&state_dir, &name)?;
    }
    match outcome? {
        ProbeCompletion::Finished(code) => Ok(code),
        ProbeCompletion::Cancelled(signal) => Ok(signal_exit_code(signal)),
    }
}

fn signal_exit_code(signal: i32) -> u8 {
    u8::try_from(128 + signal).unwrap_or(1)
}

fn cleanup_failed_probe(state_dir: &Path, name: &str) -> io::Result<()> {
    let cancellation = mark_cancelled(state_dir);
    // An attached start client can outlive a killed owner. Remove its pinned
    // container before waiting for the lock inherited by an in-flight start.
    let _ = quiesce_owner(name);
    cleanup_until_absent(state_dir);
    let _lock = lock_state_until(state_dir, OWNER_LOCK_TIMEOUT)?;
    cleanup_until_absent(state_dir);
    cleanup_state_dir(state_dir)?;
    cancellation
}

fn bind_probe_listeners(state_dir: &Path) -> io::Result<ProbeListeners> {
    Ok(ProbeListeners {
        control: UnixListener::bind(state_dir.join("owner.sock"))?,
        input: UnixListener::bind(state_dir.join("stdin.sock"))?,
        output: UnixListener::bind(state_dir.join("stdout.sock"))?,
        error: UnixListener::bind(state_dir.join("stderr.sock"))?,
    })
}

fn start_owner_service(
    state_dir: &Path,
    name: &str,
    data_home: &Path,
    runtime: &Path,
    executable: &Path,
    signals: &mut Signals,
) -> io::Result<Option<i32>> {
    let mut data_home_env = OsString::from("--setenv=XDG_DATA_HOME=");
    data_home_env.push(data_home.as_os_str());
    let mut runtime_env = OsString::from("--setenv=XDG_RUNTIME_DIR=");
    runtime_env.push(runtime.as_os_str());
    let error_path = state_dir.join("start.stderr");
    let error_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&error_path)?;
    let mut child = Command::new("systemd-run")
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
        .arg(state_dir)
        .arg("--name")
        .arg(name)
        .stdout(Stdio::null())
        .stderr(Stdio::from(error_file))
        .spawn()?;
    let deadline = Instant::now() + START_TIMEOUT;
    let status = loop {
        if let Some(signal) = signals.pending().next() {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(Some(signal));
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "independent owner startup timed out",
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        }
    };
    if status.success() {
        return Ok(None);
    }
    let mut stderr = Vec::new();
    File::open(error_path)?
        .take(4_096)
        .read_to_end(&mut stderr)?;
    Err(io::Error::other(format!(
        "cannot start independent owner: {}",
        String::from_utf8_lossy(&stderr).trim()
    )))
}

fn validate_probe_request(
    image: &str,
    workspace: Option<&Path>,
    approve_snapshot: bool,
    command: &[String],
) -> io::Result<()> {
    if nix::unistd::Uid::current().is_root() {
        return Err(io::Error::other(
            "strong owner requires an unprivileged user",
        ));
    }
    validate_image(image)?;
    if command.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty command"));
    }
    if workspace.is_some() != approve_snapshot {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workspace snapshot requires --workspace and --approve-snapshot",
        ));
    }
    Ok(())
}

fn quiesce_owner(name: &str) -> bool {
    let unit = format!("{name}.service");
    let deadline = Instant::now() + STOP_TIMEOUT;
    while Instant::now() < deadline {
        let mut stop = Command::new("systemctl");
        stop.args(["--user", "stop", &unit]);
        if matches!(bounded_status(&mut stop, PODMAN_TIMEOUT), Ok(status) if status.success()) {
            return true;
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
            return true;
        }
        thread::sleep(Duration::from_secs(1));
    }
    false
}

fn mark_cancelled(state_dir: &Path) -> io::Result<()> {
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(state_dir.join("cancelled"))
    {
        Ok(file) => file.sync_all(),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn lock_state_until(state_dir: &Path, timeout: Duration) -> io::Result<Option<Flock<File>>> {
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .open(state_dir.join("owner.lock"))
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound && !state_dir.exists() => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let deadline = Instant::now() + timeout;
    let mut file = file;
    loop {
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => return Ok(Some(lock)),
            Err((returned, nix::errno::Errno::EWOULDBLOCK)) if Instant::now() < deadline => {
                file = returned;
                thread::sleep(Duration::from_millis(100));
            }
            Err((_, error)) => return Err(io::Error::other(error)),
        }
    }
}

fn probe_connected(
    listeners: &ProbeListeners,
    state_dir: &Path,
    image: &str,
    workspace: Option<&Path>,
    metrics: bool,
    command: &[String],
    signals: &mut Signals,
) -> io::Result<ProbeCompletion> {
    listeners.control.set_nonblocking(true)?;
    let deadline = Instant::now() + START_TIMEOUT;
    let mut stream = loop {
        if let Some(signal) = signals.pending().next() {
            return Ok(ProbeCompletion::Cancelled(signal));
        }
        match listeners.control.accept() {
            Ok((stream, _)) => break stream,
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error),
        }
    };
    if let Some(signal) = signals.pending().next() {
        return Ok(ProbeCompletion::Cancelled(signal));
    }
    stream.set_read_timeout(Some(CONTROL_TICK))?;
    stream.set_write_timeout(Some(START_TIMEOUT))?;
    serde_json::to_writer(
        &mut stream,
        &Start {
            image: image.to_owned(),
            command: command.to_vec(),
            workspace_snapshot: workspace.is_some(),
        },
    )?;
    stream.write_all(b"\n")?;
    if let Some(workspace) = workspace {
        let mut interrupted = None;
        let copied = crate::remote::create_local_source_snapshot(workspace, state_dir, &mut || {
            if let Some(signal) = signals.pending().next() {
                interrupted = Some(signal);
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
            stream.write_all(b"PROGRESS\n")
        });
        if let Some(signal) = interrupted {
            return Ok(ProbeCompletion::Cancelled(signal));
        }
        copied?;
        if let Some(signal) = signals.pending().next() {
            return Ok(ProbeCompletion::Cancelled(signal));
        }
        stream.write_all(b"SNAPSHOT_READY\n")?;
    }
    let mut reader = BufReader::new(stream);
    if let Some(signal) = read_owner_ack(&mut reader, signals, "IO_CONNECTED\n")? {
        return Ok(ProbeCompletion::Cancelled(signal));
    }
    let input = listeners.input.accept()?.0;
    let output = listeners.output.accept()?.0;
    let error = listeners.error.accept()?.0;
    thread::spawn(move || {
        let mut input = input;
        let _ = io::copy(&mut io::stdin(), &mut input);
        let _ = input.shutdown(std::net::Shutdown::Write);
    });
    if let Some(signal) = signals.pending().next() {
        return Ok(ProbeCompletion::Cancelled(signal));
    }
    reader.get_mut().write_all(b"IO_READY\n")?;
    if let Some(signal) = read_owner_ack(&mut reader, signals, "READY\n")? {
        return Ok(ProbeCompletion::Cancelled(signal));
    }
    println!("READY {}", state_dir.file_name().unwrap().to_string_lossy());
    io::stdout().flush()?;
    relay_attached_command(reader, output, error, metrics, signals)
}

fn relay_attached_command(
    mut reader: BufReader<UnixStream>,
    output: UnixStream,
    error: UnixStream,
    metrics: bool,
    signals: &mut Signals,
) -> io::Result<ProbeCompletion> {
    let output_cancel = reader.get_ref().try_clone()?;
    let error_cancel = reader.get_ref().try_clone()?;
    let output_failed = Arc::new(AtomicBool::new(false));
    let output_failure = Arc::clone(&output_failed);
    let error_failure = Arc::clone(&output_failed);
    let output_relay = thread::spawn(move || {
        let result = copy_output(output, io::stdout());
        if result.is_err() {
            output_failure.store(true, Ordering::Relaxed);
            let _ = output_cancel.shutdown(std::net::Shutdown::Both);
        }
        result
    });
    let error_relay = thread::spawn(move || {
        let result = copy_output(error, io::stderr());
        if result.is_err() {
            error_failure.store(true, Ordering::Relaxed);
            let _ = error_cancel.shutdown(std::net::Shutdown::Both);
        }
        result
    });
    reader.get_mut().set_read_timeout(Some(CONTROL_TICK))?;
    let (done, signalled, undelivered_signal) = wait_done(&mut reader, signals, &output_failed)?;
    let mut drain_deadline = signalled.then(|| Instant::now() + CANCEL_DRAIN_TIMEOUT);
    let mut late_signal = undelivered_signal;
    let stdout_complete =
        finish_relay(output_relay, signals, &mut drain_deadline, &mut late_signal)?;
    let stderr_complete =
        finish_relay(error_relay, signals, &mut drain_deadline, &mut late_signal)?;
    if let Some(signal) = late_signal {
        return Ok(ProbeCompletion::Cancelled(signal));
    }
    if metrics && stdout_complete && stderr_complete {
        eprintln!("\nREEF_MEASUREMENT {}", serde_json::to_string(&done.usage)?);
    }
    Ok(ProbeCompletion::Finished(done.code))
}

fn read_owner_ack(
    reader: &mut BufReader<UnixStream>,
    signals: &mut Signals,
    expected: &str,
) -> io::Result<Option<i32>> {
    let deadline = Instant::now() + START_TIMEOUT;
    let mut line = String::new();
    loop {
        if let Some(signal) = signals.pending().next() {
            return Ok(Some(signal));
        }
        match reader.read_line(&mut line) {
            Ok(0) => return Err(io::Error::other("strong owner exited before command start")),
            Ok(_) if line == expected => return Ok(None),
            Ok(_) => return Err(io::Error::other("invalid strong owner acknowledgement")),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::Interrupted
                ) && Instant::now() < deadline => {}
            Err(error) => return Err(error),
        }
        if line.len() > 32 {
            return Err(io::Error::other("invalid strong owner acknowledgement"));
        }
    }
}

fn copy_output(mut stream: UnixStream, mut destination: impl Write) -> io::Result<u64> {
    let copied = io::copy(&mut stream, &mut destination)?;
    destination.flush()?;
    Ok(copied)
}

fn finish_relay(
    relay: thread::JoinHandle<io::Result<u64>>,
    signals: &mut Signals,
    deadline: &mut Option<Instant>,
    late_signal: &mut Option<i32>,
) -> io::Result<bool> {
    while !relay.is_finished() {
        if let Some(signal) = signals.pending().next() {
            late_signal.get_or_insert(signal);
            deadline.get_or_insert_with(|| Instant::now() + CANCEL_DRAIN_TIMEOUT);
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            // The owner has already verified teardown. On cancellation,
            // never wait indefinitely for an unread downstream pipe.
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(25));
    }
    relay
        .join()
        .map_err(|_| io::Error::other("command output relay failed"))??;
    Ok(true)
}

fn wait_done(
    reader: &mut BufReader<UnixStream>,
    signals: &mut Signals,
    output_failed: &AtomicBool,
) -> io::Result<(Done, bool, Option<i32>)> {
    let mut message = Vec::with_capacity(256);
    let mut last_heartbeat = Instant::now();
    let mut signalled = false;
    let mut undelivered_signal = None;
    loop {
        if output_failed.load(Ordering::Relaxed) {
            return Err(io::Error::other("cannot forward command output"));
        }
        for signal in signals.pending() {
            signalled = true;
            match reader
                .get_mut()
                .write_all(format!("SIGNAL {signal}\n").as_bytes())
            {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::BrokenPipe
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::NotConnected
                    ) =>
                {
                    undelivered_signal.get_or_insert(signal);
                }
                Err(error) => return Err(error),
            }
        }
        if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
            reader.get_mut().write_all(b"PING\n")?;
            last_heartbeat = Instant::now();
        }
        let mut chunk = [0_u8; 256];
        match reader.read(&mut chunk) {
            Ok(0) => return Err(io::Error::other("strong owner exited before cleanup")),
            Ok(read) => {
                message.extend_from_slice(&chunk[..read]);
                if message.len() > 1_024 {
                    return Err(io::Error::other("invalid owner completion"));
                }
                if message.last() == Some(&b'\n') {
                    return Ok((parse_done(&message)?, signalled, undelivered_signal));
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
    }
}

fn parse_done(line: &[u8]) -> io::Result<Done> {
    let contents = line
        .strip_prefix(b"DONE ")
        .and_then(|value| value.strip_suffix(b"\n"))
        .ok_or_else(|| io::Error::other("strong owner exited without verified cleanup"))?;
    serde_json::from_slice(contents).map_err(io::Error::other)
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
    let Some(lock) = lock_state_until(state_dir, OWNER_LOCK_TIMEOUT)? else {
        return Ok(());
    };
    if !state_dir.exists() {
        return Ok(());
    }
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

    let outcome = run_owner(state_dir, name, &stopping, &lock);
    if let Err(error) = &outcome {
        eprintln!("reef: strong owner failed: {error}");
    }
    cleanup_until_absent(state_dir);
    cleanup_state_dir(state_dir)?;
    if let Ok(mut outcome) = outcome {
        let _ = outcome.stream.write_all(b"DONE ");
        let _ = serde_json::to_writer(&mut outcome.stream, &outcome.done);
        let _ = outcome.stream.write_all(b"\n");
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

fn pin_podman_store(state_dir: &Path, signals: &mut Signals) -> io::Result<Option<i32>> {
    // Query with the launcher's real HOME before Podman is given its private
    // HOME. A system storage.conf may expand $HOME in rootless_storage_path.
    let mut info = launcher_podman_info(None);
    let output = match bounded_store_output(&mut info, signals)? {
        StoreOutput::Done(output) => output,
        StoreOutput::Cancelled(signal) => return Ok(Some(signal)),
    };
    let mut store: PodmanStore = serde_json::from_slice(&output)?;
    validate_store(&store)?;
    snapshot_storage_config(state_dir, &mut store)?;
    if store.config_file.is_file() {
        // The first query discovers the selected file. Re-query through the
        // exact snapshot that the owner will use so a concurrent config edit
        // cannot make the recorded baseline describe a different image store.
        let mut pinned_info = launcher_podman_info(Some(&store.config_file));
        let pinned_output = match bounded_store_output(&mut pinned_info, signals)? {
            StoreOutput::Done(output) => output,
            StoreOutput::Cancelled(signal) => return Ok(Some(signal)),
        };
        store = serde_json::from_slice(&pinned_output)?;
        validate_store(&store)?;
    }
    let mut private_info = private_podman_command(state_dir, &store)?;
    private_info.args(["info", "--format", "{{json .Store}}"]);
    let private_output = match bounded_store_output(&mut private_info, signals)? {
        StoreOutput::Done(output) => output,
        StoreOutput::Cancelled(signal) => return Ok(Some(signal)),
    };
    let private_store: PodmanStore = serde_json::from_slice(&private_output)?;
    if private_store.graph_root != store.graph_root
        || private_store.run_root != store.run_root
        || private_store.driver != store.driver
        || private_store.graph_options != store.graph_options
    {
        return Err(io::Error::other(
            "private Podman storage differs from the launcher store",
        ));
    }
    let bytes = serde_json::to_vec(&store)?;
    if bytes.len() > STORE_MAX_BYTES {
        return Err(io::Error::other("Podman storage state is too large"));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(state_dir.join("podman-store.json"))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(None)
}

fn bounded_store_output(command: &mut Command, signals: &mut Signals) -> io::Result<StoreOutput> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing Podman store output"))?;
    let reader = thread::spawn(move || {
        let mut output = Vec::new();
        stdout.take(65_537).read_to_end(&mut output)?;
        Ok::<_, io::Error>(output)
    });
    let deadline = Instant::now() + PODMAN_TIMEOUT;
    let status = loop {
        if let Some(signal) = signals.pending().next() {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(StoreOutput::Cancelled(signal));
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Podman storage inspection timed out",
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        }
    };
    if !status.success() {
        return Err(io::Error::other("Podman storage inspection failed"));
    }
    while !reader.is_finished() {
        if let Some(signal) = signals.pending().next() {
            return Ok(StoreOutput::Cancelled(signal));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Podman storage output timed out",
            ));
        }
        thread::sleep(Duration::from_millis(25));
    }
    let output = reader
        .join()
        .map_err(|_| io::Error::other("Podman store output reader failed"))??;
    if output.len() > 65_536 {
        return Err(io::Error::other("Podman storage inspection failed"));
    }
    Ok(StoreOutput::Done(output))
}

fn launcher_podman_info(storage_config: Option<&Path>) -> Command {
    let mut info = Command::new("podman");
    info.env_remove("STORAGE_DRIVER")
        .env_remove("STORAGE_OPTS")
        .env_remove("CONTAINER_HOST")
        .env_remove("CONTAINER_CONNECTION")
        .arg("--remote=false")
        .args(["info", "--format", "{{json .Store}}"]);
    if let Some(path) = storage_config {
        info.env("CONTAINERS_STORAGE_CONF", path);
    }
    info
}

fn snapshot_storage_config(state_dir: &Path, store: &mut PodmanStore) -> io::Result<()> {
    if !store.config_file.is_file() {
        store.config_file = std::path::PathBuf::new();
        return Ok(());
    }
    let bytes = fs::read(&store.config_file)?;
    if bytes.len() > 1_048_576 {
        return Err(io::Error::other(
            "Podman storage configuration is too large",
        ));
    }
    let config = std::str::from_utf8(&bytes).map_err(io::Error::other)?;
    let original_home = std::env::var("HOME").map_err(io::Error::other)?;
    if original_home.contains(['"', '\\', '\n', '\r']) {
        return Err(io::Error::other("unsupported HOME path for Podman storage"));
    }
    let config = config
        .replace("${HOME}", &original_home)
        .replace("$HOME", &original_home);
    let snapshot = state_dir.join("podman-home/.config/containers/storage.conf");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&snapshot)?;
    file.write_all(config.as_bytes())?;
    file.sync_all()?;
    store.config_file = snapshot;
    Ok(())
}

fn validate_store(store: &PodmanStore) -> io::Result<()> {
    if !store.graph_root.is_absolute()
        || !store.run_root.is_absolute()
        || (!store.config_file.as_os_str().is_empty() && !store.config_file.is_absolute())
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
    if bytes.len() > STORE_MAX_BYTES {
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

fn run_owner(
    state_dir: &Path,
    name: &str,
    stopping: &AtomicBool,
    lock: &Flock<File>,
) -> io::Result<RunOutcome> {
    ensure_active(stopping, state_dir)?;
    let mut stream = UnixStream::connect(state_dir.join("owner.sock"))?;
    stream.set_read_timeout(Some(START_TIMEOUT))?;
    let mut request = String::new();
    let mut reader = BufReader::new(stream.try_clone()?);
    (&mut reader).take(16_385).read_line(&mut request)?;
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
    if start.workspace_snapshot {
        await_snapshot_ready(&mut reader)?;
    }
    let id = create_owned_container(state_dir, name, &start, stopping, lock)?;
    let input = UnixStream::connect(state_dir.join("stdin.sock"))?;
    let output = UnixStream::connect(state_dir.join("stdout.sock"))?;
    let error = UnixStream::connect(state_dir.join("stderr.sock"))?;
    stream.write_all(b"IO_CONNECTED\n")?;
    await_io_ready(&mut reader)?;

    let mut start_command = podman_command(state_dir)?;
    let mut attached = start_command
        .args([
            "start",
            "--attach",
            "--interactive",
            "--sig-proxy=false",
            "--detach-keys=",
            &id,
        ])
        .stdin(Stdio::from(OwnedFd::from(input)))
        .stdout(Stdio::from(OwnedFd::from(output)))
        .stderr(Stdio::from(OwnedFd::from(error)))
        .spawn()?;
    if let Err(error) = wait_container_started(state_dir, &id, &mut attached, stopping) {
        let _ = attached.kill();
        let _ = attached.wait();
        return Err(error);
    }
    stream.write_all(b"READY\n")?;
    stream.set_read_timeout(Some(CONTROL_TICK))?;

    let mut waiter = podman_command(state_dir)?;
    let mut waiter = waiter
        .args(["wait", &id])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let outcome = monitor_command(
        &mut reader,
        state_dir,
        &id,
        stopping,
        &mut attached,
        &mut waiter,
    );
    if outcome
        .as_ref()
        .map_or(true, |monitored| monitored.force_stop)
    {
        let _ = attached.kill();
        let _ = attached.wait();
        let _ = waiter.kill();
        let _ = waiter.wait();
    }
    let done = outcome?.done;
    Ok(RunOutcome { stream, done })
}

fn await_io_ready(reader: &mut BufReader<UnixStream>) -> io::Result<()> {
    let mut line = String::new();
    let count = (&mut *reader).take(32).read_line(&mut line)?;
    if count == 0 || line != "IO_READY\n" {
        return Err(io::Error::other("launcher did not accept command I/O"));
    }
    Ok(())
}

fn wait_container_started(
    state_dir: &Path,
    id: &str,
    attached: &mut Child,
    stopping: &AtomicBool,
) -> io::Result<()> {
    let deadline = Instant::now() + PODMAN_TIMEOUT;
    let mut exited_at = None;
    loop {
        ensure_active(stopping, state_dir)?;
        let mut inspect = podman_command(state_dir)?;
        inspect.args(["inspect", "--format", "{{.State.Status}}", id]);
        if matches!(
            bounded_output(&mut inspect, PODMAN_TIMEOUT, 128),
            Ok(output) if matches!(output.as_slice(), b"running\n" | b"exited\n")
        ) {
            return Ok(());
        }
        if attached.try_wait()?.is_some() {
            let at = exited_at.get_or_insert_with(Instant::now);
            if at.elapsed() >= Duration::from_secs(1) {
                return Err(io::Error::other("attached container did not start"));
            }
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("attached container start timed out"));
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn monitor_command(
    reader: &mut BufReader<UnixStream>,
    state_dir: &Path,
    id: &str,
    stopping: &AtomicBool,
    attached: &mut Child,
    waiter: &mut Child,
) -> io::Result<Monitored> {
    let started = Instant::now();
    let mut last_heartbeat = Instant::now();
    let mut control = Vec::new();
    let mut first_signal = None;
    let mut attached_exited_at = None;
    let mut command_exit = None;
    let mut sample = UsageSample::default();
    let mut cgroup = locate_container_cgroup(state_dir, id);
    loop {
        ensure_active(stopping, state_dir)?;
        if last_heartbeat.elapsed() >= HEARTBEAT_TIMEOUT {
            return Err(io::Error::other("launcher heartbeat expired"));
        }
        if cgroup.is_none() {
            cgroup = locate_container_cgroup(state_dir, id);
        }
        if let Some(path) = &cgroup {
            sample_cgroup(path, &mut sample);
        }
        let mut chunk = [0_u8; 128];
        match reader.read(&mut chunk) {
            Ok(0) => return Err(io::Error::other("launcher exited")),
            Ok(read) => {
                control.extend_from_slice(&chunk[..read]);
                if control.len() > 128 {
                    return Err(io::Error::other("invalid launcher control message"));
                }
                while let Some(end) = control.iter().position(|byte| *byte == b'\n') {
                    let line = control.drain(..=end).collect::<Vec<_>>();
                    handle_control_line(&line, state_dir, id, &mut first_signal)?;
                    last_heartbeat = Instant::now();
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
        if command_exit.is_none()
            && let Some(status) = waiter.try_wait()?
        {
            ensure_podman_wait_succeeded(status)?;
            let mut stdout = waiter
                .stdout
                .take()
                .ok_or_else(|| io::Error::other("missing container exit status"))?;
            command_exit = Some(read_container_exit(&mut stdout)?);
        }
        if let Some(attached_status) = attached.try_wait()? {
            if let Some(code) = command_exit {
                if attached_status.code() != Some(i32::from(code)) {
                    return Err(io::Error::other("attached client lost command output"));
                }
                if let Some(path) = &cgroup {
                    sample_cgroup(path, &mut sample);
                }
                return Ok(Monitored {
                    done: Done {
                        code,
                        usage: sample.finish(started.elapsed()),
                    },
                    force_stop: false,
                });
            }
            let at = attached_exited_at.get_or_insert_with(Instant::now);
            if at.elapsed() >= Duration::from_secs(1) {
                return Err(io::Error::other("attached client lost command output"));
            }
        }
        if let Some((signal, at)) = first_signal
            && at.elapsed() >= SIGNAL_GRACE
        {
            return Ok(Monitored {
                done: Done {
                    code: u8::try_from(128 + signal).unwrap_or(1),
                    usage: sample.finish(started.elapsed()),
                },
                force_stop: true,
            });
        }
    }
}

fn handle_control_line(
    line: &[u8],
    state_dir: &Path,
    id: &str,
    first_signal: &mut Option<(i32, Instant)>,
) -> io::Result<()> {
    if line == b"PING\n" {
        return Ok(());
    }
    let signal = match line {
        b"SIGNAL 2\n" => (2, "INT"),
        b"SIGNAL 3\n" => (3, "QUIT"),
        b"SIGNAL 15\n" => (15, "TERM"),
        b"SIGNAL 1\n" => (1, "HUP"),
        _ => return Err(io::Error::other("invalid launcher control message")),
    };
    let status = podman_status_bounded(
        state_dir,
        &["kill", "--signal", signal.1, id],
        Duration::from_secs(5),
    )?;
    if !status.success() {
        let mut inspect = podman_command(state_dir)?;
        inspect.args(["inspect", "--format", "{{.State.Status}}", id]);
        if !matches!(
            bounded_output(&mut inspect, Duration::from_secs(2), 128),
            Ok(output) if output == b"exited\n"
        ) {
            return Err(io::Error::other("cannot forward command signal"));
        }
    }
    first_signal.get_or_insert((signal.0, Instant::now()));
    Ok(())
}

fn locate_container_cgroup(state_dir: &Path, id: &str) -> Option<PathBuf> {
    let mut inspect = podman_command(state_dir).ok()?;
    inspect.args(["inspect", "--format", "{{.State.Pid}}", id]);
    let output = bounded_output(&mut inspect, Duration::from_secs(2), 32).ok()?;
    let pid = std::str::from_utf8(&output)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()?;
    if pid == 0 {
        return None;
    }
    let cgroup = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let relative = cgroup.lines().find_map(|line| line.strip_prefix("0::/"))?;
    let relative = Path::new(relative);
    let expected = format!("libpod-{id}.scope");
    if !relative
        .components()
        .all(|part| matches!(part, std::path::Component::Normal(_)))
        || !relative
            .components()
            .any(|part| part.as_os_str() == OsStr::new(&expected))
    {
        return None;
    }
    let path = Path::new("/sys/fs/cgroup").join(relative);
    path.join("cpu.stat").is_file().then_some(path)
}

fn sample_cgroup(path: &Path, sample: &mut UsageSample) {
    if let Some(cpu_usec) = read_named_counter(&path.join("cpu.stat"), "usage_usec") {
        sample.cpu_usec = Some(sample.cpu_usec.unwrap_or(0).max(cpu_usec));
    }
    if let Ok(memory) = fs::read_to_string(path.join("memory.peak"))
        && let Ok(memory) = memory.trim().parse::<u64>()
    {
        sample.peak_memory_bytes = Some(sample.peak_memory_bytes.unwrap_or(0).max(memory));
    }
    if let Ok(io) = fs::read_to_string(path.join("io.stat")) {
        let (read, written) = parse_io_stat(&io);
        sample.io_read_bytes = Some(sample.io_read_bytes.unwrap_or(0).max(read));
        sample.io_write_bytes = Some(sample.io_write_bytes.unwrap_or(0).max(written));
    }
}

fn read_named_counter(path: &Path, key: &str) -> Option<u64> {
    fs::read_to_string(path).ok()?.lines().find_map(|line| {
        line.strip_prefix(key)
            .and_then(|value| value.strip_prefix(' '))
            .and_then(|value| value.parse().ok())
    })
}

fn parse_io_stat(value: &str) -> (u64, u64) {
    let mut read = 0_u64;
    let mut written = 0_u64;
    for field in value.split_ascii_whitespace() {
        if let Some(bytes) = field.strip_prefix("rbytes=")
            && let Ok(bytes) = bytes.parse::<u64>()
        {
            read = read.saturating_add(bytes);
        }
        if let Some(bytes) = field.strip_prefix("wbytes=")
            && let Ok(bytes) = bytes.parse::<u64>()
        {
            written = written.saturating_add(bytes);
        }
    }
    (read, written)
}

impl UsageSample {
    fn finish(self, elapsed: Duration) -> Usage {
        Usage {
            wall_ms: elapsed.as_millis(),
            observed_cpu_ms: self.cpu_usec.map(|value| value / 1_000),
            observed_peak_memory_bytes: self.peak_memory_bytes,
            observed_io_read_bytes: self.io_read_bytes,
            observed_io_write_bytes: self.io_write_bytes,
        }
    }
}

fn await_snapshot_ready(reader: &mut BufReader<UnixStream>) -> io::Result<()> {
    loop {
        let mut line = String::new();
        let count = (&mut *reader).take(32).read_line(&mut line)?;
        if count == 0 {
            return Err(io::Error::other("launcher exited during snapshot"));
        }
        match line.as_str() {
            "PROGRESS\n" => {}
            "SNAPSHOT_READY\n" => return Ok(()),
            _ => return Err(io::Error::other("invalid snapshot progress message")),
        }
    }
}

fn read_container_exit<R: Read + AsFd>(stdout: &mut R) -> io::Result<u8> {
    // A Podman descendant can retain the wait client's stdout after the
    // client exits. Read only the short status line; never wait for EOF.
    let flags = OFlag::from_bits_truncate(fcntl(&*stdout, FcntlArg::F_GETFL)?);
    fcntl(&*stdout, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
    let mut output = [0_u8; 8];
    let mut length = 0;
    loop {
        if length == output.len() {
            return Err(io::Error::other("invalid container exit status"));
        }
        match stdout.read(&mut output[length..]) {
            Ok(0) => return Err(io::Error::other("missing container exit status")),
            Ok(read) => {
                length += read;
                if output[..length].contains(&b'\n') {
                    return parse_container_exit(&output[..length]);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                return Err(io::Error::other("incomplete container exit status"));
            }
            Err(error) => return Err(error),
        }
    }
}

fn parse_container_exit(output: &[u8]) -> io::Result<u8> {
    let value = std::str::from_utf8(output).map_err(io::Error::other)?;
    let value = value
        .strip_suffix('\n')
        .ok_or_else(|| io::Error::other("invalid container exit status"))?;
    value
        .parse::<u8>()
        .map_err(|_| io::Error::other("invalid container exit status"))
}

fn ensure_podman_wait_succeeded(status: ExitStatus) -> io::Result<()> {
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other("podman wait failed"))
    }
}

fn create_owned_container(
    state_dir: &Path,
    name: &str,
    request: &Start,
    stopping: &AtomicBool,
    lock: &Flock<File>,
) -> io::Result<String> {
    ensure_active(stopping, state_dir)?;
    if podman_status(state_dir, &["image", "exists", &request.image])?.code() != Some(0) {
        return Err(io::Error::other("container image is not available locally"));
    }
    ensure_active(stopping, state_dir)?;
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
        "--interactive",
        "--image-volume=ignore",
        "--http-proxy=false",
        "--tmpfs=/tmp:rw,nosuid,nodev,size=64m",
        "--tmpfs=/run:rw,nosuid,nodev,size=8m",
        "--pids-limit=64",
        "--memory=512m",
        "--cpus=1",
    ]);
    if request.workspace_snapshot {
        create.args(["--workdir", "/workspace"]);
    }
    create
        .arg(format!("--name={name}"))
        .arg(format!("--label=io.reef.owner={name}"))
        .arg(format!("--label=io.reef.token={token}"))
        .arg(format!(
            "--cidfile={}",
            state_dir.join("container.cid").display()
        ))
        .arg(&request.image)
        .args(&request.command);
    if !bounded_status_with_lock(&mut create, PODMAN_TIMEOUT, lock)?.success() {
        return Err(io::Error::other("podman create failed"));
    }
    ensure_active(stopping, state_dir)?;
    let id = read_container_id(state_dir)?
        .ok_or_else(|| io::Error::other("podman did not record the created container ID"))?;
    verify_container_label(state_dir, &id, name, &token)?;
    persist_container_id(state_dir, &id)?;
    reject_effective_mounts(state_dir, &id)?;
    ensure_active(stopping, state_dir)?;
    if request.workspace_snapshot {
        let source = state_dir.join("workspace-snapshot/source");
        if !source.is_dir() {
            return Err(io::Error::other("missing workspace snapshot"));
        }
        let mut copy = podman_command(state_dir)?;
        copy.arg("cp")
            .arg(format!("{}/.", source.display()))
            .arg(format!("{id}:/workspace"));
        if !bounded_status_with_lock(&mut copy, PODMAN_TIMEOUT, lock)?.success() {
            return Err(io::Error::other("podman snapshot copy failed"));
        }
        ensure_active(stopping, state_dir)?;
    }
    Ok(id)
}

fn ensure_active(stopping: &AtomicBool, state_dir: &Path) -> io::Result<()> {
    if stopping.load(Ordering::Relaxed) || state_dir.join("cancelled").exists() {
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
        let id = match std::str::from_utf8(&contents) {
            Ok(id) => id.trim(),
            Err(_) if filename == "container.cid" => continue,
            Err(error) => return Err(io::Error::other(error)),
        };
        if id.len() != 64 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            if filename == "container.cid" {
                // Podman may open this file before it has written the ID. The
                // pre-created owner token can recover that creation window.
                continue;
            }
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
    podman_status_bounded(state_dir, args, PODMAN_TIMEOUT)
}

fn podman_status_bounded(
    state_dir: &Path,
    args: &[&str],
    timeout: Duration,
) -> io::Result<ExitStatus> {
    let mut command = podman_command(state_dir)?;
    command.args(args);
    bounded_status(&mut command, timeout)
}

fn podman_command(state_dir: &Path) -> io::Result<Command> {
    let store = read_podman_store(state_dir)?;
    let mut command = private_podman_command(state_dir, &store)?;
    command.arg(format!("--hooks-dir={}", state_dir.join("hooks").display()));
    Ok(command)
}

fn private_podman_command(state_dir: &Path, store: &PodmanStore) -> io::Result<Command> {
    let data_home = podman_data_home()?;
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
        .env_remove("STORAGE_DRIVER")
        .env_remove("STORAGE_OPTS")
        .env_remove("CONTAINER_HOST")
        .env_remove("CONTAINER_CONNECTION")
        .arg("--remote=false");
    if store.config_file.is_file() {
        command.env("CONTAINERS_STORAGE_CONF", &store.config_file);
    } else {
        command.env_remove("CONTAINERS_STORAGE_CONF");
    }
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

fn bounded_status_with_lock(
    command: &mut Command,
    timeout: Duration,
    lock: &Flock<File>,
) -> io::Result<ExitStatus> {
    // The owner can be killed while Podman is creating or starting a
    // container. Let that one child retain the flock until it exits, so the
    // launcher cannot verify absence while creation is still in flight.
    let flags = FdFlag::from_bits_truncate(fcntl(&**lock, FcntlArg::F_GETFD)?);
    fcntl(&**lock, FcntlArg::F_SETFD(flags & !FdFlag::FD_CLOEXEC))?;
    let spawned = command.stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    let restored = fcntl(&**lock, FcntlArg::F_SETFD(flags));
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            restored?;
            return Err(error);
        }
    };
    if let Err(error) = restored {
        let _ = child.kill();
        let _ = child.wait();
        return Err(io::Error::other(error));
    }
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

#[cfg(test)]
mod tests {
    use super::{
        PodmanStore, STORE_MAX_BYTES, await_snapshot_ready, ensure_podman_wait_succeeded,
        parse_container_exit, parse_done, parse_io_stat, read_container_exit, read_podman_store,
    };
    use std::fs;
    use std::io::{BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::ExitStatusExt;
    use std::path::PathBuf;
    use std::process::ExitStatus;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn snapshot_progress_keeps_owner_waiting_until_ready() {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        writer
            .write_all(b"PROGRESS\nPROGRESS\nSNAPSHOT_READY\n")
            .unwrap();
        assert!(await_snapshot_ready(&mut BufReader::new(reader)).is_ok());

        let (mut writer, reader) = UnixStream::pair().unwrap();
        writer.write_all(b"PROGRESS\nINVALID\n").unwrap();
        assert!(await_snapshot_ready(&mut BufReader::new(reader)).is_err());
    }

    #[test]
    fn owner_reads_a_valid_store_larger_than_four_kib() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let state =
            std::env::temp_dir().join(format!("reef-store-test-{}-{nonce}", std::process::id()));
        fs::create_dir(&state).unwrap();
        let store = PodmanStore {
            graph_root: PathBuf::from("/tmp/reef-graph"),
            run_root: PathBuf::from("/tmp/reef-run"),
            driver: "overlay".to_owned(),
            graph_options: serde_json::json!({"overlay.imagestore": "a".repeat(5000)}),
            config_file: PathBuf::new(),
        };
        let bytes = serde_json::to_vec(&store).unwrap();
        assert!(bytes.len() > 4096 && bytes.len() <= STORE_MAX_BYTES);
        fs::write(state.join("podman-store.json"), bytes).unwrap();
        let loaded = read_podman_store(&state).unwrap();
        assert_eq!(loaded.graph_options, store.graph_options);
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn failed_podman_wait_is_not_command_completion() {
        assert!(ensure_podman_wait_succeeded(ExitStatus::from_raw(0)).is_ok());
        assert!(ensure_podman_wait_succeeded(ExitStatus::from_raw(256)).is_err());
    }

    #[test]
    fn command_exit_is_not_confused_with_owner_cleanup() {
        for (container_output, expected) in [(b"0\n".as_slice(), 0), (b"7\n", 7), (b"255\n", 255)] {
            assert_eq!(parse_container_exit(container_output).unwrap(), expected);
            let completion = format!(
                "DONE {{\"code\":{expected},\"usage\":{{\"wall_ms\":4,\"observed_cpu_ms\":null,\"observed_peak_memory_bytes\":null,\"observed_io_read_bytes\":null,\"observed_io_write_bytes\":null}}}}\n"
            );
            assert_eq!(parse_done(completion.as_bytes()).unwrap().code, expected);
        }
        for invalid in [b"-1\n".as_slice(), b"256\n", b"0\nextra", b"", b" 7\n"] {
            assert!(parse_container_exit(invalid).is_err());
        }
        for invalid in [
            b"DONE\n".as_slice(),
            b"DONE 256\n",
            b"DONE -1\n",
            b"DONE 7",
            b"READY\n",
        ] {
            assert!(parse_done(invalid).is_err());
        }
    }

    #[test]
    fn sampled_block_io_sums_all_devices_without_counting_other_fields() {
        let cases = [
            (
                "8:0 rbytes=100 wbytes=20 rios=1\n8:16 rbytes=7 wbytes=3\n",
                (107, 23),
            ),
            ("", (0, 0)),
            ("8:0 rios=9 wios=2\n", (0, 0)),
        ];
        for (input, expected) in cases {
            assert_eq!(parse_io_stat(input), expected);
        }
    }

    #[test]
    fn exit_status_does_not_wait_for_a_lingering_pipe_writer() {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(b"7\n").unwrap();
        let (send, receive) = mpsc::channel();
        let worker = thread::spawn(move || {
            send.send(read_container_exit(&mut reader)).unwrap();
        });
        let result = receive.recv_timeout(Duration::from_secs(1));
        drop(writer);
        worker.join().unwrap();
        assert_eq!(result.unwrap().unwrap(), 7);
    }
}
