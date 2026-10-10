use std::env;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(super) struct ExecutableWatch {
    invocation: PathBuf,
    target: PathBuf,
}

impl ExecutableWatch {
    pub(super) fn capture() -> Option<Self> {
        let (invocation, target) = stable_invocation()?;
        Some(Self { invocation, target })
    }

    pub(super) fn reload_if_changed(&self) -> io::Result<()> {
        let Ok(target) = fs::canonicalize(&self.invocation) else {
            return Ok(());
        };
        if target == self.target {
            return Ok(());
        }
        Err(Command::new(&self.invocation)
            .args(env::args_os().skip(1))
            .exec())
    }
}

pub(super) fn stable_invocation() -> Option<(PathBuf, PathBuf)> {
    let invocation = invocation_path()?;
    let target = fs::canonicalize(&invocation).ok()?;
    (target == fs::canonicalize(env::current_exe().ok()?).ok()?).then_some((invocation, target))
}

fn invocation_path() -> Option<PathBuf> {
    let argument = PathBuf::from(env::args_os().next()?);
    if argument.components().count() > 1 || argument.is_absolute() {
        return Some(if argument.is_absolute() {
            argument
        } else {
            env::current_dir().ok()?.join(argument)
        });
    }
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|directory| directory.join(&argument))
        .find(|candidate| executable(candidate))
}

fn executable(path: &Path) -> bool {
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}
