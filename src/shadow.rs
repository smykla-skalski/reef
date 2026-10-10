use crate::pressure::{self, Policy};
use crate::report::{self, Observation};
use crate::schedule;
use chrono::{DateTime, Duration, Utc};
use clap::{Args, ValueEnum};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io;
use std::path::PathBuf;
use sysinfo::System;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Format {
    Markdown,
    Json,
}

#[derive(Args, Debug)]
pub struct ShadowOptions {
    /// Start of the replay range (inclusive), RFC 3339; defaults to 24 hours ago.
    #[arg(long, value_parser = parse_date)]
    from: Option<DateTime<Utc>>,
    /// End of the replay range (exclusive), RFC 3339; defaults to now.
    #[arg(long, value_parser = parse_date)]
    to: Option<DateTime<Utc>>,
    /// Private observation directory.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Hypothetical job CPU estimate; defaults to one logical core.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
    cpu: u32,
    /// Hypothetical job memory estimate in MiB.
    #[arg(long, default_value_t = 1024, value_parser = clap::value_parser!(u64).range(1..))]
    memory_mib: u64,
    /// Scheduler CPU budget; defaults to one less than the host's logical core count.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    budget_cpu: Option<u32>,
    /// Scheduler memory budget in MiB; defaults to 70% of physical memory.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    budget_memory_mib: Option<u64>,
    /// Logical CPU count to use instead of the current host count.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    cpu_cores: Option<u32>,
    /// Pressure policy matching `reef serve`.
    #[command(flatten)]
    pressure: pressure::Options,
    /// Output format.
    #[arg(long, value_enum, default_value_t = Format::Markdown)]
    format: Format,
}

fn parse_date(value: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(value)
        .map(|date| date.with_timezone(&Utc))
        .map_err(|error| format!("expected an RFC 3339 date: {error}"))
}

#[derive(Default, Serialize)]
struct DecisionCounts {
    samples: usize,
    would_admit: usize,
    would_hold: usize,
    unknown: usize,
    by_reason: BTreeMap<String, usize>,
}

impl DecisionCounts {
    fn add(&mut self, reason: Option<&str>, unknown: bool) {
        self.samples += 1;
        if unknown {
            self.unknown += 1;
        } else if let Some(reason) = reason {
            self.would_hold += 1;
            *self.by_reason.entry(reason.to_owned()).or_default() += 1;
        } else {
            self.would_admit += 1;
        }
    }
}

#[derive(Serialize)]
struct Replay {
    from: String,
    to: String,
    candidate_cpu: u32,
    candidate_memory_mib: u64,
    budget_cpu: u32,
    budget_memory_mib: u64,
    cpu_cores: u32,
    memory_total_bytes: u64,
    all_samples: DecisionCounts,
    samples_with_agents: DecisionCounts,
    by_agent: BTreeMap<String, DecisionCounts>,
    interpretation: &'static str,
}

fn analyze(
    samples: &[Observation],
    range: (DateTime<Utc>, DateTime<Utc>),
    candidate: (u32, u64),
    budget: (u32, u64),
    cpu_cores: u32,
    pressure: pressure::Options,
) -> io::Result<Replay> {
    let first = samples
        .iter()
        .find(|sample| {
            sample.at_unix_ms >= u64::try_from(range.0.timestamp_millis()).unwrap_or_default()
                && sample.at_unix_ms < u64::try_from(range.1.timestamp_millis()).unwrap_or_default()
                && sample.memory_total_bytes.is_some_and(|total| total > 0)
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "no host observations with memory capacity in range",
            )
        })?;
    let memory_total = first.memory_total_bytes.expect("checked memory capacity");
    let policy = Policy::new(pressure, cpu_cores, memory_total)
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
    if candidate.0 > budget.0 || candidate.1 > budget.1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "candidate exceeds scheduler budget",
        ));
    }
    let mut replay = Replay {
        from: range.0.to_rfc3339(),
        to: range.1.to_rfc3339(),
        candidate_cpu: candidate.0,
        candidate_memory_mib: candidate.1,
        budget_cpu: budget.0,
        budget_memory_mib: budget.1,
        cpu_cores,
        memory_total_bytes: memory_total,
        all_samples: DecisionCounts::default(),
        samples_with_agents: DecisionCounts::default(),
        by_agent: BTreeMap::new(),
        interpretation: "Snapshot-only hypothetical admission at each host sample. No command interception, delay, queue simulation, recovery-dwell inference, or causal attribution. Samples are not job arrivals; overlap with agents is not ownership of host pressure.",
    };
    let from_ms = u64::try_from(range.0.timestamp_millis()).unwrap_or_default();
    let to_ms = u64::try_from(range.1.timestamp_millis()).unwrap_or_default();
    for sample in samples
        .iter()
        .filter(|sample| sample.at_unix_ms >= from_ms && sample.at_unix_ms < to_ms)
    {
        let capacity_valid = sample.memory_total_bytes == Some(memory_total);
        let cpu = sample
            .cpu_percent
            .filter(|value| value.is_finite() && *value >= 0.0 && *value <= 100.0);
        let used = sample
            .memory_used_bytes
            .filter(|&value| value <= memory_total);
        let unknown =
            !capacity_valid || (!pressure.no_pressure && (cpu.is_none() || used.is_none()));
        let reason = if unknown || pressure.no_pressure {
            None
        } else {
            policy.snapshot_reason(cpu, used, candidate.0, candidate.1)
        };
        replay.all_samples.add(reason, unknown);
        if let Some(agents) = sample.agents.as_ref().filter(|agents| !agents.is_empty()) {
            replay.samples_with_agents.add(reason, unknown);
            let mut kinds = std::collections::BTreeSet::new();
            for agent in agents {
                kinds.insert(agent.kind.as_str());
            }
            for kind in kinds {
                replay
                    .by_agent
                    .entry(kind.to_owned())
                    .or_default()
                    .add(reason, unknown);
            }
        }
    }
    Ok(replay)
}

