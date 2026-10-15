#[cfg(unix)]
use crate::cache_impact;
#[cfg(unix)]
use crate::control;
use crate::history;
use crate::process_history;
use crate::retention;
use crate::schedule_events;
use chrono::{DateTime, Duration, Utc};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration as StdDuration, SystemTime, UNIX_EPOCH};

mod compare;
mod coverage;
mod html;
pub use compare::{CompareOptions, run_compare};

#[derive(Debug, clap::Args)]
pub struct Options {
    /// Start of the range, inclusive, in RFC 3339 format. Defaults to oldest retained data.
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
    /// Private control experiment directory.
    #[cfg(unix)]
    #[arg(long)]
    control_state_dir: Option<PathBuf>,
    /// Cache directory used by `reef cache run`.
    #[cfg(unix)]
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Report format.
    #[arg(long, value_enum, default_value_t = Format::Markdown)]
    format: Format,
    /// Write a self-contained HTML report, replacing an existing regular file.
    #[arg(long)]
    output: Option<PathBuf>,
    /// Open an HTML report in the default browser; requires --output.
    #[arg(long, requires = "output")]
    open: bool,
    /// Include individual pressure samples in the report.
    #[arg(long)]
    timeline: bool,
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
    Html,
}

fn parse_date(value: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(value)
        .map(|date| date.with_timezone(&Utc))
        .map_err(|error| format!("expected an RFC 3339 date: {error}"))
}

#[derive(Debug, Deserialize)]
pub(crate) struct Observation {
    pub(crate) at_unix_ms: u64,
    pub(crate) working_ms: u64,
    #[serde(default)]
    pub(crate) resolution_ms: u64,
    #[serde(default)]
    source_samples: u64,
    pub(crate) cpu_percent: Option<f64>,
    #[serde(default)]
    peak_cpu_percent: Option<f64>,
    pub(crate) memory_used_bytes: Option<u64>,
    #[serde(default)]
    peak_memory_used_bytes: Option<u64>,
    pub(crate) memory_total_bytes: Option<u64>,
    swap_used_bytes: Option<u64>,
    swap_total_bytes: Option<u64>,
    root_disk_available_bytes: Option<u64>,
    #[serde(default)]
    minimum_root_disk_available_bytes: Option<u64>,
    root_disk_total_bytes: Option<u64>,
    pub(crate) agents: Option<Vec<AgentSample>>,
    memory_consumers: Option<Vec<MemoryConsumerSample>>,
    #[serde(skip)]
    source_pid: Option<u32>,
}

impl Observation {
    fn from_rollup(rollup: retention::HostSummary) -> Self {
        let end_unix_ms = rollup.end_unix_ms();
        let memory_consumers = rollup
            .memory_consumers
            .into_iter()
            .map(|(family, summary)| MemoryConsumerSample {
                family,
                process_count: usize::try_from(summary.process_count.peak().unwrap_or_default())
                    .unwrap_or(usize::MAX),
                rss_bytes: summary.rss_bytes.mean().unwrap_or_default(),
                observed_working_ms: Some(summary.rss_bytes.weight_ms()),
                peak_rss_bytes: summary.rss_bytes.peak(),
                largest_process_rss_bytes: summary.peak_largest_process_rss_bytes,
            })
            .collect();
        Self {
            at_unix_ms: end_unix_ms,
            working_ms: rollup.observed_working_ms,
            resolution_ms: rollup.resolution_ms,
            source_samples: rollup.source_samples,
            cpu_percent: rollup.cpu_percent.mean(),
            peak_cpu_percent: rollup.cpu_percent.peak(),
            memory_used_bytes: rollup.memory_used_bytes.mean(),
            peak_memory_used_bytes: rollup.memory_used_bytes.peak(),
            memory_total_bytes: rollup.memory_total_bytes.mean(),
            swap_used_bytes: rollup.swap_used_bytes.mean(),
            swap_total_bytes: rollup.swap_total_bytes.mean(),
            root_disk_available_bytes: rollup.root_disk_available_bytes.mean(),
            minimum_root_disk_available_bytes: rollup.root_disk_available_bytes.minimum(),
            root_disk_total_bytes: rollup.root_disk_total_bytes.mean(),
            agents: None,
            memory_consumers: Some(memory_consumers),
            source_pid: None,
        }
    }

    pub(crate) fn interval_start_unix_ms(&self) -> u64 {
        self.at_unix_ms.saturating_sub(if self.resolution_ms == 0 {
            self.working_ms
        } else {
            self.resolution_ms
        })
    }

    fn source_samples(&self) -> u64 {
        self.source_samples.max(1)
    }
}

#[derive(Debug, Deserialize)]
struct MemoryConsumerSample {
    family: String,
    process_count: usize,
    rss_bytes: u64,
    #[serde(default)]
    observed_working_ms: Option<u64>,
    #[serde(default)]
    peak_rss_bytes: Option<u64>,
    largest_process_rss_bytes: u64,
}

fn safe_memory_family(value: &str) -> &'static str {
    match value {
        "agent runtime" => "agent runtime",
        "compiler/build" => "compiler/build",
        "linter" => "linter",
        "browser" => "browser",
        "editor/IDE" => "editor/IDE",
        "container/VM" => "container/VM",
        "system UI" => "system UI",
        _ => "other processes",
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct AgentSample {
    pub(crate) kind: String,
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
    commands: Option<Vec<PassiveCommandSample>>,
    #[serde(skip)]
    source_suffix: String,
}

#[derive(Debug, Deserialize)]
struct PassiveCommandSample {
    kind: String,
    family: String,
    category: String,
    process_count: usize,
    cpu_percent: Option<f64>,
    memory_bytes: u64,
    read_bytes: Option<u64>,
    written_bytes: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct PassiveCommandRollup {
    category: String,
    active_samples: usize,
    observed_working_ms: u128,
    cpu_core_ms_estimate: f64,
    peak_cpu_percent: f64,
    peak_memory_bytes: u64,
    read_bytes: u64,
    written_bytes: u64,
    peak_processes: usize,
}

#[derive(Debug, Deserialize)]
struct AgentRollup {
    minute_unix_ms: u64,
    #[serde(default = "minute_resolution")]
    resolution_ms: u64,
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
    commands: Option<BTreeMap<String, PassiveCommandRollup>>,
    #[serde(default)]
    covered_minutes: BTreeSet<u16>,
    #[serde(skip)]
    source_suffix: String,
}

fn minute_resolution() -> u64 {
    60_000
}

#[derive(Default)]
struct AgentData {
    ticks: Vec<AgentTick>,
    rollups: Vec<AgentRollup>,
}

#[derive(Debug, Deserialize)]
struct Measurement {
    category: String,
    agent_kind: Option<String>,
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
    observation_resolutions: BTreeMap<String, u64>,
    coarse_history_approximate: bool,
    retention_status: Option<retention::RetentionStatus>,
    collection_coverage: coverage::CollectionCoverage,
    host_observations_with_agents: usize,
    agent_observation_count: usize,
    agent_rollup_count: usize,
    observed_working_ms: u128,
    command_measurements_available: bool,
    command_count: Option<usize>,
    categories: Option<Vec<CategoryReport>>,
    agent_commands: Option<Vec<AgentCategoryReport>>,
    passive_commands: Option<Vec<PassiveCommandLoad>>,
    process_commands: Option<Vec<ProcessCommandLoad>>,
    peak_host_cpu_percent: Option<f64>,
    peak_host_memory_bytes: Option<u64>,
    peak_root_disk_used_percent: Option<f64>,
    swap_growth_bytes: Option<i128>,
    memory_consumers: Option<Vec<MemoryConsumerLoad>>,
    peak_command_concurrency: Option<usize>,
    pressure: PressureReport,
    pressure_overlap: PressureOverlap,
    #[serde(skip_serializing)]
    timeline: Vec<PressureEvent>,
    agents: Option<Vec<AgentLoad>>,
    agent_comparison: Option<Vec<AgentComparison>>,
    agent_activity_comparison: Option<Vec<AgentActivityComparison>>,
    scheduler: Option<schedule_events::Summary>,
    #[cfg(unix)]
    control: control::Summary,
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

#[derive(Serialize)]
struct AgentCategoryReport {
    agent: &'static str,
    category: &'static str,
    count: usize,
    wall_ms: u128,
    cpu_ms: u128,
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

#[derive(Serialize)]
struct AgentComparison {
    agent: String,
    observed_active_hours: f64,
    average_cpu_cores_while_active: Option<f64>,
    peak_memory_bytes: u64,
    read_mib_per_active_hour: Option<f64>,
    written_mib_per_active_hour: Option<f64>,
    top_command_family: Option<String>,
    /// Shares of sampled descendant-command CPU, not shares of total agent CPU.
    activity_cpu_percent: Option<BTreeMap<String, f64>>,
}

#[derive(Serialize)]
struct AgentActivityComparison {
    agent: String,
    category: String,
    sampled_family_active_hours: f64,
    average_cpu_cores_per_family_active_hour: Option<f64>,
    peak_memory_bytes: u64,
    read_mib_per_family_active_hour: Option<f64>,
    written_mib_per_family_active_hour: Option<f64>,
}

#[derive(Serialize)]
struct MemoryConsumerLoad {
    family: String,
    samples: usize,
    mean_rss_bytes: u64,
    peak_rss_bytes: u64,
    peak_largest_process_rss_bytes: u64,
    peak_process_count: usize,
}

#[derive(Default, Serialize)]
struct PassiveCommandLoad {
    agent: String,
    family: String,
    category: String,
    active_samples: usize,
    observed_working_ms: u128,
    cpu_core_ms_estimate: f64,
    peak_cpu_percent: f64,
    peak_memory_bytes: u64,
    read_bytes: u64,
    written_bytes: u64,
    peak_processes: usize,
}

#[derive(Default, Serialize)]
struct ProcessCommandLoad {
    agent: String,
    workspace_kind: String,
    family: String,
    executable: String,
    subcommand: String,
    process_count: f64,
    execution_count_estimated: bool,
    observed_working_ms: u128,
    cpu_core_ms_estimate: f64,
    peak_cpu_percent: f64,
    peak_memory_bytes: u64,
    read_bytes: u64,
    written_bytes: u64,
    detail_coverage: String,
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

#[derive(Default, Serialize)]
struct PressureOverlap {
    measured_command_ms: Option<u128>,
    no_measured_command_ms: Option<u128>,
    by_category: Option<Vec<OverlapDuration>>,
    sampled_agent_coverage_ms: Option<u128>,
    by_agent: Option<Vec<OverlapDuration>>,
}

#[derive(Serialize)]
struct OverlapDuration {
    name: &'static str,
    overlap_ms: u128,
}

#[derive(Serialize)]
struct ReportWithTimeline<'a> {
    #[serde(flatten)]
    report: &'a Report,
    timeline: &'a [PressureEvent],
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

fn measured_agent_kind(value: Option<&str>) -> Option<&'static str> {
    match value? {
        "codex" => Some("codex"),
        "claude" => Some("claude"),
        "opencode" => Some("opencode"),
        _ => None,
    }
}

pub(crate) fn state_dir(override_dir: Option<&Path>) -> io::Result<PathBuf> {
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

pub(crate) fn observations(dir: &Path) -> io::Result<Vec<Observation>> {
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
        let source_pid = segment_pid(&segment_suffix(&path, "samples-")?);
        for mut sample in read_jsonl::<Observation>(&path)? {
            sample.source_pid = source_pid;
            samples.push(sample);
        }
    }
    samples.sort_by_key(|sample| sample.at_unix_ms);
    samples.dedup_by_key(|sample| sample.at_unix_ms);
    samples.extend(
        retention::host_rollups(dir)?
            .into_iter()
            .map(Observation::from_rollup),
    );
    samples.sort_by_key(|sample| (sample.at_unix_ms, sample.resolution_ms));
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
        let suffix = segment_suffix(&path, "agent-samples-")?;
        for mut tick in read_jsonl::<AgentTick>(&path)? {
            tick.source_suffix.clone_from(&suffix);
            ticks.push(tick);
        }
    }
    ticks.sort_by_key(|tick: &AgentTick| tick.at_unix_ms);
    ticks.dedup_by_key(|tick| tick.at_unix_ms);
    Ok(ticks)
}

fn agent_rollups(dir: &Path) -> io::Result<Vec<AgentRollup>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut paths = retention::agent_rollup_paths(dir)?;
    paths.sort();
    let mut rollups = Vec::new();
    for path in paths {
        let (prefix, expected) = match path.file_name().and_then(|name| name.to_str()) {
            Some(name) if name.starts_with("agent-rollups-") => ("agent-rollups-", 60_000),
            Some(name) if name.starts_with("agent-history-5m-") => {
                ("agent-history-5m-", retention::FIVE_MINUTES_MS)
            }
            Some(name) if name.starts_with("agent-history-1h-") => {
                ("agent-history-1h-", retention::ONE_HOUR_MS)
            }
            Some(name) if name.starts_with("agent-history-1d-") => {
                ("agent-history-1d-", retention::ONE_DAY_MS)
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unknown agent rollup",
                ));
            }
        };
        let suffix = segment_suffix(&path, prefix)?;
        for mut rollup in read_jsonl::<AgentRollup>(&path)? {
            if rollup.resolution_ms != expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid agent rollup resolution: {}", path.display()),
                ));
            }
            rollup.source_suffix.clone_from(&suffix);
            rollups.push(rollup);
        }
    }
    Ok(rollups)
}

