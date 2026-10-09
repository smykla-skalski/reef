use crate::ObserveOptions;
use crate::agent_observe;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use sysinfo::{Disks, ProcessRefreshKind, ProcessesToUpdate, System};

mod platform;

const SEGMENT_BYTES: u64 = 1_048_576;

#[derive(Serialize)]
struct Observation {
    at_unix_ms: u64,
    working_ms: u64,
    cpu_percent: Option<f32>,
    memory_used_bytes: Option<u64>,
    memory_total_bytes: Option<u64>,
    swap_used_bytes: Option<u64>,
    swap_total_bytes: Option<u64>,
    root_disk_available_bytes: Option<u64>,
    root_disk_total_bytes: Option<u64>,
    processes: Vec<ProcessObservation>,
    agents: Vec<AgentObservation>,
}

#[derive(Serialize)]
struct ProcessObservation {
    pid: u32,
    parent_pid: Option<u32>,
    cpu_percent: Option<f32>,
    memory_bytes: Option<u64>,
    read_bytes: Option<u64>,
    written_bytes: Option<u64>,
}

#[derive(Clone, Deserialize, Serialize)]
struct AgentObservation {
    kind: String,
    root_pid: u32,
    process_count: usize,
    cpu_percent: Option<f32>,
    memory_bytes: u64,
    read_bytes: Option<u64>,
    written_bytes: Option<u64>,
}

#[derive(Deserialize, Serialize)]
struct AgentTick {
    at_unix_ms: u64,
    working_ms: u64,
    agents: Vec<AgentObservation>,
}

#[derive(Default, Serialize)]
struct AgentRollup {
    minute_unix_ms: u64,
    kind: String,
    active_samples: u64,
    observed_working_ms: u64,
    cpu_core_ms_estimate: f64,
    peak_cpu_percent: f32,
    peak_memory_bytes: u64,
    read_bytes: u64,
    written_bytes: u64,
    peak_processes: usize,
    peak_roots: usize,
}

pub(crate) fn state_dir(override_dir: Option<&Path>) -> io::Result<PathBuf> {
    if let Some(path) = override_dir {
        return Ok(path.to_path_buf());
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    Ok(PathBuf::from(home).join(".local/state/reef/observe"))
}

pub(crate) fn private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(io::Error::other("state directory must not be a symlink"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(crate) fn private_file(path: &Path, create_new: bool, append: bool) -> io::Result<File> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(io::Error::other("state file must not be a symlink"));
        }
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).append(append).create(true);
    if create_new {
        options.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn now_ms() -> io::Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_millis()
        .try_into()
        .map_err(io::Error::other)
}

fn sample(
    system: &mut System,
    disks: &mut Disks,
    dir: &Path,
    working_ms: u64,
) -> io::Result<Observation> {
    system.refresh_cpu_usage();
    system.refresh_memory();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_cpu()
            .with_memory()
            .with_disk_usage()
            .without_tasks(),
    );
    disks.refresh(true);
    let root = disks
        .list()
        .iter()
        .find(|disk| disk.mount_point() == Path::new("/"));
    let processes = process_observations(system);
    let roots = agent_observe::registrations(dir, system)?;
    let agents = agent_observations(&processes, &roots);
    let memory_total = system.total_memory();
    let swap_total = system.total_swap();
    Ok(Observation {
        at_unix_ms: now_ms()?,
        working_ms,
        cpu_percent: system
            .global_cpu_usage()
            .is_finite()
            .then_some(system.global_cpu_usage()),
        memory_used_bytes: (memory_total > 0).then_some(system.used_memory()),
        memory_total_bytes: (memory_total > 0).then_some(memory_total),
        swap_used_bytes: (swap_total > 0).then_some(system.used_swap()),
        swap_total_bytes: (swap_total > 0).then_some(swap_total),
        root_disk_available_bytes: root.map(sysinfo::Disk::available_space),
        root_disk_total_bytes: root.map(sysinfo::Disk::total_space),
        processes,
        agents,
    })
}

