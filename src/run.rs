mod platform;

use crate::history;
use fs2::FileExt;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use sysinfo::{Pid, ProcessesToUpdate, System};
use wait4::Wait4;

const SAMPLE_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Clone, Copy)]
pub enum RecordMode<'a> {
    Default,
    Custom(&'a Path),
    Disabled,
}

enum RecordWriter {
    History,
    Custom(File),
    Disabled,
}

pub trait Admission {
    fn started(&mut self, pid: u32) -> io::Result<()>;
    fn status(&mut self) -> io::Result<AdmissionStatus>;
    fn finished(&mut self, outcome: &str) -> io::Result<()>;
}

pub enum AdmissionStatus {
    Active,
    Cancelled,
    Disconnected,
}

#[derive(Serialize)]
struct Measurement<'a> {
    category: &'a str,
    identity: &'a str,
    status: &'a str,
    exit_code: Option<i32>,
    signal: Option<i32>,
    started_at_unix_ms: u128,
    ended_at_unix_ms: u128,
    wall_ms: u128,
    cpu_ms: u128,
    peak_memory_bytes: u64,
    tree_cpu_ms: u128,
    tree_peak_memory_bytes: u64,
    tree_usage_complete: bool,
}

#[derive(Default)]
struct TreeSampler {
    system: System,
    cpu_by_process: HashMap<(Pid, u64), u64>,
    peak_memory_bytes: u64,
}

impl TreeSampler {
    fn sample(&mut self, root: Pid) {
        self.system.refresh_processes(ProcessesToUpdate::All, true);
        let processes = self.system.processes();
        let mut members = HashSet::from([root]);
        loop {
            let count = members.len();
            for (&pid, process) in processes {
                if process
                    .parent()
                    .is_some_and(|parent| members.contains(&parent))
                {
                    members.insert(pid);
                }
            }
            if members.len() == count {
                break;
            }
        }

        let mut memory = 0_u64;
        for pid in members {
            if let Some(process) = processes.get(&pid) {
                memory = memory.saturating_add(process.memory());
                self.cpu_by_process
                    .entry((pid, process.start_time()))
                    .and_modify(|cpu| *cpu = (*cpu).max(process.accumulated_cpu_time()))
                    .or_insert_with(|| process.accumulated_cpu_time());
            }
        }
        self.peak_memory_bytes = self.peak_memory_bytes.max(memory);
    }

    fn cpu_ms(&self) -> u128 {
        self.cpu_by_process
            .values()
            .map(|&value| u128::from(value))
            .sum()
    }
}

pub fn run(
    command: &[String],
    category: &str,
    identity: &str,
    record: RecordMode<'_>,
) -> io::Result<u8> {
    run_with(command, category, identity, record, None)
}

