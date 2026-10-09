use crate::ObserveOptions;
use std::io;
use std::path::Path;
use std::process::{Child, Command, Stdio};

pub fn spawn_detached(options: &ObserveOptions, dir: &Path) -> io::Result<Child> {
    #[cfg(unix)]
    let mut command = {
        let mut command = Command::new("nohup");
        command.arg(std::env::current_exe()?);
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
        .arg("--retention-days")
        .arg(options.retention_days.to_string())
        .arg("--max-storage-mib")
        .arg(options.max_storage_mib.to_string())
        .arg("--state-dir")
        .arg(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.spawn()
}
