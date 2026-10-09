use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const OPENCODE_PLUGIN: &str = include_str!("../integrations/opencode/reef-observe.js");
const MANAGED_HEADER: &str = "// Managed by Reef.";

pub fn run(
    agent: Option<&str>,
    home: Option<&Path>,
    config_dir: Option<&Path>,
    check: bool,
) -> io::Result<()> {
    let home_overridden = home.is_some();
    let home = match home {
        Some(path) => path.to_path_buf(),
        None => user_home()?,
    };
    let selected: Vec<&str> = match agent {
        Some(agent) => vec![agent],
        None => ["codex", "claude", "opencode"]
            .into_iter()
            .filter(|agent| on_path(agent) || configured(&home, agent, home_overridden))
            .collect(),
    };
    if selected.is_empty() {
        return Err(io::Error::other(
            "no supported agent installation found; pass an agent name explicitly",
        ));
    }
    for agent in selected {
        match agent {
            "codex" => {
                hook(&home.join(".codex/hooks.json"), agent, check)?;
                let orca_home =
                    home.join("Library/Application Support/orca/codex-runtime-home/home");
                if orca_home.is_dir() {
                    hook(&orca_home.join("hooks.json"), agent, check)?;
                }
            }
            "claude" => hook(&home.join(".claude/settings.json"), agent, check)?,
            "opencode" => opencode(&home, home_overridden, config_dir, check)?,
            _ => return Err(io::Error::other("unsupported agent")),
        }
    }
    Ok(())
}

fn configured(home: &Path, agent: &str, home_overridden: bool) -> bool {
    match agent {
        "codex" => {
            home.join(".codex").is_dir()
                || home
                    .join("Library/Application Support/orca/codex-runtime-home/home")
                    .is_dir()
        }
        "claude" => home.join(".claude").is_dir(),
        "opencode" => default_config_dir(home, home_overridden).is_dir(),
        _ => false,
    }
}

fn on_path(name: &str) -> bool {
    path_on_path(name).is_some()
}

fn path_on_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path).find_map(|directory| {
        let candidate = directory.join(name);
        if executable(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        for suffix in [".exe", ".cmd", ".bat"] {
            let candidate = directory.join(format!("{name}{suffix}"));
            if executable(&candidate) {
                return Some(candidate);
            }
        }
        None
    })
}

fn executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn hook(path: &Path, agent: &str, check: bool) -> io::Result<()> {
    let reef = match path_on_path("reef") {
        Some(path) => path,
        None => env::current_exe()?,
    };
    let command = format!("{} agents observe {agent}", shell_quote(&reef));
    let mut config = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            serde_json::from_slice::<Value>(&fs::read(path)?)?
        }
        Ok(_) => {
            return Err(io::Error::other(format!(
                "refusing non-regular agent configuration: {}",
                path.display()
            )));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(error),
    };
    let root = config.as_object_mut().ok_or_else(|| {
        io::Error::other(format!("invalid agent configuration: {}", path.display()))
    })?;
    let groups = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| io::Error::other("hooks must be a JSON object"))?
        .entry("SessionStart")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| io::Error::other("SessionStart hooks must be a JSON array"))?;
    let suffix = format!(" agents observe {agent}");
    for group in groups.iter_mut() {
        let Some(group) = group.as_object_mut() else {
            return Err(io::Error::other("SessionStart group must be a JSON object"));
        };
        if group
            .get("matcher")
            .is_some_and(|matcher| matcher.as_str() != Some(""))
        {
            continue;
        }
        let Some(hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            return Err(io::Error::other("SessionStart group needs a hooks array"));
        };
        for entry in hooks {
            let Some(entry) = entry.as_object_mut() else {
                return Err(io::Error::other("SessionStart hook must be a JSON object"));
            };
            if entry
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(|existing| existing == command || existing.ends_with(&suffix))
            {
                if entry.get("command").and_then(Value::as_str) == Some(command.as_str())
                    && entry.get("type").and_then(Value::as_str) == Some("command")
                    && entry.get("async").and_then(Value::as_bool) == Some(true)
                {
                    println!("Reef {agent} hook is current: {}", path.display());
                    return Ok(());
                }
                if check {
                    return Err(io::Error::other(format!(
                        "Reef {agent} hook needs setup: {}",
                        path.display()
                    )));
                }
                entry.insert("command".into(), json!(command));
                entry.insert("type".into(), json!("command"));
                entry.insert("async".into(), json!(true));
                write_json(path, &config)?;
                println!("Updated Reef {agent} hook: {}", path.display());
                return Ok(());
            }
        }
    }
    if check {
        return Err(io::Error::other(format!(
            "Reef {agent} hook is not installed: {}",
            path.display()
        )));
    }
    groups.push(json!({"hooks": [{"type": "command", "command": command, "async": true}]}));
    write_json(path, &config)?;
    println!("Installed Reef {agent} hook: {}", path.display());
    Ok(())
}

