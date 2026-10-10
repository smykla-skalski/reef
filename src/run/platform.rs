use nix::errno::Errno;
use nix::sys::signal::{SigSet, SigmaskHow, Signal, kill, killpg, pthread_sigmask};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{Pid, getpgid, getpgrp, tcgetpgrp, tcsetpgrp};
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use signal_hook::iterator::Signals;
use std::fs::{Metadata, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use sysinfo::{ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System, UpdateKind};

const OWNER_ENV: &str = "REEF_RUN_OWNER";
const OWNER_BYTES: usize = 40;

pub struct Supervisor {
    process: Child,
    control: Option<ChildStdin>,
    group: u32,
    owner: String,
}

impl Supervisor {
    pub fn start() -> std::io::Result<Self> {
        let mut process = Command::new(std::env::current_exe()?)
            .arg("__run-supervisor")
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let control = process.stdin.take();
        let mut group = [0; 4];
        let mut owner = [0; OWNER_BYTES];
        let mut output = process.stdout.take().expect("piped stdout");
        if let Err(error) = output
            .read_exact(&mut group)
            .and_then(|()| output.read_exact(&mut owner))
        {
            drop(control);
            let _ = process.wait();
            return Err(error);
        }
        Ok(Self {
            process,
            control,
            group: u32::from_ne_bytes(group),
            owner: String::from_utf8(owner.to_vec()).map_err(std::io::Error::other)?,
        })
    }

    pub fn group(&self) -> u32 {
        self.group
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn stop(&mut self) -> std::io::Result<()> {
        self.control.take();
        let status = self.process.wait()?;
        if status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(
                "command supervisor failed to stop descendants",
            ))
        }
    }

    pub fn disarm(&mut self) -> std::io::Result<()> {
        self.control
            .take()
            .expect("supervisor control")
            .write_all(b"D")?;
        let status = self.process.wait()?;
        if status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(
                "command supervisor exited unexpectedly",
            ))
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.control.take();
        let _ = self.process.wait();
    }
}

pub fn supervise() -> std::io::Result<()> {
    let mut leader = Command::new(std::env::current_exe()?)
        .arg("__run-group-leader")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdout = std::io::stdout();
    let owner = format!(
        "{:08x}{:032x}",
        leader.id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    if let Err(error) = stdout
        .write_all(&leader.id().to_ne_bytes())
        .and_then(|()| stdout.write_all(owner.as_bytes()))
        .and_then(|()| stdout.flush())
    {
        let _ = force_stop(leader.id());
        let _ = leader.wait();
        return Err(error);
    }
    let mut action = [0];
    let disarmed = loop {
        match std::io::stdin().read(&mut action) {
            Ok(1) if action[0] == b'D' => break true,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Ok(_) | Err(_) => break false,
        }
    };
    if !disarmed {
        let _ = force_stop(leader.id());
    }
    let cleanup = if disarmed { Ok(()) } else { stop_owned(&owner) };
    let _ = leader.kill();
    let _ = leader.wait();
    cleanup
}

fn stop_owned(owner: &str) -> std::io::Result<()> {
    let mut system = System::new();
    let refresh = ProcessRefreshKind::nothing()
        .with_environ(UpdateKind::Always)
        .without_tasks();
    let marker = std::ffi::OsString::from(format!("{OWNER_ENV}={owner}"));
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
        let mut live = false;
        for (&pid, process) in system.processes() {
            if process.status() == ProcessStatus::Zombie
                || !process.environ().iter().any(|entry| entry == &marker)
            {
                continue;
            }
            live = true;
            match kill(
                Pid::from_raw(i32::try_from(pid.as_u32()).map_err(std::io::Error::other)?),
                Signal::SIGKILL,
            ) {
                Ok(()) | Err(Errno::ESRCH) => {}
                Err(error) => return Err(std::io::Error::other(error)),
            }
        }
        if !live {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(std::io::Error::other(
                "owned descendants survived supervisor cleanup",
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub fn hold_group() -> ! {
    loop {
        std::thread::park();
    }
}

pub struct Foreground {
    previous: Pid,
}

impl Foreground {
    pub fn activate(pid: u32) -> std::io::Result<Self> {
        let previous = tcgetpgrp(std::io::stdin()).map_err(std::io::Error::other)?;
        tcsetpgrp(std::io::stdin(), process_id(pid)?).map_err(std::io::Error::other)?;
        Ok(Self { previous })
    }
}

impl Drop for Foreground {
    fn drop(&mut self) {
        let mut blocked = SigSet::empty();
        blocked.add(Signal::SIGTTOU);
        let mut previous_mask = SigSet::empty();
        if pthread_sigmask(
            SigmaskHow::SIG_BLOCK,
            Some(&blocked),
            Some(&mut previous_mask),
        )
        .is_ok()
        {
            let _ = tcsetpgrp(std::io::stdin(), self.previous);
            let _ = pthread_sigmask(SigmaskHow::SIG_SETMASK, Some(&previous_mask), None);
        }
    }
}

pub fn signals() -> std::io::Result<Signals> {
    Signals::new([SIGINT, SIGTERM, SIGHUP, SIGQUIT])
}

pub fn isolate(command: &mut Command, group: u32) -> std::io::Result<()> {
    command.process_group(i32::try_from(group).map_err(std::io::Error::other)?);
    Ok(())
}

pub fn has_foreground_stdin() -> bool {
    tcgetpgrp(std::io::stdin()).is_ok_and(|group| group == getpgrp())
}

pub fn wait_stopped(pid: u32) -> std::io::Result<()> {
    loop {
        match waitpid(process_id(pid)?, Some(WaitPidFlag::WUNTRACED)) {
            Ok(WaitStatus::Stopped(_, Signal::SIGSTOP)) => return Ok(()),
            Err(Errno::EINTR) => {}
            Ok(status) => {
                return Err(std::io::Error::other(format!(
                    "command setup stopped unexpectedly: {status:?}"
                )));
            }
            Err(error) => return Err(std::io::Error::other(error)),
        }
    }
}

pub fn resume(pid: u32) -> std::io::Result<()> {
    kill(process_id(pid)?, Signal::SIGCONT).map_err(std::io::Error::other)
}

pub fn forward(pid: u32, signal: i32) -> std::io::Result<()> {
    let signal = Signal::try_from(signal).map_err(std::io::Error::other)?;
    match killpg(process_id(pid)?, signal) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(error) => Err(std::io::Error::other(error)),
    }
}

pub fn force_stop(pid: u32) -> std::io::Result<()> {
    match killpg(process_id(pid)?, Signal::SIGKILL) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(error) => Err(std::io::Error::other(error)),
    }
}

pub fn in_group(pid: u32, group: u32) -> bool {
    let Ok(pid) = process_id(pid) else {
        return false;
    };
    i32::try_from(group)
        .is_ok_and(|group| getpgid(Some(pid)).is_ok_and(|found| found.as_raw() == group))
}

fn process_id(pid: u32) -> std::io::Result<Pid> {
    Ok(Pid::from_raw(
        i32::try_from(pid).map_err(std::io::Error::other)?,
    ))
}

pub fn exit_signal(status: ExitStatus) -> Option<i32> {
    status.signal()
}

pub fn is_interrupt_signal(signal: i32) -> bool {
    matches!(signal, SIGINT | SIGTERM | SIGHUP | SIGQUIT)
}

pub fn is_public(metadata: &Metadata) -> bool {
    metadata.mode() & 0o077 != 0
}

pub fn private_creation(options: &mut OpenOptions) {
    options
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
}
