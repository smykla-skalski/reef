#[cfg(unix)]
mod run;
mod status;

#[cfg(unix)]
use clap::ValueEnum;
use clap::{Parser, Subcommand};
#[cfg(unix)]
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
    let cli = Cli::parse();

    match cli.command {
        Command::Status => {
            status::print();
            std::process::ExitCode::SUCCESS
        }
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
    }
}
