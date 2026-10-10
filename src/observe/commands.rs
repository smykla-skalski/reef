use super::{CommandObservation, sum_metric};
use crate::agent_observe::{AgentKind, Registration};
use crate::tool_classification;
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

fn owner(
    pid: u32,
    parents: &HashMap<u32, Option<u32>>,
    roots: &HashMap<u32, Registration>,
) -> Option<AgentKind> {
    let mut current = pid;
    for _ in 0..=parents.len() {
        if let Some(root) = roots.get(&current) {
            return Some(root.kind);
        }
        current = parents.get(&current).copied().flatten()?;
    }
    None
}

fn family(name: &str, args: &[OsString]) -> (&'static str, &'static str) {
    let lower = name.to_ascii_lowercase();
    let tool = lower.strip_suffix(".exe").unwrap_or(&lower);
    match tool {
        "go" | "cargo" | "mise" | "make" => {
            let category = tool_classification::classify(tool, args).unwrap_or("other");
            let family = match (tool, category) {
                ("go", "build") => "go build",
                ("go", "test") => "go test",
                ("go", "lint") => "go vet",
                ("cargo", "build") => "cargo build",
                ("cargo", "test") => "cargo test",
                ("cargo", "lint") => "cargo clippy",
                ("mise", "build") => "mise build",
                ("mise", "test") => "mise test",
                ("mise", "lint") => "mise lint/check",
                ("make", "build") => "make build",
                ("make", "test") => "make test",
                ("make", "lint") => "make lint/check",
                ("go", _) => "go other",
                ("cargo", _) => "cargo other",
                ("mise", _) => "mise other",
                _ => "make other",
            };
            (family, category)
        }
        "npm" | "pnpm" | "yarn" | "bun" => {
            let verb = args
                .iter()
                .filter_map(|arg| arg.to_str())
                .find(|arg| matches!(*arg, "build" | "test" | "lint" | "check"));
            match verb {
                Some("build") => ("package build", "build"),
                Some("test") => ("package test", "test"),
                Some("lint" | "check") => ("package lint/check", "lint"),
                _ => ("package other", "other"),
            }
        }
        "golangci-lint" => ("golangci-lint", "lint"),
        "rustc" | "compile" | "link" | "asm" | "cgo" | "cc" | "gcc" | "g++" | "clang"
        | "clang++" | "swiftc" => ("compiler/linker", "build"),
        "pytest" | "jest" | "vitest" => ("test runner", "test"),
        "ruff" | "eslint" | "staticcheck" | "clippy-driver" => ("linter", "lint"),
        "git" => ("git", "other"),
        "docker" | "podman" | "kubectl" => ("container tool", "container"),
        _ if std::path::Path::new(tool)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("test")) =>
        {
            ("Go test binary", "test")
        }
        _ => ("other", "other"),
    }
}

pub(super) fn sample(
    system: &mut System,
    roots: &HashMap<u32, Registration>,
) -> Vec<CommandObservation> {
    if roots.is_empty() {
        return Vec::new();
    }
    let parents: HashMap<_, _> = system
        .processes()
        .iter()
        .map(|(pid, process)| (pid.as_u32(), process.parent().map(Pid::as_u32)))
        .collect();
    let owned: Vec<_> = system
        .processes()
        .keys()
        .filter_map(|pid| {
            (!roots.contains_key(&pid.as_u32()))
                .then(|| owner(pid.as_u32(), &parents, roots))
                .flatten()
                .map(|kind| (*pid, kind))
        })
        .collect();
    if owned.is_empty() {
        return Vec::new();
    }
    let pids: Vec<_> = owned.iter().map(|(pid, _)| *pid).collect();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&pids),
        false,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::OnlyIfNotSet)
            .without_tasks(),
    );
    let mut groups: BTreeMap<(String, String), CommandObservation> = BTreeMap::new();
    for (pid, kind) in owned {
        let Some(process) = system.process(pid) else {
            continue;
        };
        let name = process.name().to_string_lossy();
        let args = process.cmd();
        let (family, category) = family(&name, args.get(1..).unwrap_or_default());
        let key = (kind.as_str().to_owned(), family.to_owned());
        let group = groups.entry(key).or_insert_with(|| CommandObservation {
            kind: kind.as_str().to_owned(),
            family: family.to_owned(),
            category: category.to_owned(),
            process_count: 0,
            cpu_percent: None,
            memory_bytes: 0,
            read_bytes: None,
            written_bytes: None,
        });
        let disk = process.disk_usage();
        group.process_count += 1;
        group.cpu_percent = sum_metric(group.cpu_percent, Some(process.cpu_usage()));
        group.memory_bytes = group.memory_bytes.saturating_add(process.memory());
        group.read_bytes = sum_metric(group.read_bytes, Some(disk.read_bytes));
        group.written_bytes = sum_metric(group.written_bytes, Some(disk.written_bytes));
    }
    groups.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn classifies_known_tools_without_retaining_arguments() {
        assert_eq!(
            family("go", &args(&["test", "./private/package"])),
            ("go test", "test")
        );
        assert_eq!(
            family("cargo", &args(&["clippy", "--all"])),
            ("cargo clippy", "lint")
        );
        assert_eq!(family("compile", &[]), ("compiler/linker", "build"));
        assert_eq!(
            family("golangci-lint", &args(&["run"])),
            ("golangci-lint", "lint")
        );
        assert_eq!(
            family("npm", &args(&["run", "build"])),
            ("package build", "build")
        );
        assert_eq!(family("private-secret-script", &[]), ("other", "other"));
    }

    #[test]
    fn ownership_stops_at_the_nearest_registered_agent() {
        let parents = HashMap::from([(10, None), (20, Some(10)), (30, Some(20))]);
        let roots = HashMap::from([
            (
                10,
                Registration {
                    kind: AgentKind::Codex,
                    pid: 10,
                    start_time: 1,
                },
            ),
            (
                20,
                Registration {
                    kind: AgentKind::Claude,
                    pid: 20,
                    start_time: 1,
                },
            ),
        ]);
        assert_eq!(owner(30, &parents, &roots), Some(AgentKind::Claude));
        assert_eq!(owner(20, &parents, &roots), Some(AgentKind::Claude));
        assert_eq!(owner(99, &parents, &roots), None);
    }
}
