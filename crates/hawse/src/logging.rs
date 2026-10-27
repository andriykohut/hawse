use std::io::IsTerminal as _;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::BoxMakeWriter;

use crate::LogFormat;
use crate::terminal::Style;

pub fn level(verbose: u8, quiet: bool) -> &'static str {
    match (quiet, verbose) {
        (true, _) => "warn",
        (false, 0) => "info",
        (false, 1) => "debug",
        (false, _) => "trace",
    }
}

/// Returns the style of the terminal form when that is what the run prints: a server's, a
/// client's or a status watch's, which `form` says this is, on a terminal, with nothing asking for more or fewer lines than the default. Its rows stand in for
/// the lines at info level, so only warnings and errors are logged beside them. A `form` run on a
/// terminal has its lines written through the screen whatever their format or level, so they land
/// above what it draws. `category` names the run in the unified log under `--log oslog`.
pub fn init(
    verbose: u8,
    quiet: bool,
    format: LogFormat,
    style: Style,
    form: bool,
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))] category: &'static str,
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
        #[cfg(target_os = "macos")]
        LogFormat::Oslog => return oslog(filter, category),
    };
    // A log line has to land above the client's panel or a watch, and only the screen knows where
    // it is.
    let writer = if form && tty {
        BoxMakeWriter::new(crate::screen::Stderr)
    } else {
        BoxMakeWriter::new(std::io::stderr)
    };
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        // Left on, a log line that cannot be written is reported with `eprintln!`, which panics
        // when stderr is what failed: whatever was logging dies with its reader, the task that
        // answers SIGTERM included.
        .log_internal_errors(false)
        .with_target(false);
    if json {
        builder.json().init();
    } else {
        builder.with_ansi(style.color).compact().init();
    }
    terminal.then_some(style)
}

#[cfg(target_os = "macos")]
fn oslog(filter: EnvFilter, category: &'static str) -> Option<Style> {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;

    let logger =
        tracing_oslog::OsLogger::new(crate::commands::service::launchd::SUBSYSTEM, category);
    tracing_subscriber::registry()
        .with(filter)
        .with(logger)
        .init();
    None
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