pub fn run_with(
    command: &[String],
    category: &str,
    identity: &str,
    record: RecordMode<'_>,
    mut admission: Option<&mut dyn Admission>,
) -> io::Result<u8> {
    let mut record_writer = open_record_writer(record);

    let mut signals = platform::signals()?;
    let started_at_unix_ms = unix_ms();
    let start = Instant::now();
    let interactive = io::stdin().is_terminal() && platform::has_foreground_stdin();
    let mut process = command_process(command, interactive, admission.is_some());
    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(error) => {
            let exit = spawn_failed(
                &error,
                category,
                identity,
                started_at_unix_ms,
                start,
                &mut record_writer,
            );
            if let Some(lease) = admission.as_mut() {
                let _ = lease.finished("failed");
            }
            return Ok(exit);
        }
    };
    let foreground = prepare_child(&mut child, interactive, &mut admission)?;
    let mut sampler = TreeSampler::default();
    let (result, forwarded, scheduler_lost, scheduler_cancelled) =
        wait_child(&mut child, &mut sampler, &mut signals, &mut admission)?;
    let exit_signal = platform::exit_signal(result.status);
    let cpu_ms = (result.rusage.utime + result.rusage.stime).as_millis();
    let signal = exit_signal;
    let exit_code = result.status.code();
    let status = if scheduler_cancelled
        || forwarded.is_some()
        || signal.is_some_and(platform::is_interrupt_signal)
    {
        "cancelled"
    } else if result.status.success() {
        "success"
    } else {
        "failed"
    };
    let measurement = Measurement {
        category,
        identity,
        status,
        exit_code,
        signal: signal.or(forwarded),
        started_at_unix_ms,
        ended_at_unix_ms: unix_ms(),
        wall_ms: start.elapsed().as_millis(),
        cpu_ms,
        peak_memory_bytes: result.rusage.maxrss,
        tree_cpu_ms: sampler.cpu_ms().max(cpu_ms),
        tree_peak_memory_bytes: sampler.peak_memory_bytes.max(result.rusage.maxrss),
        tree_usage_complete: false,
    };
    report(&measurement, &mut record_writer);
    if (admission.is_some() && !scheduler_cancelled)
        || forwarded.is_some()
        || (exit_signal.is_some() && !scheduler_cancelled)
    {
        cleanup_group(child.id())?;
    }
    drop(foreground);
    if scheduler_lost {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "scheduler disconnected; command stopped",
        ));
    }
    if let Some(lease) = admission.as_mut() {
        lease.finished(status)?;
    }
    if scheduler_cancelled {
        return Ok(130);
    }
    Ok(exit_code.map_or_else(
        || 128_u8.saturating_add(u8::try_from(signal.or(forwarded).unwrap_or(1)).unwrap_or(1)),
        |code| u8::try_from(code).unwrap_or(1),
    ))
}

fn prepare_child(
    child: &mut std::process::Child,
    interactive: bool,
    admission: &mut Option<&mut dyn Admission>,
) -> io::Result<Option<platform::Foreground>> {
    if (interactive || admission.is_some())
        && let Err(error) = platform::wait_stopped(child.id())
    {
        let _ = platform::force_stop(child.id());
        let _ = child.wait();
        return Err(error);
    }
    if let Some(lease) = admission.as_mut()
        && let Err(error) = lease.started(child.id())
    {
        platform::force_stop(child.id())?;
        let _ = child.wait();
        return Err(error);
    }
    let foreground = if interactive {
        match platform::Foreground::activate(child.id()) {
            Ok(guard) => Some(guard),
            Err(error) => {
                let _ = platform::force_stop(child.id());
                let _ = child.wait();
                return Err(error);
            }
        }
    } else {
        None
    };
    if (interactive || admission.is_some())
        && let Err(error) = platform::resume(child.id())
    {
        let _ = platform::force_stop(child.id());
        let _ = child.wait();
        return Err(error);
    }
    Ok(foreground)
}

fn wait_child(
    child: &mut std::process::Child,
    sampler: &mut TreeSampler,
    signals: &mut signal_hook::iterator::Signals,
    admission: &mut Option<&mut dyn Admission>,
) -> io::Result<(wait4::ResUse, Option<i32>, bool, bool)> {
    let root = Pid::from_u32(child.id());
    let mut forwarded = None;
    let mut interrupted_at = None;
    let mut scheduler_lost = false;
    let mut scheduler_cancelled = false;
    loop {
        sampler.sample(root);
        if let Some(lease) = admission.as_mut() {
            match lease.status().unwrap_or(AdmissionStatus::Disconnected) {
                AdmissionStatus::Cancelled if !scheduler_cancelled => {
                    let _ = platform::force_stop(child.id());
                    scheduler_cancelled = true;
                }
                AdmissionStatus::Disconnected if !scheduler_lost => {
                    platform::force_stop(child.id())?;
                    scheduler_lost = true;
                }
                _ => {}
            }
        }
        for signal in signals.pending() {
            platform::forward(child.id(), signal)?;
            if forwarded.is_none() {
                interrupted_at = Some(Instant::now());
            }
            forwarded = Some(signal);
        }
        if let Some(result) = child.try_wait4()? {
            if let Some(lease) = admission.as_mut() {
                match lease.status().unwrap_or(AdmissionStatus::Disconnected) {
                    AdmissionStatus::Cancelled => scheduler_cancelled = true,
                    AdmissionStatus::Disconnected => scheduler_lost = true,
                    AdmissionStatus::Active => {}
                }
            }
            return Ok((result, forwarded, scheduler_lost, scheduler_cancelled));
        }
        if interrupted_at.is_some_and(|time| time.elapsed() >= Duration::from_secs(2)) {
            platform::force_stop(child.id())?;
        }
        std::thread::sleep(SAMPLE_INTERVAL);
    }
}

