use super::{Observation, Report, readable_bytes, readable_duration, readable_optional_bytes};
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;
use std::fmt::Write as _;

const CHART_WIDTH: f64 = 720.0;
const PLOT_LEFT: f64 = 64.0;
const PLOT_RIGHT: f64 = 700.0;
const PLOT_TOP: f64 = 22.0;
const PLOT_BOTTOM: f64 = 180.0;
const MAX_CHART_POINTS: usize = 800;
const MEMORY_FAMILIES: [&str; 8] = [
    "agent runtime",
    "compiler/build",
    "linter",
    "browser",
    "editor/IDE",
    "container/VM",
    "system UI",
    "other processes",
];

fn number(value: u64) -> f64 {
    let high = u32::try_from(value >> 32).unwrap_or(u32::MAX);
    let low = u32::try_from(value & 0xffff_ffff).unwrap_or(u32::MAX);
    f64::from(high) * 4_294_967_296.0 + f64::from(low)
}

#[derive(Clone, Copy)]
struct Point {
    at_ms: u64,
    value: f64,
}

pub(super) struct HostSeries {
    from_ms: u64,
    to_ms: u64,
    cpu: Vec<Point>,
    memory: Vec<Point>,
    swap: Vec<Point>,
    disk: Vec<Point>,
    memory_families: BTreeMap<&'static str, Vec<Point>>,
}

impl HostSeries {
    pub(super) fn from_samples(
        samples: &[Observation],
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Self {
        let from_ms = u64::try_from(from.timestamp_millis()).unwrap_or_default();
        let to_ms = u64::try_from(to.timestamp_millis()).unwrap_or_default();
        let mut series = Self {
            from_ms,
            to_ms,
            cpu: Vec::new(),
            memory: Vec::new(),
            swap: Vec::new(),
            disk: Vec::new(),
            memory_families: BTreeMap::new(),
        };
        for sample in samples.iter().filter(|sample| {
            sample.at_unix_ms > from_ms
                && sample.at_unix_ms.saturating_sub(sample.working_ms) < to_ms
        }) {
            let at_ms = sample.at_unix_ms.min(to_ms);
            if let Some(cpu) = sample.cpu_percent.filter(|value| value.is_finite()) {
                series.cpu.push(Point {
                    at_ms,
                    value: cpu.clamp(0.0, 100.0),
                });
            }
            if let Some((used, total)) = sample
                .memory_used_bytes
                .zip(sample.memory_total_bytes)
                .filter(|(_, total)| *total > 0)
            {
                series.memory.push(Point {
                    at_ms,
                    value: (number(used) / number(total) * 100.0).clamp(0.0, 100.0),
                });
            }
            if let Some(used) = sample.swap_used_bytes {
                series.swap.push(Point {
                    at_ms,
                    value: number(used) / 1_073_741_824.0,
                });
            }
            if let Some((available, total)) = sample
                .root_disk_available_bytes
                .zip(sample.root_disk_total_bytes)
                .filter(|(available, total)| *total > 0 && available <= total)
            {
                series.disk.push(Point {
                    at_ms,
                    value: (1.0 - number(available) / number(total)) * 100.0,
                });
            }
            if let Some(consumers) = &sample.memory_consumers {
                let mut by_family: BTreeMap<&str, u64> = BTreeMap::new();
                for consumer in consumers {
                    let family = super::safe_memory_family(&consumer.family);
                    let value = by_family.entry(family).or_default();
                    *value = value.saturating_add(consumer.rss_bytes);
                }
                for family in MEMORY_FAMILIES {
                    series
                        .memory_families
                        .entry(family)
                        .or_default()
                        .push(Point {
                            at_ms,
                            value: number(by_family.get(family).copied().unwrap_or_default())
                                / 1_073_741_824.0,
                        });
                }
            }
        }
        series
    }
}

fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for char in value.chars() {
        match char {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(char),
        }
    }
    out
}

fn display_time(value: &str) -> String {
    DateTime::parse_from_rfc3339(value).map_or_else(
        |_| value.to_owned(),
        |time| {
            time.with_timezone(&Utc)
                .format("%d %b %Y, %H:%M:%S UTC")
                .to_string()
        },
    )
}

