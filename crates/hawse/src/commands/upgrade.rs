//! `hawse upgrade`: each host's release-day routine in one command. It fetches a release for the
//! binary that runs it, checks and tries it before anything is replaced, keeps the old binary
//! beside it, and restarts the service that runs it.

use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;

use miette::{IntoDiagnostic as _, WrapErr as _};

use crate::paths::{self, Role};

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

/// Puts release `version` for `target`, fetched from `base`, at `exe`, and keeps the binary it
/// replaces beside it with `.old` after its name. The release has to match its `SHA256SUMS` and
/// run here before anything is replaced, and its staging directory is removed either way. Returns
/// the release's unit for each role, from its `contrib/`.
fn replace(
    base: &str,
    version: Version,
    target: &str,
    exe: &Path,
) -> miette::Result<Vec<(Role, String)>> {
    use std::os::unix::fs::MetadataExt as _;

    let dir = exe.parent().unwrap_or(Path::new("/"));
    let stage = Staging(dir.join(".hawse-upgrade"));
    // Left by an upgrade that was killed.
    let _ = std::fs::remove_dir_all(&stage.0);
    if let Err(err) = std::fs::create_dir(&stage.0) {
        if err.kind() == std::io::ErrorKind::PermissionDenied {
            miette::bail!("cannot write to {}; run it with sudo", dir.display());
        }
        return Err(err)
            .into_diagnostic()
            .wrap_err_with(|| format!("cannot create {}", stage.0.display()));
    }
    let name = format!("hawse-{version}-{target}");
    let archive = stage.0.join(format!("{name}.tar.gz"));
    let sums = stage.0.join("SHA256SUMS");
    let url = |file: &str| format!("{base}/releases/download/v{version}/{file}");
    run(Command::new("curl")
        .arg("-fsSL")
        .arg("-o")
        .arg(&archive)
        .arg(url(&format!("{name}.tar.gz")))
        .arg("-o")
        .arg(&sums)
        .arg(url("SHA256SUMS")))?;
    let read = |path: &Path| {
        std::fs::read(path)
            .into_diagnostic()
            .wrap_err_with(|| format!("cannot read {}", path.display()))
    };
    let got = hex(&hawse_core::tls::sha256(&read(&archive)?));
    let sums = String::from_utf8_lossy(&read(&sums)?).into_owned();
    if sum(&sums, &format!("{name}.tar.gz")) != Some(got.as_str()) {
        miette::bail!("{name}.tar.gz does not match the release's SHA256SUMS");
    }
    run(Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(&stage.0))?;
    let unpacked = stage.0.join(&name);
    let new = unpacked.join("hawse");
    let why = match Command::new(&new).arg("--version").output() {
        Ok(out) if out.status.success() && says(&String::from_utf8_lossy(&out.stdout), version) => {
            None
        }
        Ok(out) if out.status.success() => Some(format!(
            "it says {}",
            String::from_utf8_lossy(&out.stdout).trim()
        )),
        Ok(out) => Some(format!("it ended with {}", out.status)),
        Err(err) => Some(err.to_string()),
    };
    if let Some(why) = why {
        miette::bail!("the {version} binary does not run here: {why}");
    }
    let units = [Role::Server, Role::Client]
        .into_iter()
        .map(|role| {
            let unit = unpacked.join(format!("contrib/hawse-{}.service", role.name()));
            Ok((role, String::from_utf8_lossy(&read(&unit)?).into_owned()))
        })
        .collect::<miette::Result<Vec<_>>>()?;
    let mut old_name = exe.file_name().unwrap_or_default().to_owned();
    old_name.push(".old");
    let old = exe.with_file_name(old_name);
    std::fs::copy(exe, &old)
        .into_diagnostic()
        .wrap_err_with(|| format!("cannot keep {} as {}", exe.display(), old.display()))?;
    let meta = std::fs::metadata(exe).into_diagnostic()?;
    std::fs::set_permissions(&new, meta.permissions()).into_diagnostic()?;
    if paths::is_root() {
        std::os::unix::fs::chown(&new, Some(meta.uid()), Some(meta.gid())).into_diagnostic()?;
    }
    std::fs::rename(&new, exe)
        .into_diagnostic()
        .wrap_err_with(|| format!("cannot replace {}", exe.display()))?;
    Ok(units)
}