fn process_observations(system: &System) -> Vec<ProcessObservation> {
    let mut processes: Vec<_> = system
        .processes()
        .iter()
        .map(|(pid, process)| {
            let usage = process.disk_usage();
            ProcessObservation {
                pid: pid.as_u32(),
                parent_pid: process.parent().map(sysinfo::Pid::as_u32),
                cpu_percent: process
                    .cpu_usage()
                    .is_finite()
                    .then_some(process.cpu_usage()),
                memory_bytes: Some(process.memory()),
                read_bytes: (usage.read_bytes > 0).then_some(usage.read_bytes),
                written_bytes: (usage.written_bytes > 0).then_some(usage.written_bytes),
            }
        })
        .collect();
    processes.sort_by_key(|process| process.pid);
    processes
}

fn has_agent_registration(dir: &Path) -> io::Result<bool> {
    Ok(fs::read_dir(dir)?.any(|entry| {
        entry.is_ok_and(|entry| {
            entry.file_name().to_str().is_some_and(|name| {
                name.starts_with("agent-")
                    && Path::new(name)
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
            })
        })
    }))
}

fn agent_tick(system: &mut System, dir: &Path, working_ms: u64) -> io::Result<Option<AgentTick>> {
    if !has_agent_registration(dir)? {
        return Ok(None);
    }
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_cpu()
            .with_memory()
            .with_disk_usage()
            .without_tasks(),
    );
    let roots = agent_observe::registrations(dir, system)?;
    if roots.is_empty() {
        return Ok(None);
    }
    Ok(Some(AgentTick {
        at_unix_ms: now_ms()?,
        working_ms,
        agents: agent_observations(&process_observations(system), &roots),
    }))
}

fn agent_observations(
    processes: &[ProcessObservation],
    roots: &std::collections::HashMap<u32, agent_observe::Registration>,
) -> Vec<AgentObservation> {
    let parents: std::collections::HashMap<_, _> = processes
        .iter()
        .map(|process| (process.pid, process.parent_pid))
        .collect();
    let mut groups: BTreeMap<u32, AgentObservation> = BTreeMap::new();
    for process in processes {
        let mut pid = process.pid;
        let mut owner = None;
        // No valid ancestry chain can be longer than the process snapshot.
        for _ in 0..=processes.len() {
            if let Some(root) = roots.get(&pid) {
                owner = Some(root);
                break;
            }
            let Some(parent) = parents.get(&pid).copied().flatten() else {
                break;
            };
            pid = parent;
        }
        let Some(root) = owner else { continue };
        let group = groups.entry(root.pid).or_insert_with(|| AgentObservation {
            kind: root.kind.as_str().to_owned(),
            root_pid: root.pid,
            process_count: 0,
            cpu_percent: None,
            memory_bytes: 0,
            read_bytes: None,
            written_bytes: None,
        });
        group.process_count += 1;
        group.cpu_percent = sum_metric(group.cpu_percent, process.cpu_percent);
        group.memory_bytes = group
            .memory_bytes
            .saturating_add(process.memory_bytes.unwrap_or_default());
        group.read_bytes = sum_metric(group.read_bytes, process.read_bytes);
        group.written_bytes = sum_metric(group.written_bytes, process.written_bytes);
    }
    groups.into_values().collect()
}

fn sum_metric<T: std::ops::Add<Output = T>>(left: Option<T>, right: Option<T>) -> Option<T> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left + right),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn segments_with_prefix(dir: &Path, prefix: &str) -> io::Result<Vec<(u64, PathBuf, u64)>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(timestamp) = name
            .to_str()
            .and_then(|name| name.strip_prefix(prefix))
            .and_then(|name| name.strip_suffix(".jsonl"))
            .and_then(|name| name.split('-').next())
            .and_then(|timestamp| timestamp.parse::<u64>().ok())
        else {
            continue;
        };
        if entry.file_type()?.is_file() {
            files.push((timestamp, entry.path(), entry.metadata()?.len()));
        }
    }
    files.sort_by_key(|(timestamp, _, _)| *timestamp);
    Ok(files)
}