fn spawn_failed(
    error: &io::Error,
    category: &str,
    identity: &str,
    started_at_unix_ms: u128,
    start: Instant,
    record_writer: &mut RecordWriter,
) -> u8 {
    let code = if error.kind() == io::ErrorKind::NotFound {
        127
    } else {
        126
    };
    let measurement = Measurement {
        category,
        identity,
        status: "failed",
        exit_code: Some(code),
        signal: None,
        started_at_unix_ms,
        ended_at_unix_ms: unix_ms(),
        wall_ms: start.elapsed().as_millis(),
        cpu_ms: 0,
        peak_memory_bytes: 0,
        tree_cpu_ms: 0,
        tree_peak_memory_bytes: 0,
        tree_usage_complete: false,
    };
    eprintln!("reef: command could not start: {error}");
    report(&measurement, record_writer);
    u8::try_from(code).unwrap_or(1)
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn command_process(command: &[String], interactive: bool, scheduled: bool) -> Command {
    let mut process = if interactive || scheduled {
        let mut wrapper = Command::new("/bin/sh");
        wrapper
            .arg("-c")
            .arg("kill -STOP $$; exec \"$@\"")
            .arg("reef-run")
            .args(command);
        wrapper
    } else {
        let mut direct = Command::new(&command[0]);
        direct.args(&command[1..]);
        direct
    };
    process
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if scheduled {
        process.env("REEF_ADMITTED", "1");
    }
    platform::isolate(&mut process);
    process
}

fn cleanup_group(pid: u32) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    while platform::group_exists(pid)? && Instant::now() < deadline {
        std::thread::sleep(SAMPLE_INTERVAL);
    }
    if platform::group_exists(pid)? {
        platform::force_stop(pid)?;
    }
    Ok(())
}

fn emit(measurement: &Measurement<'_>, writer: &mut RecordWriter) -> io::Result<()> {
    eprintln!("reef: {}", serde_json::to_string(measurement)?);
    match writer {
        RecordWriter::History => history::save(measurement)?,
        RecordWriter::Custom(file) => {
            file.lock_exclusive()?;
            let mut line = serde_json::to_vec(measurement)?;
            line.push(b'\n');
            file.write_all(&line)?;
            FileExt::unlock(file)?;
        }
        RecordWriter::Disabled => {}
    }
    Ok(())
}

fn report(measurement: &Measurement<'_>, writer: &mut RecordWriter) {
    if let Err(error) = emit(measurement, writer) {
        eprintln!("reef: cannot save measurement: {error}");
    }
}

fn open_record_writer(record: RecordMode<'_>) -> RecordWriter {
    match record {
        RecordMode::Default => RecordWriter::History,
        RecordMode::Disabled => RecordWriter::Disabled,
        RecordMode::Custom(path) => match open_record(path) {
            Ok(file) => RecordWriter::Custom(file),
            Err(error) => {
                eprintln!("reef: cannot open measurement record: {error}");
                RecordWriter::Disabled
            }
        },
    }
}

fn open_record(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.append(true).create(true);
    platform::private_creation(&mut options);
    let file = options.open(path)?;
    if !file.metadata()?.is_file() || platform::is_public(&file.metadata()?) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "record must be a private regular file",
        ));
    }
    Ok(file)
}
