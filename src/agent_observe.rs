use crate::observe;
use clap::ValueEnum;
use processkit::process_info;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use sysinfo::{Pid, Process, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Codex,
    Claude,
    Opencode,
}

impl AgentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Opencode => "opencode",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Registration {
    pub kind: AgentKind,
    pub pid: u32,
    pub start_time: u64,
}

fn matches_name(name: &str, kind: AgentKind) -> bool {
    let name = name.to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    name == kind.as_str()
}

fn matches_kind(process: &Process, kind: AgentKind) -> bool {
    if matches_name(&process.name().to_string_lossy(), kind) {
        return true;
    }
    process.cmd().iter().take(2).any(|arg| {
        Path::new(arg)
            .file_name()
            .is_some_and(|name| matches_name(&name.to_string_lossy(), kind))
    })
}

fn ancestor(system: &System, kind: AgentKind) -> Option<Pid> {
    let mut pid = Pid::from_u32(std::process::id());
    for _ in 0..32 {
        let process = system.process(pid)?;
        if matches_kind(process, kind) {
            return Some(pid);
        }
        pid = process.parent()?;
    }
    None
}

pub fn mark(kind: AgentKind, pid: Option<u32>, dir: Option<&Path>) -> io::Result<()> {
    if pid.is_some_and(|pid| pid <= 1) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "agent PID must be greater than one",
        ));
    }
    let dir = observe::state_dir(dir)?;
    observe::private_dir(&dir)?;
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .without_tasks(),
    );
    let pid = pid
        .map(Pid::from_u32)
        .or_else(|| ancestor(&system, kind))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "agent process could not be identified",
            )
        })?;
    if system.process(pid).is_none() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "agent process has exited",
        ));
    }
    let identity = process_info(pid.as_u32())
        .map_err(io::Error::other)?
        .and_then(|info| info.start_time())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "agent identity unavailable"))?;
    let registration = Registration {
        kind,
        pid: pid.as_u32(),
        start_time: identity,
    };
    let path = dir.join(format!(
        "agent-{}-{}-{}.json",
        kind.as_str(),
        registration.pid,
        registration.start_time
    ));
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let temporary = dir.join(format!("agent-{}-{nonce}.tmp", std::process::id()));
    let mut file = observe::private_file(&temporary, true, false)?;
    serde_json::to_writer(&mut file, &registration)?;
    file.write_all(b"\n")?;
    file.sync_data()?;
    fs::rename(temporary, path)
}

fn owned_name(name: &str) -> Option<(AgentKind, u32, u64)> {
    let name = name.strip_prefix("agent-")?.strip_suffix(".json")?;
    let mut fields = name.split('-');
    let kind = match fields.next()? {
        "codex" => AgentKind::Codex,
        "claude" => AgentKind::Claude,
        "opencode" => AgentKind::Opencode,
        _ => return None,
    };
    let pid = fields.next()?.parse().ok()?;
    let start_time = fields.next()?.parse().ok()?;
    fields.next().is_none().then_some((kind, pid, start_time))
}

pub fn registrations(dir: &Path, system: &System) -> io::Result<HashMap<u32, Registration>> {
    let mut registrations = HashMap::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some((kind, pid, start_time)) = name.to_str().and_then(owned_name) else {
            continue;
        };
        if !entry.file_type()?.is_file() {
            continue;
        }
        let Ok(file) = observe::private_file(&entry.path(), false, false) else {
            continue;
        };
        let Ok(registration): Result<Registration, _> = serde_json::from_reader(file) else {
            continue;
        };
        if registration.kind != kind
            || registration.pid != pid
            || registration.start_time != start_time
        {
            continue;
        }
        if system.process(Pid::from_u32(registration.pid)).is_none() {
            fs::remove_file(entry.path())?;
            continue;
        }
        match process_info(registration.pid) {
            Ok(Some(info)) => {
                if let Some(identity) = info.start_time() {
                    if identity == registration.start_time {
                        registrations.insert(registration.pid, registration);
                    } else {
                        fs::remove_file(entry.path())?;
                    }
                }
            }
            Ok(None) => fs::remove_file(entry.path())?,
            Err(_) => {}
        }
    }
    Ok(registrations)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_supported_agent_executables_without_matching_other_tools() {
        assert!(matches_name("codex", AgentKind::Codex));
        assert!(matches_name("Claude.exe", AgentKind::Claude));
        assert!(matches_name("opencode.exe", AgentKind::Opencode));
        assert!(!matches_name("codex-code-mode-host", AgentKind::Codex));
        assert!(!matches_name("node", AgentKind::Opencode));
    }

    #[test]
    fn only_exact_registration_names_are_owned() {
        assert_eq!(
            owned_name("agent-codex-42-100.json"),
            Some((AgentKind::Codex, 42, 100))
        );
        assert_eq!(owned_name("agent-codex-42-100.backup.json"), None);
        assert_eq!(owned_name("agent-other-42-100.json"), None);
        assert_eq!(owned_name("agent-claude-42-100.json.tmp"), None);
    }

    #[test]
    fn explicit_process_registration_is_private_and_idempotent() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("reef-agent-observe-{}-{nonce}", std::process::id()));
        mark(AgentKind::Codex, Some(std::process::id()), Some(&dir)).unwrap();
        mark(AgentKind::Codex, Some(std::process::id()), Some(&dir)).unwrap();
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing(),
        );
        let registrations = registrations(&dir, &system).unwrap();
        assert_eq!(registrations.len(), 1);
        assert_eq!(registrations[&std::process::id()].kind, AgentKind::Codex);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_process_identity_is_not_attributed() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "reef-agent-identity-{}-{nonce}",
            std::process::id()
        ));
        mark(AgentKind::Codex, Some(std::process::id()), Some(&dir)).unwrap();
        let path = fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
        let mut registration: Registration =
            serde_json::from_reader(fs::File::open(&path).unwrap()).unwrap();
        fs::remove_file(path).unwrap();
        registration.start_time += 1;
        let stale = dir.join(format!(
            "agent-codex-{}-{}.json",
            registration.pid, registration.start_time
        ));
        serde_json::to_writer(fs::File::create(&stale).unwrap(), &registration).unwrap();
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing(),
        );
        assert!(registrations(&dir, &system).unwrap().is_empty());
        assert!(!stale.exists());
        fs::remove_dir_all(dir).unwrap();
    }
}
