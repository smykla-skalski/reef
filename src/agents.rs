use crate::control;
use crate::tool_classification::classify;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const TOOLS: &[&str] = &["go", "cargo", "golangci-lint", "mise", "make"];
pub(crate) const MAX_WAIT_MS: u64 = 24 * 60 * 60 * 1000;

pub fn shim_name() -> Option<String> {
    let name = env::args_os()
        .next()
        .and_then(|arg| PathBuf::from(arg).file_name().map(OsStr::to_os_string))?;
    let name = name.to_str()?;
    if TOOLS.contains(&name) && env::var_os("REEF_AGENT_ORIGINAL_PATH").is_some() {
        Some(name.to_owned())
    } else {
        None
    }
}

pub fn launch(
    agent: &str,
    state_dir: Option<&Path>,
    control_mode: bool,
    canary_percent: Option<u8>,
    max_wait_ms: u64,
    args: &[String],
) -> io::Result<u8> {
    // Agent shells can change directory or HOME; pin every path before launch.
    let (state_root, scheduler_dir) = if let Some(path) = state_dir {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            env::current_dir()?.join(path)
        };
        (path.clone(), path)
    } else {
        let home = env::var_os("HOME").ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "HOME is not set; pass --state-dir")
        })?;
        let home = PathBuf::from(home);
        let home = if home.is_absolute() {
            home
        } else {
            env::current_dir()?.join(home)
        };
        let state_root = home.join(".local/state/reef");
        (state_root.clone(), state_root.join("schedule"))
    };
    let original_path = env::var_os("REEF_AGENT_ORIGINAL_PATH")
        .or_else(|| env::var_os("PATH"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "PATH is not set"))?;
    let agent_binary = find_binary(agent, &original_path)?;
    let reef_binary = env::current_exe()?.canonicalize()?;
    let root = state_root.join("agent-shims");
    private_dir(&root)?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    reef_binary.hash(&mut hasher);
    let shim_dir = root.join(format!("{:016x}", hasher.finish()));
    private_dir(&shim_dir)?;
    for tool in TOOLS {
        let path = shim_dir.join(tool);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                if fs::read_link(&path)? != reef_binary {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("shim path is occupied: {}", path.display()),
                    ));
                }
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("shim path is occupied: {}", path.display()),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if let Err(error) = symlink(&reef_binary, &path)
                    && (error.kind() != io::ErrorKind::AlreadyExists
                        || fs::read_link(&path)? != reef_binary)
                {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
    let paths = env::split_paths(&original_path);
    let path = env::join_paths(std::iter::once(shim_dir).chain(paths)).map_err(io::Error::other)?;
    let session = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    );
    let mut process = Command::new(agent_binary);
    process
        .args(args)
        .env("PATH", path)
        .env("REEF_AGENT_ORIGINAL_PATH", original_path)
        .env("REEF_AGENT_KIND", agent)
        .env("REEF_AGENT_SESSION", session)
        .env("REEF_AGENT_MAX_WAIT_MS", max_wait_ms.to_string())
        .env_remove("REEF_CONTROL_ATTEMPT")
        .env_remove("REEF_ADMITTED");
    if control_mode || canary_percent.is_some() {
        let control_dir = control::prepare(Some(&state_root))?;
        process.env("REEF_AGENT_CONTROL_DIR", control_dir);
    } else {
        process.env_remove("REEF_AGENT_CONTROL_DIR");
    }
    if let Some(percent) = canary_percent {
        eprintln!(
            "reef: best-effort host canary at {percent}% treatment; detached descendants may survive launcher failure; strong isolation remains off"
        );
        process.env("REEF_AGENT_CANARY_PERCENT", percent.to_string());
    } else {
        process.env_remove("REEF_AGENT_CANARY_PERCENT");
    }
    process.env("REEF_AGENT_STATE_DIR", scheduler_dir);
    Err(process.exec())
}

