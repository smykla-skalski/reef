#[cfg(unix)]
use crate::cache_impact;
use crate::history;
use crate::schedule_events;
use chrono::{DateTime, Duration, Utc};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

mod compare;
pub use compare::{CompareOptions, run_compare};

#[derive(Debug, clap::Args)]
pub struct Options {
    /// Start of the range, inclusive, in RFC 3339 format. Defaults to 24 hours ago.
    #[arg(long, value_parser = parse_date)]
    from: Option<DateTime<Utc>>,
    /// End of the range, exclusive, in RFC 3339 format. Defaults to now.
    #[arg(long, value_parser = parse_date)]
    to: Option<DateTime<Utc>>,
    /// Observation directory. Defaults to ~/.local/state/reef/observe.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Private JSON Lines file written by reef run; may be repeated.
    #[arg(long = "records")]
    records: Vec<PathBuf>,
    /// Scheduler state directory containing private intervention events.
    #[arg(long)]
    schedule_state_dir: Option<PathBuf>,
    /// Cache directory used by `reef cache run`.
    #[cfg(unix)]
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Report format.
    #[arg(long, value_enum, default_value_t = Format::Markdown)]
    format: Format,
    /// CPU usage threshold, as a percentage.
    #[arg(long, default_value_t = 90)]
    cpu_threshold: u8,
    /// Memory usage threshold, as a percentage.
    #[arg(long, default_value_t = 90)]
    memory_threshold: u8,
    /// Swap usage threshold, as a percentage.
    #[arg(long, default_value_t = 1)]
    swap_threshold: u8,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Format {
    Markdown,
    Json,
}

fn parse_date(value: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(value)
        .map(|date| date.with_timezone(&Utc))
        .map_err(|error| format!("expected an RFC 3339 date: {error}"))
}

#[derive(Debug, Deserialize)]
struct Observation {
    at_unix_ms: u64,
    working_ms: u64,
    cpu_percent: Option<f64>,
    memory_used_bytes: Option<u64>,
    memory_total_bytes: Option<u64>,
    swap_used_bytes: Option<u64>,
    swap_total_bytes: Option<u64>,
    agents: Option<Vec<AgentSample>>,
}

#[derive(Debug, Deserialize)]
struct AgentSample {
    kind: String,
    root_pid: u32,
    process_count: usize,
    cpu_percent: Option<f64>,
    memory_bytes: u64,
    read_bytes: Option<u64>,
    written_bytes: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct AgentTick {
    at_unix_ms: u64,
    working_ms: u64,
    agents: Vec<AgentSample>,
}

#[derive(Debug, Deserialize)]
struct AgentRollup {
    minute_unix_ms: u64,
    kind: String,
    active_samples: usize,
    observed_working_ms: u128,
    cpu_core_ms_estimate: f64,
    peak_cpu_percent: f64,
    peak_memory_bytes: u64,
    read_bytes: u64,
    written_bytes: u64,
    peak_processes: usize,
    peak_roots: usize,
}

#[derive(Default)]
struct AgentData {
    ticks: Vec<AgentTick>,
    rollups: Vec<AgentRollup>,
}

#[derive(Debug, Deserialize)]
struct Measurement {
    category: String,
    status: String,
    started_at_unix_ms: u128,
    ended_at_unix_ms: u128,
    wall_ms: u128,
    tree_cpu_ms: u128,
    tree_peak_memory_bytes: u64,
}

#[derive(Serialize)]
struct Report {
    from: String,
    to: String,
    empty: bool,
    observation_count: usize,
    agent_observation_count: usize,
    agent_rollup_count: usize,
    observed_working_ms: u128,
    command_measurements_available: bool,
    command_count: Option<usize>,
    categories: Option<Vec<CategoryReport>>,
    peak_host_memory_bytes: Option<u64>,
    swap_growth_bytes: Option<i128>,
    peak_command_concurrency: Option<usize>,
    pressure: PressureReport,
    timeline: Vec<PressureEvent>,
    agents: Option<Vec<AgentLoad>>,
    scheduler: Option<schedule_events::Summary>,
    #[cfg(unix)]
    cache: cache_impact::Impact,
}

#[derive(Serialize)]
struct CategoryReport {
    category: &'static str,
    count: usize,
    failed_or_cancelled: usize,
    wall_ms: u128,
    wall_percent_of_measured: f64,
    cpu_ms: u128,
    cpu_percent_of_measured: f64,
    peak_memory_bytes: u64,
}

#[derive(Default, Serialize)]
struct AgentLoad {
    kind: String,
    active_samples: usize,
    observed_working_ms: u128,
    cpu_core_ms_estimate: f64,
    peak_cpu_percent: f64,
    peak_memory_bytes: u64,
    read_bytes: u64,
    written_bytes: u64,
    peak_processes: usize,
    peak_roots: usize,
}

#[derive(Default, Serialize)]
struct PressureReport {
    cpu_threshold_percent: u8,
    memory_threshold_percent: u8,
    swap_threshold_percent: u8,
    cpu_above_ms: Option<u128>,
    memory_above_ms: Option<u128>,
    swap_above_ms: Option<u128>,
    any_above_ms: Option<u128>,
}

#[derive(Serialize)]
struct PressureEvent {
    from_unix_ms: u128,
    to_unix_ms: u128,
    triggers: Vec<&'static str>,
    overlapping_categories: Option<Vec<&'static str>>,
    overlapping_agents: Option<Vec<&'static str>>,
    attribution: &'static str,
}

fn agent_kind(value: &str) -> &'static str {
    match value {
        "codex" => "codex",
        "claude" => "claude",
        "opencode" => "opencode",
        _ => "other",
    }
}

fn state_dir(override_dir: Option<&Path>) -> io::Result<PathBuf> {
    if let Some(dir) = override_dir {
        return Ok(dir.to_path_buf());
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    Ok(PathBuf::from(home).join(".local/state/reef/observe"))
}

fn read_jsonl<T: for<'de> Deserialize<'de>>(path: &Path) -> io::Result<Vec<T>> {
    let file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    BufReader::new(file)
        .lines()
        .enumerate()
        .map(|(index, line)| {
            serde_json::from_str(&line?).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}:{}: {error}", path.display(), index + 1),
                )
            })
        })
        .collect()
}

