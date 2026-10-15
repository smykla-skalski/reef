use crate::observe;
use crate::observe::commands::ProcessSample;
use crate::retention;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const SEGMENT_BYTES: u64 = 1_048_576;
const HOUR_MS: u64 = 3_600_000;
const DAY_MS: u64 = 86_400_000;
const ACTIVE_FILE: &str = "process-active.json";
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

#[cfg(unix)]
fn sync_directory(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()?;
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct ProcessKey {
    pid: u32,
    start_time: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ActiveSpan {
    key: ProcessKey,
    parent_pid: Option<u32>,
    parent_start_time: Option<u64>,
    agent: String,
    executable: String,
    subcommand: String,
    family: String,
    category: String,
    workspace_kind: String,
    first_seen_unix_ms: u64,
    last_seen_unix_ms: u64,
    observed_working_ms: u64,
    sample_count: u64,
    cpu_core_ms_estimate: f64,
    peak_cpu_percent: f32,
    peak_memory_bytes: u64,
    first_total_read_bytes: u64,
    last_total_read_bytes: u64,
    first_total_written_bytes: u64,
    last_total_written_bytes: u64,
}

impl ActiveSpan {
    fn from_sample(sample: &ProcessSample, at_unix_ms: u64, working_ms: u64) -> Self {
        let mut span = Self {
            key: ProcessKey {
                pid: sample.pid,
                start_time: sample.start_time,
            },
            parent_pid: sample.parent_pid,
            parent_start_time: sample.parent_start_time,
            agent: sample.kind.clone(),
            executable: sample.executable.clone(),
            subcommand: sample.subcommand.clone(),
            family: sample.family.clone(),
            category: sample.category.clone(),
            workspace_kind: sample.workspace_kind.clone(),
            first_seen_unix_ms: at_unix_ms.saturating_sub(working_ms),
            last_seen_unix_ms: at_unix_ms,
            observed_working_ms: 0,
            sample_count: 0,
            cpu_core_ms_estimate: 0.0,
            peak_cpu_percent: 0.0,
            peak_memory_bytes: 0,
            first_total_read_bytes: sample.total_read_bytes,
            last_total_read_bytes: sample.total_read_bytes,
            first_total_written_bytes: sample.total_written_bytes,
            last_total_written_bytes: sample.total_written_bytes,
        };
        span.add(sample, at_unix_ms, working_ms);
        span
    }

    fn same_command(&self, sample: &ProcessSample) -> bool {
        self.agent == sample.kind
            && self.executable == sample.executable
            && self.subcommand == sample.subcommand
            && self.family == sample.family
            && self.category == sample.category
            && self.workspace_kind == sample.workspace_kind
    }

    fn add(&mut self, sample: &ProcessSample, at_unix_ms: u64, working_ms: u64) {
        self.parent_pid = sample.parent_pid;
        self.parent_start_time = sample.parent_start_time;
        self.last_seen_unix_ms = at_unix_ms;
        self.observed_working_ms = self.observed_working_ms.saturating_add(working_ms);
        self.sample_count = self.sample_count.saturating_add(1);
        self.cpu_core_ms_estimate += f64::from(sample.cpu_percent.max(0.0))
            * f64::from(u32::try_from(working_ms).unwrap_or(u32::MAX))
            / 100.0;
        self.peak_cpu_percent = self.peak_cpu_percent.max(sample.cpu_percent.max(0.0));
        self.peak_memory_bytes = self.peak_memory_bytes.max(sample.memory_bytes);
        self.last_total_read_bytes = self.last_total_read_bytes.max(sample.total_read_bytes);
        self.last_total_written_bytes = self
            .last_total_written_bytes
            .max(sample.total_written_bytes);
    }

    fn finish(self) -> ProcessSpan {
        ProcessSpan {
            agent: self.agent,
            pid: self.key.pid,
            start_time: self.key.start_time,
            parent_pid: self.parent_pid,
            parent_start_time: self.parent_start_time,
            executable: self.executable,
            subcommand: self.subcommand,
            family: self.family,
            category: self.category,
            workspace_kind: self.workspace_kind,
            first_seen_unix_ms: self.first_seen_unix_ms,
            last_seen_unix_ms: self.last_seen_unix_ms,
            observed_working_ms: self.observed_working_ms,
            sample_count: self.sample_count,
            cpu_core_ms_estimate: self.cpu_core_ms_estimate,
            peak_cpu_percent: self.peak_cpu_percent,
            peak_memory_bytes: self.peak_memory_bytes,
            read_bytes: self
                .last_total_read_bytes
                .saturating_sub(self.first_total_read_bytes),
            written_bytes: self
                .last_total_written_bytes
                .saturating_sub(self.first_total_written_bytes),
            coverage: "sampled".to_owned(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ProcessSpan {
    pub agent: String,
    pub pid: u32,
    pub start_time: u64,
    pub parent_pid: Option<u32>,
    pub parent_start_time: Option<u64>,
    pub executable: String,
    pub subcommand: String,
    pub family: String,
    pub category: String,
    pub workspace_kind: String,
    pub first_seen_unix_ms: u64,
    pub last_seen_unix_ms: u64,
    pub observed_working_ms: u64,
    pub sample_count: u64,
    pub cpu_core_ms_estimate: f64,
    pub peak_cpu_percent: f32,
    pub peak_memory_bytes: u64,
    pub read_bytes: u64,
    pub written_bytes: u64,
    pub coverage: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ProcessSummary {
    pub bucket_start_unix_ms: u64,
    pub resolution_ms: u64,
    pub agent: String,
    pub executable: String,
    pub subcommand: String,
    pub family: String,
    pub category: String,
    pub workspace_kind: String,
    pub process_count: f64,
    pub observed_working_ms: u64,
    pub cpu_core_ms_estimate: f64,
    pub peak_cpu_percent: f32,
    pub peak_memory_bytes: u64,
    pub read_bytes: u64,
    pub written_bytes: u64,
}

impl ProcessSummary {
    pub(crate) fn end_unix_ms(&self) -> u64 {
        self.bucket_start_unix_ms.saturating_add(self.resolution_ms)
    }

    fn merge(&mut self, other: &Self) {
        self.process_count += other.process_count;
        self.observed_working_ms = self
            .observed_working_ms
            .saturating_add(other.observed_working_ms);
        self.cpu_core_ms_estimate += other.cpu_core_ms_estimate;
        self.peak_cpu_percent = self.peak_cpu_percent.max(other.peak_cpu_percent);
        self.peak_memory_bytes = self.peak_memory_bytes.max(other.peak_memory_bytes);
        self.read_bytes = self.read_bytes.saturating_add(other.read_bytes);
        self.written_bytes = self.written_bytes.saturating_add(other.written_bytes);
    }
}

#[derive(Default)]
pub(crate) struct Tracker {
    active: BTreeMap<ProcessKey, ActiveSpan>,
}

impl Tracker {
    pub(crate) fn load(dir: &Path) -> io::Result<Self> {
        let path = dir.join(ACTIVE_FILE);
        let mut active: BTreeMap<ProcessKey, ActiveSpan> = match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => {
                let records: Vec<ActiveSpan> =
                    serde_json::from_reader(File::open(path)?).map_err(io::Error::other)?;
                records
                    .into_iter()
                    .filter(valid_active)
                    .map(|record| (record.key.clone(), record))
                    .collect()
            }
            Ok(_) => return Err(io::Error::other("process checkpoint is not a regular file")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error),
        };
        reconcile_completed_spans(dir, &mut active)?;
        Ok(Self { active })
    }

    pub(crate) fn recover(dir: &Path) -> io::Result<Self> {
        let tracker = Self::load(dir)?;
        tracker.checkpoint(dir)?;
        Ok(tracker)
    }

    pub(crate) fn observe(
        &mut self,
        samples: &[ProcessSample],
        at_unix_ms: u64,
        working_ms: u64,
    ) -> Vec<ProcessSpan> {
        let mut seen = BTreeSet::new();
        let mut completed = Vec::new();
        for sample in samples {
            let key = ProcessKey {
                pid: sample.pid,
                start_time: sample.start_time,
            };
            seen.insert(key.clone());
            if self
                .active
                .get(&key)
                .is_some_and(|span| !span.same_command(sample))
                && let Some(span) = self.active.remove(&key)
            {
                completed.push(span.finish());
            }
            self.active
                .entry(key)
                .and_modify(|span| span.add(sample, at_unix_ms, working_ms))
                .or_insert_with(|| ActiveSpan::from_sample(sample, at_unix_ms, working_ms));
        }
        let finished: Vec<_> = self
            .active
            .keys()
            .filter(|key| !seen.contains(*key))
            .cloned()
            .collect();
        for key in finished {
            if let Some(span) = self.active.remove(&key) {
                completed.push(span.finish());
            }
        }
        completed
    }

    pub(crate) fn checkpoint(&self, dir: &Path) -> io::Result<()> {
        let target = dir.join(ACTIVE_FILE);
        if self.active.is_empty() {
            return match fs::remove_file(target) {
                Ok(()) => {
                    #[cfg(unix)]
                    sync_directory(dir)?;
                    Ok(())
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            };
        }
        let temporary = dir.join(format!(
            ".reef-process-active-{}-{}.tmp",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = observe::private_file(&temporary, true, false)?;
        let result = (|| {
            serde_json::to_writer(&mut file, &self.active.values().collect::<Vec<_>>())?;
            file.sync_all()?;
            fs::rename(&temporary, &target)?;
            #[cfg(unix)]
            sync_directory(dir)?;
            Ok(())
        })();
        if temporary.exists() {
            let _ = fs::remove_file(temporary);
        }
        result
    }
}

fn reconcile_completed_spans(
    dir: &Path,
    active: &mut BTreeMap<ProcessKey, ActiveSpan>,
) -> io::Result<()> {
    if active.is_empty() {
        return Ok(());
    }
    let active_hours: BTreeSet<_> = active
        .values()
        .map(|span| span.last_seen_unix_ms / HOUR_MS)
        .collect();
    for (timestamp, path, _, _) in segments(dir, "process-spans-")? {
        if !active_hours.contains(&(timestamp / HOUR_MS)) {
            continue;
        }
        for completed in read_spans(&path)? {
            let key = ProcessKey {
                pid: completed.pid,
                start_time: completed.start_time,
            };
            if active
                .get(&key)
                .is_some_and(|candidate| completed_matches_active(&completed, candidate))
            {
                active.remove(&key);
            }
        }
    }
    Ok(())
}

fn completed_matches_active(completed: &ProcessSpan, active: &ActiveSpan) -> bool {
    completed.pid == active.key.pid
        && completed.start_time == active.key.start_time
        && completed.parent_pid == active.parent_pid
        && completed.parent_start_time == active.parent_start_time
        && completed.agent == active.agent
        && completed.executable == active.executable
        && completed.subcommand == active.subcommand
        && completed.family == active.family
        && completed.category == active.category
        && completed.workspace_kind == active.workspace_kind
        && completed.first_seen_unix_ms == active.first_seen_unix_ms
        && completed.last_seen_unix_ms == active.last_seen_unix_ms
        && completed.observed_working_ms == active.observed_working_ms
        && completed.sample_count == active.sample_count
        && completed.peak_memory_bytes == active.peak_memory_bytes
        && completed.read_bytes
            == active
                .last_total_read_bytes
                .saturating_sub(active.first_total_read_bytes)
        && completed.written_bytes
            == active
                .last_total_written_bytes
                .saturating_sub(active.first_total_written_bytes)
}

fn valid_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b' ' | b'/' | b'+' | b'-'))
}

fn valid_active(record: &ActiveSpan) -> bool {
    record.key.pid > 1
        && record.first_seen_unix_ms <= record.last_seen_unix_ms
        && valid_label(&record.agent)
        && valid_label(&record.executable)
        && valid_label(&record.subcommand)
        && valid_label(&record.family)
        && valid_label(&record.category)
        && valid_label(&record.workspace_kind)
}

fn segments(dir: &Path, prefix: &str) -> io::Result<Vec<(u64, PathBuf, String, u64)>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(suffix) = name
            .to_str()
            .and_then(|name| name.strip_prefix(prefix))
            .filter(|suffix| {
                Path::new(suffix)
                    .extension()
                    .is_some_and(|ext| ext == "jsonl")
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
            files.push((
                timestamp,
                entry.path(),
                suffix.to_owned(),
                entry.metadata()?.len(),
            ));
        }
    }
    files.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));
    Ok(files)
}

fn read_lines<T: DeserializeOwned>(path: &Path) -> io::Result<Vec<T>> {
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

fn process_line_error(path: &Path, line: usize, error: &serde_json::Error) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{}:{line}: {error}", path.display()),
    )
}

fn read_spans(path: &Path) -> io::Result<Vec<ProcessSpan>> {
    let bytes = fs::read(path)?;
    let ends_with_newline = bytes.ends_with(b"\n");
    let lines: Vec<_> = bytes.split(|byte| *byte == b'\n').collect();
    let last = lines.len().saturating_sub(1);
    let mut spans = Vec::new();
    for (index, line) in lines.into_iter().enumerate() {
        if line.is_empty() && index == last {
            continue;
        }
        match serde_json::from_slice(line) {
            Ok(span) => spans.push(span),
            Err(error) if index == last && !ends_with_newline && error.is_eof() => {}
            Err(error) => return Err(process_line_error(path, index + 1, &error)),
        }
    }
    Ok(spans)
}

fn repair_span_segment(path: &Path) -> io::Result<()> {
    let bytes = fs::read(path)?;
    if bytes.is_empty() || bytes.ends_with(b"\n") {
        return Ok(());
    }
    let tail_start = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let tail = &bytes[tail_start..];
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    match serde_json::from_slice::<ProcessSpan>(tail) {
        Ok(_) => {
            file.seek(SeekFrom::End(0))?;
            file.write_all(b"\n")?;
        }
        Err(error) if error.is_eof() => {
            file.set_len(u64::try_from(tail_start).unwrap_or(u64::MAX))?;
        }
        Err(error) => {
            let line = bytes[..tail_start].split(|byte| *byte == b'\n').count();
            return Err(process_line_error(path, line, &error));
        }
    }
    file.sync_data()
}

pub(crate) fn repair_span_segments(dir: &Path) -> io::Result<()> {
    for (_, path, _, _) in segments(dir, "process-spans-")? {
        repair_span_segment(&path)?;
    }
    Ok(())
}

pub(crate) fn append_spans(
    dir: &Path,
    spans: &[ProcessSpan],
    max_storage_mib: u64,
) -> io::Result<()> {
    for span in spans {
        let line = serde_json::to_vec(span).map_err(io::Error::other)?;
        let max_bytes = max_storage_mib.saturating_mul(SEGMENT_BYTES);
        let files = segments(dir, "process-spans-")?;
        let latest = files.last().filter(|(timestamp, _, _, size)| {
            *size + u64::try_from(line.len()).unwrap_or(u64::MAX) < SEGMENT_BYTES.min(max_bytes / 2)
                && *timestamp / 3_600_000 == span.last_seen_unix_ms / 3_600_000
        });
        let path = latest.map_or_else(
            || {
                dir.join(format!(
                    "process-spans-{}-{}.jsonl",
                    span.last_seen_unix_ms,
                    std::process::id()
                ))
            },
            |(_, path, _, _)| path.clone(),
        );
        #[cfg(unix)]
        let created = !path.exists();
        if path.exists() {
            repair_span_segment(&path)?;
        }
        let mut file = observe::private_file(&path, false, true)?;
        let mut record = line;
        record.push(b'\n');
        file.write_all(&record)?;
        file.sync_all()?;
        #[cfg(unix)]
        if created {
            sync_directory(dir)?;
        }
    }
    Ok(())
}

fn summary_key(
    start: u64,
    agent: &str,
    executable: &str,
    subcommand: &str,
    family: &str,
    category: &str,
    workspace_kind: &str,
) -> (u64, String, String, String, String, String, String) {
    (
        start,
        agent.to_owned(),
        executable.to_owned(),
        subcommand.to_owned(),
        family.to_owned(),
        category.to_owned(),
        workspace_kind.to_owned(),
    )
}

fn ratio(numerator_ms: u64, denominator_ms: u64) -> f64 {
    Duration::from_millis(numerator_ms).as_secs_f64()
        / Duration::from_millis(denominator_ms.max(1)).as_secs_f64()
}

fn scaled_metric(value: u64, overlap_ms: u64, duration_ms: u64) -> u64 {
    u64::try_from(
        u128::from(value).saturating_mul(u128::from(overlap_ms)) / u128::from(duration_ms.max(1)),
    )
    .unwrap_or(u64::MAX)
}

fn fold_spans(records: &[ProcessSpan], resolution: u64) -> Vec<ProcessSummary> {
    let mut groups = BTreeMap::new();
    for span in records {
        let end = span
            .last_seen_unix_ms
            .max(span.first_seen_unix_ms.saturating_add(1));
        let duration = end.saturating_sub(span.first_seen_unix_ms);
        let mut start = span.first_seen_unix_ms / resolution * resolution;
        let mut assigned_working = 0_u64;
        let mut assigned_read = 0_u64;
        let mut assigned_written = 0_u64;
        while start < end {
            let bucket_end = start.saturating_add(resolution);
            let overlap = bucket_end
                .min(end)
                .saturating_sub(start.max(span.first_seen_unix_ms));
            if overlap == 0 {
                break;
            }
            let final_bucket = bucket_end >= end;
            let working = if final_bucket {
                span.observed_working_ms.saturating_sub(assigned_working)
            } else {
                scaled_metric(span.observed_working_ms, overlap, duration)
            };
            let read = if final_bucket {
                span.read_bytes.saturating_sub(assigned_read)
            } else {
                scaled_metric(span.read_bytes, overlap, duration)
            };
            let written = if final_bucket {
                span.written_bytes.saturating_sub(assigned_written)
            } else {
                scaled_metric(span.written_bytes, overlap, duration)
            };
            assigned_working = assigned_working.saturating_add(working);
            assigned_read = assigned_read.saturating_add(read);
            assigned_written = assigned_written.saturating_add(written);
            let key = summary_key(
                start,
                &span.agent,
                &span.executable,
                &span.subcommand,
                &span.family,
                &span.category,
                &span.workspace_kind,
            );
            let summary = groups.entry(key).or_insert_with(|| ProcessSummary {
                bucket_start_unix_ms: start,
                resolution_ms: resolution,
                agent: span.agent.clone(),
                executable: span.executable.clone(),
                subcommand: span.subcommand.clone(),
                family: span.family.clone(),
                category: span.category.clone(),
                workspace_kind: span.workspace_kind.clone(),
                process_count: 0.0,
                observed_working_ms: 0,
                cpu_core_ms_estimate: 0.0,
                peak_cpu_percent: 0.0,
                peak_memory_bytes: 0,
                read_bytes: 0,
                written_bytes: 0,
            });
            let fraction = ratio(overlap, duration);
            summary.process_count += fraction;
            summary.observed_working_ms = summary.observed_working_ms.saturating_add(working);
            summary.cpu_core_ms_estimate += span.cpu_core_ms_estimate * fraction;
            summary.peak_cpu_percent = summary.peak_cpu_percent.max(span.peak_cpu_percent);
            summary.peak_memory_bytes = summary.peak_memory_bytes.max(span.peak_memory_bytes);
            summary.read_bytes = summary.read_bytes.saturating_add(read);
            summary.written_bytes = summary.written_bytes.saturating_add(written);
            start = bucket_end;
        }
    }
    groups.into_values().collect()
}

fn fold_summaries(records: &[ProcessSummary], resolution: u64) -> Vec<ProcessSummary> {
    let mut groups = BTreeMap::new();
    for record in records {
        let start = record.bucket_start_unix_ms / resolution * resolution;
        let key = summary_key(
            start,
            &record.agent,
            &record.executable,
            &record.subcommand,
            &record.family,
            &record.category,
            &record.workspace_kind,
        );
        groups
            .entry(key)
            .or_insert_with(|| ProcessSummary {
                bucket_start_unix_ms: start,
                resolution_ms: resolution,
                agent: record.agent.clone(),
                executable: record.executable.clone(),
                subcommand: record.subcommand.clone(),
                family: record.family.clone(),
                category: record.category.clone(),
                workspace_kind: record.workspace_kind.clone(),
                process_count: 0.0,
                observed_working_ms: 0,
                cpu_core_ms_estimate: 0.0,
                peak_cpu_percent: 0.0,
                peak_memory_bytes: 0,
                read_bytes: 0,
                written_bytes: 0,
            })
            .merge(record);
    }
    groups.into_values().collect()
}

pub(crate) fn compact(
    dir: &Path,
    source_prefix: &str,
    target_prefix: &str,
    source_resolution: Option<u64>,
    target_resolution: u64,
    cutoff: u64,
    failures: &mut Vec<(PathBuf, String)>,
) -> io::Result<u64> {
    let mut compacted = 0_u64;
    for (timestamp, path, suffix, _) in segments(dir, source_prefix)? {
        if timestamp >= cutoff {
            continue;
        }
        let result = (|| {
            let summaries = if let Some(expected) = source_resolution {
                let records: Vec<ProcessSummary> = read_lines(&path)?;
                if records.is_empty()
                    || records
                        .iter()
                        .any(|record| record.resolution_ms != expected)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid process summary segment",
                    ));
                }
                if records.iter().any(|record| record.end_unix_ms() >= cutoff) {
                    return Ok(false);
                }
                fold_summaries(&records, target_resolution)
            } else {
                let records = read_spans(&path)?;
                if records.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "empty process span segment",
                    ));
                }
                if records
                    .iter()
                    .any(|record| record.last_seen_unix_ms >= cutoff)
                {
                    return Ok(false);
                }
                fold_spans(&records, target_resolution)
            };
            retention::publish(
                dir,
                &dir.join(format!("{target_prefix}{suffix}")),
                &summaries,
            )?;
            fs::remove_file(&path)?;
            Ok(true)
        })();
        match result {
            Ok(true) => compacted = compacted.saturating_add(1),
            Ok(false) => {}
            Err(error) => failures.push((path, error.to_string())),
        }
    }
    Ok(compacted)
}