fn shell_quote(path: &Path) -> String {
    let path = path.to_string_lossy();
    #[cfg(unix)]
    {
        format!("'{}'", path.replace('\'', "'\\''"))
    }
    #[cfg(not(unix))]
    {
        format!("\"{path}\"")
    }
}

fn write_json(path: &Path, value: &Value) -> io::Result<()> {
    let mut data = serde_json::to_vec_pretty(value)?;
    data.push(b'\n');
    write_atomic(path, &data)
}

fn opencode(
    home: &Path,
    home_overridden: bool,
    config_dir: Option<&Path>,
    check: bool,
) -> io::Result<()> {
    let config_dir = match config_dir {
        Some(path) => path.to_path_buf(),
        None => default_config_dir(home, home_overridden),
    };
    let plugin_dir = config_dir.join("plugins");
    let plugin = plugin_dir.join("reef-observe.js");

    match fs::symlink_metadata(&plugin) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Err(io::Error::other(format!(
                    "refusing non-regular OpenCode plugin: {}",
                    plugin.display()
                )));
            }
            let current = fs::read_to_string(&plugin)?;
            if current == OPENCODE_PLUGIN {
                println!("Reef OpenCode plugin is current: {}", plugin.display());
                return Ok(());
            }
            if check {
                return Err(io::Error::other(format!(
                    "Reef OpenCode plugin needs setup: {}",
                    plugin.display()
                )));
            }
            if !current.starts_with(MANAGED_HEADER) {
                return Err(io::Error::other(format!(
                    "refusing to replace unmanaged OpenCode plugin: {}",
                    plugin.display()
                )));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if check {
                return Err(io::Error::other(format!(
                    "Reef OpenCode plugin is not installed: {}",
                    plugin.display()
                )));
            }
        }
        Err(error) => return Err(error),
    }

    write_atomic(&plugin, OPENCODE_PLUGIN.as_bytes())?;
    println!("Installed Reef OpenCode plugin: {}", plugin.display());
    Ok(())
}

fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing parent"))?;
    if fs::symlink_metadata(parent).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(io::Error::other(format!(
            "refusing symlinked configuration directory: {}",
            parent.display()
        )));
    }
    fs::create_dir_all(parent)?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let temporary = parent.join(format!(".reef-setup-{}-{nonce}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temporary)?;
        file.write_all(data)?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn default_config_dir(home: &Path, home_overridden: bool) -> PathBuf {
    if !home_overridden
        && let Some(path) = env::var_os("OPENCODE_CONFIG_DIR").filter(|path| !path.is_empty())
    {
        return PathBuf::from(path);
    }
    home.join(".config/opencode")
}

fn user_home() -> io::Result<PathBuf> {
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .ok_or_else(|| io::Error::other("HOME or USERPROFILE is required for agent setup"))?;
    Ok(PathBuf::from(home))
}
