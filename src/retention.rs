use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const HOUR_MS: u64 = 3_600_000;
const DAY_MS: u64 = 86_400_000;
pub(crate) const FIVE_MINUTES_MS: u64 = 300_000;
pub(crate) const ONE_HOUR_MS: u64 = HOUR_MS;
pub(crate) const ONE_DAY_MS: u64 = DAY_MS;
const MIB: u64 = 1_048_576;
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
pub(crate) struct Policy {
    pub raw_hours: u64,
    pub process_detail_days: u64,
    pub process_five_minute_days: u64,
    pub five_minute_days: u64,
    pub hourly_days: u64,
    pub daily_days: u64,
    pub max_storage_mib: u64,
}

impl Policy {
    pub(crate) fn validate(self) -> io::Result<()> {
        let raw_days = self.raw_hours.div_ceil(24);
        if raw_days > self.five_minute_days
            || self.process_detail_days > self.process_five_minute_days
            || self.process_five_minute_days > self.hourly_days
            || self.five_minute_days > self.hourly_days
            || self.hourly_days > self.daily_days
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "retention horizons must increase from detailed through daily history",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub(crate) struct IntegerMetric {
    weighted_sum: u128,
    weight_ms: u64,
    peak: Option<u64>,
    minimum: Option<u64>,
}

impl IntegerMetric {
    fn add(&mut self, value: Option<u64>, weight: u64) {
        if let Some(value) = value.filter(|_| weight > 0) {
            self.weighted_sum = self
                .weighted_sum
                .saturating_add(u128::from(value) * u128::from(weight));
            self.weight_ms = self.weight_ms.saturating_add(weight);
            self.peak = Some(self.peak.unwrap_or_default().max(value));
            self.minimum = Some(self.minimum.map_or(value, |minimum| minimum.min(value)));
        }
    }

    fn merge(&mut self, other: &Self) {
        self.weighted_sum = self.weighted_sum.saturating_add(other.weighted_sum);
        self.weight_ms = self.weight_ms.saturating_add(other.weight_ms);
        if let Some(peak) = other.peak {
            self.peak = Some(self.peak.unwrap_or_default().max(peak));
        }
        if let Some(minimum) = other.minimum {
            self.minimum = Some(self.minimum.map_or(minimum, |value| value.min(minimum)));
        }
    }

    pub(crate) fn mean(&self) -> Option<u64> {
        (self.weight_ms > 0).then(|| {
            u64::try_from(self.weighted_sum / u128::from(self.weight_ms)).unwrap_or(u64::MAX)
        })
    }

    pub(crate) fn weight_ms(&self) -> u64 {
        self.weight_ms
    }

    pub(crate) fn peak(&self) -> Option<u64> {
        self.peak
    }

    pub(crate) fn minimum(&self) -> Option<u64> {
        self.minimum
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub(crate) struct FloatMetric {
    weighted_sum: f64,
    weight_ms: u64,
    peak: Option<f64>,
}

impl FloatMetric {
    fn add(&mut self, value: Option<f64>, weight: u64) {
        if let Some(value) = value.filter(|value| weight > 0 && value.is_finite()) {
            self.weighted_sum += value * f64::from(u32::try_from(weight).unwrap_or(u32::MAX));
            self.weight_ms = self.weight_ms.saturating_add(weight);
            self.peak = Some(self.peak.unwrap_or_default().max(value));
        }
    }

    fn merge(&mut self, other: &Self) {
        self.weighted_sum += other.weighted_sum;
        self.weight_ms = self.weight_ms.saturating_add(other.weight_ms);
        if let Some(peak) = other.peak {
            self.peak = Some(self.peak.unwrap_or_default().max(peak));
        }
    }

    pub(crate) fn mean(&self) -> Option<f64> {
        (self.weight_ms > 0).then(|| {
            self.weighted_sum / f64::from(u32::try_from(self.weight_ms).unwrap_or(u32::MAX))
        })
    }

    pub(crate) fn peak(&self) -> Option<f64> {
        self.peak
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub(crate) struct FamilySummary {
    pub rss_bytes: IntegerMetric,
    pub process_count: IntegerMetric,
    pub peak_largest_process_rss_bytes: u64,
}

impl FamilySummary {
    fn merge(&mut self, other: &Self) {
        self.rss_bytes.merge(&other.rss_bytes);
        self.process_count.merge(&other.process_count);
        self.peak_largest_process_rss_bytes = self
            .peak_largest_process_rss_bytes
            .max(other.peak_largest_process_rss_bytes);
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct HostSummary {
    pub bucket_start_unix_ms: u64,
    pub resolution_ms: u64,
    pub source_samples: u64,
    pub observed_working_ms: u64,
    #[serde(default)]
    pub cpu_percent: FloatMetric,
    #[serde(default)]
    pub memory_used_bytes: IntegerMetric,
    #[serde(default)]
    pub memory_total_bytes: IntegerMetric,
    #[serde(default)]
    pub swap_used_bytes: IntegerMetric,
    #[serde(default)]
    pub swap_total_bytes: IntegerMetric,
    #[serde(default)]
    pub root_disk_available_bytes: IntegerMetric,
    #[serde(default)]
    pub root_disk_total_bytes: IntegerMetric,
    #[serde(default)]
    pub memory_consumers: BTreeMap<String, FamilySummary>,
}

impl HostSummary {
    fn new(bucket_start_unix_ms: u64, resolution_ms: u64) -> Self {
        Self {
            bucket_start_unix_ms,
            resolution_ms,
            source_samples: 0,
            observed_working_ms: 0,
            cpu_percent: FloatMetric::default(),
            memory_used_bytes: IntegerMetric::default(),
            memory_total_bytes: IntegerMetric::default(),
            swap_used_bytes: IntegerMetric::default(),
            swap_total_bytes: IntegerMetric::default(),
            root_disk_available_bytes: IntegerMetric::default(),
            root_disk_total_bytes: IntegerMetric::default(),
            memory_consumers: BTreeMap::new(),
        }
    }

    fn add_raw(&mut self, raw: &RawHost) {
        let weight = raw.working_ms;
        self.source_samples = self.source_samples.saturating_add(1);
        self.observed_working_ms = self.observed_working_ms.saturating_add(weight);
        self.cpu_percent.add(raw.cpu_percent, weight);
        self.memory_used_bytes.add(raw.memory_used_bytes, weight);
        self.memory_total_bytes.add(raw.memory_total_bytes, weight);
        self.swap_used_bytes.add(raw.swap_used_bytes, weight);
        self.swap_total_bytes.add(raw.swap_total_bytes, weight);
        self.root_disk_available_bytes
            .add(raw.root_disk_available_bytes, weight);
        self.root_disk_total_bytes
            .add(raw.root_disk_total_bytes, weight);
        for consumer in &raw.memory_consumers {
            let family = self
                .memory_consumers
                .entry(safe_family(&consumer.family).to_owned())
                .or_default();
            family.rss_bytes.add(Some(consumer.rss_bytes), weight);
            family.process_count.add(
                Some(u64::try_from(consumer.process_count).unwrap_or(u64::MAX)),
                weight,
            );
            family.peak_largest_process_rss_bytes = family
                .peak_largest_process_rss_bytes
                .max(consumer.largest_process_rss_bytes);
        }
    }

    fn merge(&mut self, other: &Self) {
        self.source_samples = self.source_samples.saturating_add(other.source_samples);
        self.observed_working_ms = self
            .observed_working_ms
            .saturating_add(other.observed_working_ms);
        self.cpu_percent.merge(&other.cpu_percent);
        self.memory_used_bytes.merge(&other.memory_used_bytes);
        self.memory_total_bytes.merge(&other.memory_total_bytes);
        self.swap_used_bytes.merge(&other.swap_used_bytes);
        self.swap_total_bytes.merge(&other.swap_total_bytes);
        self.root_disk_available_bytes
            .merge(&other.root_disk_available_bytes);
        self.root_disk_total_bytes
            .merge(&other.root_disk_total_bytes);
        for (name, family) in &other.memory_consumers {
            self.memory_consumers
                .entry(name.clone())
                .or_default()
                .merge(family);
        }
    }

    pub(crate) fn end_unix_ms(&self) -> u64 {
        self.bucket_start_unix_ms.saturating_add(self.resolution_ms)
    }
}

#[derive(Deserialize)]
struct RawHost {
    at_unix_ms: u64,
    working_ms: u64,
    cpu_percent: Option<f64>,
    memory_used_bytes: Option<u64>,
    memory_total_bytes: Option<u64>,
    swap_used_bytes: Option<u64>,
    swap_total_bytes: Option<u64>,
    root_disk_available_bytes: Option<u64>,
    root_disk_total_bytes: Option<u64>,
    #[serde(default)]
    memory_consumers: Vec<RawFamily>,
}

#[derive(Deserialize)]
struct RawFamily {
    family: String,
    process_count: usize,
    rss_bytes: u64,
    largest_process_rss_bytes: u64,
}

fn safe_family(value: &str) -> &'static str {
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

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub(crate) struct CommandSummary {
    pub category: String,
    pub active_samples: u64,
    pub observed_working_ms: u64,
    pub cpu_core_ms_estimate: f64,
    pub peak_cpu_percent: f64,
    pub peak_memory_bytes: u64,
    pub read_bytes: u64,
    pub written_bytes: u64,
    pub peak_processes: usize,
}

impl CommandSummary {
    fn merge(&mut self, other: &Self) -> io::Result<()> {
        if !self.category.is_empty() && self.category != other.category {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "command family changed category",
            ));
        }
        self.category.clone_from(&other.category);
        self.active_samples = self.active_samples.saturating_add(other.active_samples);
        self.observed_working_ms = self
            .observed_working_ms
            .saturating_add(other.observed_working_ms);
        self.cpu_core_ms_estimate += other.cpu_core_ms_estimate;
        self.peak_cpu_percent = self.peak_cpu_percent.max(other.peak_cpu_percent);
        self.peak_memory_bytes = self.peak_memory_bytes.max(other.peak_memory_bytes);
        self.read_bytes = self.read_bytes.saturating_add(other.read_bytes);
        self.written_bytes = self.written_bytes.saturating_add(other.written_bytes);
        self.peak_processes = self.peak_processes.max(other.peak_processes);
        Ok(())
    }
}

fn minute_resolution() -> u64 {
    60_000
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct AgentSummary {
    pub minute_unix_ms: u64,
    #[serde(default = "minute_resolution")]
    pub resolution_ms: u64,
    pub kind: String,
    pub active_samples: u64,
    pub observed_working_ms: u64,
    pub cpu_core_ms_estimate: f64,
    pub peak_cpu_percent: f64,
    pub peak_memory_bytes: u64,
    pub read_bytes: u64,
    pub written_bytes: u64,
    pub peak_processes: usize,
    pub peak_roots: usize,
    #[serde(default)]
    pub commands: BTreeMap<String, CommandSummary>,
    #[serde(default)]
    pub covered_minutes: BTreeSet<u16>,
}

impl AgentSummary {
    fn empty(minute_unix_ms: u64, resolution_ms: u64, kind: String) -> Self {
        Self {
            minute_unix_ms,
            resolution_ms,
            kind,
            active_samples: 0,
            observed_working_ms: 0,
            cpu_core_ms_estimate: 0.0,
            peak_cpu_percent: 0.0,
            peak_memory_bytes: 0,
            read_bytes: 0,
            written_bytes: 0,
            peak_processes: 0,
            peak_roots: 0,
            commands: BTreeMap::new(),
            covered_minutes: BTreeSet::new(),
        }
    }

    fn merge(&mut self, other: &Self) -> io::Result<()> {
        self.active_samples = self.active_samples.saturating_add(other.active_samples);
        self.observed_working_ms = self
            .observed_working_ms
            .saturating_add(other.observed_working_ms);
        self.cpu_core_ms_estimate += other.cpu_core_ms_estimate;
        self.peak_cpu_percent = self.peak_cpu_percent.max(other.peak_cpu_percent);
        self.peak_memory_bytes = self.peak_memory_bytes.max(other.peak_memory_bytes);
        self.read_bytes = self.read_bytes.saturating_add(other.read_bytes);
        self.written_bytes = self.written_bytes.saturating_add(other.written_bytes);
        self.peak_processes = self.peak_processes.max(other.peak_processes);
        self.peak_roots = self.peak_roots.max(other.peak_roots);
        for (name, command) in &other.commands {
            self.commands
                .entry(name.clone())
                .or_default()
                .merge(command)?;
        }
        Ok(())
    }

    pub(crate) fn end_unix_ms(&self) -> u64 {
        self.minute_unix_ms.saturating_add(self.resolution_ms)
    }
}

#[derive(Clone)]
struct Segment {
    timestamp: u64,
    path: PathBuf,
    suffix: String,
    bytes: u64,
}

fn segments(dir: &Path, prefix: &str) -> io::Result<Vec<Segment>> {
    let mut result = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(suffix) = name
            .to_str()
            .and_then(|name| name.strip_prefix(prefix))
            .filter(|suffix| {
                Path::new(suffix)
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
            })
        else {
            continue;
        };
        let Some(timestamp) = suffix
            .split('-')
            .next()
            .and_then(|value| value.parse().ok())
        else {
            continue;
        };
        if entry.file_type()?.is_file() {
            result.push(Segment {
                timestamp,
                path: entry.path(),
                suffix: suffix.to_owned(),
                bytes: entry.metadata()?.len(),
            });
        }
    }
    result.sort_by(|left, right| (left.timestamp, &left.path).cmp(&(right.timestamp, &right.path)));
    Ok(result)
}

fn read_lines<T: for<'de> Deserialize<'de>>(path: &Path) -> io::Result<Vec<T>> {
    BufReader::new(File::open(path)?)
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

pub(crate) fn publish<T: Serialize + DeserializeOwned>(
    dir: &Path,
    target: &Path,
    records: &[T],
) -> io::Result<()> {
    let mut serialized = Vec::new();
    for record in records {
        let start = serialized.len();
        serde_json::to_writer(&mut serialized, record)?;
        serde_json::from_slice::<T>(&serialized[start..]).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("compacted segment is not readable: {error}"),
            )
        })?;
        serialized.push(b'\n');
    }
    match fs::symlink_metadata(target) {
        Ok(metadata) if metadata.is_file() => {
            if fs::read(target)? == serialized {
                return Ok(());
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("existing compacted segment differs: {}", target.display()),
            ));
        }
        Ok(_) => return Err(io::Error::other("compaction target is not a regular file")),
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        Err(_) => {}
    }
    let temporary = dir.join(format!(
        ".reef-compact-{}-{}.tmp",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = crate::observe::private_file(&temporary, true, false)?;
    let result = (|| {
        file.write_all(&serialized)?;
        file.sync_all()?;
        fs::rename(&temporary, target)?;
        #[cfg(unix)]
        File::open(dir)?.sync_all()?;
        if fs::read(target)? != serialized {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "compacted segment validation failed",
            ));
        }
        Ok(())
    })();
    if temporary.exists() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn fold_host_raw(records: &[RawHost], resolution: u64) -> Vec<HostSummary> {
    let mut buckets = BTreeMap::new();
    for raw in records {
        let start = raw.at_unix_ms / resolution * resolution;
        buckets
            .entry(start)
            .or_insert_with(|| HostSummary::new(start, resolution))
            .add_raw(raw);
    }
    buckets.into_values().collect()
}

fn fold_host_rollups(records: &[HostSummary], resolution: u64) -> Vec<HostSummary> {
    let mut buckets = BTreeMap::new();
    for record in records {
        let start = record.bucket_start_unix_ms / resolution * resolution;
        buckets
            .entry(start)
            .or_insert_with(|| HostSummary::new(start, resolution))
            .merge(record);
    }
    buckets.into_values().collect()
}

fn fold_agent_rollups(records: &[AgentSummary], resolution: u64) -> io::Result<Vec<AgentSummary>> {
    let mut buckets = BTreeMap::new();
    for record in records {
        let start = record.minute_unix_ms / resolution * resolution;
        let bucket = buckets
            .entry((start, record.kind.clone()))
            .or_insert_with(|| AgentSummary::empty(start, resolution, record.kind.clone()));
        bucket.merge(record)?;
        if record.covered_minutes.is_empty() {
            let offset = (record.minute_unix_ms - start) / 60_000;
            bucket
                .covered_minutes
                .insert(u16::try_from(offset).map_err(io::Error::other)?);
        } else {
            for minute in &record.covered_minutes {
                let absolute = record
                    .minute_unix_ms
                    .saturating_add(u64::from(*minute).saturating_mul(60_000));
                let offset = absolute.saturating_sub(start) / 60_000;
                bucket
                    .covered_minutes
                    .insert(u16::try_from(offset).map_err(io::Error::other)?);
            }
        }
    }
    Ok(buckets.into_values().collect())
}

fn compact_host(
    dir: &Path,
    source_prefix: &str,
    target_prefix: &str,
    source_resolution: Option<u64>,
    target_resolution: u64,
    cutoff: u64,
    failures: &mut Vec<(PathBuf, String)>,
) -> io::Result<()> {
    for segment in segments(dir, source_prefix)? {
        if segment.timestamp >= cutoff {
            continue;
        }
        if let Err(error) = compact_host_segment(
            dir,
            &segment,
            target_prefix,
            source_resolution,
            target_resolution,
            cutoff,
        ) {
            failures.push((segment.path, error.to_string()));
        }
    }
    Ok(())
}

fn compact_host_segment(
    dir: &Path,
    segment: &Segment,
    target_prefix: &str,
    source_resolution: Option<u64>,
    target_resolution: u64,
    cutoff: u64,
) -> io::Result<()> {
    let records = if let Some(expected_resolution) = source_resolution {
        let records: Vec<HostSummary> = read_lines(&segment.path)?;
        if records.is_empty()
            || records
                .iter()
                .any(|record| record.resolution_ms != expected_resolution)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid host rollup resolution or empty segment",
            ));
        }
        if records.iter().any(|record| record.end_unix_ms() >= cutoff) {
            return Ok(());
        }
        fold_host_rollups(&records, target_resolution)
    } else {
        let records: Vec<RawHost> = read_lines(&segment.path)?;
        if records.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "empty host observation segment",
            ));
        }
        if records.iter().any(|record| record.at_unix_ms >= cutoff) {
            return Ok(());
        }
        fold_host_raw(&records, target_resolution)
    };
    let target = dir.join(format!("{target_prefix}{}", segment.suffix));
    publish(dir, &target, &records)?;
    fs::remove_file(&segment.path)?;
    Ok(())
}

