#[cfg(unix)]
mod agents;
#[cfg(unix)]
mod cache;
mod history;
mod observe;
#[cfg(unix)]
mod pressure;
#[cfg(unix)]
mod remote;
mod report;
#[cfg(unix)]
mod run;
#[cfg(unix)]
mod schedule;
#[cfg(unix)]
mod schedule_events;
mod status;

#[cfg(unix)]
use clap::ValueEnum;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "reef", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Show current machine resource usage.
    Status,
    /// Reuse successful output-only commands with explicit opt-in.
    #[cfg(unix)]
    Cache {
        #[command(subcommand)]
        command: CacheCommand,
    },
    /// Launch a coding agent with resource-aware command shims.
    #[cfg(unix)]
    Agents {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Run a command and measure its resource cost.
    #[cfg(unix)]
    Run {
        /// Workload category saved in the measurement record.
        #[arg(long, value_enum, default_value_t = Category::Other)]
        category: Category,
        /// Safe label for the record; never derived from command arguments.
        #[arg(long, default_value = "command", value_parser = parse_identity)]
        identity: String,
        /// Append a private JSON Lines measurement record to this file.
        #[arg(long, conflicts_with = "no_record")]
        record: Option<PathBuf>,
        /// Do not save this command to history.
        #[arg(long)]
        no_record: bool,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Start the shared scheduler in the foreground.
    #[cfg(unix)]
    Serve {
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
        cpu: Option<u32>,
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        memory_mib: Option<u64>,
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
        max_running: Option<u32>,
        #[arg(long)]
        state_dir: Option<PathBuf>,
        #[command(flatten)]
        pressure: pressure::Options,
    },
    /// Queue a command under the shared CPU and memory budget.
    #[cfg(unix)]
    Schedule {
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
        cpu: u32,
        #[arg(long, default_value_t = 1024, value_parser = clap::value_parser!(u64).range(1..))]
        memory_mib: u64,
        #[arg(long, value_enum, default_value_t = Category::Other)]
        category: Category,
        #[arg(long, default_value = "command", value_parser = parse_identity)]
        identity: String,
        #[arg(long, conflicts_with = "no_record")]
        record: Option<PathBuf>,
        /// Do not save this command to history.
        #[arg(long)]
        no_record: bool,
        #[arg(long)]
        state_dir: Option<PathBuf>,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Show queued and running requests.
    #[cfg(unix)]
    Queue {
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Cancel a queued or running request.
    #[cfg(unix)]
    Cancel {
        id: u64,
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Submit an explicitly approved job to a trusted SSH worker.
    #[cfg(unix)]
    Remote(remote::Options),
    /// Internal entry point for an SSH worker account.
    #[cfg(unix)]
    #[command(hide = true)]
    RemoteWorker(remote::WorkerOptions),
    /// Record resource usage over time.
    Observe {
        #[command(subcommand)]
        command: ObserveCommand,
    },
    /// Summarize workstation observations and command measurements.
    Report(report::Options),
    /// Compare two observed workstation periods without causal claims.
    Compare(report::CompareOptions),
}

#[cfg(unix)]
#[derive(Debug, Subcommand)]
enum CacheCommand {
    /// Run a pure command and replay its output on a cache hit.
    Run {
        /// Seconds a successful result remains reusable.
        #[arg(long, default_value_t = 3600, value_parser = clap::value_parser!(u64).range(1..))]
        ttl_seconds: u64,
        /// Maximum bytes held in Reef's cache, in MiB.
        #[arg(long, default_value_t = 256, value_parser = clap::value_parser!(u64).range(1..))]
        max_storage_mib: u64,
        /// Additional input file outside the Git worktree; repeat as needed.
        #[arg(long = "input")]
        inputs: Vec<PathBuf>,
        /// Private cache directory.
        #[arg(long)]
        cache_dir: Option<PathBuf>,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Report the number and total size of Reef cache entries.
    Status {
        #[arg(long)]
        cache_dir: Option<PathBuf>,
    },
    /// Evict expired and least recently used Reef cache entries.
    Prune {
        #[arg(long)]
        cache_dir: Option<PathBuf>,
        #[arg(long, default_value_t = 256, value_parser = clap::value_parser!(u64).range(1..))]
        max_storage_mib: u64,
    },
}

#[cfg(unix)]
#[derive(Debug, Subcommand)]
enum AgentCommand {
    /// Launch Codex or Claude Code with a process-scoped shim path.
    Launch {
        #[arg(value_enum)]
        agent: AgentName,
        #[arg(long)]
        state_dir: Option<PathBuf>,
        #[arg(last = true)]
        args: Vec<String>,
    },
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, ValueEnum)]
enum AgentName {
    Codex,
    Claude,
}

#[cfg(unix)]
impl AgentName {
    fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }
}

#[derive(Debug, Subcommand)]
enum ObserveCommand {
    /// Start a recorder that survives terminal sessions.
    Start(ObserveOptions),
    /// Run the recorder in the foreground.
    Run(ObserveOptions),
    /// Stop the running recorder.
    Stop {
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
}

#[derive(Debug, clap::Args, Clone)]
struct ObserveOptions {
    /// Seconds between samples.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..))]
    interval_seconds: u64,
    /// Number of days to retain samples.
    #[arg(long, default_value_t = 7, value_parser = clap::value_parser!(u64).range(1..))]
    retention_days: u64,
    /// Maximum storage in MiB.
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u64).range(1..))]
    max_storage_mib: u64,
    /// Private directory for observations.
    #[arg(long)]
    state_dir: Option<PathBuf>,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, ValueEnum)]