fn compact_duration(value: &str) -> &str {
    value.split_once(" (").map_or(value, |(short, _)| short)
}

fn compact_points(points: &[Point]) -> Vec<Point> {
    if points.len() <= MAX_CHART_POINTS {
        return points.to_vec();
    }
    let bucket_size = points.len().div_ceil(MAX_CHART_POINTS / 4);
    let mut selected = Vec::with_capacity(MAX_CHART_POINTS);
    for bucket in points.chunks(bucket_size) {
        let min = bucket
            .iter()
            .enumerate()
            .min_by(|(_, left), (_, right)| left.value.total_cmp(&right.value))
            .map(|(index, _)| index)
            .unwrap_or_default();
        let max = bucket
            .iter()
            .enumerate()
            .max_by(|(_, left), (_, right)| left.value.total_cmp(&right.value))
            .map(|(index, _)| index)
            .unwrap_or_default();
        let mut indexes = [0, min, max, bucket.len() - 1];
        indexes.sort_unstable();
        for index in indexes
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
        {
            selected.push(bucket[index]);
        }
    }
    selected
}

fn gap_boundaries(points: &[Point]) -> Vec<u64> {
    let mut intervals: Vec<_> = points
        .windows(2)
        .map(|pair| pair[1].at_ms.saturating_sub(pair[0].at_ms))
        .filter(|interval| *interval > 0)
        .collect();
    if intervals.is_empty() {
        return Vec::new();
    }
    intervals.sort_unstable();
    let cadence = intervals[intervals.len() / 2].min(300_000);
    let limit = cadence.saturating_mul(3).max(60_000);
    points
        .windows(2)
        .filter(|pair| pair[1].at_ms.saturating_sub(pair[0].at_ms) > limit)
        .map(|pair| pair[1].at_ms)
        .collect()
}

#[derive(Clone, Copy)]
struct ChartSpec<'a> {
    id: &'a str,
    title: &'a str,
    points: &'a [Point],
    from_ms: u64,
    to_ms: u64,
    max_value: f64,
    unit: &'a str,
    threshold: Option<f64>,
}

