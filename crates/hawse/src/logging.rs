use std::io::IsTerminal as _;

use tracing_subscriber::EnvFilter;

use crate::{ColorChoice, LogFormat};

pub fn level(verbose: u8, quiet: bool) -> &'static str {
    match (quiet, verbose) {
        (true, _) => "warn",
        (false, 0) => "info",
        (false, 1) => "debug",
        (false, _) => "trace",
    }
}

pub fn init(verbose: u8, quiet: bool, format: LogFormat, color: ColorChoice) {
    let level = level(verbose, quiet);
    let filter = EnvFilter::try_from_env("HAWSE_LOG").unwrap_or_else(|_| {
        EnvFilter::new(format!(
            "hawse={level},hawse_core={level},hawse_proto={level},warn"
        ))
    });
    let tty = std::io::stderr().is_terminal();
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
        .with_target(false);
    if json {
        builder.json().init();
    } else {
        builder.with_ansi(ansi).compact().init();
    }
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
