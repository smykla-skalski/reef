use super::{Boundary, Limits};
use std::fs::{self, OpenOptions};
use std::io;
use std::io::Write;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_UNIT: AtomicU64 = AtomicU64::new(0);

pub struct Containment {
    limits: Limits,
    unit: String,
    cgroup: Option<PathBuf>,
    boundaries: Vec<Boundary>,
    registry_entry: PathBuf,
    snapshot_path: PathBuf,
}

impl Containment {
    pub fn prepare(limits: &Limits) -> io::Result<Option<Self>> {
        if !limits.requested() {
            return Ok(None);
        }
        if !PathBuf::from("/sys/fs/cgroup/cgroup.controllers").is_file() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "cgroup v2 is unavailable",
            ));
        }
        let output = Command::new("systemctl")
            .args(["--user", "show", "--property=Version", "--value"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("systemd user manager unavailable: {error}"),
                )
            })?;
        if !output.success() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "systemd user manager unavailable; cannot enforce requested cgroup limits",
            ));
        }
        let _ = limits.properties()?;
        probe_controllers(limits)?;
        let registry = registry_dir()?;
        reap_stale(&registry)?;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let unit = format!(
            "reef-{}-{timestamp:x}-{}.scope",
            std::process::id(),
            NEXT_UNIT.fetch_add(1, Ordering::Relaxed)
        );
        let registry_entry = registry.join(&unit);
        let snapshot_path = registry.join(format!("{unit}.result"));
        let mut entry = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&registry_entry)?;
        let start_time = process_start_time(std::process::id())?;
        let boot_id = boot_id()?;
        writeln!(entry, "{} {start_time} {boot_id}", std::process::id())?;
        entry.sync_all()?;
        let mut boundaries = Vec::new();
        if let Some(limit) = limits.cpu_percent {
            boundaries.push(Boundary {
                resource: "cpu".into(),
                limit: u64::from(limit),
                observed_events: None,
            });
        }
        if let Some(limit) = limits.memory_cap_mib {
            boundaries.push(Boundary {
                resource: "memory".into(),
                limit: limit * 1_048_576,
                observed_events: None,
            });
        }
        if let Some(limit) = limits.tasks {
            boundaries.push(Boundary {
                resource: "tasks".into(),
                limit: u64::from(limit),
                observed_events: None,
            });
        }
        if let Some(limit) = &limits.io_read {
            boundaries.push(Boundary {
                resource: "io_read".into(),
                limit: limit.bytes_per_second,
                observed_events: None,
            });
        }
        if let Some(limit) = &limits.io_write {
            boundaries.push(Boundary {
                resource: "io_write".into(),
                limit: limit.bytes_per_second,
                observed_events: None,
            });
        }
        Ok(Some(Self {
            limits: limits.clone(),
            unit,
            cgroup: None,
            boundaries,
            registry_entry,
            snapshot_path,
        }))
    }

    pub fn command(&self, command: &[String]) -> io::Result<Command> {
        scope_command(&self.limits, &self.unit, &self.snapshot_path, command)
    }

    pub fn sample(&mut self) {
        if let Ok(bytes) = fs::read(&self.snapshot_path)
            && let Ok(snapshot) = serde_json::from_slice::<Vec<Boundary>>(&bytes)
        {
            for observed in snapshot {
                if let Some(previous) = self
                    .boundaries
                    .iter_mut()
                    .find(|item| item.resource == observed.resource)
                {
                    previous.observed_events = observed.observed_events;
                }
            }
            return;
        }
        if self.cgroup.is_none() {
            self.cgroup = self.lookup_cgroup();
        }
        let Some(path) = &self.cgroup else {
            return;
        };
        let path = path.clone();
        if let Some(limit) = self.limits.cpu_percent {
            self.capture(&path, "cpu.stat", "nr_throttled", "cpu", u64::from(limit));
        }
        if let Some(limit) = self.limits.memory_cap_mib {
            self.capture(&path, "memory.events", "max", "memory", limit * 1_048_576);
        }
        if let Some(limit) = self.limits.tasks {
            self.capture(&path, "pids.events", "max", "tasks", u64::from(limit));
        }
    }

    fn capture(&mut self, path: &Path, file: &str, key: &str, resource: &'static str, limit: u64) {
        let Ok(contents) = fs::read_to_string(path.join(file)) else {
            return;
        };
        let Some(value) = counter(&contents, key) else {
            return;
        };
        if let Some(previous) = self
            .boundaries
            .iter_mut()
            .find(|item| item.resource == resource)
        {
            previous.observed_events =
                Some(previous.observed_events.unwrap_or_default().max(value));
        } else {
            self.boundaries.push(Boundary {
                resource: resource.to_owned(),
                limit,
                observed_events: Some(value),
            });
        }
    }

    fn lookup_cgroup(&self) -> Option<PathBuf> {
        let output = Command::new("systemctl")
            .args([
                "--user",
                "show",
                &self.unit,
                "--property=ControlGroup",
                "--value",
            ])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let group = String::from_utf8(output.stdout).ok()?;
        let relative = group.trim().strip_prefix('/')?;
        if relative.is_empty()
            || PathBuf::from(relative)
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return None;
        }
        Some(PathBuf::from("/sys/fs/cgroup").join(relative))
    }

    pub fn stop(&self) -> io::Result<()> {
        stop_unit(&self.unit)?;
        Ok(())
    }

    pub fn finish(&self) -> io::Result<()> {
        self.stop()?;
        let _ = fs::remove_file(&self.snapshot_path);
        match fs::remove_file(&self.registry_entry) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub fn boundaries(&self) -> Vec<Boundary> {
        self.boundaries.clone()
    }
}

