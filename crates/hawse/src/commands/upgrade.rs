//! `hawse upgrade`: each host's release-day routine in one command. It fetches a release for the
//! binary that runs it, checks and tries it before anything is replaced, keeps the old binary
//! beside it, and restarts the service that runs it.

use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;

use miette::{IntoDiagnostic as _, WrapErr as _};

use super::service::{self, launchd, systemd};
use crate::paths::{self, Role};

const REPO: &str = "https://github.com/andriykohut/hawse";

/// Replaces this binary with the release `version` names, or the latest, and restarts each
/// systemd unit or launchd agent that runs it; with `check` it only says whether a newer release
/// exists. Nothing is written until the release is fetched, checked and tried, and the binary it
/// replaces stays beside it with `.old` after its name.
pub fn run(version: Option<&str>, check: bool) -> miette::Result<()> {
    let target = target().ok_or_else(|| {
        miette::miette!("no release binary for this system; try: cargo install --locked hawse")
    })?;
    let wanted = match version {
        Some(version) => version.parse()?,
        None => latest()?,
    };
    let running: Version = env!("CARGO_PKG_VERSION").parse()?;
    let to = match decide(running, wanted, version.is_some(), check)? {
        Decision::Current => {
            println!("hawse {running} is the one running");
            return Ok(());
        }
        Decision::Newer { latest } => {
            println!("hawse {running} is running, newer than the latest, {latest}");
            return Ok(());
        }
        Decision::Check { latest } if latest == running => {
            println!("hawse {running} is running, the latest");
            return Ok(());
        }
        Decision::Check { latest } => {
            println!("hawse {running} is running; {latest} is the latest");
            return Ok(());
        }
        Decision::Install { to } => to,
    };
    let exe = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .into_diagnostic()
        .wrap_err("cannot find the hawse binary")?;
    let cargo_home = std::env::var_os("CARGO_HOME").map(PathBuf::from);
    let home = etcetera::home_dir().ok();
    if let Some(tool) = owner(&exe, cargo_home.as_deref(), home.as_deref()) {
        miette::bail!(
            "{} was installed by {tool}; upgrade it with {tool}",
            exe.display()
        );
    }
    let services = services(&exe);
    if !paths::is_root()
        && let Some(unit) = services.iter().find(|s| matches!(s, Service::Unit(_)))
    {
        miette::bail!(
            "run it with sudo: {} runs this binary, and restarting it needs root",
            unit.name()
        );
    }
    let units = replace(REPO, to, target, &exe)?;
    for service in &services {
        restart(service).wrap_err_with(|| {
            format!(
                "the new binary is in place; mv {} {} goes back",
                kept(&exe).display(),
                exe.display()
            )
        })?;
    }
    let restarted: Vec<String> = services.iter().map(Service::name).collect();
    for line in report(running, to, &restarted) {
        println!("{line}");
    }
    if to > running {
        for service in &services {
            if let Service::Unit(role) = *service
                && changed(role, &units)
            {
                let role = role.name();
                println!(
                    "this release changes hawse-{role}.service; `sudo hawse service install {role}` again, with the --config and --user it was installed with, rewrites it"
                );
            }
        }
    }
    Ok(())
}

/// What an upgrade from `running` to `to` that restarted the services named `restarted` prints.
fn report(running: Version, to: Version, restarted: &[String]) -> Vec<String> {
    let mut lines = vec![format!("hawse {running} → {to}")];
    if restarted.is_empty() {
        lines.push(
            "no hawse-server or hawse-client unit, nor agent, runs this binary, so nothing was restarted; restart whatever runs it to run the new one".to_owned(),
        );
    }
    lines.extend(restarted.iter().map(|name| format!("restarted {name}")));
    lines
}

/// Whether a unit in systemd's `ActiveState` is running, or on its way to it, and so restarted
/// by an upgrade. One stopped or failed is left as it is.
fn running(state: &str) -> bool {
    matches!(state, "active" | "reloading" | "activating" | "refreshing")
}

/// The latest release, from where its page redirects, which costs no API call.
fn latest() -> miette::Result<Version> {
    let url = format!("{REPO}/releases/latest");
    let effective = output(Command::new("curl").args([
        "-fsSLI",
        "-o",
        "/dev/null",
        "-w",
        "%{url_effective}",
        &url,
    ]))
    .ok_or_else(|| miette::miette!("cannot reach {url}"))?;
    tag(&effective)
        .ok_or_else(|| miette::miette!("{url} led to {effective}, which names no release"))
}

