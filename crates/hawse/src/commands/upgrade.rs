//! `hawse upgrade`: each host's release-day routine in one command. It fetches a release for the
//! binary that runs it, checks and tries it before anything is replaced, keeps the old binary
//! beside it, and restarts the service that runs it.

use std::fmt::{self, Write as _};
use std::path::Path;
use std::str::FromStr;

/// A release's number, as its tag names it after the `v`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Version(u64, u64, u64);

/// The oldest release `--version` goes back to: the first that keeps an unnamed key where a unit
/// `hawse service install` wrote looks for it, and tells systemd when it is ready.
const FLOOR: Version = Version(0, 10, 0);

impl FromStr for Version {
    type Err = miette::Report;

    /// Three numbers joined by dots, after an optional `v`.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let number = |part: &str| {
            (!part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
                .then(|| part.parse().ok())
                .flatten()
        };
        let mut parts = text
            .strip_prefix('v')
            .unwrap_or(text)
            .split('.')
            .map(number);
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(Some(major)), Some(Some(minor)), Some(Some(patch)), None) => {
                Ok(Version(major, minor, patch))
            }
            _ => Err(miette::miette!(
                "`{text}` is not a release version, such as 0.10.0"
            )),
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// The release target this binary was built for, as `install.sh` names it; `None` for a system
/// with no release binary.
fn target() -> Option<&'static str> {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("x86_64-unknown-linux-musl")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some("aarch64-unknown-linux-musl")
    } else if cfg!(all(target_os = "linux", target_arch = "arm")) {
        Some("armv7-unknown-linux-musleabihf")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else {
        None
    }
}

/// The version of the release `url` names: where the latest release redirects, ending in its tag.
fn tag(url: &str) -> Option<Version> {
    url.rsplit('/').next()?.parse().ok()
}

/// The hash `sums`, a `SHA256SUMS` file, gives for `file`.
fn sum<'a>(sums: &'a str, file: &str) -> Option<&'a str> {
    sums.lines()
        .find_map(|line| line.strip_suffix(file)?.strip_suffix("  "))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}

/// The package manager that installed the binary at `exe`, which is the one to upgrade it.
/// `cargo_home` is `$CARGO_HOME`, which defaults to `.cargo` in `home`.
fn owner(exe: &Path, cargo_home: Option<&Path>, home: Option<&Path>) -> Option<&'static str> {
    let cargo = cargo_home
        .map(Path::to_path_buf)
        .or_else(|| home.map(|home| home.join(".cargo")));
    if cargo.is_some_and(|cargo| exe.starts_with(cargo.join("bin"))) {
        Some("cargo")
    } else if ["/opt/homebrew", "/usr/local/Cellar", "/home/linuxbrew"]
        .iter()
        .any(|dir| exe.starts_with(dir))
    {
        Some("Homebrew")
    } else if exe.starts_with("/nix/store") {
        Some("Nix")
    } else {
        None
    }
}

/// The binary a unit runs, from `systemctl show -p ExecStart --value`, which prints nothing for a
/// unit that is not loaded.
fn exec_path(show: &str) -> Option<&str> {
    let rest = show.split("path=").nth(1)?;
    rest.split(" ;").next().map(str::trim)
}

/// The binary an agent runs, from `launchctl print`.
fn program(print: &str) -> Option<&str> {
    print
        .lines()
        .find_map(|line| line.trim().strip_prefix("program = "))
}

/// Whether `output`, what a binary printed for `--version`, names `version`.
fn says(output: &str, version: Version) -> bool {
    output.trim() == format!("hawse {version}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_version_reads_with_or_without_a_v() {
        assert_eq!("0.10.0".parse::<Version>().unwrap(), Version(0, 10, 0));
        assert_eq!("v1.2.3".parse::<Version>().unwrap(), Version(1, 2, 3));
        for bad in ["0.10", "0.10.0.1", "0.x.0", "", "0.10.0-rc1"] {
            assert!(bad.parse::<Version>().is_err(), "{bad}");
        }
        assert!(Version(0, 9, 0) < FLOOR && Version(0, 10, 1) > FLOOR);
        assert_eq!(Version(0, 10, 0).to_string(), "0.10.0");
    }

    #[test]
    fn this_build_has_a_release_target() {
        let expected = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            Some("aarch64-apple-darwin")
        } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            Some("x86_64-unknown-linux-musl")
        } else {
            target()
        };
        assert_eq!(target(), expected);
    }

    #[test]
    fn the_latest_release_is_read_from_its_redirect() {
        let url = "https://github.com/andriykohut/hawse/releases/tag/v0.10.0";
        assert_eq!(tag(url), Some(Version(0, 10, 0)));
        assert_eq!(tag("https://github.com/andriykohut/hawse/releases"), None);
    }

    #[test]
    fn an_archive_has_its_line_in_sha256sums() {
        let sums = "aaa  hawse-0.10.0-x86_64-unknown-linux-musl.tar.gz\nbbb  hawse-0.10.0-aarch64-apple-darwin.tar.gz\n";
        assert_eq!(
            sum(sums, "hawse-0.10.0-aarch64-apple-darwin.tar.gz"),
            Some("bbb")
        );
        assert_eq!(sum(sums, "hawse-0.10.0-aarch64-apple-darwin"), None);
        assert_eq!(hex(&[0x0a, 0xff]), "0aff");
    }

    #[test]
    fn a_binary_a_package_manager_installed_is_its_to_upgrade() {
        let home = Some(Path::new("/Users/someone"));
        let at = |path: &str| owner(Path::new(path), None, home);
        assert_eq!(at("/Users/someone/.cargo/bin/hawse"), Some("cargo"));
        let cargo_home = Some(Path::new("/opt/cargo"));
        assert_eq!(
            owner(Path::new("/opt/cargo/bin/hawse"), cargo_home, home),
            Some("cargo")
        );
        assert_eq!(at("/opt/homebrew/bin/hawse"), Some("Homebrew"));
        assert_eq!(at("/nix/store/abc-hawse/bin/hawse"), Some("Nix"));
        assert_eq!(at("/usr/local/bin/hawse"), None);
    }

    #[test]
    fn a_unit_that_runs_another_binary_is_not_this_ones() {
        let show = "{ path=/usr/local/bin/hawse ; argv[]=/usr/local/bin/hawse server --config /etc/hawse/server.toml --log pretty ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }";
        assert_eq!(exec_path(show), Some("/usr/local/bin/hawse"));
        assert_ne!(exec_path(show), Some("/mnt/pool/apps/hawse/hawse"));
        assert_eq!(exec_path(""), None);
        let print = "gui/501/io.github.andriykohut.hawse.client = {\n\tactive count = 1\n\tprogram = /usr/local/bin/hawse\n\targuments = {\n";
        assert_eq!(program(print), Some("/usr/local/bin/hawse"));
        assert_eq!(program("Could not find service"), None);
    }

    #[test]
    fn a_tried_binary_has_to_name_the_release() {
        assert!(says("hawse 0.10.0\n", Version(0, 10, 0)));
        assert!(!says("hawse 0.9.0\n", Version(0, 10, 0)));
        assert!(!says("", Version(0, 10, 0)));
    }
}
