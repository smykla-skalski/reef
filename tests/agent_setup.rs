use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

struct TestConfig(PathBuf);
static NEXT_CONFIG: AtomicU64 = AtomicU64::new(0);

impl TestConfig {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "reef-opencode-setup-{}-{nonce}-{}",
            std::process::id(),
            NEXT_CONFIG.fetch_add(1, Ordering::Relaxed)
        )))
    }

    fn plugin(&self) -> PathBuf {
        self.0.join("plugins/reef-observe.js")
    }

    fn reef(&self, check: bool) -> std::process::Output {
        self.reef_agent("opencode", check)
    }

    fn reef_agent(&self, agent: &str, check: bool) -> std::process::Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_reef"));
        command.args(["agents", "setup", agent, "--home"]);
        command.arg(&self.0);
        if agent == "opencode" {
            command.arg("--config-dir").arg(&self.0);
        }
        if check {
            command.arg("--check");
        }
        command.output().unwrap()
    }
}

#[test]
fn codex_and_claude_hooks_preserve_existing_configuration() {
    let config = TestConfig::new();
    let codex = config.0.join(".codex/hooks.json");
    fs::create_dir_all(codex.parent().unwrap()).unwrap();
    fs::write(
        &codex,
        r#"{"custom":42,"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"other"}]}]}}"#,
    )
    .unwrap();
    assert!(!config.reef_agent("codex", true).status.success());
    assert!(config.reef_agent("codex", false).status.success());
    assert!(config.reef_agent("codex", true).status.success());
    let codex_json: serde_json::Value = serde_json::from_slice(&fs::read(codex).unwrap()).unwrap();
    assert_eq!(codex_json["custom"], 42);
    assert_eq!(
        codex_json["hooks"]["SessionStart"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    assert!(config.reef_agent("claude", false).status.success());
    assert!(config.reef_agent("claude", true).status.success());
    let claude_json: serde_json::Value =
        serde_json::from_slice(&fs::read(config.0.join(".claude/settings.json")).unwrap()).unwrap();
    assert!(
        claude_json["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .ends_with(" agents observe claude")
    );
}

#[test]
fn replaces_an_existing_reef_hook_with_a_stale_absolute_command() {
    let config = TestConfig::new();
    let path = config.0.join(".codex/hooks.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let original = r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"/path/that/does/not/exist/reef agents observe codex","async":true}]}]}}"#;
    fs::write(&path, original).unwrap();
    assert!(!config.reef_agent("codex", true).status.success());
    assert!(config.reef_agent("codex", false).status.success());
    assert!(config.reef_agent("codex", true).status.success());
    let updated = fs::read_to_string(path).unwrap();
    assert!(!updated.contains("/path/that/does/not/exist/reef"));
    assert!(updated.contains("agents observe codex"));
}

#[cfg(unix)]
#[test]
fn setup_without_agent_detects_only_installed_clis() {
    use std::os::unix::fs::PermissionsExt;

    let config = TestConfig::new();
    let bin = config.0.join("bin");
    fs::create_dir_all(&bin).unwrap();
    for name in ["codex", "opencode"] {
        let path = bin.join(name);
        fs::write(&path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let result = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["agents", "setup", "--home"])
        .arg(&config.0)
        .env("PATH", &bin)
        .env("OPENCODE_CONFIG_DIR", config.0.join("opencode"))
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    assert!(config.0.join(".codex/hooks.json").is_file());
    assert!(
        config
            .0
            .join(".config/opencode/plugins/reef-observe.js")
            .is_file()
    );
    assert!(!config.0.join("opencode/plugins/reef-observe.js").exists());
    assert!(!config.0.join(".claude/settings.json").exists());
}

#[cfg(windows)]
#[test]
fn setup_without_agent_detects_windows_command_shims() {
    let config = TestConfig::new();
    let bin = config.0.join("bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(bin.join("opencode.cmd"), "@echo off\r\n").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["agents", "setup", "--home"])
        .arg(&config.0)
        .env("PATH", &bin)
        .env("OPENCODE_CONFIG_DIR", config.0.join("opencode"))
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    assert!(
        config
            .0
            .join(".config/opencode/plugins/reef-observe.js")
            .is_file()
    );
    assert!(!config.0.join("opencode/plugins/reef-observe.js").exists());
}

#[test]
fn setup_detects_existing_agent_config_without_cli_on_path() {
    let config = TestConfig::new();
    let empty_bin = config.0.join("bin");
    fs::create_dir_all(&empty_bin).unwrap();
    fs::create_dir_all(config.0.join(".claude")).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["agents", "setup", "--home"])
        .arg(&config.0)
        .env("PATH", &empty_bin)
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    assert!(config.0.join(".claude/settings.json").is_file());
    assert!(!config.0.join(".codex/hooks.json").exists());
    assert!(
        !config
            .0
            .join(".config/opencode/plugins/reef-observe.js")
            .exists()
    );
}

impl Drop for TestConfig {
    fn drop(&mut self) {
        if self.0.exists() {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
}

#[test]
fn installs_bundled_plugin_and_checks_it_without_rewriting() {
    let config = TestConfig::new();
    assert!(!config.reef(true).status.success());
    assert!(!config.0.exists());
    let update = config.reef(false);
    assert!(update.status.success(), "{update:?}");
    let expected = include_str!("../integrations/opencode/reef-observe.js");
    assert_eq!(fs::read_to_string(config.plugin()).unwrap(), expected);
    assert!(config.reef(true).status.success());
    let update = config.reef(false);
    assert!(update.status.success(), "{update:?}");
}

#[test]
fn updates_only_a_reef_managed_plugin() {
    let config = TestConfig::new();
    fs::create_dir_all(config.0.join("plugins")).unwrap();
    fs::write(config.plugin(), "// user plugin\n").unwrap();
    assert!(!config.reef(false).status.success());
    assert_eq!(
        fs::read_to_string(config.plugin()).unwrap(),
        "// user plugin\n"
    );

    fs::write(config.plugin(), "// Managed by Reef. Old version\n").unwrap();
    assert!(!config.reef(true).status.success());
    let update = config.reef(false);
    assert!(update.status.success(), "{update:?}");
    assert_eq!(
        fs::read_to_string(config.plugin()).unwrap(),
        include_str!("../integrations/opencode/reef-observe.js")
    );
}

#[test]
fn adopts_the_exact_plugin_shipped_before_setup_existed() {
    let config = TestConfig::new();
    fs::create_dir_all(config.0.join("plugins")).unwrap();
    let current = include_str!("../integrations/opencode/reef-observe.js");
    let old = current
        .strip_prefix("// Managed by Reef. Install with: reef agents setup opencode\n")
        .unwrap();
    fs::write(config.plugin(), old).unwrap();
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(config.plugin()).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(config.plugin(), permissions).unwrap();
    }
    assert!(!config.reef(true).status.success());
    let update = config.reef(false);
    assert!(update.status.success(), "{update:?}");
    assert_eq!(fs::read_to_string(config.plugin()).unwrap(), current);
}

#[cfg(unix)]
#[test]
fn refuses_to_replace_symlinked_plugin() {
    use std::os::unix::fs::symlink;

    let config = TestConfig::new();
    fs::create_dir_all(config.0.join("plugins")).unwrap();
    let original = config.0.join("existing.js");
    fs::write(&original, "// Managed by Reef. Existing\n").unwrap();
    symlink(&original, config.plugin()).unwrap();
    assert!(!config.reef(false).status.success());
    assert_eq!(
        fs::read_to_string(original).unwrap(),
        "// Managed by Reef. Existing\n"
    );
}
