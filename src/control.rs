use crate::run::{self, RecordMode};
use crate::{contain, schedule};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const EVENTS: &str = "attempts.jsonl";
const MEASUREMENTS: &str = "measurements.jsonl";
const ENROLLMENT_LOCK: &str = "enrollment.lock";
const CANARY_OFF: &str = "canary.off";
const TELEMETRY_WARNING: &str = "private control telemetry unavailable; control enrollment refused";

#[derive(Clone, Deserialize, Serialize)]
struct Event {
    attempt_id: String,
    #[serde(rename = "event")]
    kind: String,
    at_unix_ms: u128,
    agent: String,
    category: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    assignment: Option<String>,
}

#[derive(Default, Serialize)]
pub struct Summary {
    pub available: bool,
    pub attempted: Option<usize>,
    pub completed: Option<usize>,
    pub pending: Option<usize>,
    pub unavailable: Option<usize>,
    pub warning: Option<&'static str>,
    pub groups: Vec<Group>,
}

#[derive(Default, Serialize)]
pub struct Group {
    pub agent: String,
    pub category: String,
    pub arm: String,
    pub assigned: usize,
    pub measured: usize,
    pub pending: usize,
    pub unavailable: usize,
    pub success: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub measured_wall_ms: u128,
    pub measured_cpu_ms: u128,
    pub peak_memory_bytes: u64,
}

impl Summary {
    pub(crate) fn unavailable() -> Self {
        Self {
            warning: Some("private control telemetry is missing, unreadable, or invalid"),
            ..Self::default()
        }
    }
}

pub fn default_dir() -> io::Result<PathBuf> {
    let home = env::var_os("HOME")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    Ok(PathBuf::from(home).join(".local/state/reef/control"))
}

fn private_dir(dir: &Path, create: bool) -> io::Result<()> {
    if create && !dir.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    let metadata = fs::symlink_metadata(dir)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != nix::unistd::Uid::current().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "control directory is not private",
        ));
    }
    Ok(())
}

pub(crate) fn private_file(path: &Path, create: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(create)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != nix::unistd::Uid::current().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "control file is not private",
        ));
    }
    Ok(file)
}

fn prepare_inner(dir: &Path) -> io::Result<()> {
    private_dir(dir, true)?;
    let lock_path = dir.join(ENROLLMENT_LOCK);
    if !lock_path.exists() && dir.join(EVENTS).exists() && dir.join(MEASUREMENTS).exists() {
        return validate_existing(dir);
    }
    let lock = private_file(&lock_path, true)?;
    lock.lock_exclusive()?;
    if dir.join(EVENTS).exists() != dir.join(MEASUREMENTS).exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "control telemetry is incomplete",
        ));
    }
    for name in [EVENTS, MEASUREMENTS] {
        private_file(&dir.join(name), true)?.sync_all()?;
    }
    validate_existing(dir)
}

fn validate_existing(dir: &Path) -> io::Result<()> {
    let state = summary(Some(dir), 0, u128::MAX);
    if !state.available || state.warning.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control telemetry is incomplete",
        ));
    }
    Ok(())
}

