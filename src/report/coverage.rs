use super::{AgentRollup, AgentTick, Observation, process_history};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io;
use std::path::Path;

#[derive(Serialize)]
pub(super) struct CollectionCoverage {
    agent_tick_count: usize,
    legacy_agent_observation_count: usize,
    agent_rollup_count: usize,
    agent_sampling_interval_ms: Option<IntervalRange>,
    agent_tick_working_interval_ms: Option<IntervalRange>,
    retained_agent_tick_working_ms: Option<u128>,
    range_without_retained_agent_ticks_ms: Option<u128>,
    agent_rollup_resolutions_ms: BTreeMap<u64, usize>,
    raw_agent_resource_samples: Option<ResourceAvailability>,
    retained_process_spans: Option<usize>,
    retained_process_samples: Option<u64>,
    process_summary_rows: Option<usize>,
    process_summary_resolutions_ms: BTreeMap<u64, usize>,
    process_detail: &'static str,
    active_checkpoint_present: bool,
    total_command_capture_percent: Option<f64>,
    limitations: Vec<&'static str>,
}

#[derive(Serialize)]
struct IntervalRange {
    minimum: u64,
    maximum: u64,
}

#[derive(Default, Serialize)]
struct ResourceAvailability {
    cpu_measured: usize,
    cpu_unavailable: usize,
    read_bytes_measured: usize,
    read_bytes_unavailable: usize,
    written_bytes_measured: usize,
    written_bytes_unavailable: usize,
}

fn resource_availability(
    samples: &[Observation],
    ticks: &[AgentTick],
) -> Option<ResourceAvailability> {
    let mut result = ResourceAvailability::default();
    let mut available = !ticks.is_empty();
    for sample in samples {
        available |= sample.agents.is_some();
    }
    for agent in ticks.iter().flat_map(|tick| &tick.agents).chain(
        samples
            .iter()
            .filter_map(|sample| sample.agents.as_ref())
            .flatten(),
    ) {
        result.cpu_measured += usize::from(agent.cpu_percent.is_some());
        result.cpu_unavailable += usize::from(agent.cpu_percent.is_none());
        result.read_bytes_measured += usize::from(agent.read_bytes.is_some());
        result.read_bytes_unavailable += usize::from(agent.read_bytes.is_none());
        result.written_bytes_measured += usize::from(agent.written_bytes.is_some());
        result.written_bytes_unavailable += usize::from(agent.written_bytes.is_none());
    }
    available.then_some(result)
}

impl CollectionCoverage {
    pub(super) fn new(
        samples: &[Observation],
        ticks: &[AgentTick],
        rollups: &[AgentRollup],
        from: u128,
        to: u128,
    ) -> Self {
        let intervals: Vec<_> = ticks
            .iter()
            .map(|tick| tick.working_ms)
            .filter(|ms| *ms > 0)
            .collect();
        let agent_tick_working_interval_ms = intervals
            .iter()
            .min()
            .zip(intervals.iter().max())
            .map(|(min, max)| IntervalRange {
                minimum: *min,
                maximum: *max,
            });
        let mut agent_rollup_resolutions_ms = BTreeMap::new();
        for rollup in rollups {
            *agent_rollup_resolutions_ms
                .entry(rollup.resolution_ms)
                .or_default() += 1;
        }
        Self {
            agent_tick_count: ticks.len(),
            legacy_agent_observation_count: samples
                .iter()
                .filter(|sample| sample.agents.is_some())
                .count(),
            agent_rollup_count: rollups.len(),
            agent_sampling_interval_ms: sampling_intervals(ticks),
            agent_tick_working_interval_ms,
            retained_agent_tick_working_ms: (!ticks.is_empty())
                .then(|| tick_working_ms(ticks, from, to)),
            range_without_retained_agent_ticks_ms: (!ticks.is_empty()).then(|| {
                to.saturating_sub(from)
                    .saturating_sub(tick_working_ms(ticks, from, to))
            }),
            agent_rollup_resolutions_ms,
            raw_agent_resource_samples: resource_availability(samples, ticks),
            retained_process_spans: None,
            retained_process_samples: None,
            process_summary_rows: None,
            process_summary_resolutions_ms: BTreeMap::new(),
            process_detail: "unavailable",
            active_checkpoint_present: false,
            total_command_capture_percent: None,
            limitations: vec![
                "Total-command capture is unknown: short processes can start and exit between samples. Counts describe retained observations, not all executions.",
                "Observed tick spacing includes gaps within each recorder segment. Recorded working intervals are capped elapsed durations, not proof of continuous monitoring. Sleep and collection downtime are not measured commands.",
                "Compaction removes individual timing and process samples; summary rows are not execution counts. Retained counts include whole rows overlapping the range.",
                "Missing tool/resource metrics and unavailable native command measurements are unknown, not zero. Legacy aggregate resource totals cannot prove complete measurement.",
            ],
        }
    }

