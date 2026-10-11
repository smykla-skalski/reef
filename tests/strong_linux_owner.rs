#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader};
use std::os::unix::ffi::OsStringExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

fn started_probe() -> (std::process::Child, String) {
    started_probe_with_path(None)
}

#[test]
fn probe_returns_the_command_exit_only_after_owner_cleanup() {
    if std::env::var_os("REEF_STRONG_LINUX_NATIVE_TEST").is_none() {
        return;
    }

    let output = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args([
            "strong-owner-probe",
            "--image",
            "docker.io/library/alpine:3.22",
            "--",
            "sh",
            "-c",
            "exit 7",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(7));
    let line = String::from_utf8(output.stdout).unwrap();
    let name = line.strip_prefix("READY ").unwrap().trim();
    let container = Command::new("podman")
        .args(["container", "exists", name])
        .status()
        .unwrap();
    assert_eq!(container.code(), Some(1), "owner returned before teardown");
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").unwrap();
    let state_dir = std::path::Path::new(&runtime).join(name);
    assert!(!state_dir.exists(), "owner returned before state cleanup");
    assert!(
        !state_dir.with_extension("done").exists(),
        "owner returned before tombstone cleanup"
    );
}

#[test]
fn opt_in_workspace_snapshot_is_read_only_and_excludes_untracked_files() {
    if std::env::var_os("REEF_STRONG_LINUX_NATIVE_TEST").is_none() {
        return;
    }

    let runtime = std::env::var_os("XDG_RUNTIME_DIR").unwrap();
    let repo =
        std::path::Path::new(&runtime).join(format!("reef-strong-source-{}", std::process::id()));
    std::fs::create_dir(&repo).unwrap();
    assert!(
        Command::new("git")
            .arg("init")
            .arg(&repo)
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(repo.join("tracked.txt"), "tracked\n").unwrap();
    std::fs::write(repo.join("untracked.txt"), "private\n").unwrap();
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["add", "tracked.txt"])
            .status()
            .unwrap()
            .success()
    );

    let output = Command::new(env!("CARGO_BIN_EXE_reef"))
        .arg("strong-owner-probe")
        .arg("--image")
        .arg("docker.io/library/alpine:3.22")
        .arg("--workspace")
        .arg(&repo)
        .arg("--approve-snapshot")
        .args([
            "--",
            "sh",
            "-c",
            "test \"$(cat tracked.txt)\" = tracked && test ! -e untracked.txt && ! touch new.txt",
        ])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let line = String::from_utf8(output.stdout).unwrap();
    let name = line.strip_prefix("READY ").unwrap().trim();
    assert!(!std::path::Path::new(&runtime).join(name).exists());
    assert_eq!(
        Command::new("podman")
            .args(["container", "exists", name])
            .status()
            .unwrap()
            .code(),
        Some(1)
    );
    std::fs::remove_dir_all(repo).unwrap();
}

fn started_probe_with_path(path: Option<std::ffi::OsString>) -> (std::process::Child, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reef"));
    command.args([
        "strong-owner-probe",
        "--image",
        "docker.io/library/alpine:3.22",
        "--",
        "sh",
        "-c",
        "setsid sleep 300 & wait",
    ]);
    if let Some(path) = path {
        command.env("PATH", path);
    }
    let mut launcher = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = launcher.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(stdout).read_line(&mut line);
        let _ = sender.send((result, line));
    });
    let (read, line) = receiver.recv_timeout(Duration::from_secs(90)).unwrap();
    read.unwrap();
    let name = line.strip_prefix("READY ").unwrap().trim().to_owned();
    assert!(name.starts_with("reef-strong-"));

    (launcher, name)
}

