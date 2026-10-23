//! `hawse service`: run the server or the client as a service that starts by itself, as a systemd
//! unit on Linux or a launchd agent on macOS, from files hawse writes and later recognises as its
//! own.

pub mod launchd;
pub mod systemd;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use hawse_core::identity::Identity;
use hawse_proto::key::PublicKey;
use miette::{IntoDiagnostic as _, WrapErr as _};

use crate::config_file;
use crate::paths::{self, Located, Role};
use systemd::Account;

/// Where a unit without `--user` keeps its key: systemd's `StateDirectory=hawse`.
const STATE: &str = "/var/lib/hawse";

/// The words that mark a unit or an agent as written by `hawse service install`, which replaces
/// and removes only a file that carries them.
pub const MARKER: &str = "Written by hawse service install.";

/// Whether `text` is a unit or an agent install wrote: the marker is on its first line, or on the
/// second after a plist's XML declaration.
pub fn is_ours(text: &str) -> bool {
    text.lines().take(2).any(|line| line.contains(MARKER))
}

/// Writes the service for `role`, enables and starts it, and prints its public key. The service
/// runs the config at `config`, or the one `role` would read; a server without one runs on the
/// defaults. With `user` the systemd unit runs as that account and keeps the key beside the
/// config. Nothing is written until every check has passed.
pub fn install(role: Role, config: Option<PathBuf>, user: Option<&str>) -> miette::Result<()> {
    let manager = manager(
        std::env::consts::OS,
        paths::is_root(),
        Path::new("/run/systemd/system").is_dir(),
    )?;
    if let Some(name) = user {
        if matches!(manager, Manager::Launchd) {
            miette::bail!("a LaunchAgent runs as whoever installs it; drop --user");
        }
        account(name)?;
    }
    let (located, key) = match role {
        Role::Server => {
            let (cfg, located) = config_file::load_server(config)?;
            (located, cfg.key)
        }
        Role::Client => {
            let (cfg, located) = config_file::load_client(config)?;
            (located, cfg.key)
        }
    };
    let located = resolve(&located)?;
    let exe = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .into_diagnostic()
        .wrap_err("cannot find the hawse binary")?;
    match manager {
        Manager::Systemd => install_unit(role, &located, key.as_deref(), &exe, user),
        Manager::Launchd => install_agent(role, &located, key.as_deref(), &exe),
    }
}

/// Stops and removes the service for `role`. The config and the key stay; nothing installed is not
/// an error.
pub fn uninstall(role: Role) -> miette::Result<()> {
    let manager = manager(
        std::env::consts::OS,
        paths::is_root(),
        Path::new("/run/systemd/system").is_dir(),
    )?;
    let (file, name) = match manager {
        Manager::Systemd => (systemd::file(role), format!("hawse-{}", role.name())),
        Manager::Launchd => (launchd::file(&home()?, role), launchd::label(role)),
    };
    if !installed(&file)? {
        println!("{name} is not installed");
        return Ok(());
    }
    match manager {
        Manager::Systemd => run("systemctl", &["disable", "--now", &name])?,
        Manager::Launchd => bootout(&name)?,
    }
    std::fs::remove_file(&file)
        .into_diagnostic()
        .wrap_err_with(|| format!("cannot remove {}", file.display()))?;
    if matches!(manager, Manager::Systemd) {
        run("systemctl", &["daemon-reload"])?;
    }
    println!(
        "removed {}; the config and the key are where they were",
        file.display()
    );
    Ok(())
}