fn compact_agents(
    dir: &Path,
    source_prefix: &str,
    target_prefix: &str,
    source_resolution: u64,
    target_resolution: u64,
    cutoff: u64,
    failures: &mut Vec<(PathBuf, String)>,
) -> io::Result<()> {
    for segment in segments(dir, source_prefix)? {
        if segment.timestamp >= cutoff {
            continue;
        }
        if let Err(error) = compact_agent_segment(
            dir,
            &segment,
            target_prefix,
            source_resolution,
            target_resolution,
            cutoff,
        ) {
            failures.push((segment.path, error.to_string()));
        }
    }
    Ok(())
}

fn compact_agent_segment(
    dir: &Path,
    segment: &Segment,
    target_prefix: &str,
    source_resolution: u64,
    target_resolution: u64,
    cutoff: u64,
) -> io::Result<()> {
    let records: Vec<AgentSummary> = read_lines(&segment.path)?;
    if records.is_empty()
        || records
            .iter()
            .any(|record| record.resolution_ms != source_resolution)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid agent rollup resolution or empty segment",
        ));
    }
    if records.iter().any(|record| record.end_unix_ms() >= cutoff) {
        return Ok(());
    }
    let compacted = fold_agent_rollups(&records, target_resolution)?;
    let target = dir.join(format!("{target_prefix}{}", segment.suffix));
    publish(dir, &target, &compacted)?;
    fs::remove_file(&segment.path)?;
    Ok(())
}