fn markdown(replay: &Replay) -> String {
    let mut output = format!(
        "# Reef shadow admission replay\n\n- Range: {} to {} (end exclusive)\n- Hypothetical job: {} CPU, {} MiB\n- Scheduler budget: {} CPU, {} MiB\n- Host samples: {}\n- Would admit at sample: {}\n- Would hold at sample: {}\n- Unknown: {}\n- Samples with agents: {}\n\n",
        replay.from,
        replay.to,
        replay.candidate_cpu,
        replay.candidate_memory_mib,
        replay.budget_cpu,
        replay.budget_memory_mib,
        replay.all_samples.samples,
        replay.all_samples.would_admit,
        replay.all_samples.would_hold,
        replay.all_samples.unknown,
        replay.samples_with_agents.samples,
    );
    output.push_str("## Hold reasons\n\n| Reason | Samples |\n| --- | ---: |\n");
    for (reason, count) in &replay.all_samples.by_reason {
        let _ = writeln!(output, "| {reason} | {count} |");
    }
    output.push_str("\n## Concurrent agents\n\n| Agent | Samples | Would admit | Would hold | Unknown |\n| --- | ---: | ---: | ---: | ---: |\n");
    for (agent, counts) in &replay.by_agent {
        let _ = writeln!(
            output,
            "| {agent} | {} | {} | {} | {} |",
            counts.samples, counts.would_admit, counts.would_hold, counts.unknown
        );
    }
    let _ = write!(output, "\n{}\n", replay.interpretation);
    output
}

pub fn run(options: &ShadowOptions) -> io::Result<()> {
    let to = options.to.unwrap_or_else(Utc::now);
    let from = options.from.unwrap_or(to - Duration::hours(24));
    if from >= to {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--from must precede --to",
        ));
    }
    let dir = report::state_dir(options.state_dir.as_deref())?;
    let samples = report::observations(&dir)?;
    let system = System::new_all();
    let cpu_cores = options
        .cpu_cores
        .unwrap_or(u32::try_from(system.cpus().len()).unwrap_or(u32::MAX));
    let memory_total = samples
        .iter()
        .find(|sample| {
            sample.at_unix_ms >= u64::try_from(from.timestamp_millis()).unwrap_or_default()
                && sample.at_unix_ms < u64::try_from(to.timestamp_millis()).unwrap_or_default()
                && sample.memory_total_bytes.is_some_and(|total| total > 0)
        })
        .and_then(|sample| sample.memory_total_bytes)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "no host observations with memory capacity in range",
            )
        })?;
    let defaults = schedule::default_budget(cpu_cores, memory_total);
    let replay = analyze(
        &samples,
        (from, to),
        (options.cpu, options.memory_mib),
        (
            options.budget_cpu.unwrap_or(defaults.0),
            options.budget_memory_mib.unwrap_or(defaults.1),
        ),
        cpu_cores,
        options.pressure,
    )?;
    match options.format {
        Format::Markdown => print!("{}", markdown(&replay)),
        Format::Json => println!("{}", serde_json::to_string_pretty(&replay)?),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const MIB: u64 = 1_048_576;

    fn options() -> pressure::Options {
        pressure::Options {
            no_pressure: false,
            cpu_reserve: Some(1),
            memory_reserve_mib: Some(1024),
            cpu_high_percent: 90,
            cpu_recover_percent: 70,
            memory_high_percent: 90,
            memory_recover_percent: 80,
            recovery_seconds: 5,
        }
    }

    fn observation(
        at: u64,
        cpu: Option<f64>,
        used_mib: Option<u64>,
        agents: Option<Vec<&str>>,
    ) -> Observation {
        serde_json::from_value(serde_json::json!({
            "at_unix_ms": at, "working_ms": 60_000, "cpu_percent": cpu,
            "memory_used_bytes": used_mib.map(|mib| mib * MIB),
            "memory_total_bytes": 16_384 * MIB,
            "agents": agents.map(|kinds| kinds.into_iter().map(|kind| serde_json::json!({"kind":kind,"root_pid":1,"process_count":1,"cpu_percent":0.0,"memory_bytes":0,"read_bytes":0,"written_bytes":0})).collect::<Vec<_>>())
        })).unwrap()
    }

    #[test]
    fn replay_counts_sampled_decisions_without_assigning_agent_causality() {
        let from = DateTime::from_timestamp_millis(1_000).unwrap();
        let to = DateTime::from_timestamp_millis(5_000).unwrap();
        let samples = vec![
            observation(1_000, Some(10.0), Some(4_000), Some(vec!["codex"])),
            observation(
                2_000,
                Some(95.0),
                Some(4_000),
                Some(vec!["codex", "claude"]),
            ),
            observation(3_000, None, Some(4_000), None),
            observation(4_000, Some(10.0), Some(16_000), Some(vec!["claude"])),
        ];
        let replay = analyze(&samples, (from, to), (1, 1024), (7, 11_468), 8, options()).unwrap();
        assert_eq!(replay.all_samples.would_admit, 1);
        assert_eq!(replay.all_samples.would_hold, 2);
        assert_eq!(replay.all_samples.unknown, 1);
        assert_eq!(replay.by_agent["codex"].samples, 2);
        assert_eq!(replay.by_agent["claude"].would_hold, 2);
    }
}
