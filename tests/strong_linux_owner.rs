#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

fn started_probe() -> (std::process::Child, String) {
    started_probe_with_path(None)
}

#[test]
fn contained_command_cannot_reach_host_control_planes() {
    if std::env::var_os("REEF_STRONG_LINUX_NATIVE_TEST").is_none() {
        return;
    }

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_listener = Arc::clone(&stop);
    let server = thread::spawn(move || {
        while !stop_listener.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                        .unwrap();
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("host canary failed: {error}"),
            }
        }
    });

    let output = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args([
            "strong-owner-probe",
            "--image",
            "docker.io/library/alpine:3.22",
            "--",
            "sh",
            "-ec",
            r#"
                test ! -e /run/user
                test ! -e /run/systemd/private
                test ! -e /run/podman/podman.sock
                test ! -e /var/run/docker.sock
                test ! -e /dev/kvm
                test -z "${DBUS_SESSION_BUS_ADDRESS:-}"
                test -z "${CONTAINER_HOST:-}"
                test -z "${DOCKER_HOST:-}"
                command -v wget >/dev/null
                test "$(wc -l < /proc/net/route)" -eq 1
                test "$(awk '/^CapEff:/ { print $2 }' /proc/self/status)" = 0000000000000000
                test "$(awk '/^NoNewPrivs:/ { print $2 }' /proc/self/status)" = 1
                if wget -q -T 2 -O /dev/null "http://127.0.0.1:$1/" 2>/dev/null; then
                    exit 1
                fi
                if mkdir /sys/fs/cgroup/reef-escape 2>/dev/null; then
                    rmdir /sys/fs/cgroup/reef-escape
                    exit 1
                fi
                printf 'sealed\n'
            "#,
            "reef-isolation-test",
        ])
        .arg(port)
        .output()
        .unwrap();

    stop.store(true, Ordering::Relaxed);
    server.join().unwrap();

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).ends_with("sealed\n"));
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
fn attached_command_forwards_input_output_error_and_exit_status() {
    if std::env::var_os("REEF_STRONG_LINUX_NATIVE_TEST").is_none() {
        return;
    }

    let mut launcher = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args([
            "strong-owner-probe",
            "--image",
            "docker.io/library/alpine:3.22",
            "--",
            "sh",
            "-c",
            "read line; printf 'out:%s\\n' \"$line\"; printf 'err:%s\\n' \"$line\" >&2; exit 7",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    launcher
        .stdin
        .take()
        .unwrap()
        .write_all(b"hello\n")
        .unwrap();
    let output = launcher.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(7));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("READY reef-strong-"), "{stdout}");
    assert!(stdout.ends_with("out:hello\n"), "{stdout}");
    assert_eq!(String::from_utf8(output.stderr).unwrap(), "err:hello\n");
}

#[test]
fn slow_stdout_reader_does_not_truncate_finished_command_output() {
    if std::env::var_os("REEF_STRONG_LINUX_NATIVE_TEST").is_none() {
        return;
    }

    let mut launcher = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args([
            "strong-owner-probe",
            "--image",
            "docker.io/library/alpine:3.22",
            "--",
            "sh",
            "-c",
            "head -c 131072 /dev/zero",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = launcher.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let result = reader.read_line(&mut line);
        let _ = sender.send((reader, line, result));
    });
    let (reader, line, read) = receiver.recv_timeout(Duration::from_secs(90)).unwrap();
    read.unwrap();
    assert!(line.starts_with("READY reef-strong-"));

    thread::sleep(Duration::from_secs(8));
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = reader;
        let mut output = Vec::new();
        let result = reader.read_to_end(&mut output);
        let _ = sender.send((result, output));
    });
    let (read, bytes) = match receiver.recv_timeout(Duration::from_secs(90)) {
        Ok(output) => output,
        Err(error) => {
            let _ = launcher.kill();
            let _ = launcher.wait();
            panic!("output drain timed out: {error}");
        }
    };
    read.unwrap();
    let result = launcher.wait_with_output().unwrap();
    assert_eq!(
        result.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(bytes.len(), 131_072);
    assert!(bytes.iter().all(|byte| *byte == 0));
}

#[test]
fn opted_in_usage_reports_observed_cgroup_load_without_command_text() {
    if std::env::var_os("REEF_STRONG_LINUX_NATIVE_TEST").is_none() {
        return;
    }

    let output = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args([
            "strong-owner-probe",
            "--image",
            "docker.io/library/alpine:3.22",
            "--metrics",
            "--",
            "sh",
            "-c",
            "dd if=/dev/zero of=/tmp/blob bs=1M count=4 2>/dev/null; sleep 1",
        ])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    let metrics = stderr
        .lines()
        .find_map(|line| line.strip_prefix("REEF_MEASUREMENT "))
        .unwrap();
    let values: serde_json::Value = serde_json::from_str(metrics).unwrap();
    assert!(values["wall_ms"].as_u64().unwrap() >= 1_000);
    assert!(values["observed_peak_memory_bytes"].as_u64().unwrap() > 0);
    assert!(!metrics.contains("/tmp/blob"));
}