/// A service that runs hawse, by whatever wrote it.
enum Service {
    Unit(Role),
    /// An agent in the launchd domain of the user `uid`.
    Agent {
        role: Role,
        uid: u32,
    },
}

impl Service {
    fn name(&self) -> String {
        match *self {
            Service::Unit(role) => format!("hawse-{}", role.name()),
            Service::Agent { role, .. } => launchd::label(role),
        }
    }
}

/// The services that run the binary at `exe`, as systemd or launchd reports them. One that runs
/// another binary is named and left out.
fn services(exe: &Path) -> Vec<Service> {
    let mut found = Vec::new();
    for role in [Role::Server, Role::Client] {
        let (service, runs) = if Path::new("/run/systemd/system").is_dir() {
            let unit = format!("hawse-{}", role.name());
            let show = output(Command::new("systemctl").args([
                "show",
                "-p",
                "ExecStart",
                "--value",
                &unit,
            ]));
            let runs = show.as_deref().and_then(exec_path).map(PathBuf::from);
            let state = output(Command::new("systemctl").args([
                "show",
                "-p",
                "ActiveState",
                "--value",
                &unit,
            ]));
            if runs.is_some() && !state.as_deref().is_some_and(|state| running(state.trim())) {
                println!("{unit} is not running, and is left as it is");
                continue;
            }
            (Service::Unit(role), runs)
        } else if cfg!(target_os = "macos") {
            let uid = agent_uid();
            let target = format!("gui/{uid}/{}", launchd::label(role));
            let print = output(Command::new("launchctl").args(["print", &target]));
            let runs = print.as_deref().and_then(program).map(PathBuf::from);
            (Service::Agent { role, uid }, runs)
        } else {
            continue;
        };
        let Some(runs) = runs else { continue };
        if std::fs::canonicalize(&runs).unwrap_or_else(|_| runs.clone()) == exe {
            found.push(service);
        } else {
            println!("{} runs {}, not this one", service.name(), runs.display());
        }
    }
    found
}

/// The user whose launchd agents are this upgrade's: the one who ran sudo, under sudo.
fn agent_uid() -> u32 {
    let sudo = paths::is_root()
        .then(|| std::env::var("SUDO_UID").ok()?.parse().ok())
        .flatten();
    sudo.unwrap_or_else(|| rustix::process::getuid().as_raw())
}

fn restart(service: &Service) -> miette::Result<()> {
    match *service {
        Service::Unit(role) => run_command(
            Command::new("systemctl").args(["restart", &format!("hawse-{}", role.name())]),
        ),
        Service::Agent { role, uid } => run_command(Command::new("launchctl").args([
            "kickstart",
            "-k",
            &format!("gui/{uid}/{}", launchd::label(role)),
        ])),
    }
}

/// Whether the unit `hawse service install` wrote for `role` comes from another contrib unit
/// than the release's, among `units`. A unit written by hand is not install's to rewrite.
fn changed(role: Role, units: &[(Role, String)]) -> bool {
    let ours =
        std::fs::read_to_string(systemd::file(role)).is_ok_and(|text| service::is_ours(&text));
    let release = units
        .iter()
        .find(|(r, _)| *r == role)
        .map(|(_, text)| text.as_str());
    ours && release.is_some_and(|text| text != systemd::contrib(role))
}