    pub(super) fn process_detail(
        &mut self,
        spans: &[process_history::ProcessSpan],
        summaries: &[process_history::ProcessSummary],
        from: u128,
        to: u128,
        dir: &Path,
    ) -> io::Result<()> {
        let mut raw_available = false;
        let mut summaries_available = false;
        if dir.exists() {
            for entry in std::fs::read_dir(dir)? {
                let entry = entry?;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                raw_available |= name.starts_with("process-spans-") && name.ends_with(".jsonl");
                summaries_available |=
                    name.starts_with("process-history-") && name.ends_with(".jsonl");
            }
        }
        self.active_checkpoint_present = dir.join("process-active.json").try_exists()?;
        let spans: Vec<_> = spans
            .iter()
            .filter(|span| {
                u128::from(span.first_seen_unix_ms) < to
                    && u128::from(span.last_seen_unix_ms) > from
            })
            .collect();
        let summaries: Vec<_> = summaries
            .iter()
            .filter(|summary| {
                u128::from(summary.bucket_start_unix_ms) < to
                    && u128::from(summary.end_unix_ms()) > from
            })
            .collect();
        self.retained_process_spans = raw_available.then_some(spans.len());
        self.retained_process_samples = raw_available.then(|| {
            spans
                .iter()
                .fold(0_u64, |count, span| count.saturating_add(span.sample_count))
        });
        self.process_summary_rows = summaries_available.then_some(summaries.len());
        for summary in &summaries {
            *self
                .process_summary_resolutions_ms
                .entry(summary.resolution_ms)
                .or_default() += 1;
        }
        self.process_detail = match (!spans.is_empty(), !summaries.is_empty()) {
            (true, true) => "mixed individual spans and compacted summaries",
            (true, false) => "individual sampled spans",
            (false, true) => "compacted summaries only",
            (false, false) if raw_available || summaries_available => {
                "no retained process rows in range"
            }
            _ => "unavailable",
        };
        Ok(())
    }

    pub(super) fn rows(&self) -> Vec<(&'static str, String)> {
        let interval = self.agent_sampling_interval_ms.as_ref().map_or_else(
            || "unavailable (requires two distinct ticks from one recorder)".to_owned(),
            |range| format!("{}–{} ms", range.minimum, range.maximum),
        );
        let working_interval = self.agent_tick_working_interval_ms.as_ref().map_or_else(
            || "unavailable".to_owned(),
            |range| format!("{}–{} ms", range.minimum, range.maximum),
        );
        let mut rows = vec![
            ("Observed agent tick spacing", interval),
            ("Agent recorded working interval", working_interval),
            (
                "Retained agent tick working time",
                format!(
                    "{} ms; range without retained ticks: {} ms (includes compacted or unavailable detail)",
                    optional(self.retained_agent_tick_working_ms),
                    optional(self.range_without_retained_agent_ticks_ms)
                ),
            ),
            (
                "Retained agent detail",
                format!(
                    "{} ticks; {} host observations with legacy agent fields; {} rollup rows",
                    self.agent_tick_count,
                    self.legacy_agent_observation_count,
                    self.agent_rollup_count
                ),
            ),
            (
                "Agent rollup resolution",
                resolutions(&self.agent_rollup_resolutions_ms),
            ),
            ("Process detail", self.process_detail.to_owned()),
            (
                "Retained process spans / samples",
                format!(
                    "{} / {}",
                    optional(self.retained_process_spans),
                    optional(self.retained_process_samples)
                ),
            ),
            (
                "Compacted process rows",
                optional(self.process_summary_rows),
            ),
            (
                "Process summary resolution",
                resolutions(&self.process_summary_resolutions_ms),
            ),
            ("Total-command capture", "unknown".to_owned()),
        ];
        if self.active_checkpoint_present {
            rows.push((
                "Active checkpoint",
                "present; unfinished processes are excluded until spans are finalized".to_owned(),
            ));
        }
        if let Some(metrics) = &self.raw_agent_resource_samples {
            rows.push(("Raw agent resource rows", format!("CPU: {} measured / {} unavailable; reads: {} / {}; writes: {} / {} (measured / unavailable)", metrics.cpu_measured, metrics.cpu_unavailable, metrics.read_bytes_measured, metrics.read_bytes_unavailable, metrics.written_bytes_measured, metrics.written_bytes_unavailable)));
        } else {
            rows.push((
                "Raw agent resource rows",
                "unavailable; aggregate resource completeness is unknown".to_owned(),
            ));
        }
        rows
    }

