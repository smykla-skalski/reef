use clap::Args;
use std::time::{Duration, Instant};
use sysinfo::System;

const MIB: u64 = 1_048_576;

#[derive(Args, Clone, Copy, Debug)]
pub struct Options {
    /// Disable live pressure admission control.
    #[arg(long)]
    pub no_pressure: bool,
    /// Logical CPU cores reserved for interactive applications.
    #[arg(long)]
    pub cpu_reserve: Option<u32>,
    /// MiB of physical memory reserved for interactive applications.
    #[arg(long)]
    pub memory_reserve_mib: Option<u64>,
    /// CPU utilization that pauses admission.
    #[arg(long, default_value_t = 90)]
    pub cpu_high_percent: u8,
    /// CPU utilization required to resume admission.
    #[arg(long, default_value_t = 70)]
    pub cpu_recover_percent: u8,
    /// Memory utilization that pauses admission.
    #[arg(long, default_value_t = 90)]
    pub memory_high_percent: u8,
    /// Memory utilization required to resume admission.
    #[arg(long, default_value_t = 80)]
    pub memory_recover_percent: u8,
    /// Seconds below both recovery thresholds before admission resumes.
    #[arg(long, default_value_t = 5)]
    pub recovery_seconds: u64,
}

#[derive(Clone, Copy)]
pub struct Policy {
    cpu_cores: u32,
    memory_total: u64,
    cpu_reserve: u32,
    memory_reserve: u64,
    cpu_high: u8,
    cpu_recover: u8,
    memory_high: u8,
    memory_recover: u8,
    recovery: Duration,
}

#[derive(Clone, Copy)]
pub struct Sample {
    pub at: Instant,
    pub cpu_percent: Option<f32>,
    pub memory_used: Option<u64>,
}

pub struct Monitor {
    policy: Policy,
    sample: Option<Sample>,
    held: bool,
    recovering_since: Option<Instant>,
    critical: bool,
    last_critical_notice: Option<Instant>,
}

impl Policy {
    pub fn new(options: Options, cpu_cores: u32, memory_total: u64) -> Result<Self, String> {
        let reserve_mib = options
            .memory_reserve_mib
            .unwrap_or_else(|| (memory_total / MIB / 10).max(1));
        let memory_reserve = reserve_mib
            .checked_mul(MIB)
            .ok_or("memory reserve overflow")?;
        if cpu_cores == 0 || memory_total == 0 {
            return Err("machine CPU and memory capacity must be available".into());
        }
        let cpu_reserve = options.cpu_reserve.unwrap_or(u32::from(cpu_cores > 2));
        if cpu_reserve >= cpu_cores || memory_reserve >= memory_total {
            return Err("interactive reserve must be smaller than machine capacity".into());
        }
        if options.cpu_high_percent == 0
            || options.cpu_high_percent > 100
            || options.memory_high_percent == 0
            || options.memory_high_percent > 100
            || options.cpu_recover_percent == 0
            || options.cpu_recover_percent >= options.cpu_high_percent
            || options.memory_recover_percent == 0
            || options.memory_recover_percent >= options.memory_high_percent
            || options.recovery_seconds == 0
        {
            return Err("invalid pressure thresholds or recovery duration".into());
        }
        Ok(Self {
            cpu_cores,
            memory_total,
            cpu_reserve,
            memory_reserve,
            cpu_high: options.cpu_high_percent,
            cpu_recover: options.cpu_recover_percent,
            memory_high: options.memory_high_percent,
            memory_recover: options.memory_recover_percent,
            recovery: Duration::from_secs(options.recovery_seconds),
        })
    }
}

