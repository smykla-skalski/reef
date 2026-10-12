use crate::ObserveOptions;
use std::io;
use std::path::Path;
use std::process::{Child, Command, Stdio};

pub fn spawn_detached(options: &ObserveOptions, dir: &Path) -> io::Result<Child> {
    #[cfg(unix)]
    let mut command = {
        let mut command = Command::new("nohup");
        let executable = match super::reload::stable_invocation() {
            Some((path, _)) => path,
            None => std::env::current_exe()?,
        };
        command.arg(executable);
        command
    };
    #[cfg(not(unix))]
    let mut command = Command::new(std::env::current_exe()?);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    command
        .arg("observe")
        .arg("run")
        .arg("--interval-seconds")
        .arg(options.interval_seconds.to_string())
        .arg("--raw-retention-hours")
        .arg(
            options
                .retention_days
                .map_or(options.raw_retention_hours, |days| days.saturating_mul(24))
                .to_string(),
        )
        .arg("--five-minute-retention-days")
        .arg(options.five_minute_retention_days.to_string())
        .arg("--hourly-retention-days")
        .arg(options.hourly_retention_days.to_string())
        .arg("--daily-retention-days")
        .arg(options.daily_retention_days.to_string())
        .arg("--max-storage-mib")
        .arg(options.max_storage_mib.to_string())
        .arg("--state-dir")
        .arg(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.spawn()
}