fn install_unit(
    role: Role,
    located: &Located,
    configured: Option<&Path>,
    exe: &Path,
    user: Option<&str>,
) -> miette::Result<()> {
    let key = unit_key(role, located, configured, user)?;
    let key_dir = key.parent().unwrap_or(Path::new("/"));
    let mut seen = vec![exe, located.file.as_path()];
    if user.is_some() {
        seen.push(key_dir);
    }
    for path in seen {
        if let Some(dir) = hidden(path) {
            miette::bail!(
                "{} is under {dir}, which the unit's ProtectHome= or PrivateTmp= hides from the service; move it",
                path.display()
            );
        }
    }
    if user.is_none()
        && located.exists
        && let Some((path, open)) = closed_to_others(&located.file).into_diagnostic()?
    {
        miette::bail!(
            "{path} keeps the unit's throwaway account from reading {config}; chmod {open} {path}",
            path = path.display(),
            config = located.file.display()
        );
    }
    if !systemd::can_execute(text(exe)?) {
        miette::bail!(
            "{} holds a quote, a backslash or a control character, which systemd runs no binary from; move it",
            exe.display()
        );
    }
    let file = systemd::file(role);
    installed(&file)?;
    let account = match user {
        Some(name) => Some(Account {
            name,
            key_dir: text(key_dir)?,
        }),
        None => None,
    };
    let unit = systemd::unit(role, text(exe)?, text(&located.file)?, account.as_ref());
    std::fs::write(&file, unit)
        .into_diagnostic()
        .wrap_err_with(|| format!("cannot write {}", file.display()))?;
    let name = format!("hawse-{}", role.name());
    run("systemctl", &["daemon-reload"])?;
    run("systemctl", &["enable", &name])?;
    // A Type=notify unit's restart returns once it is ready, and both roles load or create their
    // key before they say so.
    let created = !key.exists();
    run("systemctl", &["restart", &name])?;
    let identity = super::check::load_key(&key)?
        .ok_or_else(|| miette::miette!("the service started without a key at {}", key.display()))?;
    let how = format!(
        "`systemctl status {name}` shows how it stands, and `journalctl -u {name}` its log"
    );
    for line in report(role, &file, &identity.public_key(), created, &how) {
        println!("{line}");
    }
    Ok(())
}

fn install_agent(
    role: Role,
    located: &Located,
    configured: Option<&Path>,
    exe: &Path,
) -> miette::Result<()> {
    let home = home()?;
    let key = paths::key_path(role, located, configured);
    let file = launchd::file(&home, role);
    let log = launchd::log_file(&home, role);
    installed(&file)?;
    let plist = launchd::plist(role, text(exe)?, text(&located.file)?, text(&log)?);
    for dir in [file.parent(), log.parent()].into_iter().flatten() {
        std::fs::create_dir_all(dir)
            .into_diagnostic()
            .wrap_err_with(|| format!("cannot create {}", dir.display()))?;
    }
    std::fs::write(&file, plist)
        .into_diagnostic()
        .wrap_err_with(|| format!("cannot write {}", file.display()))?;
    let (identity, created) = Identity::load_or_create(&key)
        .into_diagnostic()
        .wrap_err_with(|| format!("cannot load the {} key at {}", role.name(), key.display()))?;
    if created {
        tracing::info!(path = %key.display(), "created {} key", role.name());
    }
    let label = launchd::label(role);
    let uid = rustix::process::getuid().as_raw();
    bootout(&label)?;
    run(
        "launchctl",
        &["bootstrap", &format!("gui/{uid}"), text(&file)?],
    )?;
    let how = format!(
        "`/usr/bin/log stream --predicate 'subsystem == \"{}\"'` follows its log, and `launchctl print gui/{uid}/{label}` shows how it stands",
        launchd::SUBSYSTEM
    );
    for line in report(role, &file, &identity.public_key(), created, &how) {
        println!("{line}");
    }
    Ok(())
}

/// What install prints for the service at `file` and its `key`, which `created` says it made, and
/// `how` to look at the service. A new key says who has to learn it.
fn report(role: Role, file: &Path, key: &PublicKey, created: bool, how: &str) -> Vec<String> {
    let mut lines = vec![
        format!("installed {} and started it", file.display()),
        format!("{} key: {key}", role.name()),
    ];
    if created {
        lines.push(match role {
            Role::Server => "it is new: each client needs it as its server_key".to_owned(),
            Role::Client => {
                "it is new: on the server, add it to server.toml under [clients.NAME]".to_owned()
            }
        });
    }
    lines.push(how.to_owned());
    lines
}

