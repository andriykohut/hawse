use std::io::IsTerminal as _;

use tracing_subscriber::EnvFilter;

use crate::terminal::Style;
use crate::{ColorChoice, LogFormat};

pub fn level(verbose: u8, quiet: bool) -> &'static str {
    match (quiet, verbose) {
        (true, _) => "warn",
        (false, 0) => "info",
        (false, 1) => "debug",
        (false, _) => "trace",
    }
}

/// Returns the style of the terminal form when that is what the run prints: a server's or a
/// client's, which `form` says this is, on a terminal, with nothing asking for more or fewer lines than the default. Its rows stand in for
/// the lines at info level, so only warnings and errors are logged beside them.
pub fn init(
    verbose: u8,
    quiet: bool,
    format: LogFormat,
    color: ColorChoice,
    form: bool,
) -> Option<Style> {
    let tty = std::io::stderr().is_terminal();
    let chosen = EnvFilter::try_from_env("HAWSE_LOG");
    let terminal = form
        && tty
        && matches!(format, LogFormat::Auto)
        && verbose == 0
        && !quiet
        && chosen.is_err();
    let level = if terminal {
        "warn"
    } else {
        level(verbose, quiet)
    };
    let filter = chosen.unwrap_or_else(|_| {
        EnvFilter::new(format!(
            "hawse={level},hawse_core={level},hawse_proto={level},warn"
        ))
    });
    let json = match format {
        LogFormat::Json => true,
        LogFormat::Pretty => false,
        LogFormat::Auto => !tty,
    };
    let ansi = match color {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => tty && std::env::var_os("NO_COLOR").is_none(),
    };
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        // Left on, a log line that cannot be written is reported with `eprintln!`, which panics
        // when stderr is what failed: whatever was logging dies with its reader, the task that
        // answers SIGTERM included.
        .log_internal_errors(false)
        .with_target(false);
    if json {
        builder.json().init();
    } else {
        builder.with_ansi(ansi).compact().init();
    }
    terminal.then(|| Style::detect(ansi))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbosity_maps_to_levels() {
        assert_eq!(level(0, false), "info");
        assert_eq!(level(1, false), "debug");
        assert_eq!(level(2, false), "trace");
        assert_eq!(level(5, false), "trace");
        assert_eq!(level(0, true), "warn");
    }
}