fn observations(dir: &Path) -> io::Result<Vec<Observation>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if name
            .to_str()
            .is_some_and(|name| name.starts_with("samples-"))
            && Path::new(&name)
                .extension()
                .is_some_and(|ext| ext == "jsonl")
            && entry.file_type()?.is_file()
        {
            paths.push(entry.path());
        }
    }
    paths.sort();
    let mut samples: Vec<Observation> = Vec::new();
    for path in paths {
        samples.extend(read_jsonl(&path)?);
    }
    samples.sort_by_key(|sample| sample.at_unix_ms);
    samples.dedup_by_key(|sample| sample.at_unix_ms);
    Ok(samples)
}

fn agent_ticks(dir: &Path) -> io::Result<Vec<AgentTick>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if name
            .to_str()
            .is_some_and(|name| name.starts_with("agent-samples-"))
            && Path::new(&name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
            && entry.file_type()?.is_file()
        {
            paths.push(entry.path());
        }
    }
    paths.sort();
    let mut ticks = Vec::new();
    for path in paths {
        ticks.extend(read_jsonl(&path)?);
    }
    ticks.sort_by_key(|tick: &AgentTick| tick.at_unix_ms);
    ticks.dedup_by_key(|tick| tick.at_unix_ms);
    Ok(ticks)
}

fn agent_rollups(dir: &Path) -> io::Result<Vec<AgentRollup>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if name
            .to_str()
            .is_some_and(|name| name.starts_with("agent-rollups-"))
            && Path::new(&name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
            && entry.file_type()?.is_file()
        {
            paths.push(entry.path());
        }
    }
    paths.sort();
    let mut rollups = Vec::new();
    for path in paths {
        rollups.extend(read_jsonl(&path)?);
    }
    Ok(rollups)
}

fn category(value: &str) -> &'static str {
    match value {
        "build" => "build",
        "lint" => "lint",
        "test" => "test",
        "agent" => "agent",
        "container" => "container",
        "interactive" => "interactive",
        _ => "other",
    }
}

fn percentage(part: u128, total: u128) -> f64 {
    let hundredths = part
        .saturating_mul(10_000)
        .checked_div(total)
        .unwrap_or_default();
    f64::from(u32::try_from(hundredths).unwrap_or(u32::MAX)) / 100.0
}

fn above(used: Option<u64>, total: Option<u64>, threshold: u8) -> Option<bool> {
    used.zip(total)
        .filter(|(_, total)| *total > 0)
        .map(|(used, total)| u128::from(used) * 100 >= u128::from(total) * u128::from(threshold))
}

fn add_duration(
    total: &mut Option<u128>,
    missing: &mut bool,
    triggered: Option<bool>,
    duration: u128,
) {
    if duration == 0 {
        return;
    }
    if let Some(triggered) = triggered {
        if !*missing {
            *total = Some(total.unwrap_or_default() + u128::from(triggered) * duration);
        }
    } else {
        *missing = true;
        *total = None;
    }
}

fn pressure_flags(sample: &Observation, thresholds: (u8, u8, u8)) -> [Option<bool>; 3] {
    [
        sample
            .cpu_percent
            .filter(|value| value.is_finite())
            .map(|value| value >= f64::from(thresholds.0)),
        above(
            sample.memory_used_bytes,
            sample.memory_total_bytes,
            thresholds.1,
        ),
        above(
            sample.swap_used_bytes,
            sample.swap_total_bytes,
            thresholds.2,
        ),
    ]
}

fn any_pressure(flags: [Option<bool>; 3]) -> Option<bool> {
    if flags.contains(&Some(true)) {
        Some(true)
    } else if flags.contains(&None) {
        None
    } else {
        Some(false)
    }
}

fn pressure_triggers([cpu, memory, swap]: [Option<bool>; 3]) -> Vec<&'static str> {
    let mut triggers = Vec::new();
    if cpu == Some(true) {
        triggers.push("cpu");
    }
    if memory == Some(true) {
        triggers.push("memory");
    }
    if swap == Some(true) {
        triggers.push("swap");
    }
    triggers
}

fn peak_concurrency(records: &[Measurement], from: u128, to: u128) -> usize {
    let mut events = Vec::new();
    for record in records {
        let start = record.started_at_unix_ms.max(from);
        let end = record.ended_at_unix_ms.min(to);
        if start < end {
            events.push((start, 1_i32));
            events.push((end, -1_i32));
        }
    }
    events.sort_by_key(|(time, change)| (*time, *change));
    let mut active = 0_i32;
    let mut peak = 0_i32;
    for (_, change) in events {
        active += change;
        peak = peak.max(active);
    }
    usize::try_from(peak).unwrap_or_default()
}