/// `located` with absolute paths, its directory resolved where it exists, so that no link or
/// `..` in it reaches a unit or the checks made for one.
fn resolve(located: &Located) -> miette::Result<Located> {
    let dir = std::path::absolute(&located.dir).into_diagnostic()?;
    let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
    let name = located
        .file
        .file_name()
        .ok_or_else(|| miette::miette!("{} names no file", located.file.display()))?;
    Ok(Located {
        file: dir.join(name),
        dir,
        exists: located.exists,
    })
}

#[derive(Debug)]
enum Manager {
    Systemd,
    Launchd,
}

/// The service manager for whoever runs `hawse service` on `os`: systemd for root on Linux while
/// systemd runs, launchd for a user on macOS. An error says what to do instead.
fn manager(os: &str, root: bool, systemd_running: bool) -> miette::Result<Manager> {
    match (os, root) {
        ("linux", false) => Err(miette::miette!(
            "run it with sudo: a systemd unit is installed as root"
        )),
        ("linux", true) if !systemd_running => {
            Err(miette::miette!("this system is not running systemd"))
        }
        ("linux", true) => Ok(Manager::Systemd),
        ("macos", true) => Err(miette::miette!(
            "run it without sudo: the agent starts when you log in"
        )),
        ("macos", false) => Ok(Manager::Launchd),
        _ => Err(miette::miette!(
            "hawse service supports systemd on Linux and launchd on macOS"
        )),
    }
}

/// `name` when it can be a unit's `User=` and nothing more. Root is refused: it would find its key
/// by root's default, in a directory the unit cannot write.
fn account(name: &str) -> miette::Result<&str> {
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.');
    if name.is_empty() || name.starts_with('-') || !name.chars().all(allowed) {
        miette::bail!("`{name}` is not an account name");
    }
    if name == "root" {
        miette::bail!(
            "--user root would run the tunnel as root; drop --user for a throwaway account, or name the account that owns the config"
        );
    }
    Ok(name)
}

/// The directory a unit's `ProtectHome=` or `PrivateTmp=` hides `path` in, if any, as written or
/// where a link in it leads.
fn hidden(path: &Path) -> Option<&'static str> {
    // Both, since on some systems `/home` is itself a link, into `/var/home`.
    let resolved = std::fs::canonicalize(path).ok();
    ["/home", "/root", "/run/user", "/tmp", "/var/tmp"]
        .into_iter()
        .find(|dir| path.starts_with(dir) || resolved.as_ref().is_some_and(|r| r.starts_with(dir)))
}

/// The key a unit for the config at `located` uses. With an `account` that is the account's own
/// default, in a directory that has to be there already; without one the key has to be in the
/// unit's state directory, since the unit's throwaway account can neither read root's key nor
/// write anywhere else. The error says how to fix a key that is not.
fn unit_key(
    role: Role,
    located: &Located,
    configured: Option<&Path>,
    account: Option<&str>,
) -> miette::Result<PathBuf> {
    let key = match configured {
        Some(key) => located.dir.join(key),
        None if account.is_some() => paths::default_key(role, &located.dir, None, false),
        None => paths::default_key(role, &located.dir, Some(OsStr::new(STATE)), false),
    };
    if let Some(name) = account {
        let dir = key.parent().unwrap_or(Path::new("/"));
        // ReadWritePaths= fails the unit's start on a directory that is not there.
        if !dir.is_dir() {
            miette::bail!(
                "{dir} is not there, and the unit can write a key only to a directory that is; sudo install -d -o {name} {dir} and run this again",
                dir = dir.display()
            );
        }
        return Ok(key);
    }
    if key.starts_with(STATE) {
        return Ok(key);
    }
    if configured.is_some() {
        miette::bail!(
            "the unit can keep its key only under {STATE}; drop `key` from the config or point it there"
        );
    }
    Err(miette::miette!(
        "{key} is root's, and the unit's throwaway account cannot read it; sudo mv {key} {STATE}/ and run this again",
        key = key.display()
    ))
}

