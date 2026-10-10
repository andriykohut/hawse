mod commands;
mod config_file;
mod logging;
mod paths;
mod terminal;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use hawse_proto::key::PublicKey;
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
    /// Log format. `auto` selects json when stderr is not a terminal. On one, a server or a client
    /// at the default level prints rows in place of log lines, and `pretty` keeps the lines.
    #[arg(long, global = true, value_enum, default_value_t = LogFormat::Auto)]
    log: LogFormat,
    /// Colored output. `auto` disables color when stderr is not a terminal.
    #[arg(long, global = true, value_enum, default_value_t = ColorChoice::Auto)]
    color: ColorChoice,
    /// Number of worker threads. Defaults to the number of CPUs.
    // Tokio panics on a zero worker count, and `panic = "abort"` would make
    // that an abort with no diagnostic.
    #[arg(long, global = true, value_parser = clap::value_parser!(u16).range(1..))]
    threads: Option<u16>,
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
    /// Set this machine up as a client: create its key, write its config, and wait for the
    /// server to authorize the key.
    Join {
        /// The server's address, as HOST or HOST:PORT.
        server: String,
        /// The server's public key, which it logs when it starts.
        #[arg(long)]
        server_key: PublicKey,
        /// Where to write the client config. Without one, where `client` would read it.
        #[arg(long, env = "HAWSE_CONFIG")]
        config: Option<PathBuf>,
    },
    /// Validate a config without starting anything, and report where its key is.
    Check {
        /// The config to check, of either role. Without one, every config `server` and `client`
        /// would read.
        #[arg(long, env = "HAWSE_CONFIG")]
        config: Option<PathBuf>,
        /// Also dial the server a client config names, and report the transport it answered on.
        #[arg(long)]
        connect: bool,
    },
    /// Generate a key if one does not exist, and print its public key.
    Keygen {
        /// The client config whose key this is. Without one, the config `client` would read.
        #[arg(long, env = "HAWSE_CONFIG")]
        config: Option<PathBuf>,
        /// Write the key here instead, whatever the config says.
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

fn main() -> ExitCode {
    let cli = Cli::parse();
    let terminal = logging::init(
        cli.verbose,
        cli.quiet,
        cli.log,
        cli.color,
        matches!(cli.command, Command::Client { .. } | Command::Server { .. }),
    );
    match run(cli, terminal) {
        Ok(code) => code,
        Err(report) => {
            // What returning the report from `main` would print.
            eprintln!("Error: {report:?}");
            // 2 is clap's exit for a usage error, and a config that does not load is the same
            // kind of mistake: starting hawse again will not fix it.
            if report.downcast_ref::<config_file::LoadError>().is_some() {
                ExitCode::from(2)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

fn run(cli: Cli, terminal: Option<terminal::Style>) -> miette::Result<ExitCode> {
    let mut runtime = tokio::runtime::Builder::new_multi_thread();
    runtime.enable_all();
    if let Some(threads) = cli.threads {
        runtime.worker_threads(usize::from(threads));
    }
    let runtime = runtime.build().into_diagnostic()?;
    runtime.block_on(async move {
        match cli.command {
            Command::Server { config, listen } => commands::server::run(config, listen, terminal)
                .await
                .map(|()| ExitCode::SUCCESS),
            Command::Client { config } => commands::client::run(config, terminal).await,
            Command::Join {
                server,
                server_key,
                config,
            } => commands::join::run(server, server_key, config).await,
            Command::Check { config, connect } => commands::check::run(config, connect)
                .await
                .map(|()| ExitCode::SUCCESS),
            Command::Keygen { config, out } => {
                commands::keygen::run(config, out).map(|()| ExitCode::SUCCESS)
            }
        }
    })
}
