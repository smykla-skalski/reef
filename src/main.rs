#[cfg(unix)]
mod agents;
mod observe;
#[cfg(unix)]
mod pressure;
mod report;
#[cfg(unix)]
mod run;
#[cfg(unix)]
mod schedule;
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
        #[arg(long)]
        record: Option<PathBuf>,
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
        #[arg(long)]
        record: Option<PathBuf>,
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
    /// Record resource usage over time.
    Observe {
        #[command(subcommand)]
        command: ObserveCommand,
    },
    /// Summarize workstation observations and command measurements.
    Report(report::Options),
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
            command,
        } => match run::run(&command, category.as_str(), &identity, record.as_deref()) {
            Ok(code) => std::process::ExitCode::from(code),
            Err(error) => {
                eprintln!("reef: {error}");
                std::process::ExitCode::FAILURE
            }
        },
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
            state_dir,
            command,
        } => match schedule::schedule(schedule::RunOptions {
            command: &command,
            category: category.as_str(),
            identity: &identity,
            record: record.as_deref(),
            cpu,
            memory_mib,
            state_dir: state_dir.as_deref(),
        }) {
            Ok(code) => std::process::ExitCode::from(code),
            Err(error) => {
                eprintln!("reef: {error}");
                std::process::ExitCode::FAILURE
            }
        },
        #[cfg(unix)]
        Command::Queue { state_dir } => result(schedule::queue(state_dir.as_deref())),
        #[cfg(unix)]
        Command::Cancel { id, state_dir } => result(schedule::cancel(id, state_dir.as_deref())),
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
        Command::Report(options) => match report::run(&options) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("reef: {error}");
                std::process::ExitCode::FAILURE
            }
        },
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
