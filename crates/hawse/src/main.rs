mod commands;
mod config_file;
mod logging;
mod notify;
mod panel;
mod paths;
mod screen;
mod terminal;

use std::io::{IsTerminal as _, Write as _};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::builder::styling::{self, Styles};
use clap::{ArgAction, CommandFactory as _, FromArgMatches as _, Parser, Subcommand, ValueEnum};
use hawse_proto::key::PublicKey;
use miette::IntoDiagnostic as _;

use crate::paths::Role;

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
    /// Run the server or the client as a service that starts by itself.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Replace this binary with a release, checked and tried first, and restart the service that
    /// runs it. The binary it replaces is kept beside it as hawse.old.
    Upgrade {
        /// The release to install, newer or older, instead of the latest.
        #[arg(long)]
        version: Option<String>,
        /// Say whether a newer release exists, and change nothing.
        #[arg(long, conflicts_with = "version")]
        check: bool,
    },
    /// Print the completion script for a shell.
    Completions { shell: clap_complete::Shell },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Write the service, enable and start it, and print its public key.
    Install {
        role: Role,
        /// The config the service runs. Without one, the config `server` or `client` would read.
        #[arg(long, env = "HAWSE_CONFIG")]
        config: Option<PathBuf>,
        /// Run the unit as this account instead of a throwaway one, keeping the key beside the
        /// config (Linux).
        #[arg(long)]
        user: Option<String>,
    },
    /// Stop and remove the service; the config and key stay.
    Uninstall { role: Role },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum LogFormat {
    Auto,
    Pretty,
    Json,
    /// macOS's unified log, under the subsystem io.github.andriykohut.hawse; what a launchd
    /// agent runs.
    #[cfg(target_os = "macos")]
    Oslog,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args_os()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let mut command = Cli::command()
        .styles(styles())
        .color(match color_arg(&args) {
            ColorChoice::Auto => clap::ColorChoice::Auto,
            ColorChoice::Always => clap::ColorChoice::Always,
            ColorChoice::Never => clap::ColorChoice::Never,
        });
    let cli = Cli::from_arg_matches(&command.get_matches_mut())
        .unwrap_or_else(|err| err.format(&mut command).exit());
    let color = match cli.color {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none()
        }
    };
    let style = terminal::Style::detect(color);
    miette::set_hook(Box::new(move |_| {
        Box::new(
            miette::MietteHandlerOpts::new()
                .graphical_theme(style.theme())
                .build(),
        )
    }))
    .expect("the hook is set before any report is made");
    let category = match cli.command {
        Command::Server { .. } => "server",
        Command::Client { .. } => "client",
        _ => "cli",
    };
    let terminal = logging::init(
        cli.verbose,
        cli.quiet,
        cli.log,
        style,
        matches!(cli.command, Command::Client { .. } | Command::Server { .. }),
        category,
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

/// clap's styles, with its headings in the accent where the terminal has one.
fn styles() -> Styles {
    match terminal::accent() {
        Some(accent) => {
            let heading = styling::Style::new().fg_color(Some(accent)).bold();
            Styles::styled().header(heading).usage(heading)
        }
        None => Styles::styled(),
    }
}

/// The last `--color` in `args` before any `--`, or `auto` without one. It is read ahead of clap
/// because clap prints `--help` and a usage error while it parses, before `Cli` holds the choice.
fn color_arg(args: &[String]) -> ColorChoice {
    let mut choice = ColorChoice::Auto;
    for (i, arg) in args.iter().enumerate().skip(1) {
        if arg == "--" {
            break;
        }
        let value = match arg.strip_prefix("--color") {
            Some("") => args.get(i + 1).map(String::as_str),
            Some(rest) => rest.strip_prefix('='),
            None => None,
        };
        if let Some(value) = value.and_then(|value| ColorChoice::from_str(value, false).ok()) {
            choice = value;
        }
    }
    choice
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
            Command::Service { action } => match action {
                ServiceAction::Install { role, config, user } => {
                    commands::service::install(role, config, user.as_deref())
                }
                ServiceAction::Uninstall { role } => commands::service::uninstall(role),
            }
            .map(|()| ExitCode::SUCCESS),
            Command::Upgrade { version, check } => {
                commands::upgrade::run(version.as_deref(), check).map(|()| ExitCode::SUCCESS)
            }
            Command::Completions { shell } => {
                // `generate` panics on a write that fails, which a reader that has gone away
                // makes of a write to stdout.
                let mut script = Vec::new();
                clap_complete::generate(shell, &mut Cli::command(), "hawse", &mut script);
                std::io::stdout().write_all(&script).into_diagnostic()?;
                Ok(ExitCode::SUCCESS)
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_reads_as_upgrade_tries_a_release() {
        let version = Cli::command().render_version();
        assert_eq!(
            version.trim(),
            format!("hawse {}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn upgrade_takes_a_version_of_its_own() {
        assert!(Cli::try_parse_from(["hawse", "upgrade", "--version", "0.10.0"]).is_ok());
        assert!(Cli::try_parse_from(["hawse", "upgrade", "--check"]).is_ok());
    }

    #[test]
    fn oslog_is_a_log_format_on_macos_only() {
        let parsed = Cli::try_parse_from(["hawse", "--log", "oslog", "keygen"]);
        assert_eq!(parsed.is_ok(), cfg!(target_os = "macos"));
    }

    #[test]
    fn the_last_color_flag_decides() {
        let args = |line: &str| line.split(' ').map(str::to_owned).collect::<Vec<_>>();
        let choice = |line| format!("{:?}", color_arg(&args(line)));
        assert_eq!(choice("hawse --help"), "Auto");
        assert_eq!(choice("hawse --color never --help"), "Never");
        assert_eq!(choice("hawse client --help --color=always"), "Always");
        assert_eq!(choice("hawse --color never --color=always"), "Always");
        assert_eq!(choice("hawse --colour never"), "Auto");
        assert_eq!(choice("hawse --color"), "Auto");
        assert_eq!(
            choice("hawse --color never check -- --color=always"),
            "Never"
        );
    }
}