fn chart(spec: ChartSpec<'_>) -> String {
    let ChartSpec {
        id,
        title,
        points,
        from_ms,
        to_ms,
        max_value,
        unit,
        threshold,
    } = spec;
    let mut out = String::new();
    let _ = write!(out, "<section class=\"chart\"><h3>{}</h3>", escape(title));
    if points.is_empty() {
        out.push_str(
            "<p class=\"empty\">No samples for this metric in the selected range.</p></section>",
        );
        return out;
    }
    let visible = compact_points(points);
    let span = number(to_ms.saturating_sub(from_ms).max(1));
    let ceiling = max_value.max(1.0);
    let gaps = gap_boundaries(points);
    let mut next_gap = 0;
    let mut path = String::new();
    let mut previous = None;
    for point in &visible {
        let x = PLOT_LEFT
            + (number(point.at_ms.saturating_sub(from_ms)) / span) * (PLOT_RIGHT - PLOT_LEFT);
        let y = PLOT_BOTTOM - point.value.min(ceiling) / ceiling * (PLOT_BOTTOM - PLOT_TOP);
        let mut crossed_gap = false;
        while next_gap < gaps.len() && gaps[next_gap] <= point.at_ms {
            crossed_gap |= previous.is_some_and(|at| gaps[next_gap] > at);
            next_gap += 1;
        }
        let command = if previous.is_some() && !crossed_gap {
            'L'
        } else {
            'M'
        };
        let _ = write!(path, "{command}{x:.1},{y:.1} ");
        previous = Some(point.at_ms);
    }
    let peak = points
        .iter()
        .map(|point| point.value)
        .fold(0.0_f64, f64::max);
    let count = u32::try_from(points.len()).unwrap_or(u32::MAX);
    let mean = points.iter().map(|point| point.value).sum::<f64>() / f64::from(count);
    let from_label =
        DateTime::<Utc>::from_timestamp_millis(i64::try_from(from_ms).unwrap_or_default())
            .map_or_else(
                || "start".to_owned(),
                |time| time.format("%d %b %H:%M UTC").to_string(),
            );
    let to_label = DateTime::<Utc>::from_timestamp_millis(i64::try_from(to_ms).unwrap_or_default())
        .map_or_else(
            || "end".to_owned(),
            |time| time.format("%d %b %H:%M UTC").to_string(),
        );
    let _ = write!(
        out,
        "<svg viewBox=\"0 0 {CHART_WIDTH} 226\" role=\"img\" aria-labelledby=\"{id}-title {id}-desc\"><title id=\"{id}-title\">{}</title><desc id=\"{id}-desc\">{} samples; average {:.1} {unit}; peak {:.1} {unit}. Gaps in collection are not connected.</desc>",
        escape(title),
        points.len(),
        mean,
        peak,
    );
    for fraction in [0.0_f64, 0.5, 1.0] {
        let y = PLOT_BOTTOM - fraction * (PLOT_BOTTOM - PLOT_TOP);
        let _ = write!(
            out,
            "<line class=\"grid\" x1=\"{PLOT_LEFT}\" x2=\"{PLOT_RIGHT}\" y1=\"{y:.1}\" y2=\"{y:.1}\"/><text class=\"axis\" x=\"5\" y=\"{:.1}\">{:.1}</text>",
            y + 4.0,
            fraction * ceiling
        );
    }
    if let Some(threshold) = threshold.filter(|value| *value <= ceiling) {
        let y = PLOT_BOTTOM - threshold / ceiling * (PLOT_BOTTOM - PLOT_TOP);
        let _ = write!(
            out,
            "<line class=\"threshold\" x1=\"{PLOT_LEFT}\" x2=\"{PLOT_RIGHT}\" y1=\"{y:.1}\" y2=\"{y:.1}\"/>"
        );
    }
    let _ = write!(
        out,
        "<path class=\"series\" d=\"{path}\"/><text class=\"axis\" x=\"{PLOT_LEFT}\" y=\"213\">{}</text><text class=\"axis\" x=\"{PLOT_RIGHT}\" y=\"213\" text-anchor=\"end\">{}</text></svg>",
        escape(&from_label),
        escape(&to_label)
    );
    let _ = write!(
        out,
        "<p class=\"chart-note\">{} samples · average {:.1} {unit} · peak {:.1} {unit}</p></section>",
        points.len(),
        mean,
        peak
    );
    out
}

fn command_cost(report: &Report) -> String {
    let mut out = String::from(
        "<section><h2>Agent command cost</h2><p>Process-family estimates from descendants of registered agents. CPU core-time, peak RSS, and I/O are sampled. A process can exit between samples; these are not exact command counts or proof of what caused host pressure.</p>",
    );
    match report.passive_commands.as_deref() {
        None => out.push_str(
            "<p class=\"empty\">Unavailable: observations predate command-family sampling.</p>",
        ),
        Some([]) => out.push_str(
            "<p class=\"empty\">No agent descendant commands were sampled in this range.</p>",
        ),
        Some(commands) => {
            let peak = commands
                .iter()
                .map(|row| row.cpu_core_ms_estimate)
                .fold(0.0_f64, f64::max)
                .max(1.0);
            out.push_str("<div class=\"table-wrap\"><table><thead><tr><th>Agent</th><th>Command family</th><th>CPU cost</th><th>Active time</th><th>Peak RSS</th><th>Read</th><th>Written</th></tr></thead><tbody>");
            for row in commands.iter().take(12) {
                let width = (row.cpu_core_ms_estimate / peak * 100.0).clamp(0.0, 100.0);
                let _ = write!(
                    out,
                    "<tr><td>{}</td><th scope=\"row\">{}</th><td><span class=\"bar\" style=\"--size:{width:.1}%\"></span>{:.0} core-ms</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                    escape(&row.agent),
                    escape(&row.family),
                    row.cpu_core_ms_estimate,
                    readable_duration(Some(row.observed_working_ms)),
                    readable_bytes(i128::from(row.peak_memory_bytes)),
                    readable_bytes(i128::from(row.read_bytes)),
                    readable_bytes(i128::from(row.written_bytes))
                );
            }
            out.push_str("</tbody></table></div>");
            if commands.len() > 12 {
                let _ = write!(
                    out,
                    "<p class=\"chart-note\">Showing the 12 highest-CPU groups of {}.</p>",
                    commands.len()
                );
            }
        }
    }
    out.push_str("</section>");
    out
}

