use super::{CommandObservation, sum_metric};
use crate::agent_observe::{AgentKind, Registration};
use crate::tool_classification;
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::Path;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

#[derive(Clone)]
pub(crate) struct ProcessSample {
    pub pid: u32,
    pub start_time: u64,
    pub parent_pid: Option<u32>,
    pub parent_start_time: Option<u64>,
    pub kind: String,
    pub executable: String,
    pub subcommand: String,
    pub family: String,
    pub category: String,
    pub workspace_kind: String,
    pub workload: crate::workload::Workload,
    pub cpu_percent: f32,
    pub memory_bytes: u64,
    pub total_read_bytes: u64,
    pub total_written_bytes: u64,
}

#[derive(Default)]
pub(super) struct Capture {
    pub commands: Vec<CommandObservation>,
    pub processes: Vec<ProcessSample>,
}

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

fn executable(name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();
    match lower.strip_suffix(".exe").unwrap_or(&lower) {
        "go" => "go",
        "cargo" => "cargo",
        "mise" => "mise",
        "make" => "make",
        "npm" => "npm",
        "pnpm" => "pnpm",
        "yarn" => "yarn",
        "bun" => "bun",
        "node" => "node",
        "golangci-lint" => "golangci-lint",
        "rustc" => "rustc",
        "compile" => "compile",
        "link" => "link",
        "asm" => "asm",
        "cgo" => "cgo",
        "cc" => "cc",
        "gcc" => "gcc",
        "g++" => "g++",
        "clang" => "clang",
        "clang++" => "clang++",
        "swiftc" => "swiftc",
        "pytest" => "pytest",
        "jest" => "jest",
        "vitest" => "vitest",
        "ruff" => "ruff",
        "eslint" => "eslint",
        "staticcheck" => "staticcheck",
        "clippy-driver" => "clippy-driver",
        "git" => "git",
        "docker" => "docker",
        "podman" => "podman",
        "kubectl" => "kubectl",
        _ => "other",
    }
}

fn subcommand(executable: &str, args: &[OsString]) -> &'static str {
    let values: Vec<_> = args.iter().filter_map(|arg| arg.to_str()).collect();
    match executable {
        "go" => match values.first().copied() {
            Some("build") => "build",
            Some("test") => "test",
            Some("vet") => "vet",
            Some("generate") => "generate",
            Some("list") => "list",
            _ => "other",
        },
        "cargo" => match values.first().copied() {
            Some("build") => "build",
            Some("test") => "test",
            Some("check") => "check",
            Some("clippy") => "clippy",
            Some("run") => "run",
            Some("bench") => "bench",
            Some("doc") => "doc",
            _ => "other",
        },
        "mise" | "make" => match values.first().copied() {
            Some("build") => "build",
            Some("test") => "test",
            Some("lint") => "lint",
            Some("check") => "check",
            _ => "other",
        },
        "npm" | "pnpm" | "yarn" | "bun" => values
            .iter()
            .find_map(|value| match *value {
                "build" => Some("build"),
                "test" => Some("test"),
                "lint" => Some("lint"),
                "check" => Some("check"),
                _ => None,
            })
            .unwrap_or("other"),
        "git" => match values.first().copied() {
            Some("status") => "status",
            Some("diff") => "diff",
            Some("log") => "log",
            Some("show") => "show",
            Some("fetch") => "fetch",
            Some("pull") => "pull",
            Some("push") => "push",
            Some("commit") => "commit",
            Some("checkout") => "checkout",
            Some("switch") => "switch",
            Some("worktree") => "worktree",
            Some("merge") => "merge",
            Some("rebase") => "rebase",
            _ => "other",
        },
        "docker" | "podman" | "kubectl" => match values.first().copied() {
            Some("build") => "build",
            Some("run") => "run",
            Some("exec") => "exec",
            Some("test") => "test",
            Some("apply") => "apply",
            Some("get") => "get",
            Some("logs") => "logs",
            _ => "other",
        },
        _ => "none",
    }
}

fn workspace_kind(cwd: Option<&Path>) -> &'static str {
    let Some(mut current) = cwd else {
        return "unknown";
    };
    loop {
        let marker = current.join(".git");
        if marker.is_file() {
            return "worktree";
        }
        if marker.is_dir() {
            return "checkout";
        }
        let Some(parent) = current.parent() else {
            return "outside repository";
        };
        current = parent;
    }
}

