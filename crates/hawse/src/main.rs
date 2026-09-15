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
    about = "Reverse TCP tunnel over QUIC with Ed25519 authentication"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// Log at debug level; repeat for trace level.
    #[arg(short, long, global = true, action = ArgAction::Count)]
    verbose: u8,
    /// Log warnings and errors only.
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    quiet: bool,
    /// Log format. `auto` selects json when stderr is not a terminal.
    #[arg(long, global = true, value_enum, default_value_t = LogFormat::Auto)]
    log: LogFormat,
    /// Colored output. `auto` disables color when stderr is not a terminal.
    #[arg(long, global = true, value_enum, default_value_t = ColorChoice::Auto)]
    color: ColorChoice,
    /// Number of worker threads. Defaults to the number of CPUs.
    #[arg(long, global = true)]
    threads: Option<usize>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server: accept clients and bind their services to public ports.
    Server {
        #[arg(long, env = "HAWSE_CONFIG")]
        config: Option<PathBuf>,
        /// Override the listen address from the config.
        #[arg(long)]
        listen: Option<SocketAddr>,
    },
    /// Run the client: connect to a server and expose the configured services.
    Client {
        #[arg(long, env = "HAWSE_CONFIG")]
        config: Option<PathBuf>,
    },
    /// Generate a key if one does not exist, and print its public key.
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