enum Category {
    Build,
    Lint,
    Test,
    Other,
}

#[cfg(unix)]
impl Category {
    fn as_str(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Lint => "lint",
            Self::Test => "test",
            Self::Other => "other",
        }
    }
}

#[cfg(unix)]
fn parse_identity(value: &str) -> Result<String, String> {
    if value.len() > 64
        || value.is_empty()
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        })
    {
        return Err(
            "identity must be 1-64 lowercase ASCII letters, digits, '.', '_' or '-'".into(),
        );
    }
    Ok(value.to_owned())
}

#[cfg(unix)]
fn cache_command(command: CacheCommand) -> std::process::ExitCode {
    match command {
        CacheCommand::Run {
            ttl_seconds,
            max_storage_mib,
            inputs,
            cache_dir,
            command,
        } => command_result(cache::run(cache::RunOptions {
            command: &command,
            inputs: &inputs,
            ttl_seconds,
            max_storage_mib,
            cache_dir: cache_dir.as_deref(),
        })),
        CacheCommand::Status { cache_dir } => result(cache::status(cache_dir.as_deref())),
        CacheCommand::Prune {
            cache_dir,
            max_storage_mib,
        } => result(cache::prune(cache_dir.as_deref(), max_storage_mib)),
    }
}

#[cfg(unix)]
fn record_mode(record: Option<&std::path::Path>, no_record: bool) -> run::RecordMode<'_> {
    if no_record {
        run::RecordMode::Disabled
    } else if let Some(path) = record {
        run::RecordMode::Custom(path)
    } else {
        run::RecordMode::Default
    }
}

fn main() -> std::process::ExitCode {
    #[cfg(unix)]
    if let Some(tool) = agents::shim_name() {
        return command_result(agents::shim(&tool));
    }
    let cli = Cli::parse();

    match cli.command {
        Command::Status => {
            status::print();
            std::process::ExitCode::SUCCESS
        }
        #[cfg(unix)]
        Command::Cache { command } => cache_command(command),
        #[cfg(unix)]
        Command::Agents { command } => match command {
            AgentCommand::Launch {
                agent,
                state_dir,
                args,
            } => command_result(agents::launch(agent.as_str(), state_dir.as_deref(), &args)),
        },
        #[cfg(unix)]
        Command::Run {
            category,
            identity,
            record,
            no_record,
            command,
        } => command_result(run::run(
            &command,
            category.as_str(),
            &identity,
            record_mode(record.as_deref(), no_record),
        )),
        #[cfg(unix)]
        Command::Serve {
            cpu,
            memory_mib,
            max_running,
            state_dir,
            pressure,
        } => result(schedule::serve(
            cpu,
            memory_mib,
            max_running,
            state_dir.as_deref(),
            pressure,
        )),
        #[cfg(unix)]
        Command::Schedule {
            cpu,
            memory_mib,
            category,
            identity,
            record,
            no_record,
            state_dir,
            command,
        } => command_result(schedule::schedule(schedule::RunOptions {
            command: &command,
            category: category.as_str(),
            identity: &identity,
            record: record_mode(record.as_deref(), no_record),
            cpu,
            memory_mib,
            state_dir: state_dir.as_deref(),
        })),
        #[cfg(unix)]
        Command::Queue { state_dir } => result(schedule::queue(state_dir.as_deref())),
        #[cfg(unix)]
        Command::Cancel { id, state_dir } => result(schedule::cancel(id, state_dir.as_deref())),
        #[cfg(unix)]
        Command::Remote(options) => command_result(remote::submit(&options)),
        #[cfg(unix)]
        Command::RemoteWorker(options) => command_result(remote::worker(&options)),
        Command::Observe { command } => {
            let result = match command {
                ObserveCommand::Start(options) => observe::start(&options),
                ObserveCommand::Run(options) => observe::run(&options),
                ObserveCommand::Stop { state_dir } => observe::stop(state_dir.as_deref()),
            };
            match result {
                Ok(()) => std::process::ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("reef: {error}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        Command::Report(options) => report_result(report::run(&options)),
        Command::Compare(options) => report_result(report::run_compare(&options)),
    }
}

fn report_result(result: std::io::Result<()>) -> std::process::ExitCode {
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("reef: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(unix)]
fn command_result(result: std::io::Result<u8>) -> std::process::ExitCode {
    match result {
        Ok(code) => std::process::ExitCode::from(code),
        Err(error) => {
            eprintln!("reef: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(unix)]
fn result(result: std::io::Result<()>) -> std::process::ExitCode {
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("reef: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