#[test]
fn metrics_start_on_a_new_line_after_unterminated_command_stderr() {
    if std::env::var_os("REEF_STRONG_LINUX_NATIVE_TEST").is_none() {
        return;
    }

    let output = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args([
            "strong-owner-probe",
            "--image",
            "docker.io/library/alpine:3.22",
            "--metrics",
            "--",
            "sh",
            "-c",
            "printf err >&2",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.starts_with("err\nREEF_MEASUREMENT "), "{stderr}");
    let measurement = stderr
        .lines()
        .find_map(|line| line.strip_prefix("REEF_MEASUREMENT "));
    assert!(measurement.is_some());
}

#[test]
fn signal_to_launcher_reaches_contained_command_and_preserves_its_exit() {
    if std::env::var_os("REEF_STRONG_LINUX_NATIVE_TEST").is_none() {
        return;
    }

    let mut launcher = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args([
            "strong-owner-probe",
            "--image",
            "docker.io/library/alpine:3.22",
            "--",
            "sh",
            "-c",
            "trap 'printf forwarded >&2; exit 23' TERM; while :; do sleep 1; done",
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

    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(launcher.id()).unwrap()),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let output = launcher.wait_with_output().unwrap();

    assert_eq!(
        output.status.code(),
        Some(23),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("forwarded"));
    assert_container_gone(&name);
    assert_owner_state_gone(&name);
}

#[test]
fn ignored_signal_forces_teardown_before_cancelled_exit() {
    if std::env::var_os("REEF_STRONG_LINUX_NATIVE_TEST").is_none() {
        return;
    }

    let mut launcher = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args([
            "strong-owner-probe",
            "--image",
            "docker.io/library/alpine:3.22",
            "--",
            "sh",
            "-c",
            "trap '' TERM; while :; do sleep 1; done",
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

    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(launcher.id()).unwrap()),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let output = launcher.wait_with_output().unwrap();

    assert_eq!(
        output.status.code(),
        Some(143),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_container_gone(&name);
    assert_owner_state_gone(&name);
}

#[test]
fn signal_during_probe_setup_removes_state_before_returning() {
    if std::env::var_os("REEF_STRONG_LINUX_NATIVE_TEST").is_none() {
        return;
    }

    let mut launcher = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args([
            "strong-owner-probe",
            "--image",
            "docker.io/library/alpine:3.22",
            "--",
            "sleep",
            "10",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").unwrap();
    let prefix = format!("reef-strong-{}-", launcher.id());
    let deadline = Instant::now() + Duration::from_secs(10);
    let name = loop {
        let match_name = std::fs::read_dir(&runtime)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .find(|name| name.starts_with(&prefix));
        if let Some(name) = match_name {
            break name;
        }
        if Instant::now() >= deadline {
            let _ = launcher.kill();
            let _ = launcher.wait();
            panic!("probe never created owner state");
        }
        thread::sleep(Duration::from_millis(5));
    };
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(launcher.id()).unwrap()),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = launcher.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = launcher.kill();
            let _ = launcher.wait();
            panic!("early cancellation did not return");
        }
        thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(status.code(), Some(143));
    assert_container_gone(&name);
    assert_owner_state_gone(&name);
}

#[test]
fn signal_during_stalled_owner_startup_removes_state() {
    if std::env::var_os("REEF_STRONG_LINUX_NATIVE_TEST").is_none() {
        return;
    }

    let runtime = std::env::var_os("XDG_RUNTIME_DIR").unwrap();
    let shim =
        std::path::Path::new(&runtime).join(format!("reef-systemd-stall-{}", std::process::id()));
    std::fs::create_dir(&shim).unwrap();
    let marker = shim.join("started");
    let executable = shim.join("systemd-run");
    std::fs::write(
        &executable,
        "#!/bin/sh\n: > \"$REEF_SYSTEMD_RUN_MARKER\"\nexec sleep 30\n",
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let original_path = std::env::var_os("PATH").unwrap();
    let path = std::env::join_paths(
        std::iter::once(shim.clone()).chain(std::env::split_paths(&original_path)),
    )
    .unwrap();
    let mut launcher = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args([
            "strong-owner-probe",
            "--image",
            "docker.io/library/alpine:3.22",
            "--",
            "sleep",
            "10",
        ])
        .env("PATH", path)
        .env("REEF_SYSTEMD_RUN_MARKER", &marker)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let prefix = format!("reef-strong-{}-", launcher.id());
    let deadline = Instant::now() + Duration::from_secs(10);
    let name = loop {
        let name = std::fs::read_dir(&runtime)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .find(|name| name.starts_with(&prefix));
        if marker.exists()
            && let Some(name) = name
        {
            break name;
        }
        if Instant::now() >= deadline {
            let _ = launcher.kill();
            let _ = launcher.wait();
            panic!("stalled systemd-run shim did not start");
        }
        thread::sleep(Duration::from_millis(5));
    };
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(launcher.id()).unwrap()),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(12);
    let status = loop {
        if let Some(status) = launcher.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = launcher.kill();
            let _ = launcher.wait();
            panic!("startup cancellation did not return");
        }
        thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(status.code(), Some(143));
    assert_owner_state_gone(&name);
    std::fs::remove_dir_all(shim).unwrap();
}

#[test]
fn signal_during_stalled_podman_store_query_removes_state() {
    if std::env::var_os("REEF_STRONG_LINUX_NATIVE_TEST").is_none() {
        return;
    }

    let runtime = std::env::var_os("XDG_RUNTIME_DIR").unwrap();
    let shim =
        std::path::Path::new(&runtime).join(format!("reef-podman-stall-{}", std::process::id()));
    std::fs::create_dir(&shim).unwrap();
    let marker = shim.join("started");
    let executable = shim.join("podman");
    std::fs::write(
        &executable,
        "#!/bin/sh\n: > \"$REEF_PODMAN_STALL_MARKER\"\nexec sleep 30\n",
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let original_path = std::env::var_os("PATH").unwrap();
    let path = std::env::join_paths(
        std::iter::once(shim.clone()).chain(std::env::split_paths(&original_path)),
    )
    .unwrap();
    let mut launcher = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args([
            "strong-owner-probe",
            "--image",
            "docker.io/library/alpine:3.22",
            "--",
            "sleep",
            "10",
        ])
        .env("PATH", path)
        .env("REEF_PODMAN_STALL_MARKER", &marker)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let prefix = format!("reef-strong-{}-", launcher.id());
    let deadline = Instant::now() + Duration::from_secs(10);
    let name = loop {
        let name = std::fs::read_dir(&runtime)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .find(|name| name.starts_with(&prefix));
        if marker.exists()
            && let Some(name) = name
        {
            break name;
        }
        if Instant::now() >= deadline {
            let _ = launcher.kill();
            let _ = launcher.wait();
            panic!("stalled Podman shim did not start");
        }
        thread::sleep(Duration::from_millis(5));
    };
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(launcher.id()).unwrap()),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = launcher.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = launcher.kill();
            let _ = launcher.wait();
            panic!("Podman setup cancellation did not return");
        }
        thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(status.code(), Some(143));
    assert_owner_state_gone(&name);
    std::fs::remove_dir_all(shim).unwrap();
}

#[test]
fn signal_after_podman_store_client_exit_stops_output_wait() {
    if std::env::var_os("REEF_STRONG_LINUX_NATIVE_TEST").is_none() {
        return;
    }

    let runtime = std::env::var_os("XDG_RUNTIME_DIR").unwrap();
    let shim = std::path::Path::new(&runtime)
        .join(format!("reef-podman-output-stall-{}", std::process::id()));
    std::fs::create_dir(&shim).unwrap();
    let marker = shim.join("started");
    let executable = shim.join("podman");
    std::fs::write(
        &executable,
        "#!/bin/sh\nsleep 3 &\n: > \"$REEF_PODMAN_STALL_MARKER\"\nexit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let original_path = std::env::var_os("PATH").unwrap();
    let path = std::env::join_paths(
        std::iter::once(shim.clone()).chain(std::env::split_paths(&original_path)),
    )
    .unwrap();
    let mut launcher = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args([
            "strong-owner-probe",
            "--image",
            "docker.io/library/alpine:3.22",
            "--",
            "sleep",
            "10",
        ])
        .env("PATH", path)
        .env("REEF_PODMAN_STALL_MARKER", &marker)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let prefix = format!("reef-strong-{}-", launcher.id());
    let deadline = Instant::now() + Duration::from_secs(10);
    let name = loop {
        let name = std::fs::read_dir(&runtime)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .find(|name| name.starts_with(&prefix));
        if marker.exists()
            && let Some(name) = name
        {
            break name;
        }
        if Instant::now() >= deadline {
            let _ = launcher.kill();
            let _ = launcher.wait();
            panic!("Podman output-stall shim did not start");
        }
        thread::sleep(Duration::from_millis(5));
    };
    thread::sleep(Duration::from_millis(200));
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(launcher.id()).unwrap()),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        if let Some(status) = launcher.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = launcher.kill();
            let _ = launcher.wait();
            panic!("Podman output wait ignored cancellation");
        }
        thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(status.code(), Some(143));
    assert_owner_state_gone(&name);
    std::fs::remove_dir_all(shim).unwrap();
}

#[test]
fn cancellation_does_not_wait_for_an_unread_stdout_pipe() {
    if std::env::var_os("REEF_STRONG_LINUX_NATIVE_TEST").is_none() {
        return;
    }

    let mut launcher = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args([
            "strong-owner-probe",
            "--image",
            "docker.io/library/alpine:3.22",
            "--",
            "sh",
            "-c",
            "trap '' TERM; head -c 131072 /dev/zero; while :; do sleep 1; done",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = launcher.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let result = reader.read_line(&mut line);
        let _ = sender.send((reader, line, result));
    });
    let (reader, line, read) = receiver.recv_timeout(Duration::from_secs(90)).unwrap();
    read.unwrap();
    let name = line.strip_prefix("READY ").unwrap().trim().to_owned();
    let _unread_output = reader;

    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(launcher.id()).unwrap()),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(12);
    let status = loop {
        if let Some(status) = launcher.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = launcher.kill();
            let _ = launcher.wait();
            panic!("cancelled launcher waited for unread stdout");
        }
        thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(status.code(), Some(143));
    assert_container_gone(&name);
    assert_owner_state_gone(&name);
}

#[test]
fn cancellation_stops_a_post_exit_attach_blocked_on_output() {
    if std::env::var_os("REEF_STRONG_LINUX_NATIVE_TEST").is_none() {
        return;
    }

    let mut launcher = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args([
            "strong-owner-probe",
            "--image",
            "docker.io/library/alpine:3.22",
            "--",
            "sh",
            "-c",
            "head -c 196608 /dev/zero",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = launcher.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let result = reader.read_line(&mut line);
        let _ = sender.send((reader, line, result));
    });
    let (reader, line, read) = receiver.recv_timeout(Duration::from_secs(90)).unwrap();
    read.unwrap();
    let name = line.strip_prefix("READY ").unwrap().trim().to_owned();
    let _unread_output = reader;

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let output = Command::new("podman")
            .args(["inspect", "--format", "{{.State.Status}}", &name])
            .output()
            .unwrap();
        if output.status.success() && output.stdout == b"exited\n" {
            break;
        }
        if Instant::now() >= deadline {
            let _ = launcher.kill();
            let _ = launcher.wait();
            panic!("container did not exit before attach drained");
        }
        thread::sleep(Duration::from_millis(50));
    }
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(launcher.id()).unwrap()),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(12);
    let status = loop {
        if let Some(status) = launcher.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = launcher.kill();
            let _ = launcher.wait();
            panic!("post-exit cancellation waited for Podman attach");
        }
        thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(status.code(), Some(143));
    assert_container_gone(&name);
    assert_owner_state_gone(&name);
}