/// Removes the staging directory however [`replace`] ends.
struct Staging(PathBuf);

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Runs `command` on this terminal; an error naming it when it fails.
fn run(command: &mut Command) -> miette::Result<()> {
    let line = std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    let status = command
        .status()
        .into_diagnostic()
        .wrap_err_with(|| format!("cannot run {line}"))?;
    if !status.success() {
        miette::bail!("{line} failed");
    }
    Ok(())
}

/// Whether `output`, what a binary printed for `--version`, names `version`.
fn says(output: &str, version: Version) -> bool {
    output.trim() == format!("hawse {version}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lays out a release of `version` under `root`, whose binary is the shell script `binary`,
    /// and returns the base URL curl fetches it from.
    fn release(root: &Path, version: &str, binary: &str) -> String {
        use std::os::unix::fs::PermissionsExt as _;
        let name = format!("hawse-{version}-{}", target().unwrap());
        let dir = root.join("stage").join(&name);
        std::fs::create_dir_all(dir.join("contrib")).unwrap();
        std::fs::write(dir.join("hawse"), binary).unwrap();
        std::fs::set_permissions(dir.join("hawse"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::write(dir.join("contrib/hawse-server.service"), "server unit").unwrap();
        std::fs::write(dir.join("contrib/hawse-client.service"), "client unit").unwrap();
        let download = root.join(format!("releases/download/v{version}"));
        std::fs::create_dir_all(&download).unwrap();
        let archive = download.join(format!("{name}.tar.gz"));
        let status = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(root.join("stage"))
            .arg(&name)
            .status()
            .unwrap();
        assert!(status.success());
        let hash = hex(&hawse_core::tls::sha256(&std::fs::read(&archive).unwrap()));
        std::fs::write(
            download.join("SHA256SUMS"),
            format!("{hash}  {name}.tar.gz\n"),
        )
        .unwrap();
        format!("file://{}", root.display())
    }

    #[test]
    fn a_release_replaces_the_binary_and_keeps_the_old_one() {
        let (root, bin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let base = release(root.path(), "9.9.9", "#!/bin/sh\necho 'hawse 9.9.9'\n");
        let exe = bin.path().join("hawse");
        std::fs::write(&exe, "old").unwrap();
        let units = replace(&base, Version(9, 9, 9), target().unwrap(), &exe).unwrap();
        assert!(
            std::fs::read_to_string(&exe)
                .unwrap()
                .contains("hawse 9.9.9")
        );
        assert_eq!(
            std::fs::read_to_string(bin.path().join("hawse.old")).unwrap(),
            "old"
        );
        assert!(!bin.path().join(".hawse-upgrade").exists());
        assert_eq!(
            units,
            vec![
                (Role::Server, "server unit".to_owned()),
                (Role::Client, "client unit".to_owned())
            ]
        );
    }

    #[test]
    fn a_bad_checksum_replaces_nothing_and_leaves_nothing() {
        let (root, bin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let base = release(root.path(), "9.9.9", "#!/bin/sh\necho 'hawse 9.9.9'\n");
        let sums = root.path().join("releases/download/v9.9.9/SHA256SUMS");
        let text = std::fs::read_to_string(&sums).unwrap();
        std::fs::write(&sums, format!("{}{}", "0".repeat(64), &text[64..])).unwrap();
        let exe = bin.path().join("hawse");
        std::fs::write(&exe, "old").unwrap();
        let err = replace(&base, Version(9, 9, 9), target().unwrap(), &exe)
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not match"), "{err}");
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
        assert!(!bin.path().join(".hawse-upgrade").exists());
        assert!(!bin.path().join("hawse.old").exists());
    }

    #[test]
    fn a_binary_that_does_not_run_replaces_nothing() {
        let (root, bin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let base = release(root.path(), "9.9.9", "#!/bin/sh\nexit 1\n");
        let exe = bin.path().join("hawse");
        std::fs::write(&exe, "old").unwrap();
        let err = replace(&base, Version(9, 9, 9), target().unwrap(), &exe)
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not run here"), "{err}");
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
    }

    #[test]
    fn the_new_binary_keeps_the_old_ones_owner_and_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let (root, bin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let base = release(root.path(), "9.9.9", "#!/bin/sh\necho 'hawse 9.9.9'\n");
        let exe = bin.path().join("hawse");
        std::fs::write(&exe, "old").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o750)).unwrap();
        replace(&base, Version(9, 9, 9), target().unwrap(), &exe).unwrap();
        let mode = std::fs::metadata(&exe).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o750);
    }

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
