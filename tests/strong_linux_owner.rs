#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

fn started_probe() -> (std::process::Child, String) {
    let mut launcher = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args([
            "strong-owner-probe",
            "--image",
            "docker.io/library/alpine:3.22",
            "--",
            "sh",
            "-c",
            "setsid sleep 300 & wait",
        ])
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
    let deadline = Instant::now() + Duration::from_secs(20);
    while state_dir.exists() {
        assert!(Instant::now() < deadline, "owner left runtime state behind");
        thread::sleep(Duration::from_millis(100));
    }
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