fn agent_comparison(report: &Report) -> String {
    let mut out = String::from(
        "<section><h2>Activity-adjusted agent comparison</h2><p>CPU and I/O are normalized by observed active time. Activity mix uses sampled descendant-command CPU, not all agent CPU. Peak RSS is a peak; concurrent observations do not establish causality.</p>",
    );
    match report.agent_comparison.as_deref() {
        None => out.push_str("<p class=\"empty\">Unavailable: no passive agent observations.</p>"),
        Some([]) => out.push_str(
            "<p class=\"empty\">No registered agent process was active in this range.</p>",
        ),
        Some(agents) => {
            out.push_str("<div class=\"table-wrap\"><table><thead><tr><th>Agent</th><th>Active time</th><th>Avg CPU cores</th><th>Peak RSS</th><th>Read MiB/active h</th><th>Written MiB/active h</th><th>Top family</th><th>Activity mix</th></tr></thead><tbody>");
            for agent in agents {
                let mix = agent.activity_cpu_percent.as_ref().map_or_else(
                    || "Unavailable".to_owned(),
                    |categories| {
                        categories
                            .iter()
                            .map(|(category, percent)| {
                                format!("{} {percent:.0}%", escape(category))
                            })
                            .collect::<Vec<_>>()
                            .join(", ")
                    },
                );
                let _ = write!(
                    out,
                    "<tr><th scope=\"row\">{}</th><td>{:.2} h</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                    escape(&agent.agent),
                    agent.observed_active_hours,
                    agent
                        .average_cpu_cores_while_active
                        .map_or_else(|| "Unavailable".to_owned(), |value| format!("{value:.2}")),
                    readable_bytes(i128::from(agent.peak_memory_bytes)),
                    agent
                        .read_mib_per_active_hour
                        .map_or_else(|| "Unavailable".to_owned(), |value| format!("{value:.1}")),
                    agent
                        .written_mib_per_active_hour
                        .map_or_else(|| "Unavailable".to_owned(), |value| format!("{value:.1}")),
                    agent
                        .top_command_family
                        .as_deref()
                        .map_or_else(|| "Unavailable".to_owned(), escape),
                    mix,
                );
            }
            out.push_str("</tbody></table></div>");
        }
    }
    out.push_str("</section>");
    out
}

fn memory_consumers(report: &Report, series: &HostSeries) -> String {
    let mut out = String::from(
        "<section><h2>Memory consumers</h2><p>Process-family RSS sampled with host memory. Shared pages can appear in several processes, so the lines do not add up to host used memory. Unknown names stay in other processes.</p>",
    );
    let Some(consumers) = report.memory_consumers.as_deref() else {
        out.push_str("<p class=\"empty\">Unavailable: observations predate memory-family sampling.</p></section>");
        return out;
    };
    if consumers.is_empty() {
        out.push_str("<p class=\"empty\">No process-family samples in this range.</p></section>");
        return out;
    }
    let ceiling = consumers
        .iter()
        .map(|consumer| number(consumer.peak_rss_bytes) / 1_073_741_824.0)
        .fold(0.0_f64, f64::max)
        .max(1.0)
        .ceil();
    out.push_str("<div class=\"charts\">");
    for (index, consumer) in consumers.iter().take(5).enumerate() {
        let id = format!("memory-family-{index}");
        out.push_str(&chart(ChartSpec {
            id: &id,
            title: &consumer.family,
            points: series
                .memory_families
                .get(consumer.family.as_str())
                .map_or(&[], Vec::as_slice),
            from_ms: series.from_ms,
            to_ms: series.to_ms,
            max_value: ceiling,
            unit: "GiB RSS",
            threshold: None,
        }));
    }
    out.push_str("</div><div class=\"table-wrap\"><table><thead><tr><th>Family</th><th>Samples present</th><th>Mean RSS</th><th>Peak RSS</th><th>Largest process peak</th><th>Peak processes</th></tr></thead><tbody>");
    for consumer in consumers {
        let _ = write!(
            out,
            "<tr><th scope=\"row\">{}</th><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            escape(&consumer.family),
            consumer.samples,
            readable_bytes(i128::from(consumer.mean_rss_bytes)),
            readable_bytes(i128::from(consumer.peak_rss_bytes)),
            readable_bytes(i128::from(consumer.peak_largest_process_rss_bytes)),
            consumer.peak_process_count
        );
    }
    out.push_str("</tbody></table></div></section>");
    out
}