fn segments(dir: &Path) -> io::Result<Vec<(u64, PathBuf, u64)>> {
    segments_with_prefix(dir, "samples-")
}

fn prune(dir: &Path, now: u64, retention_days: u64, max_bytes: u64) -> io::Result<()> {
    let cutoff = now.saturating_sub(retention_days.saturating_mul(86_400_000));
    let mut files = segments(dir)?;
    files.extend(segments_with_prefix(dir, "agent-samples-")?);
    files.extend(segments_with_prefix(dir, "agent-rollups-")?);
    files.sort_by_key(|(timestamp, _, _)| *timestamp);
    let mut total: u64 = files.iter().map(|(_, _, size)| size).sum();
    for (timestamp, path, size) in files {
        if timestamp < cutoff || total > max_bytes {
            fs::remove_file(path)?;
            total = total.saturating_sub(size);
        }
    }
    Ok(())
}

fn append_line(
    dir: &Path,
    prefix: &str,
    at_unix_ms: u64,
    line: &[u8],
    options: &ObserveOptions,
) -> io::Result<()> {
    let max_bytes = options.max_storage_mib.saturating_mul(SEGMENT_BYTES);
    let files = segments_with_prefix(dir, prefix)?;
    let cutoff = at_unix_ms.saturating_sub(options.retention_days.saturating_mul(86_400_000));
    let latest = files.last().filter(|(timestamp, _, size)| {
        *timestamp >= cutoff
            && *size + (line.len() as u64) < SEGMENT_BYTES.min(max_bytes / 2)
            && (prefix != "agent-samples-" || timestamp / 3_600_000 == at_unix_ms / 3_600_000)
    });
    let path = latest.map_or_else(
        || {
            dir.join(format!(
                "{}{}-{}.jsonl",
                prefix,
                at_unix_ms,
                std::process::id()
            ))
        },
        |(_, path, _)| path.clone(),
    );
    let mut file = private_file(&path, false, true)?;
    file.write_all(line)?;
    file.write_all(b"\n")?;
    file.sync_data()?;
    prune(dir, at_unix_ms, options.retention_days, max_bytes)
}

fn append(dir: &Path, observation: &Observation, options: &ObserveOptions) -> io::Result<()> {
    let line = serde_json::to_vec(observation).map_err(io::Error::other)?;
    append_line(dir, "samples-", observation.at_unix_ms, &line, options)
}

fn append_agent_tick(dir: &Path, tick: &AgentTick, options: &ObserveOptions) -> io::Result<()> {
    let line = serde_json::to_vec(tick).map_err(io::Error::other)?;
    append_line(dir, "agent-samples-", tick.at_unix_ms, &line, options)
}

fn fold_tick(rollups: &mut BTreeMap<(u64, String), AgentRollup>, tick: &AgentTick) {
    let minute = tick.at_unix_ms / 60_000 * 60_000;
    let mut by_kind: BTreeMap<&str, Vec<&AgentObservation>> = BTreeMap::new();
    for agent in &tick.agents {
        by_kind.entry(&agent.kind).or_default().push(agent);
    }
    for (kind, agents) in by_kind {
        let rollup = rollups
            .entry((minute, kind.to_owned()))
            .or_insert_with(|| AgentRollup {
                minute_unix_ms: minute,
                kind: kind.to_owned(),
                ..AgentRollup::default()
            });
        let cpu: f32 = agents.iter().filter_map(|agent| agent.cpu_percent).sum();
        let memory = agents
            .iter()
            .map(|agent| agent.memory_bytes)
            .fold(0_u64, u64::saturating_add);
        rollup.active_samples += 1;
        rollup.observed_working_ms += tick.working_ms;
        let duration = u32::try_from(tick.working_ms).unwrap_or(u32::MAX);
        rollup.cpu_core_ms_estimate += f64::from(cpu.max(0.0)) * f64::from(duration) / 100.0;
        rollup.peak_cpu_percent = rollup.peak_cpu_percent.max(cpu);
        rollup.peak_memory_bytes = rollup.peak_memory_bytes.max(memory);
        rollup.read_bytes += agents
            .iter()
            .filter_map(|agent| agent.read_bytes)
            .sum::<u64>();
        rollup.written_bytes += agents
            .iter()
            .filter_map(|agent| agent.written_bytes)
            .sum::<u64>();
        rollup.peak_processes = rollup
            .peak_processes
            .max(agents.iter().map(|agent| agent.process_count).sum());
        rollup.peak_roots = rollup.peak_roots.max(agents.len());
    }
}