fn scope_command(
    limits: &Limits,
    unit: &str,
    snapshot_path: &Path,
    command: &[String],
) -> io::Result<Command> {
    let mut process = Command::new("systemd-run");
    process.args([
        "--user",
        "--scope",
        "--collect",
        "--quiet",
        "--expand-environment=no",
        "--description=Reef workload",
        "--unit",
        unit,
    ]);
    for property in limits.properties()? {
        process.arg("--property").arg(property);
    }
    let executable = std::env::current_exe()?;
    let expected_json = serde_json::to_string(limits)?;
    process
        .arg("--")
        .arg(executable)
        .arg("contain-exec")
        .arg("--snapshot")
        .arg(snapshot_path)
        .arg("--expected-json")
        .arg(expected_json)
        .arg("--")
        .args(command);
    Ok(process)
}

fn probe_controllers(limits: &Limits) -> io::Result<()> {
    let output = Command::new("systemctl")
        .args(["--user", "show", "--property=ControlGroup", "--value"])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cannot inspect systemd user manager cgroup controllers",
        ));
    }
    let group = String::from_utf8(output.stdout).map_err(io::Error::other)?;
    let relative = group
        .trim()
        .strip_prefix('/')
        .filter(|path| !path.is_empty())
        .ok_or_else(|| io::Error::other("invalid systemd user manager cgroup"))?;
    if Path::new(relative)
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(io::Error::other("invalid systemd user manager cgroup"));
    }
    let controllers = fs::read_to_string(
        Path::new("/sys/fs/cgroup")
            .join(relative)
            .join("cgroup.controllers"),
    )?;
    if let Some(controller) = missing_controller(limits, &controllers) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "{controller} controller is not delegated to the systemd user manager; command was not started"
            ),
        ));
    }
    Ok(())
}

fn missing_controller(limits: &Limits, available: &str) -> Option<&'static str> {
    [
        (limits.cpu_percent.is_some(), "cpu"),
        (limits.memory_cap_mib.is_some(), "memory"),
        (limits.tasks.is_some(), "pids"),
        (limits.io_read.is_some() || limits.io_write.is_some(), "io"),
    ]
    .into_iter()
    .find_map(|(requested, controller)| {
        (requested && !available.split_whitespace().any(|item| item == controller))
            .then_some(controller)
    })
}

impl Drop for Containment {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn registry_dir() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "HOME is not set for Reef scope registry",
        )
    })?;
    let path = PathBuf::from(home).join(".local/state/reef/contain");
    if !path.exists() {
        fs::create_dir_all(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    }
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.mode() & 0o077 != 0
        || metadata.uid() != nix::unistd::Uid::current().as_raw()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Reef scope registry must be private and owned by the current user",
        ));
    }
    Ok(path)
}

