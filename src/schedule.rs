use crate::pressure::{self, Monitor, Policy};
use crate::run::{self, Admission, AdmissionStatus};
use fs2::FileExt;
use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use signal_hook::iterator::Signals;
use std::collections::{HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use sysinfo::System;

const POLL: Duration = Duration::from_millis(50);
const MIB: u64 = 1_048_576;

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    Acquire {
        cpu: u32,
        memory_mib: u64,
        identity: String,
    },
    Queue,
    Cancel {
        id: u64,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Reply {
    Queued { id: u64 },
    Granted { id: u64 },
    Cancelled { id: u64 },
    Snapshot { jobs: Vec<JobView> },
    Ok,
    Error { message: String },
}

#[derive(Clone, Serialize, Deserialize)]
struct JobView {
    id: u64,
    identity: String,
    cpu: u32,
    memory_mib: u64,
    state: String,
}

#[derive(Clone)]
struct Job {
    id: u64,
    identity: String,
    cpu: u32,
    memory_mib: u64,
    pid: Option<u32>,
    cancelled: bool,
}

struct State {
    next_id: u64,
    cpu: u32,
    memory_mib: u64,
    max_running: u32,
    waiting: VecDeque<Job>,
    running: HashMap<u64, Job>,
    pressure: Option<Monitor>,
}

impl State {
    fn new(cpu: u32, memory_mib: u64, max_running: u32, pressure: Option<Monitor>) -> Self {
        Self {
            next_id: 1,
            cpu,
            memory_mib,
            max_running,
            waiting: VecDeque::new(),
            running: HashMap::new(),
            pressure,
        }
    }

    fn add(&mut self, cpu: u32, memory_mib: u64, identity: String) -> Result<u64, String> {
        if cpu == 0 || memory_mib == 0 || cpu > self.cpu || memory_mib > self.memory_mib {
            return Err("request exceeds the scheduler budget or has a zero estimate".into());
        }
        if identity.is_empty()
            || identity.len() > 64
            || !identity.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
            })
        {
            return Err("invalid workload identity".into());
        }
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or("scheduler request ID exhausted")?;
        self.waiting.push_back(Job {
            id,
            identity,
            cpu,
            memory_mib,
            pid: None,
            cancelled: false,
        });
        Ok(id)
    }

    fn admit(&mut self, id: u64) -> bool {
        if self.waiting.front().is_none_or(|job| job.id != id) {
            return false;
        }
        let used_cpu: u32 = self.running.values().map(|job| job.cpu).sum();
        let used_memory: u64 = self.running.values().map(|job| job.memory_mib).sum();
        let first = self.waiting.front().expect("checked front");
        if self.pressure.as_ref().is_some_and(|pressure| {
            pressure
                .wait_reason(
                    first.cpu,
                    first.memory_mib,
                    used_cpu,
                    used_memory,
                    std::time::Instant::now(),
                )
                .is_some()
        }) {
            return false;
        }
        if self.running.len() >= self.max_running as usize
            || first.cpu > self.cpu - used_cpu
            || first.memory_mib > self.memory_mib - used_memory
        {
            return false;
        }
        let job = self.waiting.pop_front().expect("checked front");
        self.running.insert(id, job);
        true
    }

    fn cancel(&mut self, id: u64) -> bool {
        if let Some(job) = self.waiting.iter_mut().find(|job| job.id == id) {
            job.cancelled = true;
            return true;
        }
        if let Some(job) = self.running.get_mut(&id) {
            job.cancelled = true;
            return true;
        }
        false
    }

    fn remove(&mut self, id: u64) {
        self.waiting.retain(|job| job.id != id);
        self.running.remove(&id);
    }

    fn snapshot(&self) -> Vec<JobView> {
        let mut jobs: Vec<_> = self
            .waiting
            .iter()
            .map(|job| JobView {
                id: job.id,
                identity: job.identity.clone(),
                cpu: job.cpu,
                memory_mib: job.memory_mib,
                state: "queued".into(),
            })
            .collect();
        let mut running: Vec<_> = self
            .running
            .values()
            .map(|job| JobView {
                id: job.id,
                identity: job.identity.clone(),
                cpu: job.cpu,
                memory_mib: job.memory_mib,
                state: if job.cancelled {
                    "cancelling"
                } else {
                    "running"
                }
                .into(),
            })
            .collect();
        running.sort_by_key(|job| job.id);
        jobs.extend(running);
        jobs
    }
}

