use nix::errno::Errno;
use nix::sys::signal::{SigSet, SigmaskHow, Signal, kill, killpg, pthread_sigmask};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{Pid, getpgrp, tcgetpgrp, tcsetpgrp};
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use signal_hook::iterator::Signals;
use std::fs::{Metadata, OpenOptions};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, ExitStatus};

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

pub fn isolate(command: &mut Command) {
    command.process_group(0);
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

pub fn group_exists(pid: u32) -> std::io::Result<bool> {
    match killpg(process_id(pid)?, None) {
        Ok(()) => Ok(true),
        Err(Errno::ESRCH) => Ok(false),
        Err(error) => Err(std::io::Error::other(error)),
    }
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
