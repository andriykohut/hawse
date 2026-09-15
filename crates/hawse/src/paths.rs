use std::path::{Path, PathBuf};

use etcetera::BaseStrategy as _;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Server,
    Client,
}

impl Role {
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

fn user_dir() -> Option<PathBuf> {
    etcetera::choose_base_strategy()
        .ok()
        .map(|s| s.config_dir().join("hawse"))
}

fn is_root() -> bool {
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
    }
}