fn state_path(state_dir: Option<&Path>) -> io::Result<PathBuf> {
    if let Some(path) = state_dir {
        return Ok(path.to_path_buf());
    }
    let home = std::env::var_os("HOME").ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "HOME is not set; pass --state-dir")
    })?;
    Ok(PathBuf::from(home).join(".local/state/reef/schedule"))
}

fn socket_path(state_dir: Option<&Path>) -> io::Result<PathBuf> {
    Ok(state_path(state_dir)?.join("reef.sock"))
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
            "scheduler state directory must be private and owned by the current user",
        ));
    }
    Ok(())
}

pub fn serve(
    cpu: Option<u32>,
    memory_mib: Option<u64>,
    max_running: Option<u32>,
    state_dir: Option<&Path>,
    pressure_options: pressure::Options,
) -> io::Result<()> {
    let mut system = System::new_all();
    system.refresh_memory();
    let cpu = cpu.unwrap_or_else(|| {
        u32::try_from(system.cpus().len())
            .unwrap_or(u32::MAX)
            .saturating_sub(1)
            .max(1)
    });
    let memory_mib = memory_mib.unwrap_or_else(|| {
        (system.total_memory() / MIB)
            .saturating_mul(70)
            .saturating_div(100)
            .max(1)
    });
    let max_running = max_running.unwrap_or(cpu);
    let monitor = make_monitor(pressure_options, &system)?;
    let dir = state_path(state_dir)?;
    private_dir(&dir)?;
    let socket = dir.join("reef.sock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(dir.join("server.lock"))?;
    if lock.metadata()?.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "scheduler lock must be private",
        ));
    }
    lock.try_lock_exclusive().map_err(|error| {
        io::Error::new(error.kind(), format!("scheduler already running: {error}"))
    })?;
    match fs::symlink_metadata(&socket) {
        Ok(metadata) => {
            if !metadata.file_type().is_socket()
                || metadata.uid() != nix::unistd::Uid::current().as_raw()
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "scheduler socket path is not an owned socket",
                ));
            }
            match UnixStream::connect(&socket) {
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        "scheduler socket is active",
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                    fs::remove_file(&socket)?;
                }
                Err(error) => return Err(error),
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(&socket).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot bind scheduler socket {}: {error}", socket.display()),
        )
    })?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let state = Arc::new(Mutex::new(State::new(
        cpu,
        memory_mib,
        max_running,
        monitor,
    )));
    let stopping = Arc::new(AtomicBool::new(false));
    if !pressure_options.no_pressure {
        let state = Arc::clone(&state);
        let stopping = Arc::clone(&stopping);
        std::thread::spawn(move || monitor_pressure(&state, &stopping));
    }
    for signal in [SIGINT, SIGTERM, SIGHUP, SIGQUIT] {
        signal_hook::flag::register(signal, Arc::clone(&stopping))?;
    }
    eprintln!(
        "reef: scheduler ready (cpu={cpu}, memory_mib={memory_mib}, max_running={max_running})"
    );
    accept_clients(&listener, &state, &stopping)?;
    drop(listener);
    fs::remove_file(socket)
}

fn make_monitor(options: pressure::Options, system: &System) -> io::Result<Option<Monitor>> {
    let policy = Policy::new(
        options,
        u32::try_from(system.cpus().len()).unwrap_or(u32::MAX),
        system.total_memory(),
    )
    .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
    Ok((!options.no_pressure).then(|| Monitor::new(policy)))
}