    pub(super) fn limitations(&self) -> &[&'static str] {
        &self.limitations
    }

    pub(super) fn markdown(&self) -> String {
        let mut out = "## Collection coverage\n\n".to_owned();
        for (label, value) in self.rows() {
            let _ = writeln!(out, "- {label}: {value}");
        }
        out.push('\n');
        for limitation in &self.limitations {
            let _ = writeln!(out, "- {limitation}");
        }
        out.push('\n');
        out
    }
}

fn sampling_intervals(ticks: &[AgentTick]) -> Option<IntervalRange> {
    let mut by_source: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
    for tick in ticks {
        by_source
            .entry(&tick.source_suffix)
            .or_default()
            .push(tick.at_unix_ms);
    }
    let mut intervals = Vec::new();
    for timestamps in by_source.values_mut() {
        timestamps.sort_unstable();
        timestamps.dedup();
        intervals.extend(timestamps.windows(2).map(|pair| pair[1] - pair[0]));
    }
    intervals
        .iter()
        .min()
        .zip(intervals.iter().max())
        .map(|(min, max)| IntervalRange {
            minimum: *min,
            maximum: *max,
        })
}

fn optional<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map_or_else(|| "unavailable".to_owned(), |value| value.to_string())
}

fn tick_working_ms(ticks: &[AgentTick], from: u128, to: u128) -> u128 {
    let mut intervals: Vec<_> = ticks
        .iter()
        .map(|tick| {
            (
                u128::from(tick.at_unix_ms.saturating_sub(tick.working_ms)).max(from),
                u128::from(tick.at_unix_ms).min(to),
            )
        })
        .collect();
    intervals.sort_unstable();
    let mut end = from;
    let mut total = 0_u128;
    for (start, next_end) in intervals {
        total = total.saturating_add(next_end.saturating_sub(start.max(end)));
        end = end.max(next_end);
    }
    total
}

fn resolutions(values: &BTreeMap<u64, usize>) -> String {
    if values.is_empty() {
        return "unavailable".to_owned();
    }
    values
        .iter()
        .map(|(ms, count)| format!("{ms} ms: {count} rows"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cadence_uses_distinct_timestamps_within_one_source() {
        let tick = |at_unix_ms, source: &str| AgentTick {
            at_unix_ms,
            working_ms: 200,
            agents: vec![],
            commands: None,
            source_suffix: source.to_owned(),
        };
        let ticks = [
            tick(1_000, "first"),
            tick(1_000, "first"),
            tick(1_100, "second"),
            tick(1_300, "first"),
            tick(3_000, "first"),
        ];
        let coverage = CollectionCoverage::new(&[], &ticks, &[], 0, 4_000);
        let cadence = coverage.agent_sampling_interval_ms.unwrap();
        assert_eq!(cadence.minimum, 300);
        assert_eq!(cadence.maximum, 1_700);
        assert_eq!(coverage.retained_agent_tick_working_ms, Some(700));
        assert_eq!(coverage.range_without_retained_agent_ticks_ms, Some(3_300));
    }
}