pub fn shim(tool: &str) -> io::Result<u8> {
    let original_path = env::var_os("REEF_AGENT_ORIGINAL_PATH")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Reef agent PATH is missing"))?;
    let original = find_binary(tool, &original_path);
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    let Some(category) = classify(tool, &args) else {
        return Err(Command::new(original?).args(&args).exec());
    };
    if env::var_os("REEF_ADMITTED").as_deref() == Some(OsStr::new("1"))
        || env::var_os("REEF_CONTROL_ATTEMPT").is_some()
    {
        return Err(Command::new(original?).args(&args).exec());
    }
    if env::var_os("REEF_AGENT_CONTROL_DIR").is_some() {
        return control::run(original.as_deref().ok(), &args, category);
    }
    let original = original?;
    if args.iter().any(|arg| arg.to_str().is_none()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "scheduled command arguments must be UTF-8",
        ));
    }
    let agent = env::var("REEF_AGENT_KIND")
        .ok()
        .filter(|value| matches!(value.as_str(), "codex" | "claude" | "opencode"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid agent kind"))?;
    let session = env::var("REEF_AGENT_SESSION")
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "agent session is missing"))?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    session.hash(&mut hasher);
    if let Ok(output) = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        && output.status.success()
    {
        output.stdout.hash(&mut hasher);
    } else {
        env::current_dir()?.hash(&mut hasher);
    }
    let identity = format!("{agent}-{session}-{:016x}", hasher.finish());
    let mut process = Command::new(env::current_exe()?.canonicalize()?);
    let max_wait_ms = env::var("REEF_AGENT_MAX_WAIT_MS")
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "agent wait limit is missing"))?
        .parse::<u64>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid agent wait limit"))?;
    if max_wait_ms == 0 || max_wait_ms > MAX_WAIT_MS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "agent wait limit must be between 1 ms and 24 hours",
        ));
    }
    process
        .arg("schedule")
        .arg("--max-wait-ms")
        .arg(max_wait_ms.to_string())
        .arg("--category")
        .arg(category)
        .arg("--identity")
        .arg(identity);
    if let Some(dir) = env::var_os("REEF_AGENT_STATE_DIR") {
        process.arg("--state-dir").arg(dir);
    }
    process.arg("--").arg(original).args(args);
    Err(process.exec())
}

fn find_binary(name: &str, path: &OsStr) -> io::Result<PathBuf> {
    for directory in env::split_paths(path) {
        let candidate = directory.join(name);
        if candidate
            .metadata()
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("{name} not found on the original PATH"),
    ))
}

fn private_dir(path: &Path) -> io::Result<()> {
    if !path.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.uid() != nix::unistd::Uid::current().as_raw()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("agent shim directory must be private: {}", path.display()),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::classify;
    use std::ffi::OsString;

    #[test]
    fn recognizes_only_selected_heavy_commands() {
        for (tool, args, expected) in [
            ("go", vec!["test"], Some("test")),
            ("go", vec!["-C", "module", "test"], Some("test")),
            ("go", vec!["-C=module", "build"], Some("build")),
            ("go", vec!["env"], None),
            ("cargo", vec!["+nightly", "clippy"], Some("lint")),
            ("cargo", vec!["--offline", "test"], Some("test")),
            ("cargo", vec!["--color", "never", "test"], Some("test")),
            (
                "cargo",
                vec!["-vv", "test", "--no-run", "--offline"],
                Some("test"),
            ),
            (
                "cargo",
                vec!["+nightly", "--config", "x=y", "clippy"],
                Some("lint"),
            ),
            ("cargo", vec!["metadata"], None),
            ("cargo", vec!["--help"], None),
            ("cargo", vec!["--future-flag", "test"], Some("build")),
            ("golangci-lint", vec!["run"], Some("lint")),
            ("mise", vec!["run", "check"], Some("lint")),
            ("make", vec!["test"], Some("test")),
            ("make", vec!["-j8", "test"], Some("test")),
            (
                "make",
                vec!["--no-print-directory", "-f", "/dev/stdin", "test"],
                Some("test"),
            ),
            ("make", vec!["-j", "8", "test"], Some("test")),
            ("make", vec!["--no-print-directory"], Some("build")),
            ("make", vec!["--help"], None),
        ] {
            let args: Vec<OsString> = args.into_iter().map(OsString::from).collect();
            assert_eq!(classify(tool, &args), expected, "{tool} {args:?}");
        }
    }
}