impl Monitor {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            sample: None,
            held: true,
            recovering_since: None,
            critical: false,
            last_critical_notice: None,
        }
    }

    pub fn update(&mut self, sample: Sample) -> bool {
        let cpu = sample
            .cpu_percent
            .filter(|value| value.is_finite() && *value >= 0.0);
        let memory = sample
            .memory_used
            .filter(|&value| value <= self.policy.memory_total);
        let memory_high = memory.is_none_or(|used| {
            u128::from(used) * 100
                >= u128::from(self.policy.memory_total) * u128::from(self.policy.memory_high)
        });
        let memory_recovered = memory.is_some_and(|used| {
            u128::from(used) * 100
                < u128::from(self.policy.memory_total) * u128::from(self.policy.memory_recover)
        });
        let high = cpu.is_none_or(|value| value >= f32::from(self.policy.cpu_high)) || memory_high;
        let recovered =
            cpu.is_some_and(|value| value < f32::from(self.policy.cpu_recover)) && memory_recovered;
        if high {
            self.held = true;
            self.recovering_since = None;
        } else if self.held && recovered {
            let start = self.recovering_since.get_or_insert(sample.at);
            if sample.at.saturating_duration_since(*start) >= self.policy.recovery {
                self.held = false;
            }
        } else if self.held {
            self.recovering_since = None;
        }
        let critical = cpu.is_some_and(|value| value >= 98.0)
            || memory.is_some_and(|used| {
                u128::from(used) * 100 >= u128::from(self.policy.memory_total) * 97
            });
        let notify = critical
            && !self.critical
            && self.last_critical_notice.is_none_or(|at| {
                sample.at.saturating_duration_since(at) >= Duration::from_secs(60)
            });
        if notify {
            self.last_critical_notice = Some(sample.at);
        }
        self.critical = critical;
        self.sample = Some(sample);
        notify
    }

    pub fn wait_reason(
        &self,
        cpu: u32,
        memory_mib: u64,
        running_cpu: u32,
        running_memory_mib: u64,
        now: Instant,
    ) -> Option<&'static str> {
        let Some(sample) = self.sample else {
            return Some("waiting for a live pressure sample");
        };
        if now.saturating_duration_since(sample.at) > Duration::from_secs(3) {
            return Some("pressure sample is stale");
        }
        let Some(cpu_percent) = sample.cpu_percent else {
            return Some("CPU pressure is unavailable");
        };
        let Some(memory_used) = sample.memory_used else {
            return Some("memory pressure is unavailable");
        };
        if self.held {
            return Some("machine pressure is above the recovery threshold");
        }
        let idle_cores = (1.0 - f64::from(cpu_percent) / 100.0) * f64::from(self.policy.cpu_cores);
        if idle_cores
            < f64::from(
                cpu.saturating_add(running_cpu)
                    .saturating_add(self.policy.cpu_reserve),
            )
        {
            return Some("interactive CPU reserve would be consumed");
        }
        if u128::from(memory_used)
            + u128::from(memory_mib) * u128::from(MIB)
            + u128::from(running_memory_mib) * u128::from(MIB)
            + u128::from(self.policy.memory_reserve)
            > u128::from(self.policy.memory_total)
        {
            return Some("interactive memory reserve would be consumed");
        }
        None
    }
}