fn category_reports(records: &[Measurement]) -> Vec<CategoryReport> {
    let total_wall: u128 = records.iter().map(|record| record.wall_ms).sum();
    let total_cpu: u128 = records.iter().map(|record| record.tree_cpu_ms).sum();
    let mut groups: BTreeMap<&'static str, CategoryReport> = BTreeMap::new();
    for record in records {
        let name = category(&record.category);
        let group = groups.entry(name).or_insert_with(|| CategoryReport {
            category: name,
            count: 0,
            failed_or_cancelled: 0,
            wall_ms: 0,
            wall_percent_of_measured: 0.0,
            cpu_ms: 0,
            cpu_percent_of_measured: 0.0,
            peak_memory_bytes: 0,
        });
        group.count += 1;
        group.failed_or_cancelled += usize::from(record.status != "success");
        group.wall_ms += record.wall_ms;
        group.cpu_ms += record.tree_cpu_ms;
        group.peak_memory_bytes = group.peak_memory_bytes.max(record.tree_peak_memory_bytes);
    }
    groups
        .into_values()
        .map(|mut group| {
            group.wall_percent_of_measured = percentage(group.wall_ms, total_wall);
            group.cpu_percent_of_measured = percentage(group.cpu_ms, total_cpu);
            group
        })
        .collect()
}

fn pressure_event(
    start: u128,
    end: u128,
    triggers: Vec<&'static str>,
    records: Option<&[Measurement]>,
    agents: Option<&[AgentSample]>,
) -> PressureEvent {
    let overlapping_categories: Option<Vec<&'static str>> = records.map(|records| {
        records
            .iter()
            .filter(|record| record.started_at_unix_ms < end && record.ended_at_unix_ms > start)
            .map(|record| category(&record.category))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    });
    let overlapping_agents = agents.map(|agents| {
        agents
            .iter()
            .map(|agent| agent_kind(&agent.kind))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
    });
    let attribution = match (&overlapping_categories, &overlapping_agents) {
        (Some(categories), _) if !categories.is_empty() => "measured_command_overlap",
        (_, Some(agents)) if !agents.is_empty() => "agent_process_overlap",
        (Some(_), _) => "no_measured_command_overlap",
        (None, _) => "history_unavailable",
    };
    PressureEvent {
        from_unix_ms: start,
        to_unix_ms: end,
        triggers,
        overlapping_categories,
        overlapping_agents,
        attribution,
    }
}

fn accumulate_agents(
    load: &mut BTreeMap<String, AgentLoad>,
    agents: &[AgentSample],
    duration: u128,
) {
    let mut by_kind: BTreeMap<&str, Vec<&AgentSample>> = BTreeMap::new();
    for agent in agents {
        by_kind
            .entry(agent_kind(&agent.kind))
            .or_default()
            .push(agent);
    }
    for (kind, sessions) in by_kind {
        let group = load.entry(kind.to_owned()).or_insert_with(|| AgentLoad {
            kind: kind.to_owned(),
            ..AgentLoad::default()
        });
        let cpu: f64 = sessions.iter().filter_map(|agent| agent.cpu_percent).sum();
        let memory: u64 = sessions
            .iter()
            .map(|agent| agent.memory_bytes)
            .fold(0, u64::saturating_add);
        let duration = u32::try_from(duration).unwrap_or(u32::MAX);
        group.active_samples += 1;
        group.observed_working_ms += u128::from(duration);
        group.cpu_core_ms_estimate += cpu.max(0.0) * f64::from(duration) / 100.0;
        group.peak_cpu_percent = group.peak_cpu_percent.max(cpu);
        group.peak_memory_bytes = group.peak_memory_bytes.max(memory);
        group.read_bytes += sessions
            .iter()
            .filter_map(|agent| agent.read_bytes)
            .fold(0, u64::saturating_add);
        group.written_bytes += sessions
            .iter()
            .filter_map(|agent| agent.written_bytes)
            .fold(0, u64::saturating_add);
        group.peak_processes = group
            .peak_processes
            .max(sessions.iter().map(|agent| agent.process_count).sum());
        group.peak_roots = group.peak_roots.max(
            sessions
                .iter()
                .map(|agent| agent.root_pid)
                .collect::<BTreeSet<_>>()
                .len(),
        );
    }
}

fn accumulate_agent_ticks(
    load: &mut BTreeMap<String, AgentLoad>,
    ticks: &[AgentTick],
    from_ms: u128,
    to_ms: u128,
) {
    let mut covered_until = from_ms;
    for tick in ticks {
        let at = u128::from(tick.at_unix_ms);
        let end = at.min(to_ms);
        let start = at
            .saturating_sub(u128::from(tick.working_ms))
            .max(covered_until)
            .max(from_ms);
        let duration = end.saturating_sub(start);
        covered_until = covered_until.max(end);
        if duration > 0 {
            accumulate_agents(load, &tick.agents, duration);
        }
    }
}