fn expire(
    dir: &Path,
    prefix: &str,
    cutoff: u64,
    failures: &mut Vec<(PathBuf, String)>,
) -> io::Result<()> {
    for (timestamp, path, _, _) in segments(dir, prefix)? {
        if timestamp >= cutoff {
            continue;
        }
        match read_lines::<ProcessSummary>(&path) {
            Ok(records)
                if !records.is_empty()
                    && records.iter().all(|record| record.end_unix_ms() < cutoff) =>
            {
                fs::remove_file(path)?;
            }
            Ok(_) => {}
            Err(error) => failures.push((path, error.to_string())),
        }
    }
    Ok(())
}

pub(crate) fn maintain(
    dir: &Path,
    now: u64,
    policy: retention::Policy,
    failures: &mut Vec<(PathBuf, String)>,
) -> io::Result<()> {
    compact(
        dir,
        "process-spans-",
        "process-history-5m-",
        None,
        retention::FIVE_MINUTES_MS,
        now.saturating_sub(policy.process_detail_days.saturating_mul(DAY_MS)),
        failures,
    )?;
    compact(
        dir,
        "process-history-5m-",
        "process-history-1h-",
        Some(retention::FIVE_MINUTES_MS),
        retention::ONE_HOUR_MS,
        now.saturating_sub(policy.process_five_minute_days.saturating_mul(DAY_MS)),
        failures,
    )?;
    compact(
        dir,
        "process-history-1h-",
        "process-history-1d-",
        Some(retention::ONE_HOUR_MS),
        retention::ONE_DAY_MS,
        now.saturating_sub(policy.hourly_days.saturating_mul(DAY_MS)),
        failures,
    )?;
    expire(
        dir,
        "process-history-1d-",
        now.saturating_sub(policy.daily_days.saturating_mul(DAY_MS)),
        failures,
    )
}