pub fn sample(system: &mut System) -> Sample {
    system.refresh_cpu_usage();
    system.refresh_memory();
    let cpu = system.global_cpu_usage();
    let total = system.total_memory();
    Sample {
        at: Instant::now(),
        cpu_percent: (cpu.is_finite() && !system.cpus().is_empty()).then_some(cpu),
        memory_used: (total > 0).then_some(system.used_memory()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> Options {
        Options {
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

    fn sample(at: Instant, cpu: Option<f32>, used_mib: Option<u64>) -> Sample {
        Sample {
            at,
            cpu_percent: cpu,
            memory_used: used_mib.map(|value| value * MIB),
        }
    }

    #[test]
    fn high_pressure_holds_until_both_metrics_recover_for_dwell() {
        let mut monitor = Monitor::new(Policy::new(options(), 8, 16 * 1024 * MIB).unwrap());
        let start = Instant::now();
        monitor.update(sample(start, Some(90.0), Some(8_000)));
        assert!(monitor.wait_reason(1, 1024, 0, 0, start).is_some());
        monitor.update(sample(
            start + Duration::from_secs(1),
            Some(50.0),
            Some(8_000),
        ));
        monitor.update(sample(
            start + Duration::from_secs(5),
            Some(50.0),
            Some(8_000),
        ));
        assert!(
            monitor
                .wait_reason(1, 1024, 0, 0, start + Duration::from_secs(5))
                .is_some()
        );
        monitor.update(sample(
            start + Duration::from_secs(6),
            Some(50.0),
            Some(8_000),
        ));
        assert!(
            monitor
                .wait_reason(1, 1024, 0, 0, start + Duration::from_secs(6))
                .is_none()
        );
    }

    #[test]
    fn stale_and_missing_metrics_hold_admission() {
        let mut monitor = Monitor::new(Policy::new(options(), 8, 16 * 1024 * MIB).unwrap());
        let start = Instant::now();
        monitor.update(sample(start, None, Some(1_000)));
        assert!(monitor.wait_reason(1, 1024, 0, 0, start).is_some());
        monitor.update(sample(
            start + Duration::from_secs(6),
            Some(10.0),
            Some(1_000),
        ));
        assert!(
            monitor
                .wait_reason(1, 1024, 0, 0, start + Duration::from_secs(10))
                .is_some()
        );
    }

    #[test]
    fn reserve_checks_next_job_against_live_capacity() {
        let mut monitor = Monitor::new(Policy::new(options(), 8, 16 * 1024 * MIB).unwrap());
        let start = Instant::now();
        monitor.update(sample(start, Some(10.0), Some(12_000)));
        monitor.update(sample(
            start + Duration::from_secs(5),
            Some(10.0),
            Some(12_000),
        ));
        assert!(
            monitor
                .wait_reason(1, 1024, 0, 0, start + Duration::from_secs(5))
                .is_none()
        );
        assert!(
            monitor
                .wait_reason(1, 4096, 0, 0, start + Duration::from_secs(5))
                .is_some()
        );
        assert!(
            monitor
                .wait_reason(7, 1024, 0, 0, start + Duration::from_secs(5))
                .is_some()
        );
    }

    #[test]
    fn concurrent_grants_cannot_spend_the_same_live_headroom() {
        let mut monitor = Monitor::new(Policy::new(options(), 8, 16 * 1024 * MIB).unwrap());
        let start = Instant::now();
        monitor.update(sample(start, Some(10.0), Some(4_000)));
        monitor.update(sample(
            start + Duration::from_secs(5),
            Some(10.0),
            Some(4_000),
        ));
        let now = start + Duration::from_secs(5);
        assert!(monitor.wait_reason(4, 1024, 0, 0, now).is_none());
        assert!(monitor.wait_reason(4, 1024, 4, 1024, now).is_some());
    }

    #[test]
    fn critical_notice_occurs_once_per_episode() {
        let mut monitor = Monitor::new(Policy::new(options(), 8, 16 * 1024 * MIB).unwrap());
        let start = Instant::now();
        assert!(monitor.update(sample(start, Some(99.0), Some(1_000))));
        assert!(!monitor.update(sample(
            start + Duration::from_secs(1),
            Some(99.0),
            Some(1_000)
        )));
        assert!(!monitor.update(sample(
            start + Duration::from_secs(2),
            Some(10.0),
            Some(1_000)
        )));
        assert!(!monitor.update(sample(
            start + Duration::from_secs(3),
            Some(99.0),
            Some(1_000)
        )));
        monitor.update(sample(
            start + Duration::from_secs(4),
            Some(10.0),
            Some(1_000),
        ));
        assert!(monitor.update(sample(
            start + Duration::from_secs(64),
            Some(99.0),
            Some(1_000)
        )));
    }

    #[test]
    fn rejects_invalid_policies() {
        let mut value = options();
        value.cpu_recover_percent = 90;
        assert!(Policy::new(value, 8, 16 * 1024 * MIB).is_err());
        value = options();
        value.memory_reserve_mib = Some(16 * 1024);
        assert!(Policy::new(value, 8, 16 * 1024 * MIB).is_err());
        value = options();
        value.cpu_recover_percent = 0;
        assert!(Policy::new(value, 8, 16 * 1024 * MIB).is_err());
        value = options();
        value.memory_recover_percent = 0;
        assert!(Policy::new(value, 8, 16 * 1024 * MIB).is_err());
    }
}