pub fn prepare(state_dir: Option<&Path>) -> io::Result<PathBuf> {
    let dir = match state_dir {
        Some(path) => path.join("control"),
        None => default_dir().map_err(|_| io::Error::other(TELEMETRY_WARNING))?,
    };
    prepare_inner(&dir).map_err(|_| io::Error::other(TELEMETRY_WARNING))?;
    Ok(dir)
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn attempt_id() -> io::Result<String> {
    let mut bytes = [0_u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let mut id = String::with_capacity(32);
    for byte in bytes {
        write!(&mut id, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(id)
}

fn append(dir: &Path, event: &Event) -> io::Result<()> {
    let mut file = private_file(&dir.join(EVENTS), false)?;
    file.lock_exclusive()?;
    let mut line = serde_json::to_vec(event)?;
    line.push(b'\n');
    file.write_all(&line)?;
    file.sync_data()?;
    FileExt::unlock(&file)?;
    Ok(())
}

fn disabled(dir: &Path) -> io::Result<bool> {
    let path = dir.join(CANARY_OFF);
    let mut file = match private_file(&path, false) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    if contents != "off\n" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "canary off switch is invalid",
        ));
    }
    Ok(true)
}

pub fn canary_action(state_dir: Option<&Path>, action: &str) -> io::Result<()> {
    let dir = if state_dir.is_some() {
        prepare(state_dir)?
    } else if let Some(pinned) = env::var_os("REEF_AGENT_CONTROL_DIR") {
        let pinned = PathBuf::from(pinned);
        if !pinned.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pinned agent control directory must be absolute",
            ));
        }
        prepare_inner(&pinned).map_err(|_| io::Error::other(TELEMETRY_WARNING))?;
        pinned
    } else {
        prepare(None)?
    };
    let lock = private_file(&dir.join(ENROLLMENT_LOCK), true)?;
    lock.lock_exclusive()?;
    let was_off = disabled(&dir)?;
    match action {
        "off" if !was_off => {
            let mut file = private_file(&dir.join(CANARY_OFF), true)?;
            file.write_all(b"off\n")?;
            file.sync_all()?;
        }
        "on" if was_off => fs::remove_file(dir.join(CANARY_OFF))?,
        "on" | "off" | "status" => {}
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid canary action",
            ));
        }
    }
    eprintln!(
        "reef: new best-effort canary treatment assignments are {}",
        if action == "off" || (action == "status" && was_off) {
            "off"
        } else {
            "on"
        }
    );
    Ok(())
}

fn assignment(dir: &Path, event: &mut Event, percent: Option<u8>) -> io::Result<bool> {
    if let Some(percent) = percent {
        let lock = private_file(&dir.join(ENROLLMENT_LOCK), true)?;
        lock.lock_exclusive()?;
        let paused = disabled(dir)?;
        let treatment = !paused
            && u32::from_str_radix(&event.attempt_id[..8], 16).map_err(io::Error::other)? % 100
                < u32::from(percent);
        event.assignment = Some(
            if paused {
                "paused"
            } else if treatment {
                "treatment"
            } else {
                "control"
            }
            .to_owned(),
        );
        append(dir, event)?;
        Ok(treatment)
    } else {
        append(dir, event)?;
        Ok(false)
    }
}