fn accumulate_agent_rollups(load: &mut BTreeMap<String, AgentLoad>, rollups: &[AgentRollup]) {
    for rollup in rollups {
        let kind = agent_kind(&rollup.kind).to_owned();
        let group = load.entry(kind.clone()).or_insert_with(|| AgentLoad {
            kind,
            ..AgentLoad::default()
        });
        group.active_samples += rollup.active_samples;
        group.observed_working_ms += rollup.observed_working_ms;
        group.cpu_core_ms_estimate += rollup.cpu_core_ms_estimate;
        group.peak_cpu_percent = group.peak_cpu_percent.max(rollup.peak_cpu_percent);
        group.peak_memory_bytes = group.peak_memory_bytes.max(rollup.peak_memory_bytes);
        group.read_bytes += rollup.read_bytes;
        group.written_bytes += rollup.written_bytes;
        group.peak_processes = group.peak_processes.max(rollup.peak_processes);
        group.peak_roots = group.peak_roots.max(rollup.peak_roots);
    }
}

fn rollups_in_range(rollups: Vec<AgentRollup>, from_ms: u128, to_ms: u128) -> Vec<AgentRollup> {
    rollups
        .into_iter()
        .filter(|rollup| {
            let start = u128::from(rollup.minute_unix_ms);
            start < to_ms && start + 60_000 > from_ms
        })
        .collect()
}

fn rolled_minutes(rollups: &[AgentRollup]) -> BTreeSet<u64> {
    rollups.iter().map(|rollup| rollup.minute_unix_ms).collect()
}

fn ticks_in_range(ticks: Vec<AgentTick>, from_ms: u128, to_ms: u128) -> Vec<AgentTick> {
    ticks
        .into_iter()
        .filter(|tick| {
            let at = u128::from(tick.at_unix_ms);
            at > from_ms && at.saturating_sub(u128::from(tick.working_ms)) < to_ms
        })
        .collect()
}

fn observations_in_range(
    samples: Vec<Observation>,
    from_ms: u128,
    to_ms: u128,
) -> Vec<Observation> {
    samples
        .into_iter()
        .filter(|sample| {
            let at = u128::from(sample.at_unix_ms);
            at > from_ms && at.saturating_sub(u128::from(sample.working_ms)) < to_ms
        })
        .collect()
}

fn records_in_range(
    records: Option<Vec<Measurement>>,
    from_ms: u128,
    to_ms: u128,
) -> Option<Vec<Measurement>> {
    records.map(|records| {
        records
            .into_iter()
            .filter(|record| record.started_at_unix_ms < to_ms && record.ended_at_unix_ms > from_ms)
            .collect()
    })
}

#[cfg(test)]
fn build_report(
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    samples: Vec<Observation>,
    records: Option<Vec<Measurement>>,
    thresholds: (u8, u8, u8),
    scheduler: Option<schedule_events::Summary>,
) -> Report {
    build_report_with_ticks(
        from,
        to,
        samples,
        AgentData::default(),
        records,
        thresholds,
        scheduler,
    )
}

fn build_report_with_ticks(
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    samples: Vec<Observation>,
    agent_data: AgentData,
    records: Option<Vec<Measurement>>,
    thresholds: (u8, u8, u8),
    scheduler: Option<schedule_events::Summary>,
) -> Report {
    let from_ms = u128::try_from(from.timestamp_millis()).unwrap_or_default();
    let to_ms = u128::try_from(to.timestamp_millis()).unwrap_or_default();
    let samples = observations_in_range(samples, from_ms, to_ms);
    let mut agent_ticks = ticks_in_range(agent_data.ticks, from_ms, to_ms);
    let agent_rollups = rollups_in_range(agent_data.rollups, from_ms, to_ms);
    let rolled_minutes = rolled_minutes(&agent_rollups);
    agent_ticks.retain(|tick| !rolled_minutes.contains(&(tick.at_unix_ms / 60_000 * 60_000)));
    let tick_times: BTreeSet<_> = agent_ticks.iter().map(|tick| tick.at_unix_ms).collect();
    let records = records_in_range(records, from_ms, to_ms);
    let mut pressure = PressureReport {
        cpu_threshold_percent: thresholds.0,
        memory_threshold_percent: thresholds.1,
        swap_threshold_percent: thresholds.2,
        ..PressureReport::default()
    };
    let mut timeline = Vec::new();
    let mut observed_working_ms = 0_u128;
    let mut covered_until = from_ms;
    let mut missing = [false; 4];
    let agents_available = !agent_ticks.is_empty()
        || !agent_rollups.is_empty()
        || samples.iter().any(|sample| sample.agents.is_some());
    let mut agent_load: BTreeMap<String, AgentLoad> = BTreeMap::new();
    for sample in &samples {
        let at = u128::from(sample.at_unix_ms);
        let end = at.min(to_ms);
        let start = at
            .saturating_sub(u128::from(sample.working_ms))
            .max(covered_until);
        let duration = end.saturating_sub(start);
        covered_until = covered_until.max(end);
        observed_working_ms += duration;
        if !tick_times.contains(&sample.at_unix_ms)
            && !rolled_minutes.contains(&(sample.at_unix_ms / 60_000 * 60_000))
            && let Some(agent_samples) = &sample.agents
        {
            accumulate_agents(&mut agent_load, agent_samples, duration);
        }
        let [cpu, memory, swap] = pressure_flags(sample, thresholds);
        add_duration(&mut pressure.cpu_above_ms, &mut missing[0], cpu, duration);
        add_duration(
            &mut pressure.memory_above_ms,
            &mut missing[1],
            memory,
            duration,
        );
        add_duration(&mut pressure.swap_above_ms, &mut missing[2], swap, duration);
        let any = any_pressure([cpu, memory, swap]);
        add_duration(&mut pressure.any_above_ms, &mut missing[3], any, duration);
        let triggers = pressure_triggers([cpu, memory, swap]);
        if !triggers.is_empty() && duration > 0 {
            timeline.push(pressure_event(
                start,
                end,
                triggers,
                records.as_deref(),
                sample.agents.as_deref(),
            ));
        }
    }
    accumulate_agent_ticks(&mut agent_load, &agent_ticks, from_ms, to_ms);
    accumulate_agent_rollups(&mut agent_load, &agent_rollups);
    let peak_host_memory_bytes = samples.iter().filter_map(|s| s.memory_used_bytes).max();
    let swap_values: Vec<_> = samples.iter().filter_map(|s| s.swap_used_bytes).collect();
    let swap_growth_bytes = (swap_values.len() >= 2)
        .then(|| i128::from(*swap_values.last().unwrap()) - i128::from(swap_values[0]));
    let peak_command_concurrency = records
        .as_ref()
        .map(|records| peak_concurrency(records, from_ms, to_ms));
    let command_count = records.as_ref().map(Vec::len);
    let categories = records.as_ref().map(|records| category_reports(records));
    Report {
        from: from.to_rfc3339(),
        to: to.to_rfc3339(),
        empty: samples.is_empty()
            && agent_ticks.is_empty()
            && agent_rollups.is_empty()
            && command_count.unwrap_or_default() == 0
            && scheduler
                .as_ref()
                .is_none_or(|summary| summary.submitted == 0 && summary.rejected == 0),
        observation_count: samples.len(),
        agent_observation_count: agent_ticks.len(),
        agent_rollup_count: agent_rollups.len(),
        observed_working_ms,
        command_measurements_available: records.is_some(),
        command_count,
        categories,
        peak_host_memory_bytes,
        swap_growth_bytes,
        peak_command_concurrency,
        pressure,
        timeline,
        agents: agents_available.then(|| agent_load.into_values().collect()),
        scheduler,
        #[cfg(unix)]
        cache: cache_impact::Impact::default(),
    }
}