fn segment_suffix(path: &Path, prefix: &str) -> io::Result<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(prefix))
        .map(str::to_owned)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid agent segment name"))
}

fn segment_pid(suffix: &str) -> Option<u32> {
    suffix
        .strip_suffix(".jsonl")?
        .rsplit_once('-')?
        .1
        .parse()
        .ok()
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

fn passive_family(value: &str) -> &'static str {
    match value {
        "go build" => "go build",
        "go test" => "go test",
        "go vet" => "go vet",
        "go other" => "go other",
        "cargo build" => "cargo build",
        "cargo test" => "cargo test",
        "cargo clippy" => "cargo clippy",
        "cargo other" => "cargo other",
        "mise build" => "mise build",
        "mise test" => "mise test",
        "mise lint/check" => "mise lint/check",
        "mise other" => "mise other",
        "make build" => "make build",
        "make test" => "make test",
        "make lint/check" => "make lint/check",
        "make other" => "make other",
        "package build" => "package build",
        "package test" => "package test",
        "package lint/check" => "package lint/check",
        "package other" => "package other",
        "golangci-lint" => "golangci-lint",
        "compiler/linker" => "compiler/linker",
        "test runner" => "test runner",
        "Go test binary" => "Go test binary",
        "linter" => "linter",
        "git" => "git",
        "container tool" => "container tool",
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
    if sample.resolution_ms > 0 {
        return [None; 3];
    }
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

fn add_pressure_sample(
    pressure: &mut PressureReport,
    missing: &mut [bool; 4],
    sample: &Observation,
    thresholds: (u8, u8, u8),
    duration: u128,
) -> Vec<&'static str> {
    let [cpu, memory, swap] = pressure_flags(sample, thresholds);
    add_duration(&mut pressure.cpu_above_ms, &mut missing[0], cpu, duration);
    add_duration(
        &mut pressure.memory_above_ms,
        &mut missing[1],
        memory,
        duration,
    );
    add_duration(&mut pressure.swap_above_ms, &mut missing[2], swap, duration);
    add_duration(
        &mut pressure.any_above_ms,
        &mut missing[3],
        any_pressure([cpu, memory, swap]),
        duration,
    );
    pressure_triggers([cpu, memory, swap])
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

fn agent_command_reports(records: &[Measurement]) -> Vec<AgentCategoryReport> {
    let mut groups: BTreeMap<(&'static str, &'static str), AgentCategoryReport> = BTreeMap::new();
    for record in records {
        let Some(agent) = measured_agent_kind(record.agent_kind.as_deref()) else {
            continue;
        };
        let category = category(&record.category);
        let group = groups
            .entry((agent, category))
            .or_insert(AgentCategoryReport {
                agent,
                category,
                count: 0,
                wall_ms: 0,
                cpu_ms: 0,
                peak_memory_bytes: 0,
            });
        group.count += 1;
        group.wall_ms += record.wall_ms;
        group.cpu_ms += record.tree_cpu_ms;
        group.peak_memory_bytes = group.peak_memory_bytes.max(record.tree_peak_memory_bytes);
    }
    let mut groups: Vec<_> = groups.into_values().collect();
    groups.sort_by(|left, right| {
        right
            .cpu_ms
            .cmp(&left.cpu_ms)
            .then_with(|| left.agent.cmp(right.agent))
            .then_with(|| left.category.cmp(right.category))
    });
    groups
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

fn covered_ms(intervals: &mut [(u128, u128)]) -> u128 {
    intervals.sort_unstable();
    let mut covered = 0_u128;
    let mut end = 0_u128;
    for &(start, next_end) in intervals.iter() {
        covered += next_end.saturating_sub(start.max(end));
        end = end.max(next_end);
    }
    covered
}

fn measured_pressure_overlap(
    records: &[Measurement],
    start: u128,
    end: u128,
) -> (u128, BTreeMap<&'static str, u128>) {
    let mut all = Vec::new();
    let mut by_category: BTreeMap<&'static str, Vec<(u128, u128)>> = BTreeMap::new();
    for record in records {
        let overlap_start = record.started_at_unix_ms.max(start);
        let overlap_end = record.ended_at_unix_ms.min(end);
        if overlap_start < overlap_end {
            let interval = (overlap_start, overlap_end);
            all.push(interval);
            by_category
                .entry(category(&record.category))
                .or_default()
                .push(interval);
        }
    }
    (
        covered_ms(&mut all),
        by_category
            .into_iter()
            .map(|(name, mut intervals)| (name, covered_ms(&mut intervals)))
            .collect(),
    )
}

fn overlap_durations(groups: BTreeMap<&'static str, u128>) -> Vec<OverlapDuration> {
    let mut durations: Vec<_> = groups
        .into_iter()
        .map(|(name, overlap_ms)| OverlapDuration { name, overlap_ms })
        .collect();
    durations.sort_by(|left, right| {
        right
            .overlap_ms
            .cmp(&left.overlap_ms)
            .then_with(|| left.name.cmp(right.name))
    });
    durations
}

struct PressureOverlapAccumulator {
    report: PressureOverlap,
    categories: BTreeMap<&'static str, u128>,
    agents: BTreeMap<&'static str, u128>,
}

impl PressureOverlapAccumulator {
    fn new(records_available: bool, agent_samples_available: bool) -> Self {
        Self {
            report: PressureOverlap {
                measured_command_ms: records_available.then_some(0),
                no_measured_command_ms: records_available.then_some(0),
                sampled_agent_coverage_ms: agent_samples_available.then_some(0),
                ..PressureOverlap::default()
            },
            categories: BTreeMap::new(),
            agents: BTreeMap::new(),
        }
    }

    fn add(
        &mut self,
        records: Option<&[Measurement]>,
        agents: Option<&[AgentSample]>,
        start: u128,
        end: u128,
    ) {
        let duration = end - start;
        if let Some(records) = records {
            let (measured, categories) = measured_pressure_overlap(records, start, end);
            if let Some(total) = &mut self.report.measured_command_ms {
                *total += measured;
            }
            if let Some(total) = &mut self.report.no_measured_command_ms {
                *total += duration - measured;
            }
            for (name, overlap_ms) in categories {
                *self.categories.entry(name).or_insert(0) += overlap_ms;
            }
        }
        if let Some(agents) = agents {
            if let Some(coverage) = &mut self.report.sampled_agent_coverage_ms {
                *coverage += duration;
            }
            for name in agents
                .iter()
                .map(|agent| agent_kind(&agent.kind))
                .collect::<BTreeSet<_>>()
            {
                *self.agents.entry(name).or_insert(0) += duration;
            }
        }
    }

    fn finish(mut self) -> PressureOverlap {
        if self.report.measured_command_ms.is_some() {
            self.report.by_category = Some(overlap_durations(self.categories));
        }
        if self.report.sampled_agent_coverage_ms.is_some() {
            self.report.by_agent = Some(overlap_durations(self.agents));
        }
        self.report
    }
}

fn accumulate_agents(
    load: &mut BTreeMap<String, AgentLoad>,
    agents: &[AgentSample],
    duration: u128,
) {
    let agents: Vec<_> = agents.iter().collect();
    accumulate_agent_refs(load, &agents, duration);
}

fn accumulate_agent_refs(
    load: &mut BTreeMap<String, AgentLoad>,
    agents: &[&AgentSample],
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

fn rollup_fraction(rollup: &AgentRollup, from_ms: u128, to_ms: u128) -> (u128, f64) {
    let start = u128::from(rollup.minute_unix_ms);
    let end = start.saturating_add(u128::from(rollup.resolution_ms));
    let overlap = end.min(to_ms).saturating_sub(start.max(from_ms));
    let fraction = f64::from(u32::try_from(overlap).unwrap_or(u32::MAX))
        / f64::from(u32::try_from(rollup.resolution_ms).unwrap_or(u32::MAX));
    (overlap, fraction)
}

fn scaled_u64(value: u64, overlap_ms: u128, resolution_ms: u64) -> u64 {
    u64::try_from(u128::from(value).saturating_mul(overlap_ms) / u128::from(resolution_ms.max(1)))
        .unwrap_or(u64::MAX)
}

fn scaled_count(value: f64, overlap_ms: u128, resolution_ms: u64) -> f64 {
    if value == 0.0 || overlap_ms == 0 {
        return 0.0;
    }
    value * duration_fraction(overlap_ms, u128::from(resolution_ms))
}

fn duration_fraction(numerator_ms: u128, denominator_ms: u128) -> f64 {
    let numerator = StdDuration::from_millis(u64::try_from(numerator_ms).unwrap_or(u64::MAX));
    let denominator =
        StdDuration::from_millis(u64::try_from(denominator_ms.max(1)).unwrap_or(u64::MAX));
    numerator.as_secs_f64() / denominator.as_secs_f64()
}

fn accumulate_agent_rollups(
    load: &mut BTreeMap<String, AgentLoad>,
    rollups: &[AgentRollup],
    from_ms: u128,
    to_ms: u128,
) {
    for rollup in rollups {
        let (overlap, fraction) = rollup_fraction(rollup, from_ms, to_ms);
        let kind = agent_kind(&rollup.kind).to_owned();
        let group = load.entry(kind.clone()).or_insert_with(|| AgentLoad {
            kind,
            ..AgentLoad::default()
        });
        group.active_samples += usize::try_from(scaled_u64(
            u64::try_from(rollup.active_samples).unwrap_or(u64::MAX),
            overlap,
            rollup.resolution_ms,
        ))
        .unwrap_or(usize::MAX);
        group.observed_working_ms += rollup.observed_working_ms.saturating_mul(overlap)
            / u128::from(rollup.resolution_ms.max(1));
        group.cpu_core_ms_estimate += rollup.cpu_core_ms_estimate * fraction;
        group.peak_cpu_percent = group.peak_cpu_percent.max(rollup.peak_cpu_percent);
        group.peak_memory_bytes = group.peak_memory_bytes.max(rollup.peak_memory_bytes);
        group.read_bytes += scaled_u64(rollup.read_bytes, overlap, rollup.resolution_ms);
        group.written_bytes += scaled_u64(rollup.written_bytes, overlap, rollup.resolution_ms);
        group.peak_processes = group.peak_processes.max(rollup.peak_processes);
        group.peak_roots = group.peak_roots.max(rollup.peak_roots);
    }
}

fn passive_group<'a>(
    load: &'a mut BTreeMap<(String, String), PassiveCommandLoad>,
    kind: &str,
    family: &str,
    command_category: &str,
) -> Option<&'a mut PassiveCommandLoad> {
    let agent = measured_agent_kind(Some(kind))?;
    let family = passive_family(family);
    Some(
        load.entry((agent.to_owned(), family.to_owned()))
            .or_insert_with(|| PassiveCommandLoad {
                agent: agent.to_owned(),
                family: family.to_owned(),
                category: category(command_category).to_owned(),
                ..PassiveCommandLoad::default()
            }),
    )
}

fn accumulate_passive_ticks(
    load: &mut BTreeMap<(String, String), PassiveCommandLoad>,
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
        if duration == 0 {
            continue;
        }
        for command in tick.commands.as_deref().unwrap_or_default() {
            let Some(group) =
                passive_group(load, &command.kind, &command.family, &command.category)
            else {
                continue;
            };
            group.active_samples += 1;
            group.observed_working_ms += duration;
            let duration = u32::try_from(duration).unwrap_or(u32::MAX);
            group.cpu_core_ms_estimate +=
                command.cpu_percent.unwrap_or_default().max(0.0) * f64::from(duration) / 100.0;
            group.peak_cpu_percent = group
                .peak_cpu_percent
                .max(command.cpu_percent.unwrap_or_default());
            group.peak_memory_bytes = group.peak_memory_bytes.max(command.memory_bytes);
            group.read_bytes += command.read_bytes.unwrap_or_default();
            group.written_bytes += command.written_bytes.unwrap_or_default();
            group.peak_processes = group.peak_processes.max(command.process_count);
        }
    }
}

fn accumulate_passive_rollups(
    load: &mut BTreeMap<(String, String), PassiveCommandLoad>,
    rollups: &[AgentRollup],
    from_ms: u128,
    to_ms: u128,
) {
    for rollup in rollups {
        let (overlap, fraction) = rollup_fraction(rollup, from_ms, to_ms);
        for (family, command) in rollup.commands.iter().flatten() {
            let Some(group) = passive_group(load, &rollup.kind, family, &command.category) else {
                continue;
            };
            group.active_samples += usize::try_from(scaled_u64(
                u64::try_from(command.active_samples).unwrap_or(u64::MAX),
                overlap,
                rollup.resolution_ms,
            ))
            .unwrap_or(usize::MAX);
            group.observed_working_ms += command.observed_working_ms.saturating_mul(overlap)
                / u128::from(rollup.resolution_ms.max(1));
            group.cpu_core_ms_estimate += command.cpu_core_ms_estimate * fraction;
            group.peak_cpu_percent = group.peak_cpu_percent.max(command.peak_cpu_percent);
            group.peak_memory_bytes = group.peak_memory_bytes.max(command.peak_memory_bytes);
            group.read_bytes += scaled_u64(command.read_bytes, overlap, rollup.resolution_ms);
            group.written_bytes += scaled_u64(command.written_bytes, overlap, rollup.resolution_ms);
            group.peak_processes = group.peak_processes.max(command.peak_processes);
        }
    }
}

fn passive_command_reports(
    ticks: &[AgentTick],
    rollups: &[AgentRollup],
    from_ms: u128,
    to_ms: u128,
) -> Option<Vec<PassiveCommandLoad>> {
    let available = ticks.iter().any(|tick| tick.commands.is_some())
        || rollups.iter().any(|rollup| rollup.commands.is_some());
    if !available {
        return None;
    }
    let mut load = BTreeMap::new();
    accumulate_passive_ticks(&mut load, ticks, from_ms, to_ms);
    accumulate_passive_rollups(&mut load, rollups, from_ms, to_ms);
    let mut reports: Vec<_> = load.into_values().collect();
    reports.sort_by(|left, right| {
        right
            .cpu_core_ms_estimate
            .total_cmp(&left.cpu_core_ms_estimate)
            .then_with(|| left.agent.cmp(&right.agent))
            .then_with(|| left.family.cmp(&right.family))
    });
    Some(reports)
}

fn safe_process_executable(value: &str) -> &'static str {
    match value {
        "go" => "go",
        "cargo" => "cargo",
        "mise" => "mise",
        "make" => "make",
        "npm" => "npm",
        "pnpm" => "pnpm",
        "yarn" => "yarn",
        "bun" => "bun",
        "golangci-lint" => "golangci-lint",
        "rustc" => "rustc",
        "compile" => "compile",
        "link" => "link",
        "asm" => "asm",
        "cgo" => "cgo",
        "cc" => "cc",
        "gcc" => "gcc",
        "g++" => "g++",
        "clang" => "clang",
        "clang++" => "clang++",
        "swiftc" => "swiftc",
        "pytest" => "pytest",
        "jest" => "jest",
        "vitest" => "vitest",
        "ruff" => "ruff",
        "eslint" => "eslint",
        "staticcheck" => "staticcheck",
        "clippy-driver" => "clippy-driver",
        "git" => "git",
        "docker" => "docker",
        "podman" => "podman",
        "kubectl" => "kubectl",
        _ => "other",
    }
}