fn accept_clients(
    listener: &UnixListener,
    state: &Arc<Mutex<State>>,
    stopping: &AtomicBool,
) -> io::Result<()> {
    while !stopping.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let state = Arc::clone(state);
                std::thread::spawn(move || {
                    if let Err(error) = handle(stream, &state) {
                        eprintln!("reef: scheduler client: {error}");
                    }
                });
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(POLL),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn monitor_pressure(state: &Arc<Mutex<State>>, stopping: &AtomicBool) {
    let mut system = System::new_all();
    system.refresh_cpu_usage();
    while !stopping.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_secs(1));
        let sample = pressure::sample(&mut system);
        let critical_jobs = {
            let mut state = state.lock().unwrap();
            if state
                .pressure
                .as_mut()
                .is_some_and(|monitor| monitor.update(sample))
            {
                let mut jobs: Vec<_> = state
                    .running
                    .values()
                    .map(|job| format!("{} ({})", job.identity, job.id))
                    .collect();
                jobs.sort();
                Some(jobs.join(", "))
            } else {
                None
            }
        };
        if let Some(jobs) = critical_jobs {
            eprintln!(
                "reef: critical workstation pressure; tracked running workloads: [{jobs}]; inspect `reef queue` and reduce load"
            );
        }
    }
}

fn handle(mut stream: UnixStream, shared: &Arc<Mutex<State>>) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let request: Request = serde_json::from_slice(&read_line(&mut stream)?)?;
    match request {
        Request::Queue => {
            let jobs = shared.lock().unwrap().snapshot();
            send(&mut stream, &Reply::Snapshot { jobs })
        }
        Request::Cancel { id } => {
            let found = shared.lock().unwrap().cancel(id);
            if found {
                send(&mut stream, &Reply::Ok)
            } else {
                send(
                    &mut stream,
                    &Reply::Error {
                        message: format!("request {id} not found"),
                    },
                )
            }
        }
        Request::Acquire {
            cpu,
            memory_mib,
            identity,
        } => {
            let id = match shared.lock().unwrap().add(cpu, memory_mib, identity) {
                Ok(id) => id,
                Err(message) => return send(&mut stream, &Reply::Error { message }),
            };
            if let Err(error) = send(&mut stream, &Reply::Queued { id }) {
                shared.lock().unwrap().remove(id);
                return Err(error);
            }
            let result = hold_job(&mut stream, shared, id);
            if result.is_err()
                && let Some(pid) = shared
                    .lock()
                    .unwrap()
                    .running
                    .get(&id)
                    .and_then(|job| job.pid)
            {
                let _ = stop_group(pid);
            }
            shared.lock().unwrap().remove(id);
            result
        }
    }
}

fn hold_job(stream: &mut UnixStream, shared: &Arc<Mutex<State>>, id: u64) -> io::Result<()> {
    stream.set_read_timeout(Some(POLL))?;
    loop {
        let decision = {
            let mut state = shared.lock().unwrap();
            if state
                .waiting
                .iter()
                .any(|job| job.id == id && job.cancelled)
            {
                QueueDecision::Cancelled
            } else if state.admit(id) {
                QueueDecision::Granted
            } else {
                QueueDecision::Waiting
            }
        };
        if matches!(decision, QueueDecision::Cancelled) {
            return send(stream, &Reply::Cancelled { id });
        }
        if matches!(decision, QueueDecision::Granted) {
            send(stream, &Reply::Granted { id })?;
            break;
        }
        let mut byte = [0];
        match stream.read(&mut byte) {
            Ok(0) => return Ok(()),
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unexpected queued client data",
                ));
            }
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.kind() == io::ErrorKind::TimedOut => {}
            Err(error) => return Err(error),
        }
    }

    hold_running(stream, shared, id)
}

enum QueueDecision {
    Waiting,
    Granted,
    Cancelled,
}