fn printable<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map_or_else(|| "unavailable".to_owned(), |value| value.to_string())
}

fn scheduler_markdown(scheduler: Option<&schedule_events::Summary>) -> String {
    let mut out = String::from("\n## Scheduler interventions\n\n");
    if let Some(scheduler) = scheduler {
        let _ = write!(
            out,
            "- Submitted: {}\n- Admitted: {}\n- Completed: {}\n- Failed: {}\n- Cancelled: {}\n- Rejected: {}\n- Queue wait p50/p95: {} / {} ms\n- Submission-to-finish p50/p95: {} / {} ms\n- Wait by reason (pressure/capacity/FIFO/running limit/admission): {} / {} / {} / {} / {} ms\n\nScheduler outcomes are observations, not evidence that Reef prevented pressure.\n",
            scheduler.submitted,
            scheduler.admitted,
            scheduler.completed,
            scheduler.failed,
            scheduler.cancelled,
            scheduler.rejected,
            printable(scheduler.queue_wait_p50_ms),
            printable(scheduler.queue_wait_p95_ms),
            printable(scheduler.submission_to_finish_p50_ms),
            printable(scheduler.submission_to_finish_p95_ms),
            scheduler.wait.pressure,
            scheduler.wait.capacity,
            scheduler.wait.fifo,
            scheduler.wait.running_limit,
            scheduler.wait.admission,
        );
    } else {
        out.push_str("Unavailable: no scheduler event history.\n");
    }
    out
}