#[test]
fn cancellation_after_owner_completion_stops_an_unread_output_relay() {
    if std::env::var_os("REEF_STRONG_LINUX_NATIVE_TEST").is_none() {
        return;
    }

    let mut launcher = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args([
            "strong-owner-probe",
            "--image",
            "docker.io/library/alpine:3.22",
            "--",
            "sh",
            "-c",
            "head -c 131072 /dev/zero",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = launcher.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let result = reader.read_line(&mut line);
        let _ = sender.send((reader, line, result));
    });
    let (reader, line, read) = receiver.recv_timeout(Duration::from_secs(90)).unwrap();
    read.unwrap();
    let name = line.strip_prefix("READY ").unwrap().trim().to_owned();
    let _unread_output = reader;

    let runtime = std::env::var_os("XDG_RUNTIME_DIR").unwrap();
    let state_dir = std::path::Path::new(&runtime).join(&name);
    let deadline = Instant::now() + Duration::from_secs(20);
    while state_dir.exists() || state_dir.with_extension("done").exists() {
        if Instant::now() >= deadline {
            let _ = launcher.kill();
            let _ = launcher.wait();
            panic!("owner did not complete before output drain");
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(launcher.try_wait().unwrap().is_none());
    thread::sleep(Duration::from_millis(250));
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(launcher.id()).unwrap()),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = launcher.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = launcher.kill();
            let _ = launcher.wait();
            panic!("post-DONE cancellation waited for unread stdout");
        }
        thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(status.code(), Some(143));
    assert_container_gone(&name);
    assert_owner_state_gone(&name);
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