fn safe_process_subcommand(value: &str) -> &'static str {
    match value {
        "build" => "build",
        "test" => "test",
        "vet" => "vet",
        "generate" => "generate",
        "list" => "list",
        "check" => "check",
        "clippy" => "clippy",
        "run" => "run",
        "bench" => "bench",
        "doc" => "doc",
        "lint" => "lint",
        "status" => "status",
        "diff" => "diff",
        "log" => "log",
        "show" => "show",
        "fetch" => "fetch",
        "pull" => "pull",
        "push" => "push",
        "commit" => "commit",
        "checkout" => "checkout",
        "switch" => "switch",
        "worktree" => "worktree",
        "merge" => "merge",
        "rebase" => "rebase",
        "exec" => "exec",
        "apply" => "apply",
        "get" => "get",
        "logs" => "logs",
        "none" => "none",
        _ => "other",
    }
}

fn safe_workspace_kind(value: &str) -> &'static str {
    match value {
        "checkout" => "checkout",
        "worktree" => "worktree",
        "outside repository" => "outside repository",
        _ => "unknown",
    }
}

type ProcessGroups = BTreeMap<(String, String, String, String, String), ProcessCommandLoad>;

fn process_group<'a>(
    groups: &'a mut ProcessGroups,
    agent: &str,
    workspace_kind: &str,
    family: &str,
    executable: &str,
    subcommand: &str,
) -> &'a mut ProcessCommandLoad {
    let agent = agent_kind(agent).to_owned();
    let workspace_kind = safe_workspace_kind(workspace_kind).to_owned();
    let family = passive_family(family).to_owned();
    let executable = safe_process_executable(executable).to_owned();
    let subcommand = safe_process_subcommand(subcommand).to_owned();
    groups
        .entry((
            agent.clone(),
            workspace_kind.clone(),
            family.clone(),
            executable.clone(),
            subcommand.clone(),
        ))
        .or_insert_with(|| ProcessCommandLoad {
            agent,
            workspace_kind,
            family,
            executable,
            subcommand,
            ..ProcessCommandLoad::default()
        })
}

fn accumulate_process_summary(
    groups: &mut ProcessGroups,
    summary: &process_history::ProcessSummary,
    from_ms: u128,
    to_ms: u128,
) {
    let start = u128::from(summary.bucket_start_unix_ms);
    let resolution = u128::from(summary.resolution_ms);
    let overlap = start
        .saturating_add(resolution)
        .min(to_ms)
        .saturating_sub(start.max(from_ms));
    if overlap == 0 {
        return;
    }
    let group = process_group(
        groups,
        &summary.agent,
        &summary.workspace_kind,
        &summary.family,
        &summary.executable,
        &summary.subcommand,
    );
    group.process_count += scaled_count(summary.process_count, overlap, summary.resolution_ms);
    group.execution_count_estimated |= summary.process_count > 0.0;
    group.observed_working_ms = group.observed_working_ms.saturating_add(
        u128::from(summary.observed_working_ms).saturating_mul(overlap) / resolution.max(1),
    );
    let fraction = duration_fraction(overlap, resolution);
    group.cpu_core_ms_estimate += summary.cpu_core_ms_estimate * fraction;
    group.peak_cpu_percent = group
        .peak_cpu_percent
        .max(f64::from(summary.peak_cpu_percent));
    group.peak_memory_bytes = group.peak_memory_bytes.max(summary.peak_memory_bytes);
    group.read_bytes = group.read_bytes.saturating_add(scaled_u64(
        summary.read_bytes,
        overlap,
        summary.resolution_ms,
    ));
    group.written_bytes = group.written_bytes.saturating_add(scaled_u64(
        summary.written_bytes,
        overlap,
        summary.resolution_ms,
    ));
    group.detail_coverage = if group.detail_coverage == "individual sampled spans" {
        "mixed detail and compacted summaries".to_owned()
    } else {
        "compacted summaries".to_owned()
    };
}

fn process_command_reports(
    spans: &[process_history::ProcessSpan],
    summaries: &[process_history::ProcessSummary],
    from_ms: u128,
    to_ms: u128,
) -> Option<Vec<ProcessCommandLoad>> {
    if spans.is_empty() && summaries.is_empty() {
        return None;
    }
    let mut groups = BTreeMap::new();
    for span in spans {
        let start = u128::from(span.first_seen_unix_ms);
        let end = u128::from(span.last_seen_unix_ms);
        let overlap = end.min(to_ms).saturating_sub(start.max(from_ms));
        if overlap == 0 {
            continue;
        }
        let duration = end.saturating_sub(start).max(1);
        let fraction = duration_fraction(overlap, duration);
        let group = process_group(
            &mut groups,
            &span.agent,
            &span.workspace_kind,
            &span.family,
            &span.executable,
            &span.subcommand,
        );
        group.process_count += 1.0;
        group.observed_working_ms = group.observed_working_ms.saturating_add(
            u128::from(span.observed_working_ms).saturating_mul(overlap) / duration,
        );
        group.cpu_core_ms_estimate += span.cpu_core_ms_estimate * fraction;
        group.peak_cpu_percent = group.peak_cpu_percent.max(f64::from(span.peak_cpu_percent));
        group.peak_memory_bytes = group.peak_memory_bytes.max(span.peak_memory_bytes);
        group.read_bytes = group.read_bytes.saturating_add(scaled_u64(
            span.read_bytes,
            overlap,
            u64::try_from(duration).unwrap_or(u64::MAX),
        ));
        group.written_bytes = group.written_bytes.saturating_add(scaled_u64(
            span.written_bytes,
            overlap,
            u64::try_from(duration).unwrap_or(u64::MAX),
        ));
        "individual sampled spans".clone_into(&mut group.detail_coverage);
    }
    for summary in summaries {
        accumulate_process_summary(&mut groups, summary, from_ms, to_ms);
    }
    let mut reports: Vec<_> = groups.into_values().collect();
    reports.sort_by(|left, right| {
        right
            .cpu_core_ms_estimate
            .total_cmp(&left.cpu_core_ms_estimate)
            .then_with(|| left.agent.cmp(&right.agent))
            .then_with(|| left.executable.cmp(&right.executable))
            .then_with(|| left.subcommand.cmp(&right.subcommand))
    });
    Some(reports)
}