fn reap_stale(registry: &Path) -> io::Result<()> {
    for entry in fs::read_dir(registry)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("reef-") || !name.ends_with(".scope") {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if !metadata.file_type().is_file()
            || metadata.mode() & 0o077 != 0
            || metadata.uid() != nix::unistd::Uid::current().as_raw()
        {
            continue;
        }
        let contents = fs::read_to_string(entry.path())?;
        let mut fields = contents.split_whitespace();
        let Some(pid) = fields.next().and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        let Some(start_time) = fields.next().and_then(|value| value.parse::<u64>().ok()) else {
            continue;
        };
        let Some(entry_boot_id) = fields.next() else {
            continue;
        };
        if entry_boot_id == boot_id()?.as_str() && process_start_time(pid).ok() == Some(start_time)
        {
            continue;
        }
        stop_unit(&name)?;
        fs::remove_file(entry.path())?;
        let _ = fs::remove_file(registry.join(format!("{name}.result")));
    }
    Ok(())
}

fn boot_id() -> io::Result<String> {
    Ok(fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .to_owned())
}

fn process_start_time(pid: u32) -> io::Result<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let (_, fields) = stat
        .rsplit_once(')')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid /proc stat"))?;
    fields
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing process start time"))?
        .parse()
        .map_err(io::Error::other)
}

fn stop_unit(unit: &str) -> io::Result<()> {
    let output = Command::new("systemctl")
        .args(["--user", "stop", unit])
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        let state = Command::new("systemctl")
            .args(["--user", "show", unit, "--property=LoadState", "--value"])
            .output()?;
        if scope_absent(&state) {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "cannot stop Reef scope {unit}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }
}

fn scope_absent(state: &std::process::Output) -> bool {
    state.status.success() && state.stdout == b"not-found\n"
}

fn counter(contents: &str, key: &str) -> Option<u64> {
    contents.lines().find_map(|line| {
        let (name, value) = line.split_once(' ')?;
        (name == key).then(|| value.trim().parse().ok()).flatten()
    })
}

pub fn exec_payload(command: &[String], snapshot: &Path, expected_json: &str) -> io::Result<u8> {
    let limits: Limits = serde_json::from_str(expected_json)?;
    verify_applied_limits(&limits)?;
    let status = Command::new(&command[0])
        .args(&command[1..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();
    let boundaries = snapshot_boundaries();
    if let Ok(mut file) = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(snapshot)
    {
        let saved = serde_json::to_writer(&mut file, &boundaries)
            .map_err(io::Error::other)
            .and_then(|()| file.sync_all());
        if let Err(error) = saved {
            eprintln!("reef: cannot save cgroup counters: {error}");
        }
    }
    let status = match status {
        Ok(status) => status,
        Err(error) => {
            eprintln!("reef: contained command could not start: {error}");
            return Ok(if error.kind() == io::ErrorKind::NotFound {
                127
            } else {
                126
            });
        }
    };
    Ok(status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or_else(|| {
            128_u8.saturating_add(u8::try_from(status.signal().unwrap_or(1)).unwrap_or(1))
        }))
}

fn verify_applied_limits(limits: &Limits) -> io::Result<()> {
    let group = own_cgroup().ok_or_else(|| {
        io::Error::new(io::ErrorKind::Unsupported, "cannot locate contained cgroup")
    })?;
    if let Some(requested) = limits.cpu_percent {
        let value = fs::read_to_string(group.join("cpu.max"))?;
        let mut parts = value.split_whitespace();
        let quota = parts.next().and_then(|part| part.parse::<u128>().ok());
        let period = parts.next().and_then(|part| part.parse::<u128>().ok());
        if !matches!((quota, period), (Some(quota), Some(period)) if period > 0 && quota * 100 <= u128::from(requested) * period + period)
        {
            return Err(limit_not_applied("CPU"));
        }
    }
    if let Some(requested) = limits.memory_cap_mib {
        verify_number(&group.join("memory.max"), requested * 1_048_576, "memory")?;
    }
    if let Some(requested) = limits.tasks {
        verify_number(
            &group.join("pids.max"),
            u64::from(requested) + 1,
            "task count",
        )?;
    }
    if let Some(limit) = &limits.io_read {
        verify_io(&group, limit, "rbps", "I/O read")?;
    }
    if let Some(limit) = &limits.io_write {
        verify_io(&group, limit, "wbps", "I/O write")?;
    }
    Ok(())
}

fn verify_number(path: &Path, requested: u64, resource: &str) -> io::Result<()> {
    let actual = fs::read_to_string(path)?;
    if actual
        .trim()
        .parse::<u64>()
        .is_ok_and(|value| value <= requested)
    {
        Ok(())
    } else {
        Err(limit_not_applied(resource))
    }
}

fn verify_io(group: &Path, limit: &super::IoLimit, field: &str, resource: &str) -> io::Result<()> {
    let device_key = io_device_key(Path::new(&limit.path))?;
    let unit = group
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| name.starts_with("reef-") && name.strip_suffix(".scope").is_some())
        .ok_or_else(|| limit_not_applied(resource))?;
    let property = if field == "rbps" {
        "IOReadBandwidthMax"
    } else {
        "IOWriteBandwidthMax"
    };
    let output = Command::new("systemctl")
        .args([
            "--user",
            "show",
            unit,
            &format!("--property={property}"),
            "--value",
        ])
        .output()?;
    let expected = format!("{} {}\n", limit.path, limit.bytes_per_second);
    if !output.status.success() || output.stdout != expected.as_bytes() {
        return Err(limit_not_applied(resource));
    }
    let contents = fs::read_to_string(group.join("io.max"))?;
    let applied = contents.lines().any(|line| {
        let mut fields = line.split_whitespace();
        fields.next() == Some(device_key.as_str())
            && fields.any(|value| {
                value
                    .strip_prefix(&format!("{field}="))
                    .and_then(|value| value.parse::<u64>().ok())
                    .is_some_and(|value| value <= limit.bytes_per_second)
            })
    });
    if applied {
        Ok(())
    } else {
        Err(limit_not_applied(resource))
    }
}

fn io_device_key(path: &Path) -> io::Result<String> {
    let metadata = fs::metadata(path)?;
    let device = if metadata.file_type().is_block_device() {
        metadata.rdev()
    } else {
        let output = Command::new("findmnt")
            .args(["--noheadings", "--output", "SOURCE", "--target"])
            .arg(path)
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other("cannot resolve I/O backing device"));
        }
        let source = String::from_utf8(output.stdout).map_err(io::Error::other)?;
        let block = source_block_device(source.trim())
            .ok_or_else(|| io::Error::other("I/O path is not backed by a simple block device"))?;
        let metadata = fs::metadata(block)?;
        if !metadata.file_type().is_block_device() {
            return Err(io::Error::other("I/O path is not backed by a block device"));
        }
        metadata.rdev()
    };
    let major = ((device >> 8) & 0xfff) | ((device >> 32) & 0xffff_f000);
    let minor = (device & 0xff) | ((device >> 12) & 0xffff_ff00);
    let sysfs = fs::canonicalize(format!("/sys/dev/block/{major}:{minor}"))?;
    whole_disk_key(&sysfs)
}