pub(crate) fn spans(dir: &Path) -> io::Result<Vec<ProcessSpan>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut records: Vec<ProcessSpan> = Vec::new();
    for (_, path, _, _) in segments(dir, "process-spans-")? {
        records.extend(read_spans(&path)?);
    }
    records.sort_by_key(|record| (record.last_seen_unix_ms, record.pid, record.start_time));
    Ok(records)
}

pub(crate) fn summaries(dir: &Path) -> io::Result<Vec<ProcessSummary>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let raw_suffixes: BTreeSet<_> = segments(dir, "process-spans-")?
        .into_iter()
        .map(|(_, _, suffix, _)| suffix)
        .collect();
    let mut chosen = BTreeMap::new();
    for (rank, prefix) in [
        (0, "process-history-5m-"),
        (1, "process-history-1h-"),
        (2, "process-history-1d-"),
    ] {
        for (_, path, suffix, _) in segments(dir, prefix)? {
            if raw_suffixes.contains(&suffix) {
                continue;
            }
            chosen.entry(suffix).or_insert((rank, path));
        }
    }
    let mut records: Vec<ProcessSummary> = Vec::new();
    for (_, (_, path)) in chosen {
        records.extend(read_lines(&path)?);
    }
    records.sort_by_key(|record| record.bucket_start_unix_ms);
    Ok(records)
}