fn swap_growth(samples: &[Observation]) -> Option<i128> {
    if samples.iter().any(|sample| sample.resolution_ms > 0) {
        return None;
    }
    let mut values = samples.iter().filter_map(|sample| sample.swap_used_bytes);
    let first = values.next()?;
    let last = values.next_back()?;
    Some(i128::from(last) - i128::from(first))
}

fn rollups_in_range(rollups: Vec<AgentRollup>, from_ms: u128, to_ms: u128) -> Vec<AgentRollup> {
    rollups
        .into_iter()
        .filter(|rollup| {
            let start = u128::from(rollup.minute_unix_ms);
            start < to_ms && start + u128::from(rollup.resolution_ms) > from_ms
        })
        .collect()
}

fn covered_by_rollup(rollups: &[AgentRollup], sample: &Observation, agent: &AgentSample) -> bool {
    rollups.iter().any(|rollup| {
        let at_unix_ms = sample.at_unix_ms;
        if agent_kind(&rollup.kind) != agent_kind(&agent.kind)
            || !same_source_pid(sample.source_pid, segment_pid(&rollup.source_suffix))
        {
            return false;
        }
        if at_unix_ms < rollup.minute_unix_ms
            || at_unix_ms >= rollup.minute_unix_ms.saturating_add(rollup.resolution_ms)
        {
            return false;
        }
        rollup.covered_minutes.is_empty()
            || u16::try_from((at_unix_ms - rollup.minute_unix_ms) / 60_000)
                .is_ok_and(|minute| rollup.covered_minutes.contains(&minute))
    })
}

fn same_source_pid(left: Option<u32>, right: Option<u32>) -> bool {
    left.zip(right).is_none_or(|(left, right)| left == right)
}

fn covered_by_tick(ticks: &[AgentTick], sample: &Observation, agent: &AgentSample) -> bool {
    ticks.iter().any(|tick| {
        tick.at_unix_ms == sample.at_unix_ms
            && same_source_pid(sample.source_pid, segment_pid(&tick.source_suffix))
            && tick
                .agents
                .iter()
                .any(|tick_agent| agent_kind(&tick_agent.kind) == agent_kind(&agent.kind))
    })
}

fn accumulate_host_agent_fallback(
    load: &mut BTreeMap<String, AgentLoad>,
    sample: &Observation,
    ticks: &[AgentTick],
    rollups: &[AgentRollup],
    duration: u128,
) {
    if let Some(agent_samples) = &sample.agents {
        let fallback: Vec<_> = agent_samples
            .iter()
            .filter(|agent| {
                !covered_by_tick(ticks, sample, agent) && !covered_by_rollup(rollups, sample, agent)
            })
            .collect();
        accumulate_agent_refs(load, &fallback, duration);
    }
}

