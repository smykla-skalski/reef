use super::MemoryConsumer;
use std::collections::BTreeMap;
use sysinfo::System;

fn family(name: &str) -> &'static str {
    let name = name.to_ascii_lowercase();
    match name.as_str() {
        "codex" | "claude" | "opencode" => "agent runtime",
        "go" | "rustc" | "compile" | "link" | "cgo" | "cc" | "gcc" | "g++" | "clang"
        | "clang++" | "swiftc" => "compiler/build",
        "golangci-lint" | "clippy-driver" | "ruff" | "eslint" | "staticcheck" => "linter",
        "google chrome" | "safari" | "safari web content" | "firefox" | "firefox helper"
        | "arc" => "browser",
        name if name.starts_with("google chrome helper") => "browser",
        "code" | "code helper" | "cursor" | "cursor helper" | "goland" | "intellij idea"
        | "zed" => "editor/IDE",
        "docker"
        | "com.docker.backend"
        | "orbstack"
        | "orbstack helper"
        | "qemu-system-aarch64"
        | "qemu-system-x86_64"
        | "containerd"
        | "colima" => "container/VM",
        "windowserver" | "kernel_task" | "finder" | "dock" => "system UI",
        _ => "other processes",
    }
}

pub(super) fn sample(system: &System) -> Vec<MemoryConsumer> {
    let mut groups: BTreeMap<&'static str, MemoryConsumer> = BTreeMap::new();
    for process in system.processes().values() {
        let name = process.name().to_string_lossy();
        let label = family(&name);
        let group = groups.entry(label).or_insert(MemoryConsumer {
            family: label,
            process_count: 0,
            rss_bytes: 0,
            largest_process_rss_bytes: 0,
        });
        let rss = process.memory();
        group.process_count += 1;
        group.rss_bytes = group.rss_bytes.saturating_add(rss);
        group.largest_process_rss_bytes = group.largest_process_rss_bytes.max(rss);
    }
    groups.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_reduced_to_safe_families() {
        assert_eq!(family("Google Chrome Helper"), "browser");
        assert_eq!(family("Google Chrome Helper (Renderer)"), "browser");
        assert_eq!(family("Google Chrome Helper (GPU)"), "browser");
        assert_eq!(family("OrbStack Helper"), "container/VM");
        assert_eq!(family("rustc"), "compiler/build");
        assert_eq!(family("secret-project-name"), "other processes");
    }
}