fn expire(
    dir: &Path,
    prefix: &str,
    cutoff: u64,
    agent: bool,
    failures: &mut Vec<(PathBuf, String)>,
) -> io::Result<()> {
    for segment in segments(dir, prefix)? {
        if segment.timestamp >= cutoff {
            continue;
        }
        let result = (|| {
            let old = if agent {
                let records: Vec<AgentSummary> = read_lines(&segment.path)?;
                !records.is_empty() && records.iter().all(|record| record.end_unix_ms() < cutoff)
            } else {
                let records: Vec<HostSummary> = read_lines(&segment.path)?;
                !records.is_empty() && records.iter().all(|record| record.end_unix_ms() < cutoff)
            };
            if old {
                fs::remove_file(&segment.path)?;
            }
            Ok::<(), io::Error>(())
        })();
        if let Err(error) = result {
            failures.push((segment.path, error.to_string()));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct RetentionStatus {
    pub last_early_eviction_unix_ms: u64,
    pub evicted_files: u64,
    #[serde(default)]
    pub last_early_compaction_unix_ms: u64,
    #[serde(default)]
    pub early_compacted_files: u64,
    #[serde(default)]
    pub cap_unmet: bool,
}

const OWNED_PREFIXES: [&str; 13] = [
    "samples-",
    "agent-samples-",
    "agent-rollups-",
    "host-5m-",
    "agent-history-5m-",
    "host-1h-",
    "agent-history-1h-",
    "host-1d-",
    "agent-history-1d-",
    "process-spans-",
    "process-history-5m-",
    "process-history-1h-",
    "process-history-1d-",
];

fn total_bytes(dir: &Path) -> io::Result<u64> {
    let mut total = crate::process_history::checkpoint_bytes(dir)?;
    for prefix in OWNED_PREFIXES {
        for segment in segments(dir, prefix)? {
            total = total.saturating_add(segment.bytes);
        }
    }
    Ok(total)
}

#[derive(Clone, Copy)]
struct Stage {
    source: &'static str,
    target: &'static str,
    source_resolution: Option<u64>,
    target_resolution: u64,
    agent: bool,
}

const EARLY_STAGES: [Stage; 6] = [
    Stage {
        source: "samples-",
        target: "host-5m-",
        source_resolution: None,
        target_resolution: FIVE_MINUTES_MS,
        agent: false,
    },
    Stage {
        source: "agent-rollups-",
        target: "agent-history-5m-",
        source_resolution: Some(60_000),
        target_resolution: FIVE_MINUTES_MS,
        agent: true,
    },
    Stage {
        source: "host-5m-",
        target: "host-1h-",
        source_resolution: Some(FIVE_MINUTES_MS),
        target_resolution: ONE_HOUR_MS,
        agent: false,
    },
    Stage {
        source: "agent-history-5m-",
        target: "agent-history-1h-",
        source_resolution: Some(FIVE_MINUTES_MS),
        target_resolution: ONE_HOUR_MS,
        agent: true,
    },
    Stage {
        source: "host-1h-",
        target: "host-1d-",
        source_resolution: Some(ONE_HOUR_MS),
        target_resolution: ONE_DAY_MS,
        agent: false,
    },
    Stage {
        source: "agent-history-1h-",
        target: "agent-history-1d-",
        source_resolution: Some(ONE_HOUR_MS),
        target_resolution: ONE_DAY_MS,
        agent: true,
    },
];

fn early_compact(
    dir: &Path,
    now: u64,
    max_bytes: u64,
    status: &mut RetentionStatus,
    failures: &mut Vec<(PathBuf, String)>,
) -> io::Result<()> {
    for (source, target, source_resolution, target_resolution) in [
        (
            "process-spans-",
            "process-history-5m-",
            None,
            FIVE_MINUTES_MS,
        ),
        (
            "process-history-5m-",
            "process-history-1h-",
            Some(FIVE_MINUTES_MS),
            ONE_HOUR_MS,
        ),
        (
            "process-history-1h-",
            "process-history-1d-",
            Some(ONE_HOUR_MS),
            ONE_DAY_MS,
        ),
    ] {
        if total_bytes(dir)? <= max_bytes {
            return Ok(());
        }
        let count = crate::process_history::compact(
            dir,
            source,
            target,
            source_resolution,
            target_resolution,
            u64::MAX,
            failures,
        )?;
        if count > 0 {
            status.early_compacted_files = status.early_compacted_files.saturating_add(count);
            status.last_early_compaction_unix_ms = now;
        }
    }
    for stage in EARLY_STAGES {
        for segment in segments(dir, stage.source)? {
            if total_bytes(dir)? <= max_bytes {
                return Ok(());
            }
            if failures.iter().any(|(path, _)| *path == segment.path) {
                continue;
            }
            let result = if stage.agent {
                compact_agent_segment(
                    dir,
                    &segment,
                    stage.target,
                    stage.source_resolution.unwrap_or(60_000),
                    stage.target_resolution,
                    u64::MAX,
                )
            } else {
                compact_host_segment(
                    dir,
                    &segment,
                    stage.target,
                    stage.source_resolution,
                    stage.target_resolution,
                    u64::MAX,
                )
            };
            match result {
                Ok(()) if !segment.path.exists() => {
                    status.early_compacted_files = status.early_compacted_files.saturating_add(1);
                    status.last_early_compaction_unix_ms = now;
                }
                Ok(()) => {}
                Err(error) => failures.push((segment.path, error.to_string())),
            }
        }
    }
    Ok(())
}

fn evict_over_budget(
    dir: &Path,
    now: u64,
    max_bytes: u64,
    status: &mut RetentionStatus,
    protected: &BTreeSet<PathBuf>,
) -> io::Result<()> {
    let tiers: &[&[&str]] = &[
        &["host-1d-", "agent-history-1d-", "process-history-1d-"],
        &["host-1h-", "agent-history-1h-", "process-history-1h-"],
        &[
            "host-5m-",
            "agent-history-5m-",
            "agent-rollups-",
            "process-history-5m-",
        ],
        &["samples-", "process-spans-"],
    ];
    let mut total = total_bytes(dir)?;
    for prefixes in tiers {
        let mut files = Vec::new();
        for prefix in *prefixes {
            files.extend(segments(dir, prefix)?);
        }
        files.sort_by(|left, right| {
            (left.timestamp, &left.path).cmp(&(right.timestamp, &right.path))
        });
        for segment in files {
            if total <= max_bytes {
                break;
            }
            if protected.contains(&segment.path) {
                continue;
            }
            fs::remove_file(&segment.path)?;
            total = total.saturating_sub(segment.bytes);
            status.last_early_eviction_unix_ms = now;
            status.evicted_files = status.evicted_files.saturating_add(1);
        }
    }
    status.cap_unmet = total > max_bytes;
    Ok(())
}

fn enforce_budget(
    dir: &Path,
    now: u64,
    max_bytes: u64,
    failures: &mut Vec<(PathBuf, String)>,
) -> io::Result<()> {
    let previous = read_status(dir)?;
    let mut status = previous.clone().unwrap_or(RetentionStatus {
        last_early_eviction_unix_ms: 0,
        evicted_files: 0,
        last_early_compaction_unix_ms: 0,
        early_compacted_files: 0,
        cap_unmet: false,
    });
    if total_bytes(dir)? > max_bytes {
        early_compact(dir, now, max_bytes, &mut status, failures)?;
    }
    if total_bytes(dir)? > max_bytes {
        let protected = failures.iter().map(|(path, _)| path.clone()).collect();
        evict_over_budget(dir, now, max_bytes, &mut status, &protected)?;
    } else {
        status.cap_unmet = false;
    }
    if previous.as_ref() != Some(&status)
        && (previous.is_some()
            || status.early_compacted_files > 0
            || status.evicted_files > 0
            || status.cap_unmet)
    {
        write_status(dir, &status)?;
    }
    Ok(())
}

fn write_status(dir: &Path, status: &RetentionStatus) -> io::Result<()> {
    let target = dir.join("retention-status.json");
    let temporary = dir.join(format!(
        ".reef-retention-{}-{}.tmp",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = crate::observe::private_file(&temporary, true, false)?;
    let result = (|| {
        serde_json::to_writer(&mut file, &status)?;
        file.sync_all()?;
        fs::rename(&temporary, target)?;
        #[cfg(unix)]
        File::open(dir)?.sync_all()?;
        Ok(())
    })();
    if temporary.exists() {
        let _ = fs::remove_file(temporary);
    }
    result
}

pub(crate) fn read_status(dir: &Path) -> io::Result<Option<RetentionStatus>> {
    let path = dir.join("retention-status.json");
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() => serde_json::from_reader(File::open(path)?)
            .map(Some)
            .map_err(io::Error::other),
        Ok(_) => Err(io::Error::other("retention status is not a regular file")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) fn maintain(dir: &Path, now: u64, policy: Policy) -> io::Result<()> {
    policy.validate()?;
    let raw_cutoff = now.saturating_sub(policy.raw_hours.saturating_mul(HOUR_MS));
    let five_minute_cutoff = now.saturating_sub(policy.five_minute_days.saturating_mul(DAY_MS));
    let hourly_cutoff = now.saturating_sub(policy.hourly_days.saturating_mul(DAY_MS));
    let daily_cutoff = now.saturating_sub(policy.daily_days.saturating_mul(DAY_MS));
    let mut failures = Vec::new();
    crate::process_history::maintain(dir, now, policy, &mut failures)?;
    compact_host(
        dir,
        "samples-",
        "host-5m-",
        None,
        FIVE_MINUTES_MS,
        raw_cutoff,
        &mut failures,
    )?;
    compact_agents(
        dir,
        "agent-rollups-",
        "agent-history-5m-",
        60_000,
        FIVE_MINUTES_MS,
        raw_cutoff,
        &mut failures,
    )?;
    compact_host(
        dir,
        "host-5m-",
        "host-1h-",
        Some(FIVE_MINUTES_MS),
        ONE_HOUR_MS,
        five_minute_cutoff,
        &mut failures,
    )?;
    compact_agents(
        dir,
        "agent-history-5m-",
        "agent-history-1h-",
        FIVE_MINUTES_MS,
        ONE_HOUR_MS,
        five_minute_cutoff,
        &mut failures,
    )?;
    compact_host(
        dir,
        "host-1h-",
        "host-1d-",
        Some(ONE_HOUR_MS),
        ONE_DAY_MS,
        hourly_cutoff,
        &mut failures,
    )?;
    compact_agents(
        dir,
        "agent-history-1h-",
        "agent-history-1d-",
        ONE_HOUR_MS,
        ONE_DAY_MS,
        hourly_cutoff,
        &mut failures,
    )?;
    expire(dir, "host-1d-", daily_cutoff, false, &mut failures)?;
    expire(dir, "agent-history-1d-", daily_cutoff, true, &mut failures)?;
    enforce_budget(
        dir,
        now,
        policy.max_storage_mib.saturating_mul(MIB),
        &mut failures,
    )?;
    if let Some((path, error)) = failures.first() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: {error}", path.display()),
        ));
    }
    Ok(())
}

pub(crate) fn host_rollups(dir: &Path) -> io::Result<Vec<HostSummary>> {
    let raw_suffixes: BTreeSet<_> = segments(dir, "samples-")?
        .into_iter()
        .map(|segment| segment.suffix)
        .collect();
    let mut chosen = BTreeMap::new();
    for (rank, prefix) in [(0, "host-5m-"), (1, "host-1h-"), (2, "host-1d-")] {
        for segment in segments(dir, prefix)? {
            if raw_suffixes.contains(&segment.suffix) {
                continue;
            }
            chosen
                .entry(segment.suffix.clone())
                .or_insert((rank, segment.path));
        }
    }
    let mut buckets: BTreeMap<(u64, u64), HostSummary> = BTreeMap::new();
    for (rank, path) in chosen.into_values() {
        for record in read_lines::<HostSummary>(&path)? {
            let expected = [FIVE_MINUTES_MS, ONE_HOUR_MS, ONE_DAY_MS][rank];
            if record.resolution_ms != expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid host rollup resolution: {}", path.display()),
                ));
            }
            buckets
                .entry((record.bucket_start_unix_ms, record.resolution_ms))
                .and_modify(|group| group.merge(&record))
                .or_insert(record);
        }
    }
    let mut result: Vec<_> = buckets.into_values().collect();
    result.sort_by_key(|record| record.bucket_start_unix_ms);
    Ok(result)
}