fn fold_agent_segments(dir: &Path, now: u64, options: &ObserveOptions) -> io::Result<()> {
    let current_hour = now / 3_600_000;
    for (timestamp, path, _) in segments_with_prefix(dir, "agent-samples-")? {
        if timestamp / 3_600_000 >= current_hour {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("agent-samples-"))
            .ok_or_else(|| io::Error::other("invalid agent sample segment"))?;
        let target = dir.join(format!("agent-rollups-{name}"));
        match fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_file() => {
                fs::remove_file(path)?;
                continue;
            }
            Ok(_) => return Err(io::Error::other("rollup target must be a regular file")),
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            Err(_) => {}
        }
        let mut rollups = BTreeMap::new();
        for line in BufReader::new(File::open(&path)?).lines() {
            let tick: AgentTick = serde_json::from_str(&line?).map_err(io::Error::other)?;
            fold_tick(&mut rollups, &tick);
        }
        let temporary = dir.join(format!(
            "agent-rollups-{timestamp}-{}-{}.tmp",
            std::process::id(),
            now_ms()?
        ));
        let mut file = private_file(&temporary, true, false)?;
        for rollup in rollups.values() {
            serde_json::to_writer(&mut file, rollup)?;
            file.write_all(b"\n")?;
        }
        file.sync_data()?;
        fs::rename(temporary, target)?;
        fs::remove_file(path)?;
    }
    prune(
        dir,
        now,
        options.retention_days,
        options.max_storage_mib.saturating_mul(SEGMENT_BYTES),
    )
}

fn working_ms(elapsed: Duration, interval: Duration) -> u64 {
    u64::try_from(elapsed.min(interval).as_millis()).unwrap_or(u64::MAX)
}

fn active_recorder_pid(path: &Path) -> io::Result<Option<u32>> {
    let contents = fs::read_to_string(path)?;
    let mut fields = contents.split_whitespace();
    let (Some(pid), Some(start_time), None) = (fields.next(), fields.next(), fields.next()) else {
        return Ok(None);
    };
    let (Ok(pid), Ok(start_time)) = (pid.parse::<u32>(), start_time.parse::<u64>()) else {
        return Ok(None);
    };
    let pid = sysinfo::Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing(),
    );
    Ok(system
        .process(pid)
        .and_then(|process| (process.start_time() == start_time).then_some(pid.as_u32())))
}

