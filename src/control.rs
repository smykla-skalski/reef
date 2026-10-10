use crate::run::{self, RecordMode};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
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
    if dir.join(EVENTS).exists() != dir.join(MEASUREMENTS).exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "control telemetry is incomplete",
        ));
    }
    for name in [EVENTS, MEASUREMENTS] {
        private_file(&dir.join(name), true)?.sync_all()?;
    }
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

pub fn run(original: &Path, args: &[OsString], category: &str) -> io::Result<u8> {
    let dir =
        env::var_os("REEF_AGENT_CONTROL_DIR").ok_or_else(|| io::Error::other(TELEMETRY_WARNING))?;
    let dir = PathBuf::from(dir);
    private_dir(&dir, false).map_err(|_| io::Error::other(TELEMETRY_WARNING))?;
    private_file(&dir.join(MEASUREMENTS), false)
        .map_err(|_| io::Error::other(TELEMETRY_WARNING))?;
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
    append(&dir, &event).map_err(|_| io::Error::other(TELEMETRY_WARNING))?;
    let command = std::iter::once(original.as_os_str().to_os_string())
        .chain(args.iter().cloned())
        .map(OsString::into_string)
        .collect::<Result<Vec<_>, _>>();
    let result = match command {
        Ok(command) => run::run(
            &command,
            category,
            &id,
            RecordMode::Control(&dir.join(MEASUREMENTS)),
        ),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "control command arguments must be UTF-8",
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
    result.map_err(|_| {
        io::Error::other("control measurement unavailable; see private telemetry warning")
    })
}

fn read_telemetry(dir: &Path) -> io::Result<(Vec<Event>, BTreeSet<String>)> {
    private_dir(dir, false)?;
    let file = private_file(&dir.join(EVENTS), false)?;
    FileExt::lock_shared(&file)?;
    let events = BufReader::new(&file)
        .lines()
        .map(|line| serde_json::from_str::<Event>(&line?).map_err(io::Error::other))
        .collect::<io::Result<Vec<_>>>()?;
    let measurements = private_file(&dir.join(MEASUREMENTS), false)?;
    FileExt::lock_shared(&measurements)?;
    let mut ids = BTreeSet::new();
    for line in BufReader::new(&measurements).lines() {
        let record: serde_json::Value = serde_json::from_str(&line?)?;
        let id = record
            .get("identity")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "control measurement has no identity",
                )
            })?;
        if id.len() != 32
            || !id.bytes().all(|byte| byte.is_ascii_hexdigit())
            || !ids.insert(id.to_owned())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "control measurement identity is invalid",
            ));
        }
    }
    FileExt::unlock(&measurements)?;
    FileExt::unlock(&file)?;
    Ok((events, ids))
}

pub fn summary(dir: Option<&Path>, from_ms: u128, to_ms: u128) -> Summary {
    let Ok(path) = dir.map_or_else(default_dir, |path| Ok(path.to_path_buf())) else {
        return Summary::unavailable();
    };
    let Ok((events, measurements)) = read_telemetry(&path) else {
        return Summary::unavailable();
    };
    let mut attempts: BTreeMap<String, (u128, bool, bool)> = BTreeMap::new();
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
            "assigned" if event.assignment.as_deref() == Some("control") => {
                if attempts
                    .insert(event.attempt_id, (event.at_unix_ms, false, false))
                    .is_some()
                {
                    return Summary::unavailable();
                }
            }
            "completed" | "unavailable" if event.assignment.is_none() => {
                let Some(attempt) = attempts.get_mut(&event.attempt_id) else {
                    return Summary::unavailable();
                };
                if attempt.1 || attempt.2 || event.at_unix_ms < attempt.0 {
                    return Summary::unavailable();
                }
                attempt.1 = event.kind == "completed";
                attempt.2 = event.kind == "unavailable";
            }
            _ => return Summary::unavailable(),
        }
    }
    let selected: Vec<_> = attempts
        .values()
        .filter(|(at, _, _)| *at >= from_ms && *at < to_ms)
        .collect();
    let mut completed = 0;
    let mut unavailable = 0;
    let mut pending = 0;
    let mut gap = false;
    for (id, (_, done, failed)) in attempts
        .iter()
        .filter(|(_, (at, _, _))| *at >= from_ms && *at < to_ms)
    {
        if *done && measurements.contains(id) {
            completed += 1;
        } else if *done || *failed {
            unavailable += 1;
            gap |= *done;
        } else {
            pending += 1;
        }
    }
    Summary {
        available: true,
        attempted: Some(selected.len()),
        completed: Some(completed),
        pending: Some(pending),
        unavailable: Some(unavailable),
        warning: gap.then_some("control measurements are missing for recorded attempts"),
    }
}

pub fn measurements_path(dir: Option<&Path>) -> io::Result<PathBuf> {
    let dir = dir.map_or_else(default_dir, |path| Ok(path.to_path_buf()))?;
    private_dir(&dir, false)?;
    let path = dir.join(MEASUREMENTS);
    private_file(&path, false)?;
    Ok(path)
}
