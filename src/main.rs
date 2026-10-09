mod status;

use clap::{Parser, Subcommand};

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
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::Status => status::print(),
    }
}