pub fn run(original: Option<&Path>, args: &[OsString], category: &str) -> io::Result<u8> {
    let dir =
        env::var_os("REEF_AGENT_CONTROL_DIR").ok_or_else(|| io::Error::other(TELEMETRY_WARNING))?;
    let dir = PathBuf::from(dir);
    validate_existing(&dir).map_err(|_| io::Error::other(TELEMETRY_WARNING))?;
    let id = attempt_id().map_err(|_| io::Error::other(TELEMETRY_WARNING))?;
    let agent = env::var("REEF_AGENT_KIND")
        .ok()
        .filter(|value| matches!(value.as_str(), "codex" | "claude" | "opencode"))
        .ok_or_else(|| io::Error::other("invalid control agent kind"))?;
    let mut event = Event {
        attempt_id: id.clone(),
        kind: "assigned".to_owned(),
        at_unix_ms: now_ms(),
        agent,
        category: category.to_owned(),
        assignment: Some("control".to_owned()),
    };
    let percent = env::var("REEF_AGENT_CANARY_PERCENT")
        .ok()
        .map(|value| value.parse::<u8>())
        .transpose()
        .map_err(|_| io::Error::other("invalid canary percentage"))?;
    if percent.is_some_and(|value| value > 100) {
        return Err(io::Error::other("invalid canary percentage"));
    }
    let treatment =
        assignment(&dir, &mut event, percent).map_err(|_| io::Error::other(TELEMETRY_WARNING))?;
    let result = match original {
        Some(original) => {
            let command = std::iter::once(original.as_os_str().to_os_string())
                .chain(args.iter().cloned())
                .map(OsString::into_string)
                .collect::<Result<Vec<_>, _>>();
            match command {
                Ok(command) => {
                    let record_path = dir.join(MEASUREMENTS);
                    if treatment {
                        let max_wait_ms = env::var("REEF_AGENT_MAX_WAIT_MS")
                            .ok()
                            .and_then(|value| value.parse::<u64>().ok())
                            .filter(|value| (1..=crate::agents::MAX_WAIT_MS).contains(value))
                            .ok_or_else(|| io::Error::other("invalid agent wait limit"));
                        max_wait_ms.and_then(|wait| {
                            let state_dir = env::var_os("REEF_AGENT_STATE_DIR").map(PathBuf::from);
                            schedule::schedule(schedule::RunOptions {
                                command: &command,
                                category,
                                identity: &id,
                                record: RecordMode::Control(&record_path),
                                cpu: 1,
                                memory_mib: 1024,
                                state_dir: state_dir.as_deref(),
                                max_wait: Some(std::time::Duration::from_millis(wait)),
                                limits: &contain::Limits::default(),
                            })
                        })
                    } else {
                        run::run(&command, category, &id, RecordMode::Control(&record_path))
                    }
                }
                Err(_) => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "control command arguments must be UTF-8",
                )),
            }
        }
        None => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "control command executable is unavailable",
        )),
    };
    if result.is_ok() {
        "completed"
    } else {
        "unavailable"
    }
    .clone_into(&mut event.kind);
    event.at_unix_ms = now_ms();
    event.assignment = None;
    append(&dir, &event).map_err(|_| io::Error::other(TELEMETRY_WARNING))?;
    result.map_err(|error| {
        if treatment {
            io::Error::other(format!("canary treatment unavailable: {error}"))
        } else {
            io::Error::other("control measurement unavailable; see private telemetry warning")
        }
    })
}

fn read_telemetry(dir: &Path) -> io::Result<(Vec<Event>, BTreeMap<String, run::ControlRecord>)> {
    private_dir(dir, false)?;
    let file = private_file(&dir.join(EVENTS), false)?;
    FileExt::lock_shared(&file)?;
    let events = BufReader::new(&file)
        .lines()
        .map(|line| serde_json::from_str::<Event>(&line?).map_err(io::Error::other))
        .collect::<io::Result<Vec<_>>>()?;
    let measurements = private_file(&dir.join(MEASUREMENTS), false)?;
    FileExt::lock_shared(&measurements)?;
    let mut records = BTreeMap::new();
    for line in BufReader::new(&measurements).lines() {
        let record = run::control_record(&line?)?;
        if records.insert(record.id.clone(), record).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "control measurement identity is invalid",
            ));
        }
    }
    FileExt::unlock(&measurements)?;
    FileExt::unlock(&file)?;
    Ok((events, records))
}

struct Attempt {
    at_unix_ms: u128,
    agent: String,
    category: String,
    arm: String,
    completed: bool,
    unavailable: bool,
}