/// The first of `file` and the directories above it that keeps out an account other than their
/// owners, with the `chmod` that lets it through; `None` when such an account can read `file`.
/// Where `file` is or passes through a link, both the directories on the path as written and
/// those where it leads count, the nearest first.
fn closed_to_others(file: &Path) -> std::io::Result<Option<(PathBuf, &'static str)>> {
    use std::os::unix::fs::PermissionsExt as _;
    let real = std::fs::canonicalize(file)?;
    let mode = |path: &Path| std::fs::metadata(path).map(|meta| meta.permissions().mode());
    if mode(&real)? & 0o004 == 0 {
        return Ok(Some((real, "o+r")));
    }
    let written: Vec<&Path> = file.ancestors().skip(1).collect();
    let resolved: Vec<&Path> = real.ancestors().skip(1).collect();
    for depth in 0..written.len().max(resolved.len()) {
        for dir in [written.get(depth), resolved.get(depth)]
            .into_iter()
            .flatten()
        {
            if !dir.as_os_str().is_empty() && mode(dir)? & 0o001 == 0 {
                return Ok(Some((dir.to_path_buf(), "o+x")));
            }
        }
    }
    Ok(None)
}

/// Whether a file install wrote is at `file`: false when nothing is there, and an error for a file
/// install did not write, which it neither replaces nor removes.
fn installed(file: &Path) -> miette::Result<bool> {
    let text = match std::fs::read_to_string(file) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        read => read
            .into_diagnostic()
            .wrap_err_with(|| format!("cannot read {}", file.display()))?,
    };
    if is_ours(&text) {
        return Ok(true);
    }
    let remedy = match file.file_stem() {
        Some(unit) if file.extension() == Some(OsStr::new("service")) => format!(
            "move any changes to a drop-in (systemctl edit {}) and remove it",
            unit.display()
        ),
        _ => "remove it".to_owned(),
    };
    Err(miette::miette!(
        "{} was not written by hawse service install; {remedy}",
        file.display()
    ))
}

/// `path` as the text a unit or a plist holds it as: UTF-8, and with no control character, which
/// would end a unit's line.
fn text(path: &Path) -> miette::Result<&str> {
    let text = path.to_str().ok_or_else(|| {
        miette::miette!(
            "{} is not UTF-8, which a unit or a plist cannot hold",
            path.display()
        )
    })?;
    if text.chars().any(char::is_control) {
        miette::bail!(
            "{} holds a control character, which a unit or a plist cannot hold",
            path.display()
        );
    }
    Ok(text)
}

fn home() -> miette::Result<PathBuf> {
    etcetera::home_dir()
        .into_diagnostic()
        .wrap_err("cannot find the home directory")
}

/// Runs `program` with `args` on this terminal; an error when it fails.
fn run(program: &str, args: &[&str]) -> miette::Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .into_diagnostic()
        .wrap_err_with(|| format!("cannot run {program}"))?;
    if !status.success() {
        miette::bail!("{program} {} failed", args.join(" "));
    }
    Ok(())
}