pub(crate) fn agent_rollup_paths(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let raw_suffixes: BTreeSet<_> = segments(dir, "agent-samples-")?
        .into_iter()
        .map(|segment| segment.suffix)
        .collect();
    let mut chosen = BTreeMap::new();
    for prefix in [
        "agent-rollups-",
        "agent-history-5m-",
        "agent-history-1h-",
        "agent-history-1d-",
    ] {
        for segment in segments(dir, prefix)? {
            if raw_suffixes.contains(&segment.suffix) && prefix != "agent-rollups-" {
                continue;
            }
            chosen.entry(segment.suffix).or_insert(segment.path);
        }
    }
    Ok(chosen.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

    fn fixture() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "reef-retention-{}-{}",
            std::process::id(),
            NEXT_TEST.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        dir
    }

    fn policy() -> Policy {
        Policy {
            raw_hours: 48,
            process_detail_days: 14,
            process_five_minute_days: 30,
            five_minute_days: 14,
            hourly_days: 90,
            daily_days: 365,
            max_storage_mib: 512,
        }
    }

    fn raw_host(at: u64) -> String {
        format!(
            "{{\"at_unix_ms\":{at},\"working_ms\":60000,\"cpu_percent\":50.0,\"memory_used_bytes\":1048576,\"memory_total_bytes\":2097152,\"swap_used_bytes\":100,\"swap_total_bytes\":1000,\"root_disk_available_bytes\":1000,\"root_disk_total_bytes\":2000,\"processes\":[{{\"pid\":9123,\"command\":\"secret-token\"}}],\"memory_consumers\":[{{\"family\":\"compiler/build\",\"process_count\":2,\"rss_bytes\":512,\"largest_process_rss_bytes\":400}}]}}\n"
        )
    }

    #[test]
    fn older_host_samples_keep_trends_without_process_identifiers() {
        let dir = fixture();
        let now = 20 * DAY_MS;
        let at = now - 3 * DAY_MS;
        let source = dir.join(format!("samples-{at}-1.jsonl"));
        fs::write(&source, raw_host(at)).unwrap();

        maintain(&dir, now, policy()).unwrap();

        assert!(!source.exists());
        let rollups = host_rollups(&dir).unwrap();
        assert_eq!(rollups.len(), 1);
        assert_eq!(rollups[0].resolution_ms, FIVE_MINUTES_MS);
        assert_eq!(rollups[0].source_samples, 1);
        assert_eq!(rollups[0].cpu_percent.mean(), Some(50.0));
        assert_eq!(rollups[0].memory_used_bytes.peak(), Some(1_048_576));
        assert_eq!(
            rollups[0].memory_consumers["compiler/build"]
                .rss_bytes
                .mean(),
            Some(512)
        );
        let compacted =
            fs::read_to_string(segments(&dir, "host-5m-").unwrap()[0].path.clone()).unwrap();
        assert!(!compacted.contains("9123"));
        assert!(!compacted.contains("secret-token"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn retry_after_publish_keeps_one_copy_of_source_samples() {
        let dir = fixture();
        let now = 20 * DAY_MS;
        let at = now - 3 * DAY_MS;
        let source = dir.join(format!("samples-{at}-1.jsonl"));
        let original = raw_host(at);
        fs::write(&source, &original).unwrap();
        maintain(&dir, now, policy()).unwrap();
        fs::write(&source, original).unwrap();

        maintain(&dir, now, policy()).unwrap();

        assert!(!source.exists());
        assert_eq!(host_rollups(&dir).unwrap()[0].source_samples, 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn malformed_source_keeps_original_and_stops_compaction() {
        let dir = fixture();
        let now = 20 * DAY_MS;
        let at = now - 3 * DAY_MS;
        let source = dir.join(format!("samples-{at}-1.jsonl"));
        fs::write(&source, b"{broken\n").unwrap();

        assert!(maintain(&dir, now, policy()).is_err());

        assert_eq!(fs::read(&source).unwrap(), b"{broken\n");
        assert!(segments(&dir, "host-5m-").unwrap().is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn malformed_history_is_preserved_while_other_data_obeys_cap() {
        let dir = fixture();
        let now = 20 * DAY_MS;
        let at = now - 3 * DAY_MS;
        let malformed = dir.join(format!("samples-{at}-1.jsonl"));
        let newer = dir.join(format!("samples-{now}-2.jsonl"));
        fs::write(&malformed, b"{broken\n").unwrap();
        fs::write(&newer, raw_host(now).repeat(4_000)).unwrap();
        let mut constrained = policy();
        constrained.max_storage_mib = 1;

        assert!(maintain(&dir, now, constrained).is_err());

        assert_eq!(fs::read(&malformed).unwrap(), b"{broken\n");
        assert!(!newer.exists());
        let status = read_status(&dir).unwrap().unwrap();
        assert_eq!(status.early_compacted_files, 1);
        assert_eq!(status.evicted_files, 0);
        assert!(!status.cap_unmet);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn oversized_damaged_segment_reports_unmet_cap_without_deleting_source() {
        let dir = fixture();
        let now = 20 * DAY_MS;
        let source = dir.join(format!("samples-{}-1.jsonl", now - 3 * DAY_MS));
        let damaged = "{broken\n".repeat(180_000);
        fs::write(&source, &damaged).unwrap();
        let mut constrained = policy();
        constrained.max_storage_mib = 1;

        assert!(maintain(&dir, now, constrained).is_err());

        assert_eq!(fs::read_to_string(&source).unwrap(), damaged);
        assert!(read_status(&dir).unwrap().unwrap().cap_unmet);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn current_agent_ticks_are_not_evicted_before_they_can_be_folded() {
        let dir = fixture();
        let now = 20 * DAY_MS;
        let line = format!(
            "{{\"at_unix_ms\":{now},\"working_ms\":1000,\"agents\":[{{\"kind\":\"codex\",\"root_pid\":1,\"process_count\":1,\"cpu_percent\":50.0,\"memory_bytes\":100,\"read_bytes\":0,\"written_bytes\":0}}],\"commands\":[]}}\n"
        );
        let mut sources = Vec::new();
        for index in 0..3 {
            let source = dir.join(format!("agent-samples-{}-{index}.jsonl", now - 1000));
            fs::write(&source, line.repeat(3_000)).unwrap();
            sources.push(source);
        }
        let mut constrained = policy();
        constrained.max_storage_mib = 1;

        maintain(&dir, now, constrained).unwrap();

        assert!(sources.iter().all(|source| source.exists()));
        assert!(read_status(&dir).unwrap().unwrap().cap_unmet);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn resolved_storage_pressure_clears_persisted_cap_warning() {
        let dir = fixture();
        write_status(
            &dir,
            &RetentionStatus {
                last_early_eviction_unix_ms: 0,
                evicted_files: 0,
                last_early_compaction_unix_ms: 0,
                early_compacted_files: 0,
                cap_unmet: true,
            },
        )
        .unwrap();

        maintain(&dir, 20 * DAY_MS, policy()).unwrap();

        assert!(!read_status(&dir).unwrap().unwrap().cap_unmet);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn conflicting_published_rollup_keeps_original() {
        let dir = fixture();
        let now = 20 * DAY_MS;
        let at = now - 3 * DAY_MS;
        let source = dir.join(format!("samples-{at}-1.jsonl"));
        let target = dir.join(format!("host-5m-{at}-1.jsonl"));
        fs::write(&source, raw_host(at)).unwrap();
        fs::write(&target, b"{broken\n").unwrap();

        assert!(maintain(&dir, now, policy()).is_err());

        assert!(source.exists());
        assert_eq!(fs::read(&target).unwrap(), b"{broken\n");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn published_float_rollups_validate_by_written_bytes() {
        let dir = fixture();
        let host_target = dir.join("host-5m-float.jsonl");
        let mut host = HostSummary::new(0, FIVE_MINUTES_MS);
        host.cpu_percent.weighted_sum = f64::from_bits(0x414c_054c_23d7_0a3e);

        publish(&dir, &host_target, &[host.clone()]).unwrap();
        let host_bytes = fs::read(&host_target).unwrap();
        publish(&dir, &host_target, &[host]).unwrap();
        assert_eq!(fs::read(&host_target).unwrap(), host_bytes);

        let agent_target = dir.join("agent-rollups-float.jsonl");
        let mut agent = AgentSummary::empty(0, 60_000, "codex".to_owned());
        agent.cpu_core_ms_estimate = f64::from_bits(0x414c_054c_23d7_0a3e);

        publish(&dir, &agent_target, &[agent.clone()]).unwrap();
        let agent_bytes = fs::read(&agent_target).unwrap();
        publish(&dir, &agent_target, &[agent]).unwrap();
        assert_eq!(fs::read(&agent_target).unwrap(), agent_bytes);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn overflowing_float_rollup_keeps_raw_source() {
        let dir = fixture();
        let now = 20 * DAY_MS;
        let at = now - 3 * DAY_MS;
        let source = dir.join(format!("samples-{at}-1.jsonl"));
        let target = dir.join(format!("host-5m-{at}-1.jsonl"));
        let sample = raw_host(at).replace("\"cpu_percent\":50.0", "\"cpu_percent\":1e308");
        fs::write(&source, &sample).unwrap();

        assert!(maintain(&dir, now, policy()).is_err());
        assert_eq!(fs::read_to_string(&source).unwrap(), sample);
        assert!(!target.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn year_old_history_uses_daily_buckets() {
        let dir = fixture();
        let now = 400 * DAY_MS;
        let at = now - 200 * DAY_MS;
        fs::write(dir.join(format!("samples-{at}-1.jsonl")), raw_host(at)).unwrap();

        maintain(&dir, now, policy()).unwrap();

        assert_eq!(segments(&dir, "host-1d-").unwrap().len(), 1);
        assert!(segments(&dir, "host-5m-").unwrap().is_empty());
        assert!(segments(&dir, "host-1h-").unwrap().is_empty());
        assert_eq!(host_rollups(&dir).unwrap()[0].resolution_ms, ONE_DAY_MS);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn agent_and_command_cost_survive_daily_compaction() {
        let dir = fixture();
        let now = 400 * DAY_MS;
        let at = now - 200 * DAY_MS;
        let source = dir.join(format!("agent-rollups-{at}-1.jsonl"));
        let minute = at / 60_000 * 60_000;
        let row = serde_json::json!({
            "minute_unix_ms": minute,
            "kind": "codex",
            "active_samples": 2,
            "observed_working_ms": 2000,
            "cpu_core_ms_estimate": 1000.0,
            "peak_cpu_percent": 50.0,
            "peak_memory_bytes": 400,
            "read_bytes": 10,
            "written_bytes": 20,
            "peak_processes": 2,
            "peak_roots": 1,
            "commands": {
                "go test": {
                    "category": "test",
                    "active_samples": 2,
                    "observed_working_ms": 2000,
                    "cpu_core_ms_estimate": 500.0,
                    "peak_cpu_percent": 25.0,
                    "peak_memory_bytes": 300,
                    "read_bytes": 5,
                    "written_bytes": 7,
                    "peak_processes": 1
                }
            }
        });
        fs::write(&source, format!("{row}\n")).unwrap();

        maintain(&dir, now, policy()).unwrap();

        assert!(!source.exists());
        let paths = agent_rollup_paths(&dir).unwrap();
        assert_eq!(paths.len(), 1);
        let compacted: Vec<AgentSummary> = read_lines(&paths[0]).unwrap();
        assert_eq!(compacted[0].resolution_ms, ONE_DAY_MS);
        assert_eq!(compacted[0].cpu_core_ms_estimate, 1000.0);
        assert_eq!(compacted[0].commands["go test"].cpu_core_ms_estimate, 500.0);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sparse_agent_minutes_survive_each_coarser_tier() {
        let first = AgentSummary::empty(0, 60_000, "codex".to_owned());
        let second = AgentSummary::empty(120_000, 60_000, "codex".to_owned());

        let five_minutes = fold_agent_rollups(&[first, second], FIVE_MINUTES_MS).unwrap();
        assert_eq!(five_minutes[0].covered_minutes, BTreeSet::from([0, 2]));

        let hourly = fold_agent_rollups(&five_minutes, ONE_HOUR_MS).unwrap();
        assert_eq!(hourly[0].covered_minutes, BTreeSet::from([0, 2]));

        let daily = fold_agent_rollups(&hourly, ONE_DAY_MS).unwrap();
        assert_eq!(daily[0].covered_minutes, BTreeSet::from([0, 2]));
    }

    #[test]
    fn tight_cap_compacts_early_before_eviction() {
        let dir = fixture();
        let now = 20 * DAY_MS;
        let source = dir.join(format!("samples-{now}-1.jsonl"));
        fs::write(&source, raw_host(now).repeat(4_000)).unwrap();
        let mut constrained = policy();
        constrained.max_storage_mib = 1;

        maintain(&dir, now, constrained).unwrap();

        assert!(!source.exists());
        let status = read_status(&dir).unwrap().unwrap();
        assert_eq!(status.early_compacted_files, 1);
        assert_eq!(status.last_early_compaction_unix_ms, now);
        assert_eq!(status.evicted_files, 0);
        fs::remove_dir_all(dir).unwrap();
    }
}