#[test]
fn unavailable_systemctl_does_not_block_launcher_cleanup() {
    if std::env::var_os("REEF_STRONG_LINUX_NATIVE_TEST").is_none() {
        return;
    }

    let runtime = std::env::var_os("XDG_RUNTIME_DIR").unwrap();
    let shim =
        std::path::Path::new(&runtime).join(format!("reef-systemctl-fail-{}", std::process::id()));
    std::fs::create_dir(&shim).unwrap();
    std::os::unix::fs::symlink("/usr/bin/false", shim.join("systemctl")).unwrap();
    let original_path = std::env::var_os("PATH").unwrap();
    let path = std::env::join_paths(
        std::iter::once(shim.clone()).chain(std::env::split_paths(&original_path)),
    )
    .unwrap();
    let (mut launcher, name) = started_probe_with_path(Some(path));
    let init_pid = container_init_pid(&name);
    let pid = owner_pid(&name);

    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();

    assert!(!launcher.wait().unwrap().success());
    assert_container_gone(&name);
    assert_init_process_gone(init_pid);
    assert_owner_state_gone(&name);
    std::fs::remove_file(shim.join("systemctl")).unwrap();
    std::fs::remove_dir(&shim).unwrap();
}

fn assert_container_gone(name: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let status = Command::new("podman")
            .args(["container", "exists", name])
            .status()
            .unwrap();
        if status.code() == Some(1) {
            break;
        }
        assert!(Instant::now() < deadline, "owner left container running");
        thread::sleep(Duration::from_millis(100));
    }
}

fn container_init_pid(name: &str) -> u32 {
    let output = Command::new("podman")
        .args(["inspect", "--format", "{{.State.Pid}}", name])
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap()
}

fn assert_init_process_gone(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(20);
    let proc_entry = format!("/proc/{pid}");
    while std::path::Path::new(&proc_entry).exists() {
        assert!(Instant::now() < deadline, "container init process survived");
        thread::sleep(Duration::from_millis(100));
    }
}

fn assert_owner_state_gone(name: &str) {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").unwrap();
    let state_dir = std::path::Path::new(&runtime).join(name);
    let tombstone = state_dir.with_extension("done");
    let deadline = Instant::now() + Duration::from_secs(20);
    while state_dir.exists() || tombstone.exists() {
        assert!(Instant::now() < deadline, "owner left runtime state behind");
        thread::sleep(Duration::from_millis(100));
    }
}

fn assert_no_state_for_launcher(pid: u32) {
    let prefix = format!("reef-strong-{pid}-");
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").unwrap();
    let residue: Vec<_> = std::fs::read_dir(runtime)
        .unwrap()
        .map(Result::unwrap)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
        .collect();
    assert!(residue.is_empty(), "probe left runtime state");
}

