mod commands;
mod config_file;
mod logging;
mod paths;

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use miette::IntoDiagnostic as _;

#[derive(Parser)]
#[command(
    name = "hawse",
    version,
    about = "Reverse tunnels with keys instead of secrets"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// More detail; repeat for trace output.
    #[arg(short, long, global = true, action = ArgAction::Count)]
    verbose: u8,
    /// Warnings and errors only.
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    quiet: bool,
    /// Log format; auto picks json when stderr is not a terminal.
    #[arg(long, global = true, value_enum, default_value_t = LogFormat::Auto)]
    log: LogFormat,
    /// Colored output; auto turns color off when stderr is not a terminal.
    #[arg(long, global = true, value_enum, default_value_t = ColorChoice::Auto)]
    color: ColorChoice,
    /// Worker threads; defaults to the number of CPUs.
    #[arg(long, global = true)]
    threads: Option<usize>,
}

#[derive(Subcommand)]
enum Command {
    /// Accept clients and expose their services on public ports.
    Server {
        #[arg(long, env = "HAWSE_CONFIG")]
        config: Option<PathBuf>,
        /// Override the listen address from the config.
        #[arg(long)]
        listen: Option<SocketAddr>,
    },
    /// Connect to a server and expose the services in client.toml.
    Client {
        #[arg(long, env = "HAWSE_CONFIG")]
        config: Option<PathBuf>,
    },
    /// Create a key if none exists and print its public half.
    Keygen {
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum LogFormat {
    Auto,
    Pretty,
    Json,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

fn main() -> miette::Result<()> {
    let cli = Cli::parse();
    logging::init(cli.verbose, cli.quiet, cli.log, cli.color);
    let mut runtime = tokio::runtime::Builder::new_multi_thread();
    runtime.enable_all();
    if let Some(threads) = cli.threads {
        runtime.worker_threads(threads);
    }
    let runtime = runtime.build().into_diagnostic()?;
    runtime.block_on(async move {
        match cli.command {
            Command::Server { config, listen } => commands::server::run(config, listen).await,
            Command::Client { config } => commands::client::run(config).await,
            Command::Keygen { out } => commands::keygen::run(out),
        }
    })
}