pub fn run(options: &ObserveOptions) -> io::Result<()> {
    let dir = state_dir(options.state_dir.as_deref())?;
    private_dir(&dir)?;
    let lock = private_file(&dir.join("recorder.lock"), false, false)
        .map_err(|error| io::Error::new(error.kind(), format!("open recorder lock: {error}")))?;
    lock.try_lock_exclusive().map_err(|error| {
        if error.kind() == io::ErrorKind::WouldBlock {
            io::Error::new(io::ErrorKind::AlreadyExists, "recorder already running")
        } else {
            io::Error::new(error.kind(), format!("lock recorder: {error}"))
        }
    })?;
    let mut pid_file = private_file(&dir.join("recorder.pid"), false, false)
        .map_err(|error| io::Error::new(error.kind(), format!("open recorder PID: {error}")))?;
    pid_file
        .set_len(0)
        .map_err(|error| io::Error::new(error.kind(), format!("clear recorder PID: {error}")))?;
    let stop_path = dir.join(format!("stop-{}", std::process::id()));
    if stop_path.exists() {
        fs::remove_file(&stop_path)?;
    }
    let pid = sysinfo::Pid::from_u32(std::process::id());
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing(),
    );
    let started = system
        .process(pid)
        .ok_or_else(|| io::Error::other("cannot identify recorder process"))?
        .start_time();
    writeln!(pid_file, "{} {started}", std::process::id())?;
    let result = record_loop(&dir, &stop_path, options);
    pid_file.set_len(0)?;
    result
}

fn record_loop(dir: &Path, stop_path: &Path, options: &ObserveOptions) -> io::Result<()> {
    let interval = Duration::from_secs(options.interval_seconds);
    let tick_interval = Duration::from_secs(1);
    let mut system = System::new();
    let mut disks = Disks::new_with_refreshed_list();
    system.refresh_cpu_usage();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_cpu()
            .with_disk_usage()
            .without_tasks(),
    );
    let mut previous_host = std::time::Instant::now();
    let mut previous_tick = std::time::Instant::now();
    loop {
        thread::sleep(tick_interval);
        if stop_path.exists() {
            fs::remove_file(stop_path)?;
            return Ok(());
        }
        let tick_elapsed = previous_tick.elapsed();
        previous_tick = std::time::Instant::now();
        let tick_working_ms = working_ms(tick_elapsed, tick_interval);
        if previous_host.elapsed() >= interval {
            let host_elapsed = previous_host.elapsed();
            previous_host = std::time::Instant::now();
            let observation = sample(
                &mut system,
                &mut disks,
                dir,
                working_ms(host_elapsed, interval),
            )?;
            fold_agent_segments(dir, observation.at_unix_ms, options)?;
            if !observation.agents.is_empty() {
                append_agent_tick(
                    dir,
                    &AgentTick {
                        at_unix_ms: observation.at_unix_ms,
                        working_ms: tick_working_ms,
                        agents: observation.agents.clone(),
                    },
                    options,
                )?;
            }
            append(dir, &observation, options)?;
        } else if let Some(tick) = agent_tick(&mut system, dir, tick_working_ms)? {
            append_agent_tick(dir, &tick, options)?;
        }
    }
}

pub fn start(options: &ObserveOptions) -> io::Result<()> {
    let dir = state_dir(options.state_dir.as_deref())?;
    private_dir(&dir)?;
    let lock_path = dir.join("recorder.pid");
    if lock_path.exists() && active_recorder_pid(&lock_path)?.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "recorder already running",
        ));
    }
    let mut child = platform::spawn_detached(options, &dir)?;
    for _ in 0..20 {
        if active_recorder_pid(&lock_path).ok().flatten() == Some(child.id()) {
            println!(
                "Recorder started (PID {}). Observations: {}",
                child.id(),
                dir.display()
            );
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!(
                "recorder exited during startup: {status}"
            )));
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(io::Error::other("recorder did not start within one second"))
}