fn owner_pid(name: &str) -> i32 {
    let output = Command::new("systemctl")
        .args([
            "--user",
            "show",
            "--property=MainPID",
            "--value",
            &format!("{name}.service"),
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let pid: i32 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap();
    assert!(pid > 0);
    pid
}

#[test]
fn killing_the_reef_launcher_removes_a_detached_container_child() {
    if std::env::var_os("REEF_STRONG_LINUX_NATIVE_TEST").is_none() {
        return;
    }

    let (mut launcher, name) = started_probe();

    let top = Command::new("podman")
        .args(["top", &name])
        .output()
        .unwrap();
    assert!(top.status.success(), "container did not start");
    assert!(String::from_utf8_lossy(&top.stdout).contains("sleep"));
    let init_pid = container_init_pid(&name);
    assert!(init_pid > 0);

    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(launcher.id().try_into().unwrap()),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    launcher.wait().unwrap();

    assert_container_gone(&name);
    assert_init_process_gone(init_pid);
    assert_owner_state_gone(&name);
}

#[test]
fn killing_the_supervised_owner_cleans_the_existing_container() {
    if std::env::var_os("REEF_STRONG_LINUX_NATIVE_TEST").is_none() {
        return;
    }

    let (mut launcher, name) = started_probe();
    let init_pid = container_init_pid(&name);
    assert!(init_pid > 0);
    let pid = owner_pid(&name);

    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();

    assert!(!launcher.wait().unwrap().success());
    assert_container_gone(&name);
    assert_init_process_gone(init_pid);
    assert_owner_state_gone(&name);
}

#[test]
fn stopping_the_owner_service_removes_its_container() {
    if std::env::var_os("REEF_STRONG_LINUX_NATIVE_TEST").is_none() {
        return;
    }

    let (mut launcher, name) = started_probe();
    let init_pid = container_init_pid(&name);
    assert!(init_pid > 0);

    let status = Command::new("systemctl")
        .args(["--user", "stop", &format!("{name}.service")])
        .status()
        .unwrap();

    assert!(status.success());
    assert!(!launcher.wait().unwrap().success());
    assert_container_gone(&name);
    assert_init_process_gone(init_pid);
    assert_owner_state_gone(&name);
}

#[test]
fn stopping_an_unresponsive_owner_still_removes_its_container() {
    if std::env::var_os("REEF_STRONG_LINUX_NATIVE_TEST").is_none() {
        return;
    }

    let (mut launcher, name) = started_probe();
    let init_pid = container_init_pid(&name);
    assert!(init_pid > 0);
    let pid = owner_pid(&name);
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGSTOP,
    )
    .unwrap();

    let status = Command::new("systemctl")
        .args(["--user", "stop", &format!("{name}.service")])
        .status()
        .unwrap();

    assert!(status.success());
    assert!(!launcher.wait().unwrap().success());
    assert_container_gone(&name);
    assert_init_process_gone(init_pid);
}

#[test]
fn missing_owner_state_does_not_remove_an_unowned_container() {
    if std::env::var_os("REEF_STRONG_LINUX_NATIVE_TEST").is_none() {
        return;
    }

    let name = format!("reef-strong-unowned-{}", std::process::id());
    let state_dir = std::path::Path::new(&std::env::var_os("XDG_RUNTIME_DIR").unwrap()).join(&name);
    assert!(!state_dir.exists());
    let created = Command::new("podman")
        .args([
            "create",
            "--pull=never",
            &format!("--name={name}"),
            "docker.io/library/alpine:3.22",
            "true",
        ])
        .status()
        .unwrap();
    assert!(created.success());

    let owner = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args([
            "strong-owner",
            "--state-dir",
            state_dir.to_str().unwrap(),
            "--name",
            &name,
        ])
        .status()
        .unwrap();
    let still_exists = Command::new("podman")
        .args(["container", "exists", &name])
        .status()
        .unwrap();
    let removed = Command::new("podman")
        .args(["rm", "--force", &name])
        .status()
        .unwrap();

    assert!(owner.success());
    assert!(still_exists.success(), "owner removed an unowned container");
    assert!(removed.success());
}

#[test]
fn existing_owner_state_does_not_remove_a_name_collision() {
    if std::env::var_os("REEF_STRONG_LINUX_NATIVE_TEST").is_none() {
        return;
    }

    let name = format!("reef-strong-collision-{}", std::process::id());
    let state_dir = std::path::Path::new(&std::env::var_os("XDG_RUNTIME_DIR").unwrap()).join(&name);
    let config = state_dir.join("podman-home/.config/containers");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::set_permissions(
        &state_dir,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    std::fs::create_dir(state_dir.join("hooks")).unwrap();
    std::fs::File::create(state_dir.join("owner.lock")).unwrap();
    std::fs::File::create(config.join("mounts.conf")).unwrap();
    std::fs::File::create(config.join("containers.conf")).unwrap();
    std::fs::File::create(state_dir.join("attempted")).unwrap();
    let created = Command::new("podman")
        .args([
            "create",
            "--pull=never",
            &format!("--name={name}"),
            "docker.io/library/alpine:3.22",
            "true",
        ])
        .status()
        .unwrap();
    assert!(created.success());

    let owner = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args([
            "strong-owner",
            "--state-dir",
            state_dir.to_str().unwrap(),
            "--name",
            &name,
        ])
        .status()
        .unwrap();
    let still_exists = Command::new("podman")
        .args(["container", "exists", &name])
        .status()
        .unwrap();
    let removed = Command::new("podman")
        .args(["rm", "--force", &name])
        .status()
        .unwrap();

    assert!(owner.success());
    assert!(still_exists.success(), "owner removed a name collision");
    assert!(removed.success());
    assert_owner_state_gone(&name);
}

#[test]
fn failed_probe_removes_podman_created_cache_and_tombstone() {
    if std::env::var_os("REEF_STRONG_LINUX_NATIVE_TEST").is_none() {
        return;
    }

    for _ in 0..2 {
        let image = format!("reef-test-image-missing-{}", std::process::id());
        let child = Command::new(env!("CARGO_BIN_EXE_reef"))
            .args(["strong-owner-probe", "--image", &image, "--", "true"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        assert!(!child.wait_with_output().unwrap().status.success());
        assert_no_state_for_launcher(pid);
    }
}

#[test]
fn probe_uses_the_launchers_selected_storage_configuration() {
    if std::env::var_os("REEF_STRONG_LINUX_NATIVE_TEST").is_none() {
        return;
    }

    let runtime = std::env::var_os("XDG_RUNTIME_DIR").unwrap();
    let fixture = std::path::Path::new(&runtime)
        .join(format!("reef-storage-selector-test-{}", std::process::id()));
    std::fs::create_dir(&fixture).unwrap();
    let store = fixture.join("image-store");
    let config = fixture.join("storage.conf");
    std::fs::write(
        &config,
        format!(
            "[storage]\ndriver = \"overlay\"\nrootless_storage_path = \"{}\"\n",
            store.display()
        ),
    )
    .unwrap();

    let info = Command::new("podman")
        .env("CONTAINERS_STORAGE_CONF", &config)
        .args(["info", "--format", "{{.Store.GraphRoot}}"])
        .output()
        .unwrap();
    assert!(
        info.status.success(),
        "Podman rejected the test storage config"
    );
    assert_eq!(
        String::from_utf8_lossy(&info.stdout).trim(),
        store.to_str().unwrap()
    );

    let child = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args([
            "strong-owner-probe",
            "--image",
            "docker.io/library/alpine:3.22",
            "--",
            "true",
        ])
        .env("CONTAINERS_STORAGE_CONF", &config)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let output = child.wait_with_output().unwrap();
    assert!(
        !output.status.success(),
        "probe used an image outside the selected store"
    );
    assert_no_state_for_launcher(pid);
    std::fs::remove_dir_all(&fixture).unwrap();
}

#[test]
fn non_utf8_image_store_is_rejected_before_owner_start() {
    if std::env::var_os("REEF_STRONG_LINUX_NATIVE_TEST").is_none() {
        return;
    }

    let child = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args([
            "strong-owner-probe",
            "--image",
            "docker.io/library/alpine:3.22",
            "--",
            "true",
        ])
        .env(
            "XDG_DATA_HOME",
            std::ffi::OsString::from_vec(b"/tmp/reef-nonutf8-\xff".to_vec()),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("image store path must be UTF-8"));
    assert_no_state_for_launcher(pid);
}

#[test]
fn recovery_does_not_remove_a_replacement_with_the_same_name_and_label() {
    if std::env::var_os("REEF_STRONG_LINUX_NATIVE_TEST").is_none() {
        return;
    }

    let name = format!("reef-strong-replacement-{}", std::process::id());
    let state_dir = std::path::Path::new(&std::env::var_os("XDG_RUNTIME_DIR").unwrap()).join(&name);
    let config = state_dir.join("podman-home/.config/containers");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::set_permissions(
        &state_dir,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    std::fs::create_dir(state_dir.join("hooks")).unwrap();
    std::fs::File::create(state_dir.join("owner.lock")).unwrap();
    std::fs::File::create(config.join("mounts.conf")).unwrap();
    std::fs::File::create(config.join("containers.conf")).unwrap();
    std::fs::File::create(state_dir.join("attempted")).unwrap();

    let create = || {
        let output = Command::new("podman")
            .args([
                "create",
                "--pull=never",
                &format!("--name={name}"),
                &format!("--label=io.reef.owner={name}"),
                "docker.io/library/alpine:3.22",
                "true",
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    let original_id = create();
    assert!(
        Command::new("podman")
            .args(["rm", "--force", &original_id])
            .status()
            .unwrap()
            .success()
    );
    let replacement_id = create();
    std::fs::write(state_dir.join("owned.id"), original_id).unwrap();
    let store = Command::new("podman")
        .args(["info", "--format", "{{json .Store}}"])
        .output()
        .unwrap();
    assert!(store.status.success());
    std::fs::write(state_dir.join("podman-store.json"), store.stdout).unwrap();

    let owner = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args([
            "strong-owner",
            "--state-dir",
            state_dir.to_str().unwrap(),
            "--name",
            &name,
        ])
        .status()
        .unwrap();
    let still_exists = Command::new("podman")
        .args(["container", "exists", &replacement_id])
        .status()
        .unwrap();
    let removed = Command::new("podman")
        .args(["rm", "--force", &replacement_id])
        .status()
        .unwrap();

    assert!(owner.success());
    assert!(
        still_exists.success(),
        "owner removed a replacement container"
    );
    assert!(removed.success());
    assert_owner_state_gone(&name);
}

#[test]
fn recovery_removes_a_tombstone_after_the_original_state_is_gone() {
    if std::env::var_os("REEF_STRONG_LINUX_NATIVE_TEST").is_none() {
        return;
    }

    let name = format!("reef-strong-tombstone-{}", std::process::id());
    let state_dir = std::path::Path::new(&std::env::var_os("XDG_RUNTIME_DIR").unwrap()).join(&name);
    let tombstone = state_dir.with_extension("done");
    std::fs::create_dir_all(tombstone.join("podman-home/.cache/containers")).unwrap();
    std::fs::write(
        tombstone.join("podman-home/.cache/containers/residue"),
        b"cache",
    )
    .unwrap();
    assert!(!state_dir.exists());

    let owner = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args([
            "strong-owner",
            "--state-dir",
            state_dir.to_str().unwrap(),
            "--name",
            &name,
        ])
        .status()
        .unwrap();

    assert!(owner.success());
    assert!(!tombstone.exists(), "recovery left its tombstone");
}

#[test]
fn recovery_finds_its_container_before_podman_writes_the_cid_file() {
    if std::env::var_os("REEF_STRONG_LINUX_NATIVE_TEST").is_none() {
        return;
    }

    let name = format!("reef-strong-cid-gap-{}", std::process::id());
    let token = "1234567890abcdef1234567890abcdef";
    let state_dir = std::path::Path::new(&std::env::var_os("XDG_RUNTIME_DIR").unwrap()).join(&name);
    let config = state_dir.join("podman-home/.config/containers");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::set_permissions(
        &state_dir,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    std::fs::create_dir(state_dir.join("hooks")).unwrap();
    std::fs::File::create(state_dir.join("owner.lock")).unwrap();
    std::fs::File::create(config.join("mounts.conf")).unwrap();
    std::fs::File::create(config.join("containers.conf")).unwrap();
    std::fs::File::create(state_dir.join("attempted")).unwrap();
    std::fs::write(state_dir.join("owner.token"), token).unwrap();
    let store = Command::new("podman")
        .args(["info", "--format", "{{json .Store}}"])
        .output()
        .unwrap();
    assert!(store.status.success());
    std::fs::write(state_dir.join("podman-store.json"), store.stdout).unwrap();

    let created = Command::new("podman")
        .args([
            "create",
            "--pull=never",
            &format!("--name={name}"),
            &format!("--label=io.reef.owner={name}"),
            &format!("--label=io.reef.token={token}"),
            "docker.io/library/alpine:3.22",
            "true",
        ])
        .status()
        .unwrap();
    assert!(created.success());
    // Podman can open the CID file before writing its ID and then be killed.
    std::fs::File::create(state_dir.join("container.cid")).unwrap();

    let owner = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args([
            "strong-owner",
            "--state-dir",
            state_dir.to_str().unwrap(),
            "--name",
            &name,
        ])
        .status()
        .unwrap();
    let exists = Command::new("podman")
        .args(["container", "exists", &name])
        .status()
        .unwrap();

    assert!(owner.success());
    assert_eq!(
        exists.code(),
        Some(1),
        "CID-gap container survived recovery"
    );
    assert_owner_state_gone(&name);
}
