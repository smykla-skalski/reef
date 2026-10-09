use chrono::{DateTime, Duration, Utc};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

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
    observed_working_ms: u128,
    command_measurements_available: bool,
    command_count: Option<usize>,
    categories: Option<Vec<CategoryReport>>,
    peak_host_memory_bytes: Option<u64>,
    swap_growth_bytes: Option<i128>,
    peak_command_concurrency: Option<usize>,
    pressure: PressureReport,
    timeline: Vec<PressureEvent>,
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

fn add_duration(total: &mut Option<u128>, triggered: Option<bool>, duration: u128) {
    if let Some(triggered) = triggered {
        *total = Some(total.unwrap_or_default() + u128::from(triggered) * duration);
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

fn build_report(
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    samples: Vec<Observation>,
    records: Option<Vec<Measurement>>,
    thresholds: (u8, u8, u8),
) -> Report {
    let from_ms = u128::try_from(from.timestamp_millis()).unwrap_or_default();
    let to_ms = u128::try_from(to.timestamp_millis()).unwrap_or_default();
    let samples: Vec<_> = samples
        .into_iter()
        .filter(|sample| {
            let at = u128::from(sample.at_unix_ms);
            at > from_ms && at.saturating_sub(u128::from(sample.working_ms)) < to_ms
        })
        .collect();
    let records = records.map(|records| {
        records
            .into_iter()
            .filter(|record| record.started_at_unix_ms < to_ms && record.ended_at_unix_ms > from_ms)
            .collect::<Vec<_>>()
    });
    let mut pressure = PressureReport {
        cpu_threshold_percent: thresholds.0,
        memory_threshold_percent: thresholds.1,
        swap_threshold_percent: thresholds.2,
        ..PressureReport::default()
    };
    let mut timeline = Vec::new();
    let mut observed_working_ms = 0_u128;
    for sample in &samples {
        let at = u128::from(sample.at_unix_ms);
        let end = at.min(to_ms);
        let start = at
            .saturating_sub(u128::from(sample.working_ms))
            .max(from_ms);
        let duration = end.saturating_sub(start);
        observed_working_ms += duration;
        let [cpu, memory, swap] = pressure_flags(sample, thresholds);
        add_duration(&mut pressure.cpu_above_ms, cpu, duration);
        add_duration(&mut pressure.memory_above_ms, memory, duration);
        add_duration(&mut pressure.swap_above_ms, swap, duration);
        let any = if [cpu, memory, swap].contains(&Some(true)) {
            Some(true)
        } else if [cpu, memory, swap].contains(&None) {
            None
        } else {
            Some(false)
        };
        add_duration(&mut pressure.any_above_ms, any, duration);
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
        if !triggers.is_empty() && duration > 0 {
            let overlapping_categories = records.as_ref().map(|records| {
                records
                    .iter()
                    .filter(|record| {
                        record.started_at_unix_ms < end && record.ended_at_unix_ms > start
                    })
                    .map(|record| category(&record.category))
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect()
            });
            timeline.push(PressureEvent {
                from_unix_ms: start,
                to_unix_ms: end,
                triggers,
                overlapping_categories,
            });
        }
    }
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
        empty: samples.is_empty() && command_count.unwrap_or_default() == 0,
        observation_count: samples.len(),
        observed_working_ms,
        command_measurements_available: records.is_some(),
        command_count,
        categories,
        peak_host_memory_bytes,
        swap_growth_bytes,
        peak_command_concurrency,
        pressure,
        timeline,
    }
}

fn printable<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map_or_else(|| "unavailable".to_owned(), |value| value.to_string())
}

fn markdown(report: &Report) -> String {
    let mut out = format!(
        "# Reef workload report\n\n- Range: {} to {} (end exclusive)\n- Empty: {}\n- Observations: {}\n- Observed working time: {} ms\n- Peak host memory: {} bytes\n- Swap growth: {} bytes\n- Peak command concurrency: {}\n\n",
        report.from,
        report.to,
        report.empty,
        report.observation_count,
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
            out.push_str("\nNo commands overlap the range.\n");
        }
    } else {
        out.push_str(
            "Unavailable. Pass --records with a private JSON Lines file written by reef run.\n",
        );
    }
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
                        "none measured".to_owned()
                    } else {
                        categories.join(", ")
                    }
                },
            );
            let _ = writeln!(
                out,
                "- {}..{} ms: {}; overlapping categories: {}",
                event.from_unix_ms,
                event.to_unix_ms,
                event.triggers.join(", "),
                categories
            );
        }
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
    let samples = observations(&state_dir(options.state_dir.as_deref())?)?;
    let records = if options.records.is_empty() {
        None
    } else {
        let mut all = Vec::new();
        for path in &options.records {
            all.extend(read_jsonl(path)?);
        }
        Some(all)
    };
    let report = build_report(
        from,
        to,
        samples,
        records,
        (
            options.cpu_threshold,
            options.memory_threshold,
            options.swap_threshold,
        ),
    );
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
        }
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
        let report = build_report(date(100), date(200), vec![], None, (90, 90, 1));
        assert!(report.empty);
        assert!(!report.command_measurements_available);
        assert!(report.categories.is_none());
        assert!(report.pressure.any_above_ms.is_none());
        assert!(markdown(&report).contains("Unavailable. Pass --records"));

        let measured = build_report(
            date(100),
            date(200),
            vec![sample(150, Some(0.0), None)],
            Some(vec![]),
            (90, 90, 1),
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
    fn range_excludes_commands_ending_at_start_and_clips_samples_ending_after_end() {
        let report = build_report(
            date(100),
            date(200),
            vec![sample(250, Some(95.0), None)],
            Some(vec![command("build", 0, 100, 1)]),
            (90, 90, 1),
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
            }],
            None,
            (90, 90, 1),
        );
        assert_eq!(report.pressure.cpu_above_ms, Some(0));
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
        );
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("secret-command-arguments"));
        assert_eq!(report.categories.unwrap()[0].category, "other");
    }
}