pub fn stop(dir: Option<&Path>) -> io::Result<()> {
    let dir = state_dir(dir)?;
    let pid = active_recorder_pid(&dir.join("recorder.pid"))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "recorder is not running"))?;
    let stop_path = dir.join(format!("stop-{pid}"));
    match private_file(&stop_path, true, false) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    println!("Recorder will stop after its current interval");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(dir: &Path) -> ObserveOptions {
        ObserveOptions {
            interval_seconds: 1,
            retention_days: 1,
            max_storage_mib: 1,
            state_dir: Some(dir.to_path_buf()),
        }
    }

    fn observation(at_unix_ms: u64) -> Observation {
        Observation {
            at_unix_ms,
            working_ms: 1000,
            cpu_percent: None,
            memory_used_bytes: None,
            memory_total_bytes: None,
            swap_used_bytes: None,
            swap_total_bytes: None,
            root_disk_available_bytes: None,
            root_disk_total_bytes: None,
            processes: vec![],
            agents: vec![],
        }
    }

    #[test]
    fn observations_are_private_and_omit_sensitive_process_data() {
        let dir = std::env::temp_dir().join(format!("reef-observe-test-{}", std::process::id()));
        private_dir(&dir).unwrap();

        append(&dir, &observation(100_000_000), &options(&dir)).unwrap();

        let path = segments(&dir).unwrap().pop().unwrap().1;
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"cpu_percent\":null"));
        assert!(!content.contains("command"));
        assert!(!content.contains("environ"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn old_observations_expire_at_retention_boundary() {
        let dir = std::env::temp_dir().join(format!("reef-retention-test-{}", std::process::id()));
        private_dir(&dir).unwrap();
        let old = dir.join("samples-1-1.jsonl");
        fs::write(&old, b"old\n").unwrap();

        append(&dir, &observation(100_000_000), &options(&dir)).unwrap();

        assert!(!old.exists());
        assert_eq!(segments(&dir).unwrap().len(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn storage_limit_removes_oldest_segments() {
        let dir = std::env::temp_dir().join(format!("reef-storage-test-{}", std::process::id()));
        private_dir(&dir).unwrap();
        fs::write(dir.join("samples-99999999-1.jsonl"), vec![b'x'; 800_000]).unwrap();

        append(&dir, &observation(100_000_000), &options(&dir)).unwrap();
        prune(&dir, 100_000_000, 1, 100).unwrap();

        assert!(
            segments(&dir)
                .unwrap()
                .iter()
                .map(|(_, _, size)| size)
                .sum::<u64>()
                <= 100
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn delayed_sampling_does_not_count_sleep_as_work() {
        assert_eq!(
            working_ms(Duration::from_secs(3_600), Duration::from_secs(30)),
            30_000
        );
    }

    #[test]
    fn on_time_sampling_counts_elapsed_work() {
        assert_eq!(
            working_ms(Duration::from_millis(750), Duration::from_secs(1)),
            750
        );
    }

    #[test]
    fn unrelated_live_pid_does_not_claim_recorder_state() {
        let dir = std::env::temp_dir().join(format!("reef-pid-test-{}", std::process::id()));
        private_dir(&dir).unwrap();
        let path = dir.join("recorder.pid");
        fs::write(&path, format!("{} 0\n", std::process::id())).unwrap();

        assert_eq!(active_recorder_pid(&path).unwrap(), None);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn descendant_load_belongs_to_nearest_registered_agent() {
        let roots = std::collections::HashMap::from([
            (
                10,
                agent_observe::Registration {
                    kind: agent_observe::AgentKind::Codex,
                    pid: 10,
                    start_time: 1,
                },
            ),
            (
                12,
                agent_observe::Registration {
                    kind: agent_observe::AgentKind::Claude,
                    pid: 12,
                    start_time: 1,
                },
            ),
        ]);
        let process = |pid, parent_pid, cpu_percent, memory_bytes| ProcessObservation {
            pid,
            parent_pid,
            cpu_percent: Some(cpu_percent),
            memory_bytes: Some(memory_bytes),
            read_bytes: Some(5),
            written_bytes: Some(7),
        };
        let agents = agent_observations(
            &[
                process(10, None, 2.0, 100),
                process(11, Some(10), 3.0, 200),
                process(12, Some(11), 4.0, 300),
                process(13, Some(12), 5.0, 400),
                process(14, None, 6.0, 500),
            ],
            &roots,
        );
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].kind, "codex");
        assert_eq!(agents[0].process_count, 2);
        assert_eq!(agents[0].cpu_percent, Some(5.0));
        assert_eq!(agents[0].memory_bytes, 300);
        assert_eq!(agents[1].kind, "claude");
        assert_eq!(agents[1].process_count, 2);
        assert_eq!(agents[1].cpu_percent, Some(9.0));
        assert_eq!(agents[1].memory_bytes, 700);
    }

    #[test]
    fn deep_process_tree_is_fully_attributed() {
        let roots = std::collections::HashMap::from([(
            10,
            agent_observe::Registration {
                kind: agent_observe::AgentKind::Codex,
                pid: 10,
                start_time: 1,
            },
        )]);
        let processes = (10..=80)
            .map(|pid| ProcessObservation {
                pid,
                parent_pid: (pid > 10).then_some(pid - 1),
                cpu_percent: Some(1.0),
                memory_bytes: Some(1),
                read_bytes: Some(1),
                written_bytes: Some(1),
            })
            .collect::<Vec<_>>();
        let agents = agent_observations(&processes, &roots);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].process_count, 71);
        assert_eq!(agents[0].memory_bytes, 71);
    }

    #[test]
    fn older_agent_samples_fold_into_minute_load_without_raw_history() {
        let dir = std::env::temp_dir().join(format!("reef-agent-fold-test-{}", std::process::id()));
        private_dir(&dir).unwrap();
        let make_tick = |at_unix_ms| AgentTick {
            at_unix_ms,
            working_ms: 1_000,
            agents: vec![AgentObservation {
                kind: "claude".to_owned(),
                root_pid: 42,
                process_count: 2,
                cpu_percent: Some(50.0),
                memory_bytes: 400,
                read_bytes: Some(10),
                written_bytes: Some(20),
            }],
        };
        append_agent_tick(&dir, &make_tick(3_600_100), &options(&dir)).unwrap();
        append_agent_tick(&dir, &make_tick(3_601_100), &options(&dir)).unwrap();

        fold_agent_segments(&dir, 7_200_000, &options(&dir)).unwrap();

        assert_eq!(
            segments_with_prefix(&dir, "agent-samples-").unwrap(),
            Vec::new()
        );
        let rollups = segments_with_prefix(&dir, "agent-rollups-").unwrap();
        assert_eq!(rollups.len(), 1);
        let content = fs::read_to_string(&rollups[0].1).unwrap();
        let rollup: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(rollup["kind"], "claude");
        assert_eq!(rollup["active_samples"], 2);
        assert_eq!(rollup["observed_working_ms"], 2_000);
        assert_eq!(rollup["cpu_core_ms_estimate"], 1_000.0);
        assert_eq!(rollup["peak_memory_bytes"], 400);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn retry_after_rollup_publish_does_not_duplicate_load() {
        let dir =
            std::env::temp_dir().join(format!("reef-agent-fold-retry-test-{}", std::process::id()));
        private_dir(&dir).unwrap();
        let tick = AgentTick {
            at_unix_ms: 3_600_100,
            working_ms: 1_000,
            agents: vec![AgentObservation {
                kind: "codex".to_owned(),
                root_pid: 42,
                process_count: 1,
                cpu_percent: Some(50.0),
                memory_bytes: 400,
                read_bytes: None,
                written_bytes: None,
            }],
        };
        append_agent_tick(&dir, &tick, &options(&dir)).unwrap();
        let raw = segments_with_prefix(&dir, "agent-samples-").unwrap()[0]
            .1
            .clone();
        fold_agent_segments(&dir, 7_200_000, &options(&dir)).unwrap();
        append_agent_tick(&dir, &tick, &options(&dir)).unwrap();
        assert_eq!(
            segments_with_prefix(&dir, "agent-samples-").unwrap()[0].1,
            raw
        );

        fold_agent_segments(&dir, 7_200_000, &options(&dir)).unwrap();

        assert_eq!(
            segments_with_prefix(&dir, "agent-samples-").unwrap(),
            Vec::new()
        );
        assert_eq!(
            segments_with_prefix(&dir, "agent-rollups-").unwrap().len(),
            1
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