/// Unloads the agent `label` if launchd has it loaded, and returns once launchd has let go of it,
/// so it can be loaded again. An error when it has not within 30 s.
fn bootout(label: &str) -> miette::Result<()> {
    let target = format!("gui/{}/{label}", rustix::process::getuid().as_raw());
    let launchctl = |command: &str| {
        Command::new("launchctl")
            .args([command, &target])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    // An agent that is not loaded is the usual case on a first install, and launchctl says so as
    // an error.
    launchctl("bootout");
    // bootout returns once it has sent SIGTERM, and launchd refuses to load the label again until
    // the process has exited, which it forces with SIGKILL after 20 s.
    let deadline = Instant::now() + Duration::from_secs(30);
    while launchctl("print") {
        if Instant::now() > deadline {
            miette::bail!("{label} is still stopping; run this again");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Located;

    #[test]
    fn the_service_manager_follows_the_system_and_the_user() {
        assert!(matches!(manager("linux", true, true), Ok(Manager::Systemd)));
        assert!(
            manager("linux", false, true)
                .unwrap_err()
                .to_string()
                .contains("sudo")
        );
        assert!(
            manager("linux", true, false)
                .unwrap_err()
                .to_string()
                .contains("not running systemd")
        );
        assert!(matches!(
            manager("macos", false, false),
            Ok(Manager::Launchd)
        ));
        assert!(
            manager("macos", true, false)
                .unwrap_err()
                .to_string()
                .contains("without sudo")
        );
        assert!(manager("freebsd", true, false).is_err());
    }

    #[test]
    fn a_path_the_unit_hides_names_what_hides_it() {
        assert_eq!(hidden(Path::new("/home/someone/hawse")), Some("/home"));
        assert_eq!(hidden(Path::new("/tmp/hawse")), Some("/tmp"));
        assert_eq!(hidden(Path::new("/homer/hawse")), None);
        assert_eq!(hidden(Path::new("/usr/local/bin/hawse")), None);
    }

    #[test]
    fn a_root_key_beside_the_config_is_refused_for_a_throwaway_account() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("client.key"), "").unwrap();
        let located = Located {
            file: dir.path().join("client.toml"),
            dir: dir.path().into(),
            exists: true,
        };
        let err = unit_key(Role::Client, &located, None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("/var/lib/hawse/"), "{err}");
        assert_eq!(
            unit_key(Role::Client, &located, None, Some("someone")).unwrap(),
            dir.path().join("client.key")
        );
    }

    #[test]
    fn a_unit_keeps_an_unnamed_key_in_var_lib_and_refuses_one_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let located = Located {
            file: dir.path().join("server.toml"),
            dir: dir.path().into(),
            exists: true,
        };
        assert_eq!(
            unit_key(Role::Server, &located, None, None).unwrap(),
            Path::new("/var/lib/hawse/server.key")
        );
        let var_lib = Path::new("/var/lib/hawse/server.key");
        assert!(unit_key(Role::Server, &located, Some(var_lib), None).is_ok());
        let beside = Path::new("server.key");
        assert!(unit_key(Role::Server, &located, Some(beside), None).is_err());
    }

    #[test]
    fn an_account_writes_its_key_only_to_a_directory_that_is_there() {
        let dir = tempfile::tempdir().unwrap();
        let located = Located {
            file: dir.path().join("client.toml"),
            dir: dir.path().into(),
            exists: true,
        };
        let keys = Path::new("keys/client.key");
        let err = unit_key(Role::Client, &located, Some(keys), Some("someone"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("install -d -o someone"), "{err}");
        std::fs::create_dir(dir.path().join("keys")).unwrap();
        assert_eq!(
            unit_key(Role::Client, &located, Some(keys), Some("someone")).unwrap(),
            dir.path().join("keys/client.key")
        );
    }

    #[test]
    fn a_config_only_root_reads_is_refused_for_a_throwaway_account() {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |path: &Path, mode| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("hawse");
        let file = dir.join("server.toml");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(&file, "").unwrap();
        let real = |path: &Path| std::fs::canonicalize(path).unwrap();
        mode(&file, 0o600);
        assert_eq!(closed_to_others(&file).unwrap(), Some((real(&file), "o+r")));
        mode(&file, 0o644);
        mode(&dir, 0o750);
        assert_eq!(closed_to_others(&file).unwrap(), Some((dir.clone(), "o+x")));
        assert_eq!(closed_to_others(Path::new("/etc/hosts")).unwrap(), None);
    }

    #[test]
    fn a_config_reached_through_a_closed_directory_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let (open, private) = (root.path().join("open"), root.path().join("private"));
        for dir in [&open, &private] {
            std::fs::create_dir(dir).unwrap();
        }
        std::fs::write(open.join("server.toml"), "").unwrap();
        let link = private.join("server.toml");
        std::os::unix::fs::symlink(open.join("server.toml"), &link).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(closed_to_others(&link).unwrap(), Some((private, "o+x")));
    }

    #[test]
    fn a_linked_config_is_judged_where_it_points() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let (open, closed) = (root.path().join("open"), root.path().join("closed"));
        for dir in [&open, &closed] {
            std::fs::create_dir(dir).unwrap();
        }
        std::fs::write(closed.join("client.toml"), "").unwrap();
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o750)).unwrap();
        let link = open.join("client.toml");
        std::os::unix::fs::symlink(closed.join("client.toml"), &link).unwrap();
        let found = closed_to_others(&link).unwrap();
        assert_eq!(
            found,
            Some((std::fs::canonicalize(&closed).unwrap(), "o+x"))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_link_into_a_hidden_directory_is_hidden() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("hawse");
        std::os::unix::fs::symlink("/home", &link).unwrap();
        assert_eq!(hidden(&link), Some("/home"));
    }

    #[test]
    fn an_account_name_cannot_carry_other_settings() {
        assert!(account("someone").is_ok());
        assert!(account("svc-hawse_2").is_ok());
        assert!(account("").is_err());
        assert!(account("-x").is_err());
        assert!(account("a\nExecStartPre=/bin/true").is_err());
        assert!(
            account("root")
                .unwrap_err()
                .to_string()
                .contains("drop --user")
        );
    }

    #[test]
    fn a_path_with_a_control_character_is_refused() {
        assert_eq!(text(Path::new("/srv/hawse")).unwrap(), "/srv/hawse");
        let err = text(Path::new("/srv/a\nb")).unwrap_err().to_string();
        assert!(err.contains("control character"), "{err}");
    }

    #[test]
    fn a_config_path_through_dot_dot_is_resolved() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("a")).unwrap();
        std::fs::write(root.path().join("client.toml"), "").unwrap();
        let located = Located {
            file: root.path().join("a/../client.toml"),
            dir: root.path().join("a/.."),
            exists: true,
        };
        let resolved = resolve(&located).unwrap();
        let real = std::fs::canonicalize(root.path()).unwrap();
        assert_eq!(resolved.dir, real);
        assert_eq!(resolved.file, real.join("client.toml"));
    }

    #[test]
    fn a_new_key_says_who_has_to_learn_it() {
        let key: PublicKey = "ed25519:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
            .parse()
            .unwrap();
        let file = Path::new("/etc/systemd/system/hawse-server.service");
        let lines = report(Role::Server, file, &key, true, "how");
        assert_eq!(lines[1], format!("server key: {key}"));
        assert!(lines[2].contains("server_key"), "{lines:?}");
        let lines = report(Role::Client, file, &key, true, "how");
        assert!(lines[2].contains("[clients.NAME]"), "{lines:?}");
        let lines = report(Role::Client, file, &key, false, "how");
        assert_eq!(lines.len(), 3, "{lines:?}");
    }

    #[test]
    fn nothing_installed_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hawse-server.service");
        assert!(!installed(&file).unwrap());
        std::fs::write(&file, systemd::unit(Role::Server, "/h", "/c", None)).unwrap();
        assert!(installed(&file).unwrap());
        std::fs::write(&file, "[Unit]\n").unwrap();
        let err = installed(&file).unwrap_err().to_string();
        assert!(
            err.contains("not written by hawse service install"),
            "{err}"
        );
    }
}