fn tick_covered_by_rollup(rollups: &[AgentRollup], tick: &AgentTick) -> bool {
    rollups.iter().any(|rollup| {
        rollup.source_suffix == tick.source_suffix
            && tick.at_unix_ms >= rollup.minute_unix_ms
            && tick.at_unix_ms < rollup.minute_unix_ms.saturating_add(rollup.resolution_ms)
    })
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
            at > from_ms && u128::from(sample.interval_start_unix_ms()) < to_ms
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

fn observation_resolutions(samples: &[Observation]) -> BTreeMap<String, u64> {
    let mut resolutions = BTreeMap::new();
    for sample in samples {
        let label = match sample.resolution_ms {
            0 => "raw",
            retention::FIVE_MINUTES_MS => "5m",
            retention::ONE_HOUR_MS => "1h",
            retention::ONE_DAY_MS => "1d",
            _ => "other",
        };
        *resolutions.entry(label.to_owned()).or_insert(0) += sample.source_samples();
    }
    resolutions
}

fn clipped_sample_duration(sample: &Observation, start: u128, end: u128) -> u128 {
    let overlap = end.saturating_sub(start);
    if sample.resolution_ms == 0 {
        overlap
    } else {
        overlap.min(
            overlap.saturating_mul(u128::from(sample.working_ms))
                / u128::from(sample.resolution_ms),
        )
    }
}

fn peak_host_cpu(samples: &[Observation]) -> Option<f64> {
    samples
        .iter()
        .filter_map(|sample| sample.peak_cpu_percent.or(sample.cpu_percent))
        .max_by(f64::total_cmp)
}

fn pressure_report(thresholds: (u8, u8, u8)) -> PressureReport {
    PressureReport {
        cpu_threshold_percent: thresholds.0,
        memory_threshold_percent: thresholds.1,
        swap_threshold_percent: thresholds.2,
        ..PressureReport::default()
    }
}

fn report_is_empty(
    samples: &[Observation],
    ticks: &[AgentTick],
    rollups: &[AgentRollup],
    command_count: Option<usize>,
    scheduler: Option<&schedule_events::Summary>,
) -> bool {
    samples.is_empty()
        && ticks.is_empty()
        && rollups.is_empty()
        && command_count.unwrap_or_default() == 0
        && scheduler.is_none_or(|summary| summary.submitted == 0 && summary.rejected == 0)
}

fn agent_data_available(
    samples: &[Observation],
    ticks: &[AgentTick],
    rollups: &[AgentRollup],
) -> bool {
    !ticks.is_empty() || !rollups.is_empty() || samples.iter().any(|sample| sample.agents.is_some())
}

fn has_coarse_history(samples: &[Observation], rollups: &[AgentRollup]) -> bool {
    samples.iter().any(|sample| sample.resolution_ms > 0)
        || rollups.iter().any(|rollup| rollup.resolution_ms > 60_000)
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
    let (from_ms, to_ms) = range_millis(from, to);
    let samples = observations_in_range(samples, from_ms, to_ms);
    let observation_resolutions = observation_resolutions(&samples);
    let (agent_ticks, agent_rollups) = filtered_agent_data(agent_data, from_ms, to_ms);
    let coarse_history_approximate = has_coarse_history(&samples, &agent_rollups);
    let records = records_in_range(records, from_ms, to_ms);
    let collection_coverage =
        coverage::CollectionCoverage::new(&samples, &agent_ticks, &agent_rollups, from_ms, to_ms);
    let mut pressure = pressure_report(thresholds);
    let agent_sample_coverage = samples.iter().any(|sample| sample.agents.is_some());
    let host_observations_with_agents = count_host_observations_with_agents(&samples);
    let mut pressure_overlap =
        PressureOverlapAccumulator::new(records.is_some(), agent_sample_coverage);
    let mut timeline = Vec::new();
    let mut observed_working_ms = 0_u128;
    let mut covered_until = from_ms;
    let mut missing = [false; 4];
    let agents_available = agent_data_available(&samples, &agent_ticks, &agent_rollups);
    let mut agent_load: BTreeMap<String, AgentLoad> = BTreeMap::new();
    for sample in &samples {
        let at = u128::from(sample.at_unix_ms);
        let end = at.min(to_ms);
        let start = u128::from(sample.interval_start_unix_ms()).max(covered_until);
        let duration = clipped_sample_duration(sample, start, end);
        covered_until = covered_until.max(end);
        observed_working_ms += duration;
        accumulate_host_agent_fallback(
            &mut agent_load,
            sample,
            &agent_ticks,
            &agent_rollups,
            duration,
        );
        let triggers =
            add_pressure_sample(&mut pressure, &mut missing, sample, thresholds, duration);
        if !triggers.is_empty() && duration > 0 {
            pressure_overlap.add(records.as_deref(), sample.agents.as_deref(), start, end);
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
    accumulate_agent_rollups(&mut agent_load, &agent_rollups, from_ms, to_ms);
    let passive_commands = passive_command_reports(&agent_ticks, &agent_rollups, from_ms, to_ms);
    let command_count = records.as_ref().map(Vec::len);
    Report {
        from: from.to_rfc3339(),
        to: to.to_rfc3339(),
        empty: report_is_empty(
            &samples,
            &agent_ticks,
            &agent_rollups,
            command_count,
            scheduler.as_ref(),
        ),
        observation_count: samples.len(),
        observation_resolutions,
        coarse_history_approximate,
        retention_status: None,
        collection_coverage,
        host_observations_with_agents,
        agent_observation_count: agent_ticks.len(),
        agent_rollup_count: agent_rollups.len(),
        observed_working_ms,
        command_measurements_available: records.is_some(),
        command_count,
        categories: records.as_ref().map(|records| category_reports(records)),
        agent_commands: records
            .as_ref()
            .map(|records| agent_command_reports(records)),
        agent_comparison: agents_available
            .then(|| compare_agents(&agent_load, passive_commands.as_deref())),
        agent_activity_comparison: passive_commands.as_deref().map(compare_agent_activities),
        passive_commands,
        process_commands: None,
        peak_host_cpu_percent: peak_host_cpu(&samples),
        peak_host_memory_bytes: peak_host_memory(&samples),
        peak_root_disk_used_percent: peak_disk_used_percent(&samples),
        swap_growth_bytes: swap_growth(&samples),
        memory_consumers: memory_consumer_reports(&samples),
        peak_command_concurrency: records
            .as_ref()
            .map(|records| peak_concurrency(records, from_ms, to_ms)),
        pressure,
        pressure_overlap: pressure_overlap.finish(),
        timeline,
        agents: agents_available.then(|| agent_load.into_values().collect()),
        scheduler,
        #[cfg(unix)]
        control: control::Summary::default(),
        #[cfg(unix)]
        cache: cache_impact::Impact::default(),
    }
}

fn peak_host_memory(samples: &[Observation]) -> Option<u64> {
    samples
        .iter()
        .filter_map(|sample| sample.peak_memory_used_bytes.or(sample.memory_used_bytes))
        .max()
}

fn filtered_agent_data(
    data: AgentData,
    from: u128,
    to: u128,
) -> (Vec<AgentTick>, Vec<AgentRollup>) {
    let rollups = rollups_in_range(data.rollups, from, to);
    let mut ticks = ticks_in_range(data.ticks, from, to);
    ticks.retain(|tick| !tick_covered_by_rollup(&rollups, tick));
    (ticks, rollups)
}

fn range_millis(from: DateTime<Utc>, to: DateTime<Utc>) -> (u128, u128) {
    (
        u128::try_from(from.timestamp_millis()).unwrap_or_default(),
        u128::try_from(to.timestamp_millis()).unwrap_or_default(),
    )
}

fn printable<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map_or_else(|| "unavailable".to_owned(), |value| value.to_string())
}

fn count_host_observations_with_agents(samples: &[Observation]) -> usize {
    samples
        .iter()
        .filter(|sample| {
            sample
                .agents
                .as_ref()
                .is_some_and(|agents| !agents.is_empty())
        })
        .count()
}

fn float_u64(value: u64) -> f64 {
    let high = u32::try_from(value >> 32).unwrap_or(u32::MAX);
    let low = u32::try_from(value & 0xffff_ffff).unwrap_or(u32::MAX);
    f64::from(high) * 4_294_967_296.0 + f64::from(low)
}

fn float_u128(value: u128) -> f64 {
    let high = u64::try_from(value >> 64).unwrap_or(u64::MAX);
    let low = u64::try_from(value & u128::from(u64::MAX)).unwrap_or(u64::MAX);
    float_u64(high) * 18_446_744_073_709_551_616.0 + float_u64(low)
}

fn compare_agents(
    agents: &BTreeMap<String, AgentLoad>,
    commands: Option<&[PassiveCommandLoad]>,
) -> Vec<AgentComparison> {
    let mut comparison = Vec::new();
    for (kind, agent) in agents {
        let active_ms = float_u128(agent.observed_working_ms);
        let active_hours = active_ms / 3_600_000.0;
        let related: Vec<_> = commands
            .unwrap_or_default()
            .iter()
            .filter(|command| command.agent == *kind)
            .collect();
        let command_cpu: f64 = related
            .iter()
            .map(|command| command.cpu_core_ms_estimate.max(0.0))
            .sum();
        let mut by_category: BTreeMap<String, f64> = BTreeMap::new();
        for command in &related {
            *by_category.entry(command.category.clone()).or_default() +=
                command.cpu_core_ms_estimate.max(0.0);
        }
        let activity_cpu_percent = (command_cpu > 0.0).then(|| {
            by_category
                .into_iter()
                .map(|(category, cpu)| (category, cpu / command_cpu * 100.0))
                .collect()
        });
        let top_command_family = related
            .iter()
            .max_by(|left, right| {
                left.cpu_core_ms_estimate
                    .total_cmp(&right.cpu_core_ms_estimate)
                    .then_with(|| right.family.cmp(&left.family))
            })
            .map(|command| command.family.clone());
        comparison.push(AgentComparison {
            agent: kind.clone(),
            observed_active_hours: active_hours,
            average_cpu_cores_while_active: (active_ms > 0.0)
                .then(|| agent.cpu_core_ms_estimate / active_ms),
            peak_memory_bytes: agent.peak_memory_bytes,
            read_mib_per_active_hour: (active_hours > 0.0)
                .then(|| float_u64(agent.read_bytes) / 1_048_576.0 / active_hours),
            written_mib_per_active_hour: (active_hours > 0.0)
                .then(|| float_u64(agent.written_bytes) / 1_048_576.0 / active_hours),
            top_command_family,
            activity_cpu_percent,
        });
    }
    comparison.sort_by(|left, right| {
        right
            .average_cpu_cores_while_active
            .unwrap_or_default()
            .total_cmp(&left.average_cpu_cores_while_active.unwrap_or_default())
            .then_with(|| left.agent.cmp(&right.agent))
    });
    comparison
}

#[derive(Default)]
struct ActivityTotals {
    active_ms: u128,
    cpu_core_ms_estimate: f64,
    peak_memory_bytes: u64,
    read_bytes: u64,
    written_bytes: u64,
}

fn compare_agent_activities(commands: &[PassiveCommandLoad]) -> Vec<AgentActivityComparison> {
    let mut groups: BTreeMap<(&str, &str), ActivityTotals> = BTreeMap::new();
    for command in commands {
        let group = groups
            .entry((&command.agent, &command.category))
            .or_default();
        group.active_ms = group.active_ms.saturating_add(command.observed_working_ms);
        group.cpu_core_ms_estimate += command.cpu_core_ms_estimate.max(0.0);
        group.peak_memory_bytes = group.peak_memory_bytes.max(command.peak_memory_bytes);
        group.read_bytes = group.read_bytes.saturating_add(command.read_bytes);
        group.written_bytes = group.written_bytes.saturating_add(command.written_bytes);
    }
    groups
        .into_iter()
        .map(|((agent, category), totals)| {
            let active_ms = float_u128(totals.active_ms);
            let active_hours = active_ms / 3_600_000.0;
            AgentActivityComparison {
                agent: agent.to_owned(),
                category: category.to_owned(),
                sampled_family_active_hours: active_hours,
                average_cpu_cores_per_family_active_hour: (active_ms > 0.0)
                    .then(|| totals.cpu_core_ms_estimate / active_ms),
                peak_memory_bytes: totals.peak_memory_bytes,
                read_mib_per_family_active_hour: (active_hours > 0.0)
                    .then(|| float_u64(totals.read_bytes) / 1_048_576.0 / active_hours),
                written_mib_per_family_active_hour: (active_hours > 0.0)
                    .then(|| float_u64(totals.written_bytes) / 1_048_576.0 / active_hours),
            }
        })
        .collect()
}

fn memory_consumer_reports(samples: &[Observation]) -> Option<Vec<MemoryConsumerLoad>> {
    let observed = samples
        .iter()
        .filter(|sample| sample.memory_consumers.is_some())
        .map(|sample| u128::from(sample.working_ms))
        .sum::<u128>();
    if observed == 0 {
        return None;
    }
    let mut groups: BTreeMap<&str, (usize, u128, u64, u64, usize)> = BTreeMap::new();
    for sample in samples {
        for consumer in sample.memory_consumers.as_deref().unwrap_or_default() {
            let group = groups
                .entry(safe_memory_family(&consumer.family))
                .or_default();
            group.0 = group
                .0
                .saturating_add(usize::try_from(sample.source_samples()).unwrap_or(usize::MAX));
            group.1 = group.1.saturating_add(
                u128::from(consumer.rss_bytes)
                    * u128::from(consumer.observed_working_ms.unwrap_or(sample.working_ms)),
            );
            group.2 = group
                .2
                .max(consumer.peak_rss_bytes.unwrap_or(consumer.rss_bytes));
            group.3 = group.3.max(consumer.largest_process_rss_bytes);
            group.4 = group.4.max(consumer.process_count);
        }
    }
    let mut result: Vec<_> = groups
        .into_iter()
        .map(
            |(family, (samples, total_rss, peak_rss, largest, processes))| MemoryConsumerLoad {
                family: family.to_owned(),
                samples,
                mean_rss_bytes: u64::try_from(total_rss / observed).unwrap_or(u64::MAX),
                peak_rss_bytes: peak_rss,
                peak_largest_process_rss_bytes: largest,
                peak_process_count: processes,
            },
        )
        .collect();
    result.sort_by(|left, right| {
        right
            .mean_rss_bytes
            .cmp(&left.mean_rss_bytes)
            .then_with(|| left.family.cmp(&right.family))
    });
    Some(result)
}

fn peak_disk_used_percent(samples: &[Observation]) -> Option<f64> {
    samples
        .iter()
        .filter_map(|sample| {
            sample
                .minimum_root_disk_available_bytes
                .or(sample.root_disk_available_bytes)
                .zip(sample.root_disk_total_bytes)
                .filter(|(available, total)| *total > 0 && available <= total)
                .map(|(available, total)| (1.0 - float_u64(available) / float_u64(total)) * 100.0)
        })
        .max_by(f64::total_cmp)
}

fn readable_bytes(value: i128) -> String {
    let amount = value.unsigned_abs();
    let sign = if value < 0 { "-" } else { "" };
    if amount < 1_024 {
        return format!("{sign}{amount} B");
    }
    let units = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB", "ZiB", "YiB"];
    let mut divisor = 1_024_u128;
    let mut unit_index = 0;
    while amount / divisor >= 1_024 && unit_index + 1 < units.len() {
        divisor *= 1_024;
        unit_index += 1;
    }
    let whole = amount / divisor;
    let fraction = (amount % divisor * 10 + divisor / 2) / divisor;
    format!(
        "{sign}{}.{:01} {}",
        whole + fraction / 10,
        fraction % 10,
        units[unit_index]
    )
}

fn readable_optional_bytes(value: Option<i128>) -> String {
    value.map_or_else(|| "unavailable".to_owned(), readable_bytes)
}

fn readable_duration(value: Option<u128>) -> String {
    let Some(ms) = value else {
        return "unavailable".to_owned();
    };
    if ms < 1_000 {
        return format!("{ms} ms");
    }
    let seconds = ms / 1_000;
    let hours = seconds / 3_600;
    let minutes = seconds % 3_600 / 60;
    let seconds = seconds % 60;
    let label = if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    };
    format!("{label} ({ms} ms)")
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
        out.push_str("| Agent | Active samples | CPU core-ms | Peak CPU % | Peak RSS | Read | Written | Peak processes | Peak roots |\n| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
        for agent in agents {
            let _ = writeln!(
                out,
                "| {} | {} | {:.0} | {:.1} | {} | {} | {} | {} | {} |",
                agent.kind,
                agent.active_samples,
                agent.cpu_core_ms_estimate,
                if agent.peak_cpu_percent == 0.0 {
                    0.0
                } else {
                    agent.peak_cpu_percent
                },
                readable_bytes(i128::from(agent.peak_memory_bytes)),
                readable_bytes(i128::from(agent.read_bytes)),
                readable_bytes(i128::from(agent.written_bytes)),
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

fn agent_comparison_markdown(comparison: Option<&[AgentComparison]>) -> String {
    let mut out = String::from("\n## Agent activity-adjusted comparison\n\n");
    let Some(comparison) = comparison else {
        out.push_str("Unavailable: no passive agent observations.\n");
        return out;
    };
    out.push_str("Average CPU cores and I/O rates are divided by each agent's observed active time. Activity mix is the share of *classified descendant-command CPU*, not all agent CPU. Peak RSS is a peak, not a rate. Concurrent agents can share a host sample; this is not causal attribution.\n\n");
    out.push_str("| Agent | Active time | Avg CPU cores | Peak RSS | Read MiB/active h | Written MiB/active h | Top family | Activity mix |\n| --- | ---: | ---: | ---: | ---: | ---: | --- | --- |\n");
    for agent in comparison {
        let mix = agent.activity_cpu_percent.as_ref().map_or_else(
            || "unavailable".to_owned(),
            |categories| {
                categories
                    .iter()
                    .map(|(category, percent)| format!("{category} {percent:.0}%"))
                    .collect::<Vec<_>>()
                    .join(", ")
            },
        );
        let _ = writeln!(
            out,
            "| {} | {:.2} h | {} | {} | {} | {} | {} | {} |",
            agent.agent,
            agent.observed_active_hours,
            agent
                .average_cpu_cores_while_active
                .map_or_else(|| "unavailable".to_owned(), |value| format!("{value:.2}")),
            readable_bytes(i128::from(agent.peak_memory_bytes)),
            agent
                .read_mib_per_active_hour
                .map_or_else(|| "unavailable".to_owned(), |value| format!("{value:.1}")),
            agent
                .written_mib_per_active_hour
                .map_or_else(|| "unavailable".to_owned(), |value| format!("{value:.1}")),
            agent.top_command_family.as_deref().unwrap_or("unavailable"),
            mix,
        );
    }
    if comparison.is_empty() {
        out.push_str("\nNo registered agent process was active in the range.\n");
    }
    out
}

fn agent_activity_comparison_markdown(activities: Option<&[AgentActivityComparison]>) -> String {
    let mut out = String::from("\n## Resource use by agent activity\n\n");
    let Some(activities) = activities else {
        out.push_str("Unavailable: observations predate passive command activity.\n");
        return out;
    };
    out.push_str("Rows group sanitized descendant-command families by agent and category. Rates divide sampled CPU and I/O by summed family-active time; concurrent families can make this time exceed wall time. Peak RSS is not additive.\n\n");
    out.push_str("| Agent | Activity | Family-active time | Avg CPU cores | Peak RSS | Read MiB/active h | Written MiB/active h |\n| --- | --- | ---: | ---: | ---: | ---: | ---: |\n");
    for activity in activities {
        let _ = writeln!(
            out,
            "| {} | {} | {:.2} h | {} | {} | {} | {} |",
            activity.agent,
            activity.category,
            activity.sampled_family_active_hours,
            activity
                .average_cpu_cores_per_family_active_hour
                .map_or_else(|| "unavailable".to_owned(), |value| format!("{value:.2}")),
            readable_bytes(i128::from(activity.peak_memory_bytes)),
            activity
                .read_mib_per_family_active_hour
                .map_or_else(|| "unavailable".to_owned(), |value| format!("{value:.1}")),
            activity
                .written_mib_per_family_active_hour
                .map_or_else(|| "unavailable".to_owned(), |value| format!("{value:.1}")),
        );
    }
    out
}

fn memory_consumers_markdown(consumers: Option<&[MemoryConsumerLoad]>) -> String {
    let mut out = String::from("\n## Memory consumers\n\n");
    let Some(consumers) = consumers else {
        out.push_str("Unavailable: observations predate sanitized memory-family sampling.\n");
        return out;
    };
    out.push_str("Sanitized process-family RSS at host sample times. Mean includes zero when a family was absent. RSS can count shared pages more than once and does not add up to host used memory. Unknown process names are grouped as `other processes`.\n\n");
    out.push_str("| Family | Samples present | Mean RSS | Peak RSS | Largest process peak RSS | Peak processes |\n| --- | ---: | ---: | ---: | ---: | ---: |\n");
    for consumer in consumers {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} |",
            consumer.family,
            consumer.samples,
            readable_bytes(i128::from(consumer.mean_rss_bytes)),
            readable_bytes(i128::from(consumer.peak_rss_bytes)),
            readable_bytes(i128::from(consumer.peak_largest_process_rss_bytes)),
            consumer.peak_process_count,
        );
    }
    out
}

fn agent_commands_markdown(commands: Option<&[AgentCategoryReport]>) -> String {
    let mut out = String::from("\n## Measured commands by agent\n\n");
    match commands {
        Some(commands) if !commands.is_empty() => {
            out.push_str("Exact measurements require a Reef-managed command. Passive process estimates appear below.\n\n");
            out.push_str("| Agent | Category | Count | Wall ms | CPU ms | Peak memory |\n| --- | --- | ---: | ---: | ---: | ---: |\n");
            for group in commands {
                let _ = writeln!(
                    out,
                    "| {} | {} | {} | {} | {} | {} |",
                    group.agent,
                    group.category,
                    group.count,
                    group.wall_ms,
                    group.cpu_ms,
                    readable_bytes(i128::from(group.peak_memory_bytes))
                );
            }
        }
        Some(_) => {
            out.push_str("No agent-tagged Reef-managed command measurements in this range.\n");
        }
        None => out.push_str("Unavailable: no command history.\n"),
    }
    out
}

fn passive_commands_markdown(commands: Option<&[PassiveCommandLoad]>) -> String {
    let mut out = String::from("\n## Passive agent command activity\n\n");
    match commands {
        Some(commands) if !commands.is_empty() => {
            out.push_str("Sanitized process families observed beneath registered agents. CPU, RSS, and I/O are sampled estimates, not exact command totals. Sub-second and detached processes can be missed; no arguments are stored. Showing the 12 highest-CPU groups.\n\n");
            out.push_str("| Agent | Family | Category | Active time | CPU core-ms | Peak RSS | Read | Written | Peak processes |\n| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |\n");
            for command in commands.iter().take(12) {
                let _ = writeln!(
                    out,
                    "| {} | {} | {} | {} | {:.0} | {} | {} | {} | {} |",
                    command.agent,
                    command.family,
                    command.category,
                    readable_duration(Some(command.observed_working_ms)),
                    command.cpu_core_ms_estimate,
                    readable_bytes(i128::from(command.peak_memory_bytes)),
                    readable_bytes(i128::from(command.read_bytes)),
                    readable_bytes(i128::from(command.written_bytes)),
                    command.peak_processes,
                );
            }
        }
        Some(_) => out.push_str("No agent descendant commands were sampled in this range.\n"),
        None => out.push_str("Unavailable: observations predate passive command activity.\n"),
    }
    out
}

fn process_commands_markdown(commands: Option<&[ProcessCommandLoad]>) -> String {
    let mut out = String::from("## Agent process drilldown\n\n");
    match commands {
        None => out.push_str("No process-span history is available.\n\n"),
        Some([]) => out.push_str("No completed agent descendant process overlaps the range.\n\n"),
        Some(commands) => {
            out.push_str("Command arguments, paths, environment, prompts, and file contents are excluded. Older rows are compacted under the configured storage cap; a tilde marks proportional execution-equivalent estimates.\n\n");
            out.push_str("| Agent | Workspace | Family | Process | Subcommand | Executions | Active time | CPU core-ms | Peak RSS | Read | Written | Detail |\n| --- | --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |\n");
            for command in commands.iter().take(50) {
                let _ = writeln!(
                    out,
                    "| {} | {} | {} | {} | {} | {} | {} | {:.0} | {} | {} | {} | {} |",
                    command.agent,
                    command.workspace_kind,
                    command.family,
                    command.executable,
                    command.subcommand,
                    if command.execution_count_estimated {
                        format!("~{:.2}", command.process_count)
                    } else {
                        format!("{:.0}", command.process_count)
                    },
                    readable_duration(Some(command.observed_working_ms)),
                    command.cpu_core_ms_estimate,
                    readable_bytes(i128::from(command.peak_memory_bytes)),
                    readable_bytes(i128::from(command.read_bytes)),
                    readable_bytes(i128::from(command.written_bytes)),
                    command.detail_coverage,
                );
            }
            if commands.len() > 50 {
                let _ = writeln!(
                    out,
                    "\nShowing the 50 highest-CPU groups of {}.",
                    commands.len()
                );
            }
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
fn markdown(report: &Report) -> String {
    markdown_with_timeline(report, false)
}

fn pressure_markdown(report: &Report) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "\n## Pressure\n\n- CPU >= {}%: {}\n- Memory >= {}%: {}\n- Swap >= {}%: {}\n- Any threshold: {}\n- With a measured command: {}\n- Without a measured command: {}\n- With agent sample data: {}\n",
        report.pressure.cpu_threshold_percent,
        readable_duration(report.pressure.cpu_above_ms),
        report.pressure.memory_threshold_percent,
        readable_duration(report.pressure.memory_above_ms),
        report.pressure.swap_threshold_percent,
        readable_duration(report.pressure.swap_above_ms),
        readable_duration(report.pressure.any_above_ms),
        readable_duration(report.pressure_overlap.measured_command_ms),
        readable_duration(report.pressure_overlap.no_measured_command_ms),
        readable_duration(report.pressure_overlap.sampled_agent_coverage_ms),
    );
    out.push_str("\nOverlap is concurrent activity, not proof of cause. Category times use measured command intervals; agent times are process-sample estimates. Concurrent groups can exceed total pressure time.\n");
    if let Some(categories) = &report.pressure_overlap.by_category
        && !categories.is_empty()
    {
        out.push_str("\nMeasured command categories during pressure:\n\n| Category | Overlap ms |\n| --- | ---: |\n");
        for category in categories {
            let _ = writeln!(out, "| {} | {} |", category.name, category.overlap_ms);
        }
    }
    if let Some(agents) = &report.pressure_overlap.by_agent
        && !agents.is_empty()
    {
        out.push_str(
            "\nSampled agents during pressure:\n\n| Agent | Overlap ms |\n| --- | ---: |\n",
        );
        for agent in agents {
            let _ = writeln!(out, "| {} | {} |", agent.name, agent.overlap_ms);
        }
    }
    out
}

fn timeline_markdown(events: &[PressureEvent]) -> String {
    let mut out = String::from("\n## Pressure timeline\n\n");
    if events.is_empty() {
        out.push_str("No recorded pressure events.\n");
    } else {
        out.push_str("Sample overlap does not establish cause.\n\n");
        for event in events {
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
    out
}

#[cfg(unix)]
fn control_markdown(control: &control::Summary) -> String {
    let mut out = String::from("## Opt-in measured agent attempts\n\n");
    if !control.available {
        out.push_str("Warning: private control telemetry is missing, unreadable, or invalid. Counts are unavailable.\n\n");
        return out;
    }
    let _ = write!(
        out,
        "- Attempted: {}\n- Completed: {}\n- Pending: {}\n- Unavailable: {}\n\n",
        control.attempted.unwrap_or_default(),
        control.completed.unwrap_or_default(),
        control.pending.unwrap_or_default(),
        control.unavailable.unwrap_or_default(),
    );
    if let Some(warning) = control.warning {
        let _ = writeln!(out, "Warning: {warning}.\n");
    }
    if !control.groups.is_empty() {
        out.push_str("Measured costs cover completed attempts with a valid command record; pending and unavailable attempts are excluded from cost totals.\n\n");
        out.push_str("| Agent | Category | Arm | Assigned | Measured | Pending | Unavailable | Success | Failed | Cancelled | Measured wall | Measured CPU | Peak memory |\n| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
        for group in &control.groups {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                group.agent,
                group.category,
                group.arm,
                group.assigned,
                group.measured,
                group.pending,
                group.unavailable,
                group.success,
                group.failed,
                group.cancelled,
                readable_duration(Some(group.measured_wall_ms)),
                readable_duration(Some(group.measured_cpu_ms)),
                readable_bytes(i128::from(group.peak_memory_bytes)),
            );
        }
        out.push('\n');
    }
    out
}

fn markdown_with_timeline(report: &Report, include_timeline: bool) -> String {
    let mut out = format!(
        "# Reef workload report\n\n- Range: {} to {} (end exclusive)\n- Empty: {}\n- Host observations: {}\n- Host observations with agents: {}\n- Observed working time: {}\n- Peak host CPU: {}\n- Peak host memory: {}\n- Peak root disk used: {}\n- Swap growth: {}\n- Peak command concurrency: {}\n\n",
        report.from,
        report.to,
        report.empty,
        report.observation_count,
        report.host_observations_with_agents,
        readable_duration(Some(report.observed_working_ms)),
        report
            .peak_host_cpu_percent
            .map_or_else(|| "unavailable".to_owned(), |value| format!("{value:.1}%")),
        readable_optional_bytes(report.peak_host_memory_bytes.map(i128::from)),
        report
            .peak_root_disk_used_percent
            .map_or_else(|| "unavailable".to_owned(), |value| format!("{value:.1}%")),
        readable_optional_bytes(report.swap_growth_bytes),
        printable(report.peak_command_concurrency),
    );
    out.push_str(&report.collection_coverage.markdown());
    let resolutions = report
        .observation_resolutions
        .iter()
        .map(|(resolution, count)| format!("{resolution}: {count} source samples"))
        .collect::<Vec<_>>()
        .join(", ");
    let _ = writeln!(out, "- Observation resolution: {resolutions}");
    if report.coarse_history_approximate {
        out.push_str("\nOlder intervals use bucket averages and retained peaks. Partial-bucket coverage is approximate; arbitrary-threshold pressure and precise swap growth are unavailable when coarse history is included.\n");
    }
    if let Some(status) = &report.retention_status {
        let _ = writeln!(
            out,
            "\nStorage cap compacted {} segments early and evicted {} retained segments; latest early compaction at Unix ms {}, latest eviction at Unix ms {}.",
            status.early_compacted_files,
            status.evicted_files,
            status.last_early_compaction_unix_ms,
            status.last_early_eviction_unix_ms
        );
        if status.cap_unmet {
            out.push_str("Storage cap remains unmet because protected history could not be compacted or removed.\n");
        }
    }
    out.push('\n');
    #[cfg(unix)]
    out.push_str(&control_markdown(&report.control));
    out.push_str("## Commands\n\n");
    if let Some(categories) = &report.categories {
        out.push_str("Whole commands overlapping the range; percentages are shares of measured commands. Concurrent wall times can exceed elapsed time.\n\n");
        out.push_str("| Category | Count | Failed/cancelled | Wall ms | Wall % | CPU ms | CPU % | Peak memory |\n| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
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
                readable_bytes(i128::from(group.peak_memory_bytes))
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
    out.push_str(&agent_commands_markdown(report.agent_commands.as_deref()));
    out.push_str(&passive_commands_markdown(
        report.passive_commands.as_deref(),
    ));
    out.push_str(&process_commands_markdown(
        report.process_commands.as_deref(),
    ));
    out.push_str(&agents_markdown(report.agents.as_deref()));
    out.push_str(&agent_comparison_markdown(
        report.agent_comparison.as_deref(),
    ));
    out.push_str(&agent_activity_comparison_markdown(
        report.agent_activity_comparison.as_deref(),
    ));
    out.push_str(&memory_consumers_markdown(
        report.memory_consumers.as_deref(),
    ));
    out.push_str(&pressure_markdown(report));
    if include_timeline {
        out.push_str(&timeline_markdown(&report.timeline));
    }
    out.push_str(&scheduler_markdown(report.scheduler.as_ref()));
    #[cfg(unix)]
    {
        out.push_str(&cache_impact::markdown(&report.cache));
    }
    out
}

#[cfg(unix)]
fn normalize_control_timestamps(mut measurements: Vec<Measurement>) -> Vec<Measurement> {
    for measurement in &mut measurements {
        if measurement.ended_at_unix_ms < measurement.started_at_unix_ms {
            measurement.started_at_unix_ms = measurement
                .ended_at_unix_ms
                .saturating_sub(measurement.wall_ms);
        }
    }
    measurements
}

fn read_measurements(
    options: &Options,
    #[cfg(unix)] control: &mut control::Summary,
) -> io::Result<Option<Vec<Measurement>>> {
    #[cfg(unix)]
    let control_path = control
        .available
        .then(|| control::measurements_path(options.control_state_dir.as_deref()).ok())
        .flatten();
    #[cfg(unix)]
    if control.available && control_path.is_none() {
        *control = control::Summary::unavailable();
    }
    #[cfg(unix)]
    let control_canonical = control_path
        .as_ref()
        .and_then(|path| fs::canonicalize(path).ok());
    let default_paths = history::paths()?;
    let mut paths = default_paths.clone().unwrap_or_default();
    paths.extend(options.records.iter().cloned());
    #[cfg(unix)]
    let available = default_paths.is_some() || !paths.is_empty() || control_path.is_some();
    #[cfg(not(unix))]
    let available = default_paths.is_some() || !paths.is_empty();
    if !available {
        return Ok(None);
    }
    let mut all = Vec::new();
    let mut seen = BTreeSet::new();
    for path in paths {
        let canonical = fs::canonicalize(path)?;
        if seen.insert(canonical.clone()) {
            let measurements: Vec<Measurement> = read_jsonl(&canonical)?;
            #[cfg(unix)]
            let measurements = if control_canonical.as_ref() == Some(&canonical) {
                normalize_control_timestamps(measurements)
            } else {
                measurements
            };
            all.extend(measurements);
        }
    }
    #[cfg(unix)]
    if control_path.is_some() {
        match control_canonical {
            Some(canonical) if seen.insert(canonical.clone()) => {
                match read_jsonl::<Measurement>(&canonical) {
                    Ok(measurements) => all.extend(normalize_control_timestamps(measurements)),
                    Err(_) => *control = control::Summary::unavailable(),
                }
            }
            Some(_) => {}
            None => *control = control::Summary::unavailable(),
        }
    }
    Ok(Some(all))
}

fn default_from(
    to: DateTime<Utc>,
    samples: &[Observation],
    agent_ticks: &[AgentTick],
    agent_rollups: &[AgentRollup],
    records: Option<&[Measurement]>,
    scheduler_events: Option<&[schedule_events::Event]>,
    extra_event_ms: [Option<u128>; 2],
) -> DateTime<Utc> {
    let to_ms = u128::try_from(to.timestamp_millis()).unwrap_or_default();
    let oldest_ms = samples
        .iter()
        .map(|sample| u128::from(sample.interval_start_unix_ms()))
        .chain(
            agent_ticks
                .iter()
                .map(|tick| u128::from(tick.at_unix_ms.saturating_sub(tick.working_ms))),
        )
        .chain(
            agent_rollups
                .iter()
                .map(|rollup| u128::from(rollup.minute_unix_ms)),
        )
        .chain(
            records
                .into_iter()
                .flatten()
                .map(|record| record.started_at_unix_ms),
        )
        .chain(
            scheduler_events
                .into_iter()
                .flatten()
                .map(|event| u128::from(event.at_unix_ms)),
        )
        .chain(extra_event_ms.into_iter().flatten())
        .filter(|timestamp| *timestamp < to_ms)
        .min();
    oldest_ms
        .and_then(|timestamp| i64::try_from(timestamp).ok())
        .and_then(DateTime::from_timestamp_millis)
        .unwrap_or(to - Duration::hours(24))
}

fn read_scheduler_events(options: &Options) -> io::Result<Option<Vec<schedule_events::Event>>> {
    let dir = options.schedule_state_dir.clone().or_else(|| {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state/reef/schedule"))
    });
    dir.map_or(Ok(None), |dir| schedule_events::read(&dir))
}

fn validate_options(options: &Options, to: DateTime<Utc>) -> io::Result<()> {
    if (options.output.is_some() || options.open) && !matches!(options.format, Format::Html) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--output and --open require --format html",
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
    if options.from.is_some_and(|from| from >= to) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--from must be before --to",
        ));
    }
    Ok(())
}

pub fn run(options: &Options) -> io::Result<()> {
    let to = options.to.unwrap_or_else(Utc::now);
    validate_options(options, to)?;
    let dir = state_dir(options.state_dir.as_deref())?;
    let samples = observations(&dir)?;
    let agent_samples = agent_ticks(&dir)?;
    let agent_summaries = agent_rollups(&dir)?;
    let process_spans = process_history::spans(&dir)?;
    let process_summaries = process_history::summaries(&dir)?;
    let process_event_ms = earliest_process_event(&process_spans, &process_summaries);
    let scheduler_events = read_scheduler_events(options)?;
    #[cfg(unix)]
    let control_event_ms = control::earliest_event_ms(options.control_state_dir.as_deref());
    #[cfg(not(unix))]
    let control_event_ms = None;
    #[cfg(unix)]
    let mut control = control::summary(
        options.control_state_dir.as_deref(),
        0,
        u128::try_from(to.timestamp_millis()).unwrap_or_default(),
    );
    #[cfg(unix)]
    let records = read_measurements(options, &mut control)?;
    #[cfg(not(unix))]
    let records = read_measurements(options)?;
    let from = options.from.unwrap_or_else(|| {
        default_from(
            to,
            &samples,
            &agent_samples,
            &agent_summaries,
            records.as_deref(),
            scheduler_events.as_deref(),
            [control_event_ms, process_event_ms],
        )
    });
    if from >= to {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--from must be before --to",
        ));
    }
    let host_series = html::HostSeries::from_samples(&samples, from, to);
    #[cfg(unix)]
    let control = control::summary(
        options.control_state_dir.as_deref(),
        u128::try_from(from.timestamp_millis()).unwrap_or_default(),
        u128::try_from(to.timestamp_millis()).unwrap_or_default(),
    );
    let mut report = build_report_with_ticks(
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
        scheduler_events.as_ref().map(|events| {
            schedule_events::summary(
                events,
                u64::try_from(from.timestamp_millis()).unwrap_or_default(),
                u64::try_from(to.timestamp_millis()).unwrap_or_default(),
            )
        }),
    );
    let (from_ms, to_ms) = range_millis(from, to);
    report.process_commands =
        process_command_reports(&process_spans, &process_summaries, from_ms, to_ms);
    report.collection_coverage.process_detail(
        &process_spans,
        &process_summaries,
        from_ms,
        to_ms,
        &dir,
    )?;
    report.empty = report.empty && report.process_commands.as_ref().is_none_or(Vec::is_empty);
    report.retention_status = retention::read_status(&dir)?;
    #[cfg(unix)]
    let report = {
        let mut report = report;
        report.control = control;
        report.empty = report.empty && report.control.attempted.unwrap_or_default() == 0;
        report.cache = cache_impact::report(
            options.cache_dir.as_deref(),
            u128::try_from(from.timestamp_millis()).unwrap_or_default(),
            u128::try_from(to.timestamp_millis()).unwrap_or_default(),
        )?;
        report.empty = report.empty && report.cache.hits == 0 && report.cache.misses == 0;
        report
    };
    write_report(&report, &host_series, options)
}

fn earliest_process_event(
    spans: &[process_history::ProcessSpan],
    summaries: &[process_history::ProcessSummary],
) -> Option<u128> {
    spans
        .iter()
        .map(|span| u128::from(span.first_seen_unix_ms))
        .chain(
            summaries
                .iter()
                .map(|summary| u128::from(summary.bucket_start_unix_ms)),
        )
        .min()
}

fn write_report(
    report: &Report,
    host_series: &html::HostSeries,
    options: &Options,
) -> io::Result<()> {
    match options.format {
        Format::Markdown => print!("{}", markdown_with_timeline(report, options.timeline)),
        Format::Json if options.timeline => println!(
            "{}",
            serde_json::to_string_pretty(&ReportWithTimeline {
                report,
                timeline: &report.timeline,
            })?
        ),
        Format::Json => println!("{}", serde_json::to_string_pretty(report)?),
        Format::Html => {
            let page = html::render(report, host_series, options.timeline);
            if let Some(path) = &options.output {
                write_html(path, page.as_bytes())?;
                if options.open {
                    let path = fs::canonicalize(path)?;
                    let path = path.to_str().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "HTML path is not UTF-8")
                    })?;
                    webbrowser::open(path)?;
                }
                println!("{}", path.display());
            } else {
                print!("{page}");
            }
        }
    }
    Ok(())
}