fn agents_markdown(agents: Option<&[AgentLoad]>) -> String {
    let mut out = String::from("\n## Passive agent load\n\n");
    if let Some(agents) = agents {
        out.push_str("Sampled process-tree usage. CPU core-ms and RSS are estimates; overlap with command measurements is expected.\n\n");
        out.push_str("| Agent | Active samples | CPU core-ms | Peak CPU % | Peak RSS bytes | Read bytes | Written bytes | Peak processes | Peak roots |\n| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
        for agent in agents {
            let _ = writeln!(
                out,
                "| {} | {} | {:.0} | {:.1} | {} | {} | {} | {} | {} |",
                agent.kind,
                agent.active_samples,
                agent.cpu_core_ms_estimate,
                agent.peak_cpu_percent,
                agent.peak_memory_bytes,
                agent.read_bytes,
                agent.written_bytes,
                agent.peak_processes,
                agent.peak_roots
            );
        }
        if agents.is_empty() {
            out.push_str("\nNo registered agent process was active in the range.\n");
        }
    } else {
        out.push_str("Unavailable: observations predate passive agent attribution.\n");
    }
    out
}

fn markdown(report: &Report) -> String {
    let mut out = format!(
        "# Reef workload report\n\n- Range: {} to {} (end exclusive)\n- Empty: {}\n- Host observations: {}\n- Agent observations: {}\n- Agent rollups: {}\n- Observed working time: {} ms\n- Peak host memory: {} bytes\n- Swap growth: {} bytes\n- Peak command concurrency: {}\n\n",
        report.from,
        report.to,
        report.empty,
        report.observation_count,
        report.agent_observation_count,
        report.agent_rollup_count,
        report.observed_working_ms,
        printable(report.peak_host_memory_bytes),
        printable(report.swap_growth_bytes),
        printable(report.peak_command_concurrency),
    );
    out.push_str("## Commands\n\n");
    if let Some(categories) = &report.categories {
        out.push_str("Whole commands overlapping the range; percentages are shares of measured commands. Concurrent wall times can exceed elapsed time.\n\n");
        out.push_str("| Category | Count | Failed/cancelled | Wall ms | Wall % | CPU ms | CPU % | Peak memory bytes |\n| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
        for group in categories {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {:.1} | {} | {:.1} | {} |",
                group.category,
                group.count,
                group.failed_or_cancelled,
                group.wall_ms,
                group.wall_percent_of_measured,
                group.cpu_ms,
                group.cpu_percent_of_measured,
                group.peak_memory_bytes
            );
        }
        if categories.is_empty() {
            out.push_str("\nNo Reef-managed commands overlap the range. Observed host activity remains unattributed.\n");
        }
    } else {
        out.push_str(
            "No command history is available. Host activity is uninstrumented or unknown.\n",
        );
    }
    out.push_str(&agents_markdown(report.agents.as_deref()));
    let _ = write!(
        out,
        "\n## Pressure\n\n- CPU >= {}%: {} ms\n- Memory >= {}%: {} ms\n- Swap >= {}%: {} ms\n- Any threshold: {} ms\n\n## Pressure timeline\n\n",
        report.pressure.cpu_threshold_percent,
        printable(report.pressure.cpu_above_ms),
        report.pressure.memory_threshold_percent,
        printable(report.pressure.memory_above_ms),
        report.pressure.swap_threshold_percent,
        printable(report.pressure.swap_above_ms),
        printable(report.pressure.any_above_ms)
    );
    if report.timeline.is_empty() {
        out.push_str("No recorded pressure events.\n");
    } else {
        out.push_str(
            "Categories overlapped these samples in time; overlap does not establish cause.\n\n",
        );
        for event in &report.timeline {
            let categories = event.overlapping_categories.as_ref().map_or_else(
                || "unavailable".to_owned(),
                |categories| {
                    if categories.is_empty() {
                        "none measured (host activity unattributed)".to_owned()
                    } else {
                        categories.join(", ")
                    }
                },
            );
            let _ = writeln!(
                out,
                "- {}..{} ms: {}; overlapping categories: {}; overlapping agents: {}",
                event.from_unix_ms,
                event.to_unix_ms,
                event.triggers.join(", "),
                categories,
                event.overlapping_agents.as_ref().map_or_else(
                    || "unavailable".to_owned(),
                    |agents| {
                        if agents.is_empty() {
                            "none".to_owned()
                        } else {
                            agents.join(", ")
                        }
                    }
                )
            );
        }
    }
    out.push_str(&scheduler_markdown(report.scheduler.as_ref()));
    #[cfg(unix)]
    {
        out.push_str(&cache_impact::markdown(&report.cache));
    }
    out
}

pub fn run(options: &Options) -> io::Result<()> {
    let to = options.to.unwrap_or_else(Utc::now);
    let from = options.from.unwrap_or(to - Duration::hours(24));
    if from >= to {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--from must be before --to",
        ));
    }
    for value in [
        options.cpu_threshold,
        options.memory_threshold,
        options.swap_threshold,
    ] {
        if value > 100 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pressure thresholds must be between 0 and 100",
            ));
        }
    }
    let dir = state_dir(options.state_dir.as_deref())?;
    let samples = observations(&dir)?;
    let agent_samples = agent_ticks(&dir)?;
    let agent_summaries = agent_rollups(&dir)?;
    let default_paths = history::paths()?;
    let records = if default_paths.is_none() && options.records.is_empty() {
        None
    } else {
        let mut all = Vec::new();
        let mut seen = BTreeSet::new();
        for path in default_paths.iter().flatten().chain(options.records.iter()) {
            let canonical = fs::canonicalize(path)?;
            if seen.insert(canonical.clone()) {
                all.extend(read_jsonl(&canonical)?);
            }
        }
        Some(all)
    };
    let report = build_report_with_ticks(
        from,
        to,
        samples,
        AgentData {
            ticks: agent_samples,
            rollups: agent_summaries,
        },
        records,
        (
            options.cpu_threshold,
            options.memory_threshold,
            options.swap_threshold,
        ),
        {
            let dir = options.schedule_state_dir.clone().or_else(|| {
                std::env::var_os("HOME")
                    .map(|home| PathBuf::from(home).join(".local/state/reef/schedule"))
            });
            match dir {
                Some(dir) => schedule_events::read(&dir)?.map(|events| {
                    schedule_events::summary(
                        &events,
                        u64::try_from(from.timestamp_millis()).unwrap_or_default(),
                        u64::try_from(to.timestamp_millis()).unwrap_or_default(),
                    )
                }),
                None => None,
            }
        },
    );
    #[cfg(unix)]
    let report = {
        let mut report = report;
        report.cache = cache_impact::report(
            options.cache_dir.as_deref(),
            u128::try_from(from.timestamp_millis()).unwrap_or_default(),
            u128::try_from(to.timestamp_millis()).unwrap_or_default(),
        )?;
        report.empty = report.empty && report.cache.hits == 0 && report.cache.misses == 0;
        report
    };
    match options.format {
        Format::Markdown => print!("{}", markdown(&report)),
        Format::Json => println!("{}", serde_json::to_string_pretty(&report)?),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(ms: i64) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(ms).unwrap()
    }

    fn sample(at: u64, cpu: Option<f64>, swap: Option<u64>) -> Observation {
        Observation {
            at_unix_ms: at,
            working_ms: 100,
            cpu_percent: cpu,
            memory_used_bytes: Some(900),
            memory_total_bytes: Some(1000),
            swap_used_bytes: swap,
            swap_total_bytes: Some(1000),
            agents: None,
        }
    }

    #[test]
    fn passive_agent_load_is_reported_without_managed_command_history() {
        let mut observation = sample(200, Some(95.0), Some(10));
        observation.agents = Some(vec![AgentSample {
            kind: "codex".to_owned(),
            root_pid: 42,
            process_count: 3,
            cpu_percent: Some(150.0),
            memory_bytes: 500,
            read_bytes: Some(100),
            written_bytes: Some(50),
        }]);
        let report = build_report(
            date(100),
            date(300),
            vec![observation],
            None,
            (90, 90, 1),
            None,
        );
        let agent = &report.agents.as_ref().unwrap()[0];
        assert_eq!(agent.kind, "codex");
        assert_eq!(agent.active_samples, 1);
        assert_eq!(agent.cpu_core_ms_estimate, 150.0);
        assert_eq!(agent.peak_memory_bytes, 500);
        assert_eq!(agent.peak_roots, 1);
        assert_eq!(report.timeline[0].attribution, "agent_process_overlap");
        assert_eq!(report.timeline[0].overlapping_agents, Some(vec!["codex"]));
    }

    fn command(category: &str, start: u128, end: u128, cpu: u128) -> Measurement {
        Measurement {
            category: category.to_owned(),
            status: "success".to_owned(),
            started_at_unix_ms: start,
            ended_at_unix_ms: end,
            wall_ms: end - start,
            tree_cpu_ms: cpu,
            tree_peak_memory_bytes: 123,
        }
    }

    #[test]
    fn groups_overlapping_commands_and_counts_pressure_without_sleep() {
        let report = build_report(
            date(100),
            date(400),
            vec![
                sample(200, Some(95.0), Some(10)),
                sample(300, Some(10.0), Some(20)),
            ],
            Some(vec![
                command("build", 150, 250, 80),
                command("lint", 175, 275, 20),
                command("test", 400, 500, 10),
            ]),
            (90, 90, 1),
            None,
        );
        assert_eq!(report.command_count, Some(2));
        assert_eq!(report.peak_command_concurrency, Some(2));
        assert_eq!(report.observed_working_ms, 200);
        assert_eq!(report.swap_growth_bytes, Some(10));
        assert_eq!(report.pressure.cpu_above_ms, Some(100));
        assert_eq!(report.pressure.memory_above_ms, Some(200));
        assert_eq!(report.timeline.len(), 2);
        assert_eq!(
            report.timeline[0].overlapping_categories,
            Some(vec!["build", "lint"])
        );
        let categories = report.categories.unwrap();
        assert_eq!(categories.len(), 2);
        assert_eq!(categories[0].wall_percent_of_measured, 50.0);
        assert_eq!(categories[0].cpu_percent_of_measured, 80.0);
    }

    #[test]
    fn empty_and_unavailable_are_distinct_from_zero() {
        let report = build_report(date(100), date(200), vec![], None, (90, 90, 1), None);
        assert!(report.empty);
        assert!(!report.command_measurements_available);
        assert!(report.categories.is_none());
        assert!(report.pressure.any_above_ms.is_none());
        assert!(markdown(&report).contains("Host activity is uninstrumented or unknown"));

        let measured = build_report(
            date(100),
            date(200),
            vec![sample(150, Some(0.0), None)],
            Some(vec![]),
            (90, 90, 1),
            None,
        );
        assert!(!measured.empty);
        assert_eq!(measured.command_count, Some(0));
        assert_eq!(measured.pressure.cpu_above_ms, Some(0));
        assert!(measured.pressure.swap_above_ms.is_none());
    }

    #[test]
    fn simultaneous_end_and_start_do_not_overlap() {
        let records = [command("build", 100, 200, 1), command("lint", 200, 300, 1)];
        assert_eq!(peak_concurrency(&records, 100, 300), 1);
    }

    #[test]
    fn pressure_without_managed_command_overlap_is_unattributed() {
        let report = build_report(
            date(100),
            date(300),
            vec![sample(250, Some(95.0), Some(10))],
            Some(vec![command("build", 0, 100, 1)]),
            (90, 90, 1),
            None,
        );
        assert_eq!(report.timeline.len(), 1);
        assert_eq!(
            report.timeline[0].attribution,
            "no_measured_command_overlap"
        );
        assert_eq!(report.timeline[0].overlapping_categories, Some(vec![]));
        assert!(markdown(&report).contains("host activity unattributed"));
    }

    #[test]
    fn range_excludes_commands_ending_at_start_and_clips_samples_ending_after_end() {
        let report = build_report(
            date(100),
            date(200),
            vec![sample(250, Some(95.0), None)],
            Some(vec![command("build", 0, 100, 1)]),
            (90, 90, 1),
            None,
        );
        assert_eq!(report.command_count, Some(0));
        assert_eq!(report.observation_count, 1);
        assert_eq!(report.observed_working_ms, 50);
        assert_eq!(report.pressure.cpu_above_ms, Some(50));
    }

    #[test]
    fn any_pressure_is_unavailable_when_no_known_metric_triggers_and_one_is_missing() {
        let report = build_report(
            date(100),
            date(200),
            vec![Observation {
                at_unix_ms: 200,
                working_ms: 100,
                cpu_percent: Some(10.0),
                memory_used_bytes: None,
                memory_total_bytes: None,
                swap_used_bytes: None,
                swap_total_bytes: None,
                agents: None,
            }],
            None,
            (90, 90, 1),
            None,
        );
        assert_eq!(report.pressure.cpu_above_ms, Some(0));
        assert!(report.pressure.any_above_ms.is_none());
    }

    #[test]
    fn overlapping_samples_count_each_millisecond_once() {
        let report = build_report(
            date(0),
            date(1_500),
            vec![
                sample(1_000, Some(95.0), Some(10)),
                sample(1_500, Some(95.0), Some(10)),
            ]
            .into_iter()
            .map(|mut sample| {
                sample.working_ms = 1_000;
                sample
            })
            .collect(),
            None,
            (90, 90, 1),
            None,
        );
        assert_eq!(report.observed_working_ms, 1_500);
        assert_eq!(report.pressure.cpu_above_ms, Some(1_500));
        assert_eq!(report.pressure.any_above_ms, Some(1_500));
        assert_eq!(report.timeline[1].from_unix_ms, 1_000);
    }

    #[test]
    fn missing_metric_in_any_covered_interval_marks_total_unavailable() {
        let mut missing = sample(300, Some(10.0), None);
        missing.memory_used_bytes = None;
        missing.memory_total_bytes = None;
        let report = build_report(
            date(100),
            date(400),
            vec![sample(200, Some(95.0), Some(10)), missing],
            None,
            (90, 90, 1),
            None,
        );
        assert_eq!(report.pressure.cpu_above_ms, Some(100));
        assert!(report.pressure.memory_above_ms.is_none());
        assert!(report.pressure.any_above_ms.is_none());
    }

    #[test]
    fn unknown_category_is_not_echoed_into_report() {
        let report = build_report(
            date(100),
            date(300),
            vec![],
            Some(vec![command("secret-command-arguments", 100, 200, 1)]),
            (90, 90, 1),
            None,
        );
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("secret-command-arguments"));
        assert_eq!(report.categories.unwrap()[0].category, "other");
    }

    #[test]
    fn short_agent_session_is_reported_without_host_sample() {
        let report = build_report_with_ticks(
            date(1_000),
            date(5_000),
            vec![],
            AgentData {
                ticks: vec![AgentTick {
                    at_unix_ms: 3_000,
                    working_ms: 1_000,
                    agents: vec![AgentSample {
                        kind: "codex".to_owned(),
                        root_pid: 42,
                        process_count: 1,
                        cpu_percent: Some(50.0),
                        memory_bytes: 400,
                        read_bytes: Some(10),
                        written_bytes: Some(20),
                    }],
                }],
                rollups: vec![],
            },
            None,
            (90, 90, 1),
            None,
        );

        assert!(!report.empty);
        assert_eq!(report.observation_count, 0);
        assert_eq!(report.agent_observation_count, 1);
        let agent = &report.agents.unwrap()[0];
        assert_eq!(agent.kind, "codex");
        assert_eq!(agent.cpu_core_ms_estimate, 500.0);
        assert_eq!(agent.peak_memory_bytes, 400);
    }

    #[test]
    fn partial_agent_tick_is_clipped_to_report_window() {
        let report = build_report_with_ticks(
            date(0),
            date(1_000),
            vec![],
            AgentData {
                ticks: vec![AgentTick {
                    at_unix_ms: 1_500,
                    working_ms: 1_000,
                    agents: vec![AgentSample {
                        kind: "codex".to_owned(),
                        root_pid: 42,
                        process_count: 1,
                        cpu_percent: Some(50.0),
                        memory_bytes: 400,
                        read_bytes: None,
                        written_bytes: None,
                    }],
                }],
                rollups: vec![],
            },
            None,
            (90, 90, 1),
            None,
        );

        let agent = &report.agents.unwrap()[0];
        assert_eq!(agent.observed_working_ms, 500);
        assert_eq!(agent.cpu_core_ms_estimate, 250.0);
    }

    #[test]
    fn folded_agent_load_does_not_double_count_raw_samples() {
        let report = build_report_with_ticks(
            date(60_000),
            date(120_000),
            vec![],
            AgentData {
                ticks: vec![AgentTick {
                    at_unix_ms: 61_000,
                    working_ms: 1_000,
                    agents: vec![AgentSample {
                        kind: "claude".to_owned(),
                        root_pid: 42,
                        process_count: 1,
                        cpu_percent: Some(50.0),
                        memory_bytes: 400,
                        read_bytes: None,
                        written_bytes: None,
                    }],
                }],
                rollups: vec![AgentRollup {
                    minute_unix_ms: 60_000,
                    kind: "claude".to_owned(),
                    active_samples: 2,
                    observed_working_ms: 2_000,
                    cpu_core_ms_estimate: 1_000.0,
                    peak_cpu_percent: 50.0,
                    peak_memory_bytes: 500,
                    read_bytes: 20,
                    written_bytes: 40,
                    peak_processes: 2,
                    peak_roots: 1,
                }],
            },
            None,
            (90, 90, 1),
            None,
        );

        assert_eq!(report.agent_observation_count, 0);
        assert_eq!(report.agent_rollup_count, 1);
        let agent = &report.agents.unwrap()[0];
        assert_eq!(agent.active_samples, 2);
        assert_eq!(agent.cpu_core_ms_estimate, 1_000.0);
        assert_eq!(agent.peak_memory_bytes, 500);
    }
}