fn host_charts(report: &Report, series: &HostSeries) -> String {
    let mut out = String::from(
        "<p class=\"note\">Charts show sampled host state, not continuous measurements. Gaps mean Reef did not collect data. Agent overlap does not establish causation.</p><section><h2>Host time series</h2><div class=\"charts\">",
    );
    out.push_str(&chart(ChartSpec {
        id: "cpu",
        title: "CPU usage",
        points: &series.cpu,
        from_ms: series.from_ms,
        to_ms: series.to_ms,
        max_value: 100.0,
        unit: "%",
        threshold: Some(f64::from(report.pressure.cpu_threshold_percent)),
    }));
    out.push_str(&chart(ChartSpec {
        id: "memory",
        title: "Memory usage",
        points: &series.memory,
        from_ms: series.from_ms,
        to_ms: series.to_ms,
        max_value: 100.0,
        unit: "%",
        threshold: Some(f64::from(report.pressure.memory_threshold_percent)),
    }));
    let swap_max = series
        .swap
        .iter()
        .map(|point| point.value)
        .fold(0.0_f64, f64::max)
        .max(1.0)
        .ceil();
    out.push_str(&chart(ChartSpec {
        id: "swap",
        title: "Swap used",
        points: &series.swap,
        from_ms: series.from_ms,
        to_ms: series.to_ms,
        max_value: swap_max,
        unit: "GiB",
        threshold: None,
    }));
    out.push_str(&chart(ChartSpec {
        id: "disk",
        title: "Root disk used",
        points: &series.disk,
        from_ms: series.from_ms,
        to_ms: series.to_ms,
        max_value: 100.0,
        unit: "%",
        threshold: None,
    }));
    out.push_str("</div></section>");
    out
}