pub fn summary(dir: Option<&Path>, from_ms: u128, to_ms: u128) -> Summary {
    let Ok(path) = dir.map_or_else(default_dir, |path| Ok(path.to_path_buf())) else {
        return Summary::unavailable();
    };
    let Ok((events, measurements)) = read_telemetry(&path) else {
        return Summary::unavailable();
    };
    let mut attempts: BTreeMap<String, Attempt> = BTreeMap::new();
    for event in events {
        if event.attempt_id.len() != 32
            || !event
                .attempt_id
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || !matches!(event.agent.as_str(), "codex" | "claude" | "opencode")
            || !matches!(event.category.as_str(), "build" | "lint" | "test")
        {
            return Summary::unavailable();
        }
        match event.kind.as_str() {
            "assigned"
                if matches!(
                    event.assignment.as_deref(),
                    Some("control" | "treatment" | "paused")
                ) =>
            {
                if attempts
                    .insert(
                        event.attempt_id,
                        Attempt {
                            at_unix_ms: event.at_unix_ms,
                            agent: event.agent,
                            category: event.category,
                            arm: event.assignment.unwrap_or_default(),
                            completed: false,
                            unavailable: false,
                        },
                    )
                    .is_some()
                {
                    return Summary::unavailable();
                }
            }
            "completed" | "unavailable" if event.assignment.is_none() => {
                let Some(attempt) = attempts.get_mut(&event.attempt_id) else {
                    return Summary::unavailable();
                };
                if attempt.completed
                    || attempt.unavailable
                    || event.agent != attempt.agent
                    || event.category != attempt.category
                {
                    return Summary::unavailable();
                }
                attempt.at_unix_ms = attempt.at_unix_ms.min(event.at_unix_ms);
                attempt.completed = event.kind == "completed";
                attempt.unavailable = event.kind == "unavailable";
            }
            _ => return Summary::unavailable(),
        }
    }
    if !measurements.iter().all(|(id, measurement)| {
        attempts.get(id).is_some_and(|attempt| {
            measurement.agent == attempt.agent && measurement.category == attempt.category
        })
    }) {
        return Summary::unavailable();
    }
    summarize_attempts(&attempts, &measurements, from_ms, to_ms)
}

fn summarize_attempts(
    attempts: &BTreeMap<String, Attempt>,
    measurements: &BTreeMap<String, run::ControlRecord>,
    from_ms: u128,
    to_ms: u128,
) -> Summary {
    let selected: Vec<_> = attempts
        .values()
        .filter(|attempt| attempt.at_unix_ms >= from_ms && attempt.at_unix_ms < to_ms)
        .collect();
    let mut completed = 0;
    let mut unavailable = 0;
    let mut pending = 0;
    let mut gap = false;
    let mut groups: BTreeMap<(String, String, String), Group> = BTreeMap::new();
    for (id, attempt) in attempts
        .iter()
        .filter(|(_, attempt)| attempt.at_unix_ms >= from_ms && attempt.at_unix_ms < to_ms)
    {
        let group = groups
            .entry((
                attempt.agent.clone(),
                attempt.category.clone(),
                attempt.arm.clone(),
            ))
            .or_insert_with(|| Group {
                agent: attempt.agent.clone(),
                category: attempt.category.clone(),
                arm: attempt.arm.clone(),
                ..Group::default()
            });
        group.assigned += 1;
        if attempt.completed && measurements.contains_key(id) {
            completed += 1;
            let measurement = &measurements[id];
            group.measured += 1;
            match measurement.status.as_str() {
                "success" => group.success += 1,
                "failed" => group.failed += 1,
                "cancelled" => group.cancelled += 1,
                _ => return Summary::unavailable(),
            }
            group.measured_wall_ms = group.measured_wall_ms.saturating_add(measurement.wall_ms);
            group.measured_cpu_ms = group.measured_cpu_ms.saturating_add(measurement.cpu_ms);
            group.peak_memory_bytes = group.peak_memory_bytes.max(measurement.peak_memory_bytes);
        } else if attempt.completed || attempt.unavailable {
            unavailable += 1;
            group.unavailable += 1;
            gap |= attempt.completed;
        } else {
            pending += 1;
            group.pending += 1;
        }
    }
    Summary {
        available: true,
        attempted: Some(selected.len()),
        completed: Some(completed),
        pending: Some(pending),
        unavailable: Some(unavailable),
        warning: gap.then_some("control measurements are missing for recorded attempts"),
        groups: groups.into_values().collect(),
    }
}

pub fn measurements_path(dir: Option<&Path>) -> io::Result<PathBuf> {
    let dir = dir.map_or_else(default_dir, |path| Ok(path.to_path_buf()))?;
    private_dir(&dir, false)?;
    let path = dir.join(MEASUREMENTS);
    private_file(&path, false)?;
    Ok(path)
}