fn source_block_device(source: &str) -> Option<&str> {
    let block = source.split_once('[').map_or(source, |(block, _)| block);
    block.starts_with("/dev/").then_some(block)
}

fn whole_disk_key(sysfs: &Path) -> io::Result<String> {
    let disk = if sysfs.join("partition").is_file() {
        sysfs
            .parent()
            .ok_or_else(|| io::Error::other("invalid partition device"))?
    } else {
        sysfs
    };
    let key = fs::read_to_string(disk.join("dev"))?;
    let key = key.trim();
    let (major, minor) = key
        .split_once(':')
        .ok_or_else(|| io::Error::other("invalid block device number"))?;
    let major = major
        .parse::<u32>()
        .map_err(|_| io::Error::other("invalid block device number"))?;
    let minor = minor
        .parse::<u32>()
        .map_err(|_| io::Error::other("invalid block device number"))?;
    Ok(format!("{major}:{minor}"))
}

fn limit_not_applied(resource: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("{resource} cgroup limit was not applied; command was not started"),
    )
}

fn snapshot_boundaries() -> Vec<Boundary> {
    let Some(group) = own_cgroup() else {
        return Vec::new();
    };
    let mut boundaries = Vec::new();
    for (file, key, resource) in [
        ("cpu.stat", "nr_throttled", "cpu"),
        ("memory.events", "max", "memory"),
        ("pids.events", "max", "tasks"),
    ] {
        let observed_events = fs::read_to_string(group.join(file))
            .ok()
            .and_then(|content| counter(&content, key));
        boundaries.push(Boundary {
            resource: resource.to_owned(),
            limit: 0,
            observed_events,
        });
    }
    boundaries
}