fn write_html(path: &Path, page: &[u8]) -> io::Result<()> {
    if path.file_name().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid HTML output path",
        ));
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let temporary = path.with_file_name(format!(".reef-report-{}-{nonce}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let result = (|| {
        file.write_all(page)?;
        file.sync_all()?;
        drop(file);
        match fs::symlink_metadata(path) {
            Ok(metadata) if !metadata.file_type().is_file() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "HTML output path is not a regular file",
                ));
            }
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_comparison_normalizes_by_activity_and_uses_classified_cpu_mix() {
        let agents = BTreeMap::from([
            (
                "claude".to_owned(),
                AgentLoad {
                    kind: "claude".into(),
                    observed_working_ms: 3_600_000,
                    cpu_core_ms_estimate: 1_800_000.0,
                    read_bytes: 100 * 1_048_576,
                    ..AgentLoad::default()
                },
            ),
            (
                "codex".to_owned(),
                AgentLoad {
                    kind: "codex".into(),
                    observed_working_ms: 7_200_000,
                    cpu_core_ms_estimate: 7_200_000.0,
                    read_bytes: 100 * 1_048_576,
                    ..AgentLoad::default()
                },
            ),
        ]);
        let commands = vec![
            PassiveCommandLoad {
                agent: "codex".into(),
                family: "go build".into(),
                category: "build".into(),
                cpu_core_ms_estimate: 300.0,
                ..PassiveCommandLoad::default()
            },
            PassiveCommandLoad {
                agent: "codex".into(),
                family: "go test".into(),
                category: "test".into(),
                cpu_core_ms_estimate: 100.0,
                ..PassiveCommandLoad::default()
            },
        ];
        let rows = compare_agents(&agents, Some(&commands));
        assert_eq!(rows[0].agent, "codex");
        assert_eq!(rows[0].average_cpu_cores_while_active, Some(1.0));
        assert_eq!(rows[0].read_mib_per_active_hour, Some(50.0));
        assert_eq!(rows[0].top_command_family.as_deref(), Some("go build"));
        assert_eq!(
            rows[0].activity_cpu_percent.as_ref().unwrap()["build"],
            75.0
        );
        assert_eq!(rows[1].average_cpu_cores_while_active, Some(0.5));
        assert!(rows[1].activity_cpu_percent.is_none());
    }

    fn date(ms: i64) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(ms).unwrap()
    }

    fn sample(at: u64, cpu: Option<f64>, swap: Option<u64>) -> Observation {
        Observation {
            at_unix_ms: at,
            working_ms: 100,
            resolution_ms: 0,
            source_samples: 1,
            cpu_percent: cpu,
            peak_cpu_percent: None,
            memory_used_bytes: Some(900),
            peak_memory_used_bytes: None,
            memory_total_bytes: Some(1000),
            swap_used_bytes: swap,
            swap_total_bytes: Some(1000),
            root_disk_available_bytes: None,
            minimum_root_disk_available_bytes: None,
            root_disk_total_bytes: None,
            agents: None,
            memory_consumers: None,
            source_pid: None,
        }
    }

    #[test]
    fn overview_uses_readable_durations_without_losing_precision() {
        assert_eq!(readable_duration(Some(100)), "100 ms");
        assert_eq!(
            readable_duration(Some(52_530_000)),
            "14h 35m 30s (52530000 ms)"
        );
        assert_eq!(readable_duration(None), "unavailable");
    }

    #[test]
    fn overview_uses_binary_units_for_byte_values() {
        assert_eq!(readable_bytes(0), "0 B");
        assert_eq!(readable_bytes(1_023), "1023 B");
        assert_eq!(readable_bytes(1_024), "1.0 KiB");
        assert_eq!(readable_bytes(1_048_576), "1.0 MiB");
        assert_eq!(readable_bytes(3_221_225_472), "3.0 GiB");
        assert_eq!(readable_bytes(-1_073_741_824), "-1.0 GiB");
        assert_eq!(readable_optional_bytes(None), "unavailable");
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
            agent_kind: None,
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
    fn overview_uses_exact_command_overlap_and_hides_timestamps() {
        let mut observation = sample(1_000, Some(95.0), Some(10));
        observation.working_ms = 1_000;
        observation.agents = Some(vec![AgentSample {
            kind: "codex".to_owned(),
            root_pid: 42,
            process_count: 1,
            cpu_percent: Some(50.0),
            memory_bytes: 100,
            read_bytes: None,
            written_bytes: None,
        }]);
        let report = build_report(
            date(0),
            date(1_000),
            vec![observation],
            Some(vec![
                command("build", 100, 500, 10),
                command("lint", 300, 700, 10),
            ]),
            (90, 90, 1),
            None,
        );
        assert_eq!(report.pressure.any_above_ms, Some(1_000));
        assert_eq!(report.pressure_overlap.measured_command_ms, Some(600));
        assert_eq!(report.pressure_overlap.no_measured_command_ms, Some(400));
        assert_eq!(
            report.pressure_overlap.by_category.as_ref().unwrap().len(),
            2
        );
        assert_eq!(
            report.pressure_overlap.by_category.as_ref().unwrap()[0].overlap_ms,
            400
        );
        assert_eq!(
            report.pressure_overlap.sampled_agent_coverage_ms,
            Some(1_000)
        );
        assert_eq!(
            report.pressure_overlap.by_agent.as_ref().unwrap()[0].name,
            "codex"
        );
        assert_eq!(
            report.pressure_overlap.by_agent.as_ref().unwrap()[0].overlap_ms,
            1_000
        );
        assert!(!markdown(&report).contains("Pressure timeline"));
        assert!(markdown_with_timeline(&report, true).contains("Pressure timeline"));
        assert!(
            serde_json::to_value(&report)
                .unwrap()
                .get("timeline")
                .is_none()
        );
        assert_eq!(
            serde_json::to_value(ReportWithTimeline {
                report: &report,
                timeline: &report.timeline,
            })
            .unwrap()["timeline"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn concurrent_commands_in_one_category_count_once() {
        let (measured, categories) = measured_pressure_overlap(
            &[
                command("build", 100, 500, 10),
                command("build", 300, 700, 10),
            ],
            0,
            1_000,
        );
        assert_eq!(measured, 600);
        assert_eq!(categories["build"], 600);
    }

    #[test]
    fn measured_commands_are_grouped_by_valid_agent_and_category() {
        let mut build = command("build", 100, 200, 80);
        build.agent_kind = Some("codex".to_owned());
        let mut lint = command("lint", 200, 300, 20);
        lint.agent_kind = Some("codex".to_owned());
        let mut unknown = command("build", 300, 400, 10);
        unknown.agent_kind = Some("private-agent-name".to_owned());
        let report = build_report(
            date(0),
            date(500),
            vec![],
            Some(vec![build, lint, unknown]),
            (90, 90, 1),
            None,
        );
        let groups = report.agent_commands.as_ref().unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!((groups[0].agent, groups[0].category), ("codex", "build"));
        assert_eq!(groups[0].cpu_ms, 80);
        assert_eq!((groups[1].agent, groups[1].category), ("codex", "lint"));
        assert!(!markdown(&report).contains("private-agent-name"));
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
        assert_eq!(report.pressure_overlap.no_measured_command_ms, Some(100));
        assert!(markdown(&report).contains("Without a measured command: 100 ms"));
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
                resolution_ms: 0,
                source_samples: 1,
                cpu_percent: Some(10.0),
                peak_cpu_percent: None,
                memory_used_bytes: None,
                peak_memory_used_bytes: None,
                memory_total_bytes: None,
                swap_used_bytes: None,
                swap_total_bytes: None,
                root_disk_available_bytes: None,
                minimum_root_disk_available_bytes: None,
                root_disk_total_bytes: None,
                agents: None,
                memory_consumers: None,
                source_pid: None,
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
                    source_suffix: String::new(),
                    working_ms: 1_000,
                    commands: None,
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
                    source_suffix: String::new(),
                    working_ms: 1_000,
                    commands: None,
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
                    source_suffix: String::new(),
                    working_ms: 1_000,
                    commands: None,
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
                    source_suffix: String::new(),
                    covered_minutes: BTreeSet::new(),
                    resolution_ms: 60_000,
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
                    commands: None,
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

    #[test]
    fn passive_command_report_uses_rollup_once_and_sanitizes_names() {
        let ticks = vec![AgentTick {
            at_unix_ms: 61_000,
            source_suffix: String::new(),
            working_ms: 1_000,
            agents: vec![],
            commands: Some(vec![PassiveCommandSample {
                kind: "claude".to_owned(),
                family: "secret-token".to_owned(),
                category: "test".to_owned(),
                process_count: 1,
                cpu_percent: Some(50.0),
                memory_bytes: 400,
                read_bytes: Some(10),
                written_bytes: Some(20),
            }]),
        }];
        let rollups = vec![AgentRollup {
            minute_unix_ms: 60_000,
            source_suffix: String::new(),
            covered_minutes: BTreeSet::new(),
            resolution_ms: 60_000,
            kind: "claude".to_owned(),
            active_samples: 2,
            observed_working_ms: 2_000,
            cpu_core_ms_estimate: 1_000.0,
            peak_cpu_percent: 50.0,
            peak_memory_bytes: 500,
            read_bytes: 30,
            written_bytes: 40,
            peak_processes: 2,
            peak_roots: 1,
            commands: Some(BTreeMap::from([(
                "go test".to_owned(),
                PassiveCommandRollup {
                    category: "test".to_owned(),
                    active_samples: 2,
                    observed_working_ms: 2_000,
                    cpu_core_ms_estimate: 1_000.0,
                    peak_cpu_percent: 50.0,
                    peak_memory_bytes: 500,
                    read_bytes: 30,
                    written_bytes: 40,
                    peak_processes: 2,
                },
            )])),
        }];
        let report = build_report_with_ticks(
            date(60_000),
            date(120_000),
            vec![],
            AgentData { ticks, rollups },
            None,
            (90, 90, 1),
            None,
        );
        let groups = report.passive_commands.as_ref().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].family, "go test");
        assert_eq!(groups[0].active_samples, 2);
        assert_eq!(groups[0].cpu_core_ms_estimate, 1_000.0);
        assert!(
            !serde_json::to_string(&groups)
                .unwrap()
                .contains("secret-token")
        );
    }

    #[test]
    fn process_report_keeps_cost_and_sanitizes_persisted_labels() {
        let spans = vec![process_history::ProcessSpan {
            agent: "codex".to_owned(),
            pid: 42,
            start_time: 1,
            parent_pid: Some(10),
            parent_start_time: Some(1),
            executable: "secret-executable".to_owned(),
            subcommand: "secret-subcommand".to_owned(),
            family: "secret-family".to_owned(),
            category: "test".to_owned(),
            workspace_kind: "/private/worktree".to_owned(),
            first_seen_unix_ms: 1_000,
            last_seen_unix_ms: 3_000,
            observed_working_ms: 2_000,
            sample_count: 2,
            cpu_core_ms_estimate: 1_500.0,
            peak_cpu_percent: 100.0,
            peak_memory_bytes: 200,
            read_bytes: 30,
            written_bytes: 40,
            coverage: "sampled".to_owned(),
        }];
        let groups = process_command_reports(&spans, &[], 0, 4_000).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].agent, "codex");
        assert_eq!(groups[0].executable, "other");
        assert_eq!(groups[0].subcommand, "other");
        assert_eq!(groups[0].family, "other");
        assert_eq!(groups[0].workspace_kind, "unknown");
        assert_eq!(groups[0].cpu_core_ms_estimate, 1_500.0);
        let json = serde_json::to_string(&groups).unwrap();
        assert!(!json.contains("secret"));
        assert!(!json.contains("/private"));
        assert!(!json.contains("\"pid\""));
    }

    #[test]
    fn process_report_marks_partial_compacted_execution_counts_as_estimates() {
        let summaries = vec![process_history::ProcessSummary {
            bucket_start_unix_ms: 0,
            resolution_ms: 86_400_000,
            agent: "codex".to_owned(),
            executable: "go".to_owned(),
            subcommand: "test".to_owned(),
            family: "go test".to_owned(),
            category: "test".to_owned(),
            workspace_kind: "worktree".to_owned(),
            process_count: 1.0,
            observed_working_ms: 24_000,
            cpu_core_ms_estimate: 24_000.0,
            peak_cpu_percent: 100.0,
            peak_memory_bytes: 200,
            read_bytes: 24,
            written_bytes: 48,
        }];
        let groups = process_command_reports(&[], &summaries, 0, 3_600_000).unwrap();
        assert_eq!(groups.len(), 1);
        assert!((groups[0].process_count - 1.0 / 24.0).abs() < f64::EPSILON);
        assert!(groups[0].execution_count_estimated);
        assert_eq!(groups[0].observed_working_ms, 1_000);
        assert_eq!(groups[0].cpu_core_ms_estimate, 1_000.0);
    }

    #[test]
    fn process_report_scales_cpu_for_a_long_running_process() {
        const DAY_MS: u64 = 86_400_000;
        let spans = vec![process_history::ProcessSpan {
            agent: "codex".to_owned(),
            pid: 42,
            start_time: 1,
            parent_pid: Some(10),
            parent_start_time: Some(1),
            executable: "go".to_owned(),
            subcommand: "test".to_owned(),
            family: "go test".to_owned(),
            category: "test".to_owned(),
            workspace_kind: "worktree".to_owned(),
            first_seen_unix_ms: 0,
            last_seen_unix_ms: 100 * DAY_MS,
            observed_working_ms: 100 * DAY_MS,
            sample_count: 1,
            cpu_core_ms_estimate: 100_000.0,
            peak_cpu_percent: 100.0,
            peak_memory_bytes: 200,
            read_bytes: 100_000,
            written_bytes: 0,
            coverage: "sampled".to_owned(),
        }];
        let groups = process_command_reports(&spans, &[], 0, u128::from(60 * DAY_MS)).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].cpu_core_ms_estimate, 60_000.0);
        assert_eq!(groups[0].observed_working_ms, u128::from(60 * DAY_MS));
        assert_eq!(groups[0].read_bytes, 60_000);
    }
}