fn hold_running(stream: &mut UnixStream, shared: &Arc<Mutex<State>>, id: u64) -> io::Result<()> {
    let mut buffer = Vec::new();
    let mut stop_sent = false;
    loop {
        let cancel_pid = {
            let state = shared.lock().unwrap();
            state
                .running
                .get(&id)
                .and_then(|job| if job.cancelled { job.pid } else { None })
        };
        if let Some(pid) = cancel_pid.filter(|_| !stop_sent) {
            send_raw(stream, b"C")?;
            stop_group(pid)?;
            stop_sent = true;
        }
        let mut byte = [0];
        match stream.read(&mut byte) {
            Ok(0) => {
                if let Some(pid) = shared
                    .lock()
                    .unwrap()
                    .running
                    .get(&id)
                    .and_then(|job| job.pid)
                {
                    stop_group(pid)?;
                }
                return Ok(());
            }
            Ok(_) if byte[0] == b'\n' => {
                let line = std::str::from_utf8(&buffer).map_err(io::Error::other)?;
                if line == "done" {
                    return Ok(());
                }
                if let Some(pid) = line
                    .strip_prefix("started ")
                    .and_then(|pid| pid.parse::<u32>().ok())
                {
                    if pid == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "invalid child PID",
                        ));
                    }
                    if let Some(job) = shared.lock().unwrap().running.get_mut(&id) {
                        job.pid = Some(pid);
                    }
                } else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid lease update",
                    ));
                }
                buffer.clear();
            }
            Ok(_) => {
                if buffer.len() >= 64 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "lease update too large",
                    ));
                }
                buffer.push(byte[0]);
            }
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.kind() == io::ErrorKind::TimedOut => {}
            Err(error) => return Err(error),
        }
    }
}

fn stop_group(pid: u32) -> io::Result<()> {
    let pid = Pid::from_raw(i32::try_from(pid).map_err(io::Error::other)?);
    match killpg(pid, Signal::SIGKILL) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(error) => Err(io::Error::other(error)),
    }
}

fn send(stream: &mut UnixStream, reply: &Reply) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(reply)?;
    bytes.push(b'\n');
    send_raw(stream, &bytes)
}

fn send_raw(stream: &mut UnixStream, bytes: &[u8]) -> io::Result<()> {
    stream.write_all(bytes)
}

fn read_line(stream: &mut UnixStream) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        if byte[0] == b'\n' {
            return Ok(bytes);
        }
        if bytes.len() >= 4096 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "scheduler message too large",
            ));
        }
        bytes.push(byte[0]);
    }
}

fn poll_line(stream: &mut UnixStream, buffer: &mut Vec<u8>) -> io::Result<Option<Vec<u8>>> {
    let mut byte = [0];
    match stream.read(&mut byte) {
        Ok(0) => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "scheduler disconnected",
        )),
        Ok(_) if byte[0] == b'\n' => Ok(Some(std::mem::take(buffer))),
        Ok(_) => {
            if buffer.len() >= 4096 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "scheduler message too large",
                ));
            }
            buffer.push(byte[0]);
            Ok(None)
        }
        Err(error)
            if error.kind() == io::ErrorKind::WouldBlock
                || error.kind() == io::ErrorKind::TimedOut =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn connect(state_dir: Option<&Path>) -> io::Result<UnixStream> {
    let path = socket_path(state_dir)?;
    UnixStream::connect(&path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "scheduler unavailable at {}: {error}; start `reef serve`",
                path.display()
            ),
        )
    })
}

fn request(stream: &mut UnixStream, request: &Request) -> io::Result<Reply> {
    let mut bytes = serde_json::to_vec(request)?;
    bytes.push(b'\n');
    stream.write_all(&bytes)?;
    serde_json::from_slice(&read_line(stream)?).map_err(io::Error::other)
}

pub fn queue(state_dir: Option<&Path>) -> io::Result<()> {
    let mut stream = connect(state_dir)?;
    match request(&mut stream, &Request::Queue)? {
        Reply::Snapshot { jobs } => {
            println!("{}", serde_json::to_string_pretty(&jobs)?);
            Ok(())
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid queue response",
        )),
    }
}

pub fn cancel(id: u64, state_dir: Option<&Path>) -> io::Result<()> {
    let mut stream = connect(state_dir)?;
    match request(&mut stream, &Request::Cancel { id })? {
        Reply::Ok => Ok(()),
        Reply::Error { message } => Err(io::Error::new(io::ErrorKind::NotFound, message)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid cancel response",
        )),
    }
}

struct Lease {
    stream: UnixStream,
    cancelled: bool,
}

impl Admission for Lease {
    fn started(&mut self, pid: u32) -> io::Result<()> {
        self.stream
            .write_all(format!("started {pid}\n").as_bytes())?;
        self.stream.set_nonblocking(true)
    }

