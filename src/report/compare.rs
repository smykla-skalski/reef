use super::*;

const HOUR_MS: u128 = 3_600_000;

#[derive(Debug, clap::Args)]
pub struct CompareOptions {
    /// Start of the baseline period, inclusive, in RFC 3339 format.
    #[arg(long, value_parser = parse_date)]
    baseline_from: DateTime<Utc>,
    /// End of the baseline period, exclusive, in RFC 3339 format.
    #[arg(long, value_parser = parse_date)]
    baseline_to: DateTime<Utc>,
    /// Start of the comparison period, inclusive, in RFC 3339 format.
    #[arg(long, value_parser = parse_date)]
    comparison_from: DateTime<Utc>,
    /// End of the comparison period, exclusive, in RFC 3339 format.
    #[arg(long, value_parser = parse_date)]
    comparison_to: DateTime<Utc>,
    /// Observation directory. Defaults to ~/.local/state/reef/observe.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Private JSON Lines command history file; may be repeated.
    #[arg(long = "records")]
    records: Vec<PathBuf>,
    /// Report format.
    #[arg(long, value_enum, default_value_t = CompareFormat::Markdown)]
    format: CompareFormat,
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
enum CompareFormat {
    Markdown,
    Json,
}

#[derive(Serialize)]
struct ComparisonReport {
    interpretation: &'static str,
    baseline: Period,
    comparison: Period,
}

#[derive(Serialize)]
struct Period {
    from: String,
    to: String,
    observation_count: usize,
    observed_working_ms: Option<u128>,
    pressure_ms_per_observed_hour: PressureRates,
    swap_growth_bytes: Option<i128>,
    command_history_available: bool,
    completed_command_count: Option<usize>,
    command_categories: Option<Vec<CommandCategory>>,
    pressure_without_measured_command_overlap_ms: Option<u128>,
    pressure_without_measured_command_overlap_percent: Option<f64>,
}

#[derive(Serialize)]
struct PressureRates {
    cpu: Option<u128>,
    memory: Option<u128>,
    swap: Option<u128>,
    any: Option<u128>,
}

#[derive(Serialize)]
struct CommandCategory {
    category: &'static str,
    count: usize,
    wall_ms: u128,
    cpu_ms: u128,
}

fn rate(duration: Option<u128>, observed: u128) -> Option<u128> {
    duration
        .filter(|_| observed > 0)
        .map(|value| value.saturating_mul(HOUR_MS) / observed)
}

fn command_intervals(records: &[Measurement], from: u128, to: u128) -> Vec<(u128, u128)> {
    let mut intervals: Vec<_> = records
        .iter()
        .filter_map(|record| {
            let overlap_start = record.started_at_unix_ms.max(from);
            let overlap_end = record.ended_at_unix_ms.min(to);
            (overlap_start < overlap_end).then_some((overlap_start, overlap_end))
        })
        .collect();
    intervals.sort_unstable();
    let mut merged: Vec<(u128, u128)> = Vec::new();
    for (start, end) in intervals {
        if let Some((_, last_end)) = merged.last_mut()
            && start <= *last_end
        {
            *last_end = (*last_end).max(end);
        } else {
            merged.push((start, end));
        }
    }
    merged
}

fn without_overlap(start: u128, end: u128, intervals: &[(u128, u128)]) -> u128 {
    let mut covered = 0_u128;
    let first = intervals.partition_point(|(_, finish)| *finish <= start);
    for (begin, finish) in &intervals[first..] {
        if *begin >= end {
            break;
        }
        covered += finish.min(&end).saturating_sub((*begin).max(start));
    }
    end.saturating_sub(start).saturating_sub(covered)
}

fn categories(records: &[Measurement], from_ms: u128, to_ms: u128) -> Vec<CommandCategory> {
    let mut groups: BTreeMap<&'static str, CommandCategory> = BTreeMap::new();
    for record in records {
        if record.ended_at_unix_ms < from_ms || record.ended_at_unix_ms >= to_ms {
            continue;
        }
        let name = category(&record.category);
        let group = groups.entry(name).or_insert(CommandCategory {
            category: name,
            count: 0,
            wall_ms: 0,
            cpu_ms: 0,
        });
        group.count += 1;
        group.wall_ms = group.wall_ms.saturating_add(record.wall_ms);
        group.cpu_ms = group.cpu_ms.saturating_add(record.tree_cpu_ms);
    }
    groups.into_values().collect()
}

fn period(
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    samples: &[Observation],
    records: Option<&[Measurement]>,
    thresholds: (u8, u8, u8),
) -> Period {
    let from_ms = u128::try_from(from.timestamp_millis()).unwrap_or_default();
    let to_ms = u128::try_from(to.timestamp_millis()).unwrap_or_default();
    let mut covered_until = from_ms;
    let mut count = 0;
    let mut observed = 0_u128;
    let mut pressure = [None; 4];
    let mut missing = [false; 4];
    let mut without_command = 0_u128;
    let mut first_swap = None;
    let mut last_swap = None;
    let mut swap_samples = 0;
    let intervals = records.map(|records| command_intervals(records, from_ms, to_ms));
    for sample in samples {
        let at = u128::from(sample.at_unix_ms);
        if at < from_ms || at >= to_ms {
            continue;
        }
        if let Some(swap) = sample.swap_used_bytes {
            first_swap.get_or_insert(swap);
            last_swap = Some(swap);
            swap_samples += 1;
        }
    }
    for sample in samples {
        let at = u128::from(sample.at_unix_ms);
        if at <= from_ms || at.saturating_sub(u128::from(sample.working_ms)) >= to_ms {
            continue;
        }
        count += 1;
        let end = at.min(to_ms);
        let start = at
            .saturating_sub(u128::from(sample.working_ms))
            .max(covered_until)
            .max(from_ms);
        let duration = end.saturating_sub(start);
        covered_until = covered_until.max(end);
        observed += duration;
        let [cpu, memory, swap] = pressure_flags(sample, thresholds);
        let any = any_pressure([cpu, memory, swap]);
        for (index, triggered) in [cpu, memory, swap, any].into_iter().enumerate() {
            add_duration(
                &mut pressure[index],
                &mut missing[index],
                triggered,
                duration,
            );
        }
        if any == Some(true)
            && let Some(intervals) = &intervals
        {
            without_command += without_overlap(start, end, intervals);
        }
    }
    let any_above = pressure[3];
    let pressure_without_measured_command_overlap_ms = records
        .filter(|_| observed > 0)
        .and(any_above.map(|_| without_command));
    let pressure_without_measured_command_overlap_percent =
        pressure_without_measured_command_overlap_ms
            .zip(any_above)
            .and_then(|(unattributed, total)| (total > 0).then(|| percentage(unattributed, total)));
    let command_categories = records.map(|records| categories(records, from_ms, to_ms));
    let completed_command_count = command_categories
        .as_ref()
        .map(|groups| groups.iter().map(|group| group.count).sum());
    Period {
        from: from.to_rfc3339(),
        to: to.to_rfc3339(),
        observation_count: count,
        observed_working_ms: (observed > 0).then_some(observed),
        pressure_ms_per_observed_hour: PressureRates {
            cpu: rate(pressure[0], observed),
            memory: rate(pressure[1], observed),
            swap: rate(pressure[2], observed),
            any: rate(any_above, observed),
        },
        swap_growth_bytes: first_swap
            .zip(last_swap)
            .filter(|_| swap_samples >= 2)
            .map(|(first, last)| i128::from(last) - i128::from(first)),
        command_history_available: records.is_some(),
        completed_command_count,
        command_categories,
        pressure_without_measured_command_overlap_ms,
        pressure_without_measured_command_overlap_percent,
    }
}

fn markdown(comparison: &ComparisonReport) -> String {
    let baseline = &comparison.baseline;
    let later = &comparison.comparison;
    let mut out = String::from(
        "# Reef period comparison\n\nObservational difference only; this report does not establish that Reef caused a change. Pressure rates are milliseconds above threshold per observed working hour, not per calendar hour.\n\n",
    );
    out.push_str("| Metric | Baseline | Comparison |\n| --- | ---: | ---: |\n");
    for (label, left, right) in [
        (
            "Range (end exclusive)",
            format!("{} to {}", baseline.from, baseline.to),
            format!("{} to {}", later.from, later.to),
        ),
        (
            "Observations",
            baseline.observation_count.to_string(),
            later.observation_count.to_string(),
        ),
        (
            "Observed working ms",
            printable(baseline.observed_working_ms),
            printable(later.observed_working_ms),
        ),
        (
            "CPU pressure ms/observed hour",
            printable(baseline.pressure_ms_per_observed_hour.cpu),
            printable(later.pressure_ms_per_observed_hour.cpu),
        ),
        (
            "Memory pressure ms/observed hour",
            printable(baseline.pressure_ms_per_observed_hour.memory),
            printable(later.pressure_ms_per_observed_hour.memory),
        ),
        (
            "Swap pressure ms/observed hour",
            printable(baseline.pressure_ms_per_observed_hour.swap),
            printable(later.pressure_ms_per_observed_hour.swap),
        ),
        (
            "Any pressure ms/observed hour",
            printable(baseline.pressure_ms_per_observed_hour.any),
            printable(later.pressure_ms_per_observed_hour.any),
        ),
        (
            "Swap growth",
            readable_optional_bytes(baseline.swap_growth_bytes),
            readable_optional_bytes(later.swap_growth_bytes),
        ),
        (
            "Completed commands",
            printable(baseline.completed_command_count),
            printable(later.completed_command_count),
        ),
        (
            "Pressure with no measured command overlap",
            printable(
                baseline
                    .pressure_without_measured_command_overlap_percent
                    .map(|value| format!("{value:.1}%")),
            ),
            printable(
                later
                    .pressure_without_measured_command_overlap_percent
                    .map(|value| format!("{value:.1}%")),
            ),
        ),
    ] {
        let _ = writeln!(out, "| {label} | {left} | {right} |");
    }
    out.push_str("\nCommand durations count whole commands that finished in each period. Concurrent wall times can exceed elapsed time. A missing observation or command history is unavailable, not zero.\n\n");
    out.push_str("| Period | Category | Completed | Wall ms | CPU ms |\n| --- | --- | ---: | ---: | ---: |\n");
    for (label, period) in [("Baseline", baseline), ("Comparison", later)] {
        match &period.command_categories {
            Some(groups) if groups.is_empty() => {
                let _ = writeln!(out, "| {label} | none | 0 | 0 | 0 |");
            }
            Some(groups) => {
                for group in groups {
                    let _ = writeln!(
                        out,
                        "| {label} | {} | {} | {} | {} |",
                        group.category, group.count, group.wall_ms, group.cpu_ms
                    );
                }
            }
            None => {
                let _ = writeln!(
                    out,
                    "| {label} | unavailable | unavailable | unavailable | unavailable |"
                );
            }
        }
    }
    out
}

pub fn run_compare(options: &CompareOptions) -> io::Result<()> {
    if options.baseline_from >= options.baseline_to
        || options.baseline_to > options.comparison_from
        || options.comparison_from >= options.comparison_to
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "periods must be ordered, non-overlapping, and nonempty",
        ));
    }
    if [
        options.cpu_threshold,
        options.memory_threshold,
        options.swap_threshold,
    ]
    .iter()
    .any(|value| *value > 100)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "pressure thresholds must be between 0 and 100",
        ));
    }
    let samples = observations(&state_dir(options.state_dir.as_deref())?)?;
    let default_paths = history::paths()?;
    let explicit_records = !options.records.is_empty();
    let records = if default_paths.is_none() && !explicit_records {
        None
    } else {
        let mut all: Vec<Measurement> = Vec::new();
        let mut seen = BTreeSet::new();
        for path in default_paths.iter().flatten().chain(options.records.iter()) {
            let canonical = fs::canonicalize(path)?;
            if seen.insert(canonical.clone()) {
                all.extend(read_jsonl(&canonical)?);
            }
        }
        Some(all)
    };
    // Retention prunes by the history file's save timestamp, not by a
    // command's start. A long retained command can start before a shorter
    // pruned command, so record start times cannot establish coverage.
    let default_history_start = (!explicit_records)
        .then(|| {
            default_paths.as_ref().and_then(|paths| {
                paths
                    .iter()
                    .filter_map(|path| {
                        path.file_name()?
                            .to_str()?
                            .strip_prefix("command-")?
                            .split('-')
                            .next()?
                            .parse::<u128>()
                            .ok()
                    })
                    .min()
            })
        })
        .flatten();
    let records_for = |from: DateTime<Utc>| {
        let from_ms = u128::try_from(from.timestamp_millis()).unwrap_or_default();
        (explicit_records || default_history_start.is_some_and(|start| from_ms > start))
            .then_some(records.as_deref())
            .flatten()
    };
    let thresholds = (
        options.cpu_threshold,
        options.memory_threshold,
        options.swap_threshold,
    );
    let comparison = ComparisonReport {
        interpretation: "observational_not_causal",
        baseline: period(
            options.baseline_from,
            options.baseline_to,
            &samples,
            records_for(options.baseline_from),
            thresholds,
        ),
        comparison: period(
            options.comparison_from,
            options.comparison_to,
            &samples,
            records_for(options.comparison_from),
            thresholds,
        ),
    };
    match options.format {
        CompareFormat::Markdown => print!("{}", markdown(&comparison)),
        CompareFormat::Json => println!("{}", serde_json::to_string_pretty(&comparison)?),
    }
    Ok(())
}