fn owned_processes(system: &System, roots: &HashMap<u32, Registration>) -> Vec<(Pid, AgentKind)> {
    let parents: HashMap<_, _> = system
        .processes()
        .iter()
        .map(|(pid, process)| (pid.as_u32(), process.parent().map(Pid::as_u32)))
        .collect();
    system
        .processes()
        .keys()
        .filter_map(|pid| {
            (!roots.contains_key(&pid.as_u32()))
                .then(|| owner(pid.as_u32(), &parents, roots))
                .flatten()
                .map(|kind| (*pid, kind))
        })
        .collect()
}

pub(super) fn sample(
    system: &mut System,
    roots: &HashMap<u32, Registration>,
    dir: &Path,
) -> Capture {
    if roots.is_empty() {
        return Capture::default();
    }
    let owned = owned_processes(system, roots);
    if owned.is_empty() {
        return Capture::default();
    }
    let pids: Vec<_> = owned.iter().map(|(pid, _)| *pid).collect();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&pids),
        false,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_cwd(UpdateKind::Always)
            .without_tasks(),
    );
    let mut groups: BTreeMap<(String, String), CommandObservation> = BTreeMap::new();
    let mut workspace_cache = HashMap::new();
    let mut samples = Vec::new();
    let key = crate::workload::installation_key(dir).ok();
    for (pid, kind) in owned {
        let Some(process) = system.process(pid) else {
            continue;
        };
        let name = process.name().to_string_lossy();
        let args = process.cmd();
        let (family, category) = family(&name, args.get(1..).unwrap_or_default());
        let executable = executable(&name);
        let subcommand = subcommand(executable, args.get(1..).unwrap_or_default());
        let parent_pid = process.parent().map(Pid::as_u32);
        let parent_start_time = process
            .parent()
            .and_then(|parent| system.process(parent))
            .map(sysinfo::Process::start_time);
        let disk = process.disk_usage();
        let workspace_kind = process.cwd().map_or("unknown", |cwd| {
            *workspace_cache
                .entry(cwd.to_path_buf())
                .or_insert_with(|| workspace_kind(Some(cwd)))
        });
        let mut workload = crate::workload::identify(
            key.as_ref(),
            process.cwd(),
            executable,
            args.get(1..).unwrap_or_default(),
        );
        if family == "Go test binary" {
            "Go".clone_into(&mut workload.ecosystem);
        }
        samples.push(ProcessSample {
            pid: pid.as_u32(),
            start_time: process.start_time(),
            parent_pid,
            parent_start_time,
            kind: kind.as_str().to_owned(),
            executable: executable.to_owned(),
            subcommand: subcommand.to_owned(),
            family: family.to_owned(),
            category: category.to_owned(),
            workspace_kind: workspace_kind.to_owned(),
            workload,
            cpu_percent: process.cpu_usage(),
            memory_bytes: process.memory(),
            total_read_bytes: disk.total_read_bytes,
            total_written_bytes: disk.total_written_bytes,
        });
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
        group.process_count += 1;
        group.cpu_percent = sum_metric(group.cpu_percent, Some(process.cpu_usage()));
        group.memory_bytes = group.memory_bytes.saturating_add(process.memory());
        group.read_bytes = sum_metric(group.read_bytes, Some(disk.read_bytes));
        group.written_bytes = sum_metric(group.written_bytes, Some(disk.written_bytes));
    }
    samples.sort_by_key(|sample| (sample.pid, sample.start_time));
    Capture {
        commands: groups.into_values().collect(),
        processes: samples,
    }
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
        assert_eq!(executable("private-secret-script"), "other");
        assert_eq!(
            subcommand("go", &args(&["test", "./private/package"])),
            "test"
        );
        assert_eq!(subcommand("npm", &args(&["run", "private-task"])), "other");
    }

    #[test]
    fn recognizes_worktrees_without_retaining_paths() {
        let root = std::env::temp_dir().join(format!("reef-workspace-kind-{}", std::process::id()));
        let checkout = root.join("checkout");
        let worktree = root.join("worktree");
        std::fs::create_dir_all(checkout.join(".git")).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(worktree.join(".git"), "gitdir: private").unwrap();
        assert_eq!(workspace_kind(Some(&checkout)), "checkout");
        assert_eq!(workspace_kind(Some(&worktree)), "worktree");
        std::fs::remove_dir_all(root).unwrap();
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