    fn status(&mut self) -> io::Result<AdmissionStatus> {
        let mut byte = [0];
        match self.stream.read(&mut byte) {
            Ok(0) => Ok(AdmissionStatus::Disconnected),
            Ok(_) if byte[0] == b'C' => {
                self.cancelled = true;
                Ok(AdmissionStatus::Cancelled)
            }
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid lease response",
            )),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(if self.cancelled {
                AdmissionStatus::Cancelled
            } else {
                AdmissionStatus::Active
            }),
            Err(error) => Err(error),
        }
    }

    fn finished(&mut self) -> io::Result<()> {
        self.stream.set_nonblocking(false)?;
        self.stream.write_all(b"done\n")
    }
}

#[derive(Clone, Copy)]
pub struct RunOptions<'a> {
    pub command: &'a [String],
    pub category: &'a str,
    pub identity: &'a str,
    pub record: Option<&'a Path>,
    pub cpu: u32,
    pub memory_mib: u64,
    pub state_dir: Option<&'a Path>,
}

pub fn schedule(options: RunOptions<'_>) -> io::Result<u8> {
    let RunOptions {
        command,
        category,
        identity,
        record,
        cpu,
        memory_mib,
        state_dir,
    } = options;
    if std::env::var_os("REEF_ADMITTED").as_deref() == Some(std::ffi::OsStr::new("1")) {
        return run::run(command, category, identity, record);
    }
    let mut stream = connect(state_dir)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let response = request(
        &mut stream,
        &Request::Acquire {
            cpu,
            memory_mib,
            identity: identity.to_owned(),
        },
    )?;
    let id = match response {
        Reply::Queued { id } => id,
        Reply::Error { message } => {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, message));
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid admission response",
            ));
        }
    };
    eprintln!("reef: queued request {id}");
    let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP, SIGQUIT])?;
    stream.set_read_timeout(Some(POLL))?;
    let mut response_buffer = Vec::new();
    loop {
        if let Some(signal) = signals.pending().next() {
            return Ok(128_u8.saturating_add(u8::try_from(signal).unwrap_or(1)));
        }
        match poll_line(&mut stream, &mut response_buffer) {
            Ok(Some(line)) => match serde_json::from_slice::<Reply>(&line)? {
                Reply::Granted { id: granted } if granted == id => {
                    eprintln!("reef: admitted request {id}");
                    let mut lease = Lease {
                        stream,
                        cancelled: false,
                    };
                    return run::run_with(command, category, identity, record, Some(&mut lease));
                }
                Reply::Cancelled { id: cancelled } if cancelled == id => return Ok(130),
                Reply::Error { message } => return Err(io::Error::other(message)),
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid admission update",
                    ));
                }
            },
            Ok(None) => {}
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("scheduler disconnected while queued: {error}"),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::State;

    #[test]
    fn fifo_admission_holds_small_job_behind_large_one() {
        let mut state = State::new(4, 8, 3, None);
        let first = state.add(3, 4, "first".into()).unwrap();
        let second = state.add(2, 4, "second".into()).unwrap();
        let third = state.add(1, 1, "third".into()).unwrap();
        assert!(state.admit(first));
        assert!(!state.admit(third));
        assert!(!state.admit(second));
        state.remove(first);
        assert!(state.admit(second));
        assert!(state.admit(third));
    }

    #[test]
    fn rejects_requests_that_cannot_fit() {
        let mut state = State::new(2, 4096, 2, None);
        assert!(state.add(3, 1, "oversized".into()).is_err());
        assert!(state.add(1, 4097, "oversized".into()).is_err());
        assert!(state.add(0, 1, "zero".into()).is_err());
    }

    #[test]
    fn cancellation_and_release_free_capacity() {
        let mut state = State::new(1, 1024, 1, None);
        let first = state.add(1, 1024, "first".into()).unwrap();
        let second = state.add(1, 1024, "second".into()).unwrap();
        assert!(state.admit(first));
        assert!(!state.admit(second));
        assert!(state.cancel(second));
        state.remove(second);
        state.remove(first);
        let third = state.add(1, 1024, "third".into()).unwrap();
        assert!(state.admit(third));
    }
}
