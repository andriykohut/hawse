use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use etcetera::BaseStrategy as _;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Role {
    Server,
    Client,
}

impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Role::Server => "server",
            Role::Client => "client",
        }
    }

    pub fn file_name(self) -> &'static str {
        match self {
            Role::Server => "server.toml",
            Role::Client => "client.toml",
        }
    }

    pub fn key_name(self) -> &'static str {
        match self {
            Role::Server => "server.key",
            Role::Client => "client.key",
        }
    }
}

/// `dir` is where relative paths in the file resolve and where hawse writes keys; it is set even when `exists` is false.
#[derive(Debug)]
pub struct Located {
    pub file: PathBuf,
    pub dir: PathBuf,
    pub exists: bool,
}

pub fn locate(role: Role, explicit: Option<PathBuf>) -> Located {
    if let Some(file) = explicit {
        let dir = file
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        let exists = file.is_file();
        return Located { file, dir, exists };
    }
    let system = PathBuf::from("/etc/hawse");
    let candidates = [user_dir(), Some(system.clone())];
    for dir in candidates.into_iter().flatten() {
        let file = dir.join(role.file_name());
        if file.is_file() {
            return Located {
                file,
                dir,
                exists: true,
            };
        }
    }
    let dir = if is_root() {
        system
    } else {
        user_dir().unwrap_or(system)
    };
    Located {
        file: dir.join(role.file_name()),
        dir,
        exists: false,
    }
}

/// The key a config at `located` names, or the default for one that names none: see
/// [`default_key`].
pub fn key_path(role: Role, located: &Located, configured: Option<&Path>) -> PathBuf {
    match configured {
        Some(key) => located.dir.join(key),
        None => default_key(
            role,
            &located.dir,
            std::env::var_os("STATE_DIRECTORY").as_deref(),
            is_root(),
        ),
    }
}

/// Where the key of a config in `config_dir` that names none is: one already beside the config;
/// else in the first of systemd's `state_directory`; else in `/var/lib/hawse` for root; else
/// beside the config.
pub fn default_key(
    role: Role,
    config_dir: &Path,
    state_directory: Option<&OsStr>,
    root: bool,
) -> PathBuf {
    let beside = config_dir.join(role.key_name());
    // A key made where the default used to put it stays the identity, whatever runs it now.
    if beside.is_file() {
        return beside;
    }
    // systemd joins several StateDirectory= entries with colons.
    let state = state_directory.and_then(|dirs| std::env::split_paths(dirs).next());
    match state {
        Some(dir) => dir.join(role.key_name()),
        None if root => Path::new("/var/lib/hawse").join(role.key_name()),
        None => beside,
    }
}

fn user_dir() -> Option<PathBuf> {
    etcetera::choose_base_strategy()
        .ok()
        .map(|s| s.config_dir().join("hawse"))
}

pub fn is_root() -> bool {
    rustix::process::getuid().is_root()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_path_wins_and_sets_the_dir() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("custom.toml");
        std::fs::write(&file, "").unwrap();
        let found = locate(Role::Server, Some(file.clone()));
        assert_eq!(found.file, file);
        assert_eq!(found.dir, dir.path());
        assert!(found.exists);
    }

    #[test]
    fn missing_explicit_path_is_reported_not_invented() {
        let found = locate(Role::Client, Some("/nonexistent/dir/client.toml".into()));
        assert!(!found.exists);
        assert_eq!(found.dir, std::path::Path::new("/nonexistent/dir"));
    }

    #[test]
    fn role_names_files() {
        assert_eq!(Role::Server.file_name(), "server.toml");
        assert_eq!(Role::Client.file_name(), "client.toml");
        assert_eq!(Role::Client.key_name(), "client.key");
        assert_eq!(Role::Server.name(), "server");
        assert_eq!(Role::Client.name(), "client");
    }

    #[test]
    fn a_key_beside_the_config_keeps_being_used() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("client.key"), "").unwrap();
        let state = OsStr::new("/var/lib/hawse");
        assert_eq!(
            default_key(Role::Client, dir.path(), Some(state), true),
            dir.path().join("client.key")
        );
    }

    #[test]
    fn a_unit_keeps_its_key_in_its_state_directory() {
        let dir = tempfile::tempdir().unwrap();
        let key = default_key(
            Role::Server,
            dir.path(),
            Some(OsStr::new("/var/lib/hawse")),
            false,
        );
        assert_eq!(key, Path::new("/var/lib/hawse/server.key"));
        let first = default_key(Role::Server, dir.path(), Some(OsStr::new("/a:/b")), false);
        assert_eq!(first, Path::new("/a/server.key"));
    }

    #[test]
    fn root_keeps_its_key_in_var_lib() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            default_key(Role::Client, dir.path(), None, true),
            Path::new("/var/lib/hawse/client.key")
        );
    }

    #[test]
    fn a_user_keeps_its_key_beside_the_config() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            default_key(Role::Client, dir.path(), None, false),
            dir.path().join("client.key")
        );
    }

    #[test]
    fn an_explicit_key_resolves_against_the_config() {
        let located = Located {
            file: "/etc/hawse/server.toml".into(),
            dir: "/etc/hawse".into(),
            exists: true,
        };
        assert_eq!(
            key_path(Role::Server, &located, Some(Path::new("k.pem"))),
            Path::new("/etc/hawse/k.pem")
        );
        assert_eq!(
            key_path(Role::Server, &located, Some(Path::new("/srv/k.pem"))),
            Path::new("/srv/k.pem")
        );
    }
}