/// What `command` printed, when it succeeded.
fn output(command: &mut Command) -> Option<String> {
    let out = command.output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Where [`replace`] keeps the binary at `exe` that it replaces: beside it, `.old` after its name.
fn kept(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_owned();
    name.push(".old");
    exe.with_file_name(name)
}

/// A release's number, as its tag names it after the `v`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Version(u64, u64, u64);

/// The oldest release `--version` goes back to: the first that looks for an unnamed key where a
/// unit `hawse service install` wrote keeps it. One before it would make a new key there.
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

/// What an upgrade from `running` to `wanted` comes to before anything is downloaded.
#[derive(Debug)]
enum Decision {
    /// `wanted` is the release running.
    Current,
    /// Only `--check` was asked for.
    Check {
        latest: Version,
    },
    /// The release running is newer than the latest, and no `--version` asked to go back.
    Newer {
        latest: Version,
    },
    Install {
        to: Version,
    },
}

/// What to do to go from `running` to `wanted`, which `pinned` says `--version` named rather
/// than the latest release: with `check` only say so, and never go back unless asked. An error
/// for a release below [`FLOOR`], which an upgrade does not install.
fn decide(
    running: Version,
    wanted: Version,
    pinned: bool,
    check: bool,
) -> miette::Result<Decision> {
    if check {
        return Ok(Decision::Check { latest: wanted });
    }
    if wanted == running {
        return Ok(Decision::Current);
    }
    if !pinned && wanted < running {
        return Ok(Decision::Newer { latest: wanted });
    }
    if wanted < FLOOR {
        miette::bail!(
            "hawse {wanted} looks for a key the config does not name beside the config, not where a unit hawse service install wrote keeps it; install a release before {FLOOR} by hand"
        );
    }
    Ok(Decision::Install { to: wanted })
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
    // With two URLs, curl's exit is the last transfer's unless it stops at the first failure.
    run_command(
        Command::new("curl")
            .arg("-fsSL")
            .arg("--fail-early")
            .arg("-o")
            .arg(&archive)
            .arg(url(&format!("{name}.tar.gz")))
            .arg("-o")
            .arg(&sums)
            .arg(url("SHA256SUMS")),
    )?;
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
    run_command(
        Command::new("tar")
            .arg("-xzf")
            .arg(&archive)
            .arg("-C")
            .arg(&stage.0),
    )?;
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
    let old = kept(exe);
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
fn run_command(command: &mut Command) -> miette::Result<()> {
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
    fn a_version_below_the_floor_or_the_running_one_downloads_nothing() {
        let running = Version(0, 10, 0);
        assert!(matches!(
            decide(running, running, false, false),
            Ok(Decision::Current)
        ));
        assert!(matches!(
            decide(running, Version(0, 11, 0), false, true),
            Ok(Decision::Check { .. })
        ));
        let err = decide(Version(0, 11, 0), Version(0, 9, 0), true, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("by hand") && err.contains("key"), "{err}");
        assert!(!err.contains("ready"), "0.9.0 does tell systemd: {err}");
        assert!(matches!(
            decide(running, Version(0, 11, 0), false, false),
            Ok(Decision::Install { to }) if to == Version(0, 11, 0)
        ));
        assert!(matches!(
            decide(Version(0, 11, 0), running, true, false),
            Ok(Decision::Install { .. })
        ));
    }

    #[test]
    fn a_build_newer_than_the_latest_is_not_taken_back_unasked() {
        assert!(matches!(
            decide(Version(0, 11, 0), Version(0, 10, 0), false, false),
            Ok(Decision::Newer { latest }) if latest == Version(0, 10, 0)
        ));
    }

    #[test]
    fn only_a_unit_that_runs_is_restarted() {
        for state in ["active", "reloading", "activating", "refreshing"] {
            assert!(running(state), "{state}");
        }
        for state in ["inactive", "failed", "deactivating", ""] {
            assert!(!running(state), "{state}");
        }
    }

    #[test]
    fn an_upgrade_that_restarted_nothing_says_so() {
        let lines = report(Version(0, 10, 0), Version(0, 11, 0), &[]);
        assert_eq!(lines[0], "hawse 0.10.0 → 0.11.0");
        assert!(lines[1].contains("nothing was restarted"), "{lines:?}");
        let lines = report(
            Version(0, 10, 0),
            Version(0, 11, 0),
            &["hawse-server".to_owned()],
        );
        assert_eq!(lines, ["hawse 0.10.0 → 0.11.0", "restarted hawse-server"]);
    }

    #[test]
    fn a_missing_archive_is_a_failed_download() {
        let (root, bin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let base = release(root.path(), "9.9.9", "#!/bin/sh\necho 'hawse 9.9.9'\n");
        let name = format!("hawse-9.9.9-{}.tar.gz", target().unwrap());
        std::fs::remove_file(root.path().join("releases/download/v9.9.9").join(name)).unwrap();
        let exe = bin.path().join("hawse");
        std::fs::write(&exe, "old").unwrap();
        let err = replace(&base, Version(9, 9, 9), target().unwrap(), &exe)
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("curl"), "{err}");
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
