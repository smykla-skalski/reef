mod observe;
mod status;

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
    /// Record resource usage over time.
    Observe {
        #[command(subcommand)]
        command: ObserveCommand,
    },
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

fn main() -> std::io::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Status => status::print(),
        Command::Observe { command } => match command {
            ObserveCommand::Start(options) => observe::start(&options)?,
            ObserveCommand::Run(options) => observe::run(&options)?,
            ObserveCommand::Stop { state_dir } => observe::stop(state_dir.as_deref())?,
        },
    }
    Ok(())
}