pub(super) fn render(report: &Report, series: &HostSeries, include_timeline: bool) -> String {
    let mut out = String::from(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; img-src data:"><title>Reef workload report</title><style>
:root{color-scheme:dark;font-family:ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;background:#0b1220;color:#e7edf6}*{box-sizing:border-box}body{margin:0 auto;max-width:1160px;padding:2rem 1.2rem 5rem}h1{font-size:2rem;margin-bottom:.3rem}h2{margin:2.2rem 0 .6rem}h3{margin:.2rem 0 1rem;font-size:1rem}p{line-height:1.55;color:#b9c7da}.muted,.chart-note{color:#92a4bb;font-size:.9rem}.cards{display:grid;grid-template-columns:repeat(auto-fit,minmax(175px,1fr));gap:.8rem;margin:1.7rem 0}.card,.chart{background:#132033;border:1px solid #29405c;border-radius:14px;padding:1rem}.card span{display:block;color:#9bb0ca;font-size:.85rem}.card strong{display:block;font-size:1.35rem;margin-top:.4rem}.charts{display:grid;grid-template-columns:repeat(auto-fit,minmax(min(100%,460px),1fr));gap:1rem}.chart svg{width:100%;height:auto}.grid{stroke:#344b66;stroke-width:1}.axis{fill:#9bb0ca;font-size:12px}.series{fill:none;stroke:#5eead4;stroke-width:2.6;stroke-linecap:round;stroke-linejoin:round}.threshold{stroke:#fbbf24;stroke-width:1.4;stroke-dasharray:5 5}.empty{border:1px dashed #405776;padding:1rem;border-radius:9px}.table-wrap{overflow-x:auto}table{width:100%;border-collapse:collapse;font-size:.92rem}th,td{text-align:left;padding:.65rem;border-bottom:1px solid #29405c;vertical-align:middle}th{color:#dbe7f8}.bar{display:inline-block;width:var(--size);min-width:2px;max-width:110px;height:.6rem;margin-right:.5rem;border-radius:10px;background:#5eead4}.note{border-left:3px solid #fbbf24;padding-left:.9rem}footer{margin-top:3rem;font-size:.85rem;color:#92a4bb}@media print{:root{color-scheme:light;background:white;color:#172033}.card,.chart{background:white;border-color:#bbb}p,.muted,.chart-note{color:#444}.axis{fill:#555}.grid{stroke:#ccc}th{color:#222}}
</style></head><body><main><h1>Reef workload report</h1>"#,
    );
    let _ = write!(
        out,
        "<p class=\"muted\"><time datetime=\"{}\">{}</time> to <time datetime=\"{}\">{}</time> (end exclusive)</p>",
        escape(&report.from),
        escape(&display_time(&report.from)),
        escape(&report.to),
        escape(&display_time(&report.to))
    );
    out.push_str("<div class=\"cards\">");
    for (label, value) in [
        (
            "Observed working time",
            readable_duration(Some(report.observed_working_ms)),
        ),
        (
            "Peak host memory",
            readable_optional_bytes(report.peak_host_memory_bytes.map(i128::from)),
        ),
        (
            "Peak root disk used",
            report
                .peak_root_disk_used_percent
                .map_or_else(|| "Unavailable".to_owned(), |value| format!("{value:.1}%")),
        ),
        (
            "Swap growth",
            readable_optional_bytes(report.swap_growth_bytes),
        ),
        (
            "Pressure time",
            readable_duration(report.pressure.any_above_ms),
        ),
        (
            "Measured commands",
            report
                .command_count
                .map_or_else(|| "Unavailable".to_owned(), |count| count.to_string()),
        ),
    ] {
        let _ = write!(
            out,
            "<div class=\"card\"><span>{}</span><strong title=\"{}\">{}</strong></div>",
            escape(label),
            escape(&value),
            escape(compact_duration(&value))
        );
    }
    out.push_str("</div>");
    out.push_str(&host_charts(report, series));
    out.push_str(&memory_consumers(report, series));
    out.push_str(&agent_comparison(report));
    out.push_str(&command_cost(report));
    if include_timeline {
        out.push_str(&pressure_timeline(report));
    }
    out.push_str("<footer>Generated locally by Reef. This page has no scripts, network dependencies, or external assets.</footer></main></body></html>");
    out
}

fn pressure_timeline(report: &Report) -> String {
    let mut out = String::new();
    {
        out.push_str("<section><h2>Pressure timeline</h2>");
        if report.timeline.is_empty() {
            out.push_str("<p class=\"empty\">No pressure intervals in this range.</p>");
        } else {
            out.push_str("<div class=\"table-wrap\"><table><thead><tr><th>From (Unix ms)</th><th>To (Unix ms)</th><th>Trigger</th><th>Overlapping agents</th></tr></thead><tbody>");
            for event in &report.timeline {
                let triggers = event.triggers.join(", ");
                let agents = event
                    .overlapping_agents
                    .as_ref()
                    .map_or_else(|| "Unavailable".to_owned(), |agents| agents.join(", "));
                let _ = write!(
                    out,
                    "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                    event.from_unix_ms,
                    event.to_unix_ms,
                    escape(&triggers),
                    escape(&agents)
                );
            }
            out.push_str("</tbody></table></div>");
        }
        out.push_str("</section>");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_html_special_characters() {
        assert_eq!(escape("<&>\"'"), "&lt;&amp;&gt;&quot;&#39;");
    }

    #[test]
    fn disk_and_memory_family_series_use_valid_private_samples() {
        let samples: Vec<Observation> = serde_json::from_value(serde_json::json!([
            {
                "at_unix_ms": 1000,
                "working_ms": 1000,
                "root_disk_available_bytes": 25,
                "root_disk_total_bytes": 100,
                "memory_consumers": [
                    {"family":"container/VM","process_count":1,"rss_bytes":1_073_741_824,"largest_process_rss_bytes":1_073_741_824},
                    {"family":"private-process-name","process_count":1,"rss_bytes":536_870_912,"largest_process_rss_bytes":536_870_912}
                ]
            },
            {
                "at_unix_ms": 2000,
                "working_ms": 1000,
                "root_disk_available_bytes": 150,
                "root_disk_total_bytes": 100,
                "memory_consumers": []
            }
        ]))
        .unwrap();
        let from = DateTime::<Utc>::from_timestamp_millis(0).unwrap();
        let to = DateTime::<Utc>::from_timestamp_millis(3000).unwrap();
        let series = HostSeries::from_samples(&samples, from, to);
        assert_eq!(series.disk.len(), 1);
        assert!((series.disk[0].value - 75.0).abs() < f64::EPSILON);
        assert_eq!(series.memory_families["container/VM"].len(), 2);
        assert_eq!(series.memory_families["container/VM"][0].value, 1.0);
        assert_eq!(series.memory_families["container/VM"][1].value, 0.0);
        assert_eq!(series.memory_families["other processes"][0].value, 0.5);
        assert!(!series.memory_families.contains_key("private-process-name"));
    }

    #[test]
    fn series_keeps_overlapping_samples_at_the_exclusive_range_end() {
        let samples: Vec<Observation> = serde_json::from_value(serde_json::json!([{
            "at_unix_ms": 2500,
            "working_ms": 1000,
            "root_disk_available_bytes": 10,
            "root_disk_total_bytes": 100,
            "memory_consumers": [{
                "family": "browser",
                "process_count": 1,
                "rss_bytes": 2_147_483_648_u64,
                "largest_process_rss_bytes": 2_147_483_648_u64
            }]
        }]))
        .unwrap();
        let from = DateTime::<Utc>::from_timestamp_millis(1000).unwrap();
        let to = DateTime::<Utc>::from_timestamp_millis(2000).unwrap();
        let series = HostSeries::from_samples(&samples, from, to);
        assert_eq!(series.disk.len(), 1);
        assert_eq!(series.disk[0].at_ms, 2000);
        assert_eq!(series.disk[0].value, 90.0);
        assert_eq!(series.memory_families["browser"][0].at_ms, 2000);
        assert_eq!(series.memory_families["browser"][0].value, 2.0);
    }

    #[test]
    fn summary_cards_hide_raw_milliseconds_but_preserve_exact_title() {
        assert_eq!(compact_duration("18h 4m 26s (65066097 ms)"), "18h 4m 26s");
        assert_eq!(compact_duration("Unavailable"), "Unavailable");
        assert_eq!(
            display_time("2026-10-10T11:50:00.225613+00:00"),
            "10 Oct 2026, 11:50:00 UTC"
        );
    }

    #[test]
    fn downsampling_preserves_extremes_and_stays_bounded() {
        let mut points: Vec<_> = (0..10_000)
            .map(|at_ms| Point { at_ms, value: 10.0 })
            .collect();
        points[5_123].value = 99.0;
        let reduced = compact_points(&points);
        assert!(reduced.len() <= MAX_CHART_POINTS);
        assert!(
            reduced
                .iter()
                .any(|point| (point.value - 99.0).abs() < f64::EPSILON)
        );
        assert_eq!(reduced.first().unwrap().at_ms, 0);
        assert_eq!(reduced.last().unwrap().at_ms, 9_999);
    }

    #[test]
    fn a_collection_gap_remains_disconnected_after_downsampling() {
        let points: Vec<_> = (0..1_000)
            .map(|index| Point {
                at_ms: index * 1_000 + u64::from(index >= 500) * 120_000,
                value: 50.0,
            })
            .collect();
        let output = chart(ChartSpec {
            id: "cpu",
            title: "CPU",
            points: &points,
            from_ms: 0,
            to_ms: 1_200_000,
            max_value: 100.0,
            unit: "%",
            threshold: None,
        });
        let path = output
            .split("class=\"series\" d=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        assert_eq!(path.matches('M').count(), 2);
    }
}