pub(crate) fn checkpoint_bytes(dir: &Path) -> io::Result<u64> {
    match fs::metadata(dir.join(ACTIVE_FILE)) {
        Ok(metadata) if metadata.is_file() => Ok(metadata.len()),
        Ok(_) => Err(io::Error::other("process checkpoint is not a regular file")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    fn test_dir() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "reef-process-history-{}-{nonce}-{}",
            std::process::id(),
            NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        observe::private_dir(&dir).unwrap();
        dir
    }

    fn sample(pid: u32, cpu: f32, memory: u64, read: u64) -> ProcessSample {
        ProcessSample {
            pid,
            start_time: 10,
            parent_pid: Some(1),
            parent_start_time: Some(1),
            kind: "codex".to_owned(),
            executable: "go".to_owned(),
            subcommand: "test".to_owned(),
            family: "go test".to_owned(),
            category: "test".to_owned(),
            workspace_kind: "worktree".to_owned(),
            cpu_percent: cpu,
            memory_bytes: memory,
            total_read_bytes: read,
            total_written_bytes: 0,
        }
    }

    #[test]
    fn tracker_emits_one_bounded_span_when_a_process_exits() {
        let mut tracker = Tracker::default();
        assert!(
            tracker
                .observe(&[sample(42, 50.0, 100, 10)], 1_000, 1_000)
                .is_empty()
        );
        assert!(
            tracker
                .observe(&[sample(42, 100.0, 200, 30)], 2_000, 1_000)
                .is_empty()
        );
        let spans = tracker.observe(&[], 3_000, 1_000);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].sample_count, 2);
        assert_eq!(spans[0].observed_working_ms, 2_000);
        assert_eq!(spans[0].cpu_core_ms_estimate, 1_500.0);
        assert_eq!(spans[0].peak_memory_bytes, 200);
        assert_eq!(spans[0].read_bytes, 20);
        assert_eq!(spans[0].executable, "go");
        assert_eq!(spans[0].subcommand, "test");
    }

    #[test]
    fn folding_keeps_safe_command_and_workspace_dimensions() {
        let mut tracker = Tracker::default();
        tracker.observe(&[sample(42, 50.0, 100, 10)], 301_000, 1_000);
        let spans = tracker.observe(&[], 302_000, 1_000);
        let summaries = fold_spans(&spans, retention::FIVE_MINUTES_MS);
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].process_count, 1.0);
        assert_eq!(summaries[0].workspace_kind, "worktree");
        assert_eq!(summaries[0].executable, "go");
    }

    #[test]
    fn active_checkpoint_replaces_instead_of_appending() {
        let dir = test_dir();
        let mut tracker = Tracker::default();
        tracker.observe(&[sample(42, 50.0, 100, 10)], 1_000, 1_000);
        tracker.checkpoint(&dir).unwrap();
        tracker.observe(&[sample(42, 50.0, 100, 20)], 2_000, 1_000);
        tracker.checkpoint(&dir).unwrap();

        let records: Vec<ActiveSpan> =
            serde_json::from_reader(File::open(dir.join(ACTIVE_FILE)).unwrap()).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].sample_count, 2);
        assert_eq!(Tracker::load(&dir).unwrap().active.len(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tracker_does_not_repeat_a_span_appended_before_a_crash() {
        let dir = test_dir();
        let mut tracker = Tracker::default();
        tracker.observe(&[sample(42, 50.0, 100, 10)], 1_000, 1_000);
        tracker.checkpoint(&dir).unwrap();

        let completed = tracker.observe(&[], 2_000, 1_000);
        append_spans(&dir, &completed, 1).unwrap();

        let recovered = Tracker::recover(&dir).unwrap();
        assert!(recovered.active.is_empty());
        let mut failures = Vec::new();
        assert_eq!(
            compact(
                &dir,
                "process-spans-",
                "process-history-5m-",
                None,
                retention::FIVE_MINUTES_MS,
                u64::MAX,
                &mut failures,
            )
            .unwrap(),
            1
        );
        assert_eq!(failures, Vec::<(PathBuf, String)>::new());
        let mut recovered = Tracker::load(&dir).unwrap();
        assert!(recovered.active.is_empty());
        assert!(recovered.observe(&[], 3_000, 1_000).is_empty());
        assert!(spans(&dir).unwrap().is_empty());
        assert_eq!(summaries(&dir).unwrap().len(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn append_repairs_an_incomplete_final_record_without_losing_valid_spans() {
        let dir = test_dir();
        let mut tracker = Tracker::default();
        tracker.observe(&[sample(42, 50.0, 100, 10)], 1_000, 1_000);
        let first = tracker.observe(&[], 2_000, 1_000);
        append_spans(&dir, &first, 1).unwrap();

        let path = segments(&dir, "process-spans-").unwrap()[0].1.clone();
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"agent\":\"codex\"").unwrap();
        file.sync_data().unwrap();
        assert_eq!(read_spans(&path).unwrap().len(), 1);

        tracker.observe(&[sample(43, 25.0, 200, 20)], 3_000, 1_000);
        let second = tracker.observe(&[], 4_000, 1_000);
        append_spans(&dir, &second, 1).unwrap();

        let records = read_spans(&path).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].pid, 42);
        assert_eq!(records[1].pid, 43);
        assert!(fs::read(&path).unwrap().ends_with(b"\n"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn append_normalizes_a_complete_final_record_without_a_newline() {
        let dir = test_dir();
        let mut tracker = Tracker::default();
        tracker.observe(&[sample(42, 50.0, 100, 10)], 1_000, 1_000);
        let first = tracker.observe(&[], 2_000, 1_000);
        append_spans(&dir, &first, 1).unwrap();

        let path = segments(&dir, "process-spans-").unwrap()[0].1.clone();
        let mut bytes = fs::read(&path).unwrap();
        assert_eq!(bytes.pop(), Some(b'\n'));
        fs::write(&path, bytes).unwrap();

        tracker.observe(&[sample(43, 25.0, 200, 20)], 3_000, 1_000);
        let second = tracker.observe(&[], 4_000, 1_000);
        append_spans(&dir, &second, 1).unwrap();

        let records = read_spans(&path).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].pid, 42);
        assert_eq!(records[1].pid, 43);
        assert!(fs::read(&path).unwrap().ends_with(b"\n"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn compaction_discards_process_identity_but_keeps_cost() {
        let dir = test_dir();
        let mut tracker = Tracker::default();
        tracker.observe(&[sample(42, 50.0, 100, 10)], 1_000, 1_000);
        let completed = tracker.observe(&[], 2_000, 1_000);
        append_spans(&dir, &completed, 1).unwrap();
        let mut failures = Vec::new();
        assert_eq!(
            compact(
                &dir,
                "process-spans-",
                "process-history-5m-",
                None,
                retention::FIVE_MINUTES_MS,
                u64::MAX,
                &mut failures,
            )
            .unwrap(),
            1
        );
        assert_eq!(failures, Vec::<(PathBuf, String)>::new());
        assert!(spans(&dir).unwrap().is_empty());
        let summaries = summaries(&dir).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].process_count, 1.0);
        assert_eq!(summaries[0].cpu_core_ms_estimate, 500.0);
        let content =
            fs::read_to_string(segments(&dir, "process-history-5m-").unwrap()[0].1.clone())
                .unwrap();
        assert!(!content.contains("\"pid\""));
        assert!(!content.contains("start_time"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn folding_distributes_a_long_process_across_every_overlapped_bucket() {
        let spans = vec![ProcessSpan {
            agent: "codex".to_owned(),
            pid: 42,
            start_time: 1,
            parent_pid: Some(1),
            parent_start_time: Some(1),
            executable: "go".to_owned(),
            subcommand: "test".to_owned(),
            family: "go test".to_owned(),
            category: "test".to_owned(),
            workspace_kind: "worktree".to_owned(),
            first_seen_unix_ms: 0,
            last_seen_unix_ms: 7_200_000,
            observed_working_ms: 7_200_000,
            sample_count: 7_200,
            cpu_core_ms_estimate: 7_200_000.0,
            peak_cpu_percent: 100.0,
            peak_memory_bytes: 100,
            read_bytes: 7_200,
            written_bytes: 3_600,
            coverage: "sampled".to_owned(),
        }];
        let summaries = fold_spans(&spans, retention::ONE_HOUR_MS);
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].bucket_start_unix_ms, 0);
        assert_eq!(summaries[0].process_count, 0.5);
        assert_eq!(summaries[0].observed_working_ms, 3_600_000);
        assert_eq!(summaries[0].cpu_core_ms_estimate, 3_600_000.0);
        assert_eq!(summaries[0].read_bytes, 3_600);
        assert_eq!(summaries[1].bucket_start_unix_ms, 3_600_000);
        assert_eq!(summaries[1].process_count, 0.5);
        assert_eq!(summaries[1].observed_working_ms, 3_600_000);
        assert_eq!(summaries[1].written_bytes, 1_800);
    }
}
