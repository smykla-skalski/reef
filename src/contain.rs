use clap::Args;
use serde::{Deserialize, Serialize};
use std::io;
use std::path::Path;
#[cfg(not(target_os = "linux"))]
use std::process::Command;

#[derive(Clone, Debug, Default, Args, Deserialize, Serialize)]
pub struct Limits {
    /// Maximum CPU use as a percentage of one CPU.
    #[arg(long = "limit-cpu-percent", value_parser = clap::value_parser!(u32).range(1..))]
    pub cpu_percent: Option<u32>,
    /// Maximum cgroup memory in MiB.
    #[arg(long = "limit-memory-mib", value_parser = clap::value_parser!(u64).range(1..))]
    pub memory_cap_mib: Option<u64>,
    /// Maximum number of tasks, including threads.
    #[arg(long = "limit-tasks", value_parser = clap::value_parser!(u32).range(1..))]
    pub tasks: Option<u32>,
    /// Read bandwidth limit as `PATH=BYTES_PER_SECOND`.
    #[arg(long = "limit-io-read", value_parser = parse_io_limit)]
    pub io_read: Option<IoLimit>,
    /// Write bandwidth limit as `PATH=BYTES_PER_SECOND`.
    #[arg(long = "limit-io-write", value_parser = parse_io_limit)]
    pub io_write: Option<IoLimit>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct IoLimit {
    path: String,
    bytes_per_second: u64,
}

fn parse_io_limit(value: &str) -> Result<IoLimit, String> {
    let (path, rate) = value
        .rsplit_once('=')
        .ok_or("I/O limit must be PATH=BYTES_PER_SECOND")?;
    let bytes_per_second = rate
        .parse::<u64>()
        .map_err(|_| "I/O bandwidth must be a positive integer")?;
    if !Path::new(path).is_absolute() || bytes_per_second == 0 {
        return Err("I/O limit needs an absolute path and positive rate".into());
    }
    Ok(IoLimit {
        path: path.to_owned(),
        bytes_per_second,
    })
}

impl Limits {
    pub fn requested(&self) -> bool {
        self.cpu_percent.is_some()
            || self.memory_cap_mib.is_some()
            || self.tasks.is_some()
            || self
                .io_read
                .as_ref()
                .is_some_and(|limit| !limit.path.is_empty() && limit.bytes_per_second > 0)
            || self
                .io_write
                .as_ref()
                .is_some_and(|limit| !limit.path.is_empty() && limit.bytes_per_second > 0)
    }

    #[cfg(target_os = "linux")]
    fn properties(&self) -> io::Result<Vec<String>> {
        let mut properties = Vec::new();
        if let Some(cpu) = self.cpu_percent {
            properties.push(format!("CPUQuota={cpu}%"));
        }
        if let Some(memory) = self.memory_cap_mib {
            let bytes = memory.checked_mul(1_048_576).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "memory limit overflows")
            })?;
            properties.push(format!("MemoryMax={bytes}"));
            properties.push("OOMPolicy=continue".into());
        }
        if let Some(tasks) = self.tasks {
            properties.push(format!("TasksMax={}", u64::from(tasks) + 1));
        }
        if let Some(limit) = &self.io_read {
            properties.push(format!(
                "IOReadBandwidthMax={} {}",
                limit.path, limit.bytes_per_second
            ));
        }
        if let Some(limit) = &self.io_write {
            properties.push(format!(
                "IOWriteBandwidthMax={} {}",
                limit.path, limit.bytes_per_second
            ));
        }
        Ok(properties)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Boundary {
    pub resource: String,
    pub limit: u64,
    pub observed_events: Option<u64>,
}

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub use linux::Containment;
#[cfg(target_os = "linux")]
pub use linux::exec_payload;

#[cfg(not(target_os = "linux"))]
pub struct Containment {
    unit: String,
}

#[cfg(not(target_os = "linux"))]
impl Containment {
    pub fn prepare(limits: &Limits) -> io::Result<Option<Self>> {
        if limits.requested() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "cgroup limits require Linux with a systemd user manager",
            ));
        }
        Ok(None)
    }

    pub fn command(&self, command: &[String]) -> io::Result<Command> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("{} cannot contain {}", self.unit, command[0]),
        ))
    }

    pub fn sample(&mut self) {
        unreachable!("non-Linux scope {} cannot exist", self.unit);
    }

    pub fn stop(&self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("non-Linux scope {} cannot exist", self.unit),
        ))
    }

    pub fn finish(&self) -> io::Result<()> {
        self.stop()
    }

    pub fn boundaries(&self) -> Vec<Boundary> {
        unreachable!("non-Linux scope {} cannot exist", self.unit);
    }
}

#[cfg(test)]
mod tests {
    use super::parse_io_limit;

    #[test]
    fn io_limit_requires_absolute_path_and_positive_rate() {
        assert!(parse_io_limit("/dev/nvme0n1=1048576").is_ok());
        for input in ["", "relative=100", "/dev/nvme0n1=0", "/dev/nvme0n1=no"] {
            assert!(parse_io_limit(input).is_err(), "{input}");
        }
    }
}