fn own_cgroup() -> Option<PathBuf> {
    let Ok(cgroup) = fs::read_to_string("/proc/self/cgroup") else {
        return None;
    };
    let path = cgroup.lines().find_map(|line| line.strip_prefix("0::/"))?;
    let path = PathBuf::from(path);
    if path
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return None;
    }
    Some(PathBuf::from("/sys/fs/cgroup").join(path))
}

#[cfg(test)]
mod tests {
    use super::{
        counter, missing_controller, scope_absent, scope_command, source_block_device,
        verify_number, whole_disk_key,
    };
    use crate::contain::Limits;
    use std::fs;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{ExitStatus, Output};

    #[test]
    fn reads_cgroup_counter_by_name() {
        assert_eq!(
            counter("usage_usec 99\nnr_throttled 4\n", "nr_throttled"),
            Some(4)
        );
        assert_eq!(counter("max broken\n", "max"), None);
    }

    #[test]
    fn rejects_unapplied_numeric_limit() {
        let path = std::env::temp_dir().join(format!("reef-limit-check-{}", std::process::id()));
        fs::write(&path, "max\n").unwrap();
        assert!(verify_number(&path, 100, "memory").is_err());
        fs::write(&path, "101\n").unwrap();
        assert!(verify_number(&path, 100, "memory").is_err());
        fs::write(&path, "99\n").unwrap();
        assert!(verify_number(&path, 100, "memory").is_ok());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn reserves_one_task_for_containment_helper() {
        let limits = Limits {
            tasks: Some(1),
            ..Limits::default()
        };
        assert!(limits.properties().unwrap().contains(&"TasksMax=2".into()));
    }

    #[test]
    fn passes_dollars_verbatim_with_expansion_disabled() {
        let command = ["sh".to_owned(), "-c".to_owned(), "echo $HOME $$".to_owned()];
        let scope = scope_command(
            &Limits::default(),
            "reef-test.scope",
            std::path::Path::new("/tmp/reef-test-snapshot"),
            &command,
        )
        .unwrap();
        let args: Vec<_> = scope.get_args().collect();
        assert!(args.contains(&std::ffi::OsStr::new("--expand-environment=no")));
        assert_eq!(args.last(), Some(&std::ffi::OsStr::new("echo $HOME $$")));
    }

    #[test]
    fn names_missing_delegated_controller() {
        let limits = Limits {
            io_write: Some(super::super::IoLimit {
                path: "/tmp".into(),
                bytes_per_second: 1024,
            }),
            ..Limits::default()
        };
        assert_eq!(missing_controller(&limits, "cpu memory pids"), Some("io"));
        assert_eq!(missing_controller(&limits, "cpu io memory pids"), None);
    }

    #[test]
    fn resolves_subvolume_source_to_parent_disk() {
        assert_eq!(
            source_block_device("/dev/vdb1[/scon/containers/test/rootfs]"),
            Some("/dev/vdb1")
        );
        assert_eq!(source_block_device("tmpfs"), None);
        let root = std::env::temp_dir().join(format!(
            "reef-io-device-{}-{}",
            std::process::id(),
            super::NEXT_UNIT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let partition = root.join("vdb/vdb1");
        fs::create_dir_all(&partition).unwrap();
        fs::write(root.join("vdb/dev"), "254:16\n").unwrap();
        fs::write(partition.join("partition"), "1\n").unwrap();
        assert_eq!(whole_disk_key(&partition).unwrap(), "254:16");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn activating_scope_is_not_treated_as_absent() {
        let mut state = Output {
            status: ExitStatus::from_raw(0),
            stdout: b"activating\n".to_vec(),
            stderr: Vec::new(),
        };
        assert!(!scope_absent(&state));
        state.stdout = b"not-found\n".to_vec();
        assert!(scope_absent(&state));
        state.status = ExitStatus::from_raw(256);
        assert!(!scope_absent(&state));
    }
}
