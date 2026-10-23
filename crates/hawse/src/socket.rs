//! The local Unix socket a running server or client answers requests on, so that `hawse status`
//! can ask it how it stands. The socket is generic over what a status is: the caller hands
//! [`Bound::serve`] a function that returns anything serializable, and each request calls it
//! afresh, so nothing runs in the process while nobody asks.
//!
//! A request is one JSON line, a reply is one JSON line, and the connection then closes.

use std::fs::{DirBuilder, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt as _, FileTypeExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener as StdListener, UnixStream as StdStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::time::{Instant, timeout_at};

use crate::paths::{Role, is_root};

/// The longest request line a caller may send, newline included.
pub const REQUEST_LIMIT: u64 = 4096;
/// How long a caller has to send its request line before the connection is dropped.
pub const REQUEST_WAIT: Duration = Duration::from_secs(5);

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What a caller may ask. The reply to [`Request::Status`] is whatever the status function
/// given to [`Bound::serve`] returns.
#[derive(Deserialize)]
#[serde(tag = "request", rename_all = "lowercase")]
pub enum Request {
    Status,
}

/// The reply to a request that cannot be answered: unreadable, unknown, or too long.
#[derive(Serialize, Deserialize)]
pub struct Refusal {
    pub error: String,
}

/// The facts about the process and its environment that decide where its socket goes, gathered
/// in one place so that [`place`] and [`candidates`] are pure.
#[derive(Clone, Debug, PartialEq)]
pub struct Env {
    /// `NOTIFY_SOCKET` is set: a `Type=notify` unit runs this process.
    pub notify: bool,
    /// The first path of systemd's `RUNTIME_DIRECTORY`.
    pub runtime_directory: Option<PathBuf>,
    pub root: bool,
    pub linux: bool,
    pub xdg_runtime_dir: Option<PathBuf>,
    pub state_dir: Option<PathBuf>,
}

impl Env {
    #[expect(dead_code, reason = "`hawse status` calls it to find the socket")]
    pub fn current() -> Env {
        let strategy = etcetera::choose_base_strategy().ok();
        Env {
            notify: crate::notify::socket().is_some(),
            // systemd joins several RuntimeDirectory= entries with colons.
            runtime_directory: std::env::var_os("RUNTIME_DIRECTORY")
                .and_then(|dirs| std::env::split_paths(&dirs).next())
                .filter(|dir| !dir.as_os_str().is_empty()),
            root: is_root(),
            linux: cfg!(target_os = "linux"),
            xdg_runtime_dir: strategy
                .as_ref()
                .and_then(etcetera::BaseStrategy::runtime_dir),
            state_dir: strategy
                .as_ref()
                .and_then(etcetera::BaseStrategy::state_dir),
        }
    }
}

/// Where a process binds its socket.
#[derive(Clone, Debug, PartialEq)]
pub enum Place {
    At(PathBuf),
    /// A systemd unit without `RuntimeDirectory=`: it has nowhere writable that a caller could
    /// also find.
    UnitWithout,
    Nowhere,
}

fn file_name(role: Role) -> String {
    format!("{}.sock", role.name())
}

fn system_path(role: Role) -> PathBuf {
    Path::new("/run")
        .join(format!("hawse-{}", role.name()))
        .join(file_name(role))
}

/// Where `role` binds its socket in `env`: in the unit's runtime directory; else nowhere for a
/// unit without one; else `/run/hawse-<role>` for root on Linux; else `hawse/` under the user's
/// runtime directory or state directory.
pub fn place(role: Role, env: &Env) -> Place {
    let under_hawse = |dir: &PathBuf| dir.join("hawse").join(file_name(role));
    if let Some(dir) = &env.runtime_directory {
        Place::At(dir.join(file_name(role)))
    } else if env.notify {
        Place::UnitWithout
    } else if env.root && env.linux {
        Place::At(system_path(role))
    } else if let Some(dir) = &env.xdg_runtime_dir {
        Place::At(under_hawse(dir))
    } else if let Some(dir) = &env.state_dir {
        Place::At(under_hawse(dir))
    } else {
        Place::Nowhere
    }
}

/// Every path a caller of `role` tries, in order. A caller does not know how the process it asks
/// was started, so this is the union of the places [`place`] can choose, minus those the
/// platform does not have.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "`hawse status` calls it to find the socket")
)]
pub fn candidates(role: Role, env: &Env) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    paths.extend(
        env.runtime_directory
            .iter()
            .map(|d| d.join(file_name(role))),
    );
    if env.linux {
        paths.push(system_path(role));
    }
    for dir in [&env.xdg_runtime_dir, &env.state_dir].into_iter().flatten() {
        paths.push(dir.join("hawse").join(file_name(role)));
    }
    paths
}

/// A socket bound at `path`; dropping it removes the file.
#[derive(Debug)]
pub struct Bound {
    listener: StdListener,
    path: PathBuf,
}

impl Drop for Bound {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Debug)]
enum BindError {
    /// The socket's directory is missing and cannot be made.
    NoDir(io::Error),
    /// A live process already answers on the path.
    Taken,
    Io(io::Error),
}

/// Binds the socket for `role` where [`place`] says. Returns `None` when there is no place or the
/// bind fails, after logging why, since a server or client runs on without its socket.
#[expect(
    dead_code,
    reason = "the server and the client call it once they serve status"
)]
pub fn bind(role: Role) -> Option<Bound> {
    let path = match place(role, &Env::current()) {
        Place::At(path) => path,
        Place::UnitWithout => {
            tracing::warn!(
                "this unit has no RuntimeDirectory=, so hawse status cannot reach it; \
                 sudo hawse service install {} again rewrites it",
                role.name()
            );
            return None;
        }
        Place::Nowhere => {
            tracing::debug!("no directory for a status socket");
            return None;
        }
    };
    match bind_at(&path) {
        Ok(bound) => Some(bound),
        Err(BindError::NoDir(err)) => {
            tracing::debug!(path = %path.display(), %err, "no directory for a status socket");
            None
        }
        Err(BindError::Taken) => {
            tracing::warn!(
                path = %path.display(),
                "another process answers on the status socket, so this one has none"
            );
            None
        }
        Err(BindError::Io(err)) => {
            tracing::warn!(path = %path.display(), %err, "cannot bind the status socket");
            None
        }
    }
}

/// Binds `path` for its owner only, making its directory (also for the owner only) if missing and
/// replacing a socket left by a process that is gone.
fn bind_at(path: &Path) -> Result<Bound, BindError> {
    if let Some(dir) = path.parent() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(BindError::NoDir)?;
    }
    // Only a refused connection proves the socket is stale: a bind over a live one would steal
    // its callers.
    match StdStream::connect(path) {
        Ok(_) => return Err(BindError::Taken),
        Err(err) if err.kind() == io::ErrorKind::ConnectionRefused => {
            let stale = std::fs::symlink_metadata(path).map_err(BindError::Io)?;
            if !stale.file_type().is_socket() {
                return Err(BindError::Io(io::ErrorKind::AddrInUse.into()));
            }
            std::fs::remove_file(path).map_err(BindError::Io)?;
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(BindError::Io(err)),
    }
    let listener = StdListener::bind(path).map_err(BindError::Io)?;
    let bound = Bound {
        listener,
        path: path.to_path_buf(),
    };
    std::fs::set_permissions(path, Permissions::from_mode(0o600)).map_err(BindError::Io)?;
    bound
        .listener
        .set_nonblocking(true)
        .map_err(BindError::Io)?;
    Ok(bound)
}

impl Bound {
    /// Answers requests until the future is dropped, which also removes the socket file. Each
    /// connection runs in its own task, so a slow caller holds up no other. `status` runs once
    /// per `status` request.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the server and the client call it once they serve status"
        )
    )]
    pub async fn serve<S: Serialize>(self, status: impl Fn() -> S + Send + Sync + 'static) {
        let listener = match self.listener.try_clone().and_then(UnixListener::from_std) {
            Ok(listener) => listener,
            Err(err) => {
                tracing::warn!(%err, "cannot serve the status socket");
                return;
            }
        };
        let status = Arc::new(status);
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let status = Arc::clone(&status);
                    tokio::spawn(async move { answer(stream, &*status).await });
                }
                Err(err) => {
                    // EMFILE and ENFILE fail again at once; waiting keeps this from spinning.
                    tracing::warn!(%err, "cannot accept on the status socket");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }
}

async fn answer<S: Serialize>(mut stream: UnixStream, status: &impl Fn() -> S) {
    let deadline = Instant::now() + REQUEST_WAIT;
    let (read, mut write) = stream.split();
    let mut reader = BufReader::new(read.take(REQUEST_LIMIT));
    let mut line = String::new();
    let mut over_limit = false;
    let reply = match timeout_at(deadline, reader.read_line(&mut line)).await {
        Err(_) | Ok(Ok(0)) => return,
        Ok(Err(err)) => refusal(&format!("cannot read this request: {err}")),
        Ok(Ok(n)) if n as u64 >= REQUEST_LIMIT && !line.ends_with('\n') => {
            over_limit = true;
            refusal(&format!(
                "a request is at most {REQUEST_LIMIT} bytes on one line"
            ))
        }
        Ok(Ok(_)) => match serde_json::from_str::<Request>(&line) {
            Ok(Request::Status) => {
                serde_json::to_string(&status()).unwrap_or_else(|err| refusal(&err.to_string()))
            }
            Err(err) => refusal(&format!("cannot read this request: {err}")),
        },
    };
    // A caller that has gone has nobody to tell.
    let _ = write.write_all(format!("{reply}\n").as_bytes()).await;
    let _ = write.shutdown().await;
    if over_limit {
        // On Linux, closing a stream socket with unread data resets the peer's connection, which
        // would turn the caller's read of the refusal's end into an error. Reading to the
        // caller's EOF first lets it see a clean close.
        let mut rest = reader.into_inner().into_inner();
        let _ = timeout_at(deadline, tokio::io::copy(&mut rest, &mut tokio::io::sink())).await;
    }
}

fn refusal(error: &str) -> String {
    let error = format!("hawse {VERSION}: {error}");
    serde_json::to_string(&Refusal { error }).expect("a string field serializes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn none() -> Env {
        Env {
            notify: false,
            runtime_directory: None,
            root: false,
            linux: true,
            xdg_runtime_dir: None,
            state_dir: None,
        }
    }

    #[test]
    fn place_follows_the_table() {
        let none = none();
        let at = |p: &str| Place::At(PathBuf::from(p));
        let unit = Env {
            runtime_directory: Some("/run/hawse-client".into()),
            notify: true,
            ..none.clone()
        };
        assert_eq!(
            place(Role::Client, &unit),
            at("/run/hawse-client/client.sock")
        );
        assert_eq!(
            place(
                Role::Server,
                &Env {
                    notify: true,
                    ..none.clone()
                }
            ),
            Place::UnitWithout
        );
        assert_eq!(
            place(
                Role::Server,
                &Env {
                    root: true,
                    ..none.clone()
                }
            ),
            at("/run/hawse-server/server.sock")
        );
        assert_eq!(
            place(
                Role::Server,
                &Env {
                    root: true,
                    linux: false,
                    state_dir: Some("/var/root/.local/state".into()),
                    ..none.clone()
                }
            ),
            at("/var/root/.local/state/hawse/server.sock")
        );
        assert_eq!(
            place(
                Role::Client,
                &Env {
                    xdg_runtime_dir: Some("/run/user/1000".into()),
                    state_dir: Some("/home/a/.local/state".into()),
                    ..none.clone()
                }
            ),
            at("/run/user/1000/hawse/client.sock")
        );
        assert_eq!(
            place(
                Role::Client,
                &Env {
                    linux: false,
                    state_dir: Some("/Users/a/.local/state".into()),
                    ..none.clone()
                }
            ),
            at("/Users/a/.local/state/hawse/client.sock")
        );
        assert_eq!(place(Role::Client, &none), Place::Nowhere);
    }

    #[test]
    fn candidates_try_every_place_whoever_asks() {
        let env = Env {
            xdg_runtime_dir: Some("/run/user/1000".into()),
            state_dir: Some("/home/a/.local/state".into()),
            ..none()
        };
        let paths = |env: &Env| -> Vec<String> {
            candidates(Role::Client, env)
                .iter()
                .map(|p| p.display().to_string())
                .collect()
        };
        assert_eq!(
            paths(&env),
            [
                "/run/hawse-client/client.sock",
                "/run/user/1000/hawse/client.sock",
                "/home/a/.local/state/hawse/client.sock",
            ]
        );
        assert_eq!(
            paths(&Env {
                linux: false,
                ..env.clone()
            }),
            [
                "/run/user/1000/hawse/client.sock",
                "/home/a/.local/state/hawse/client.sock",
            ]
        );
        assert_eq!(
            paths(&Env {
                runtime_directory: Some("/run/custom".into()),
                ..env
            })[0],
            "/run/custom/client.sock"
        );
    }

    #[test]
    fn a_socket_left_by_a_dead_process_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.sock");
        drop(StdListener::bind(&path).unwrap());
        assert!(path.exists());
        bind_at(&path).unwrap();
    }

    #[test]
    fn a_socket_another_process_answers_on_is_not_taken() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.sock");
        let _live = StdListener::bind(&path).unwrap();
        assert!(matches!(bind_at(&path), Err(BindError::Taken)));
        assert!(path.exists());
    }

    #[test]
    fn the_socket_is_for_its_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.sock");
        let _bound = bind_at(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn a_missing_directory_is_made_for_its_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let _bound = bind_at(&dir.path().join("hawse/client.sock")).unwrap();
        let mode = std::fs::metadata(dir.path().join("hawse"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn a_directory_that_cannot_be_made_is_no_dir() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, "").unwrap();
        assert!(matches!(
            bind_at(&file.join("client.sock")),
            Err(BindError::NoDir(_))
        ));
    }

    #[test]
    fn dropping_the_socket_removes_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.sock");
        drop(bind_at(&path).unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn a_path_too_long_for_a_socket_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a".repeat(200));
        assert!(matches!(bind_at(&path), Err(BindError::Io(_))));
    }

    /// Serves `json!({"role": "client"})` on a fresh socket; the returned `TempDir` guard keeps the socket's directory alive.
    fn serving() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.sock");
        let bound = bind_at(&path).unwrap();
        tokio::spawn(bound.serve(|| json!({"role": "client"})));
        (dir, path)
    }

    async fn ask(path: &Path, request: &[u8]) -> (String, usize) {
        let mut stream = UnixStream::connect(path).await.unwrap();
        stream.write_all(request).await.unwrap();
        let mut reply = String::new();
        BufReader::new(&mut stream)
            .read_line(&mut reply)
            .await
            .unwrap();
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest).await.unwrap();
        (reply, rest.len())
    }

    #[tokio::test]
    async fn a_status_request_gets_one_line_and_the_connection_closes() {
        let (_dir, path) = serving();
        let (reply, rest) = ask(&path, b"{\"request\":\"status\"}\n").await;
        assert_eq!(reply, "{\"role\":\"client\"}\n");
        assert_eq!(rest, 0);
    }

    async fn is_refused(request: &[u8]) {
        let (_dir, path) = serving();
        let (reply, rest) = ask(&path, request).await;
        let refusal: Refusal = serde_json::from_str(&reply).unwrap();
        assert!(refusal.error.contains(VERSION), "{}", refusal.error);
        assert_eq!(rest, 0);
    }

    #[tokio::test]
    async fn an_unknown_request_gets_an_error_line() {
        is_refused(b"{\"request\":\"restart\"}\n").await;
    }

    #[tokio::test]
    async fn a_line_that_is_not_json_gets_an_error_line() {
        is_refused(b"status please\n").await;
    }

    #[tokio::test]
    async fn a_request_over_4_kib_gets_an_error_line() {
        let mut request = vec![b'a'; 5000];
        request.push(b'\n');
        is_refused(&request).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_caller_that_sends_nothing_is_dropped_after_5_s() {
        let (_dir, path) = serving();
        let mut silent = UnixStream::connect(&path).await.unwrap();
        let (reply, _) = ask(&path, b"{\"request\":\"status\"}\n").await;
        assert_eq!(reply, "{\"role\":\"client\"}\n");
        tokio::time::sleep(REQUEST_WAIT).await;
        let mut rest = Vec::new();
        silent.read_to_end(&mut rest).await.unwrap();
        assert_eq!(rest, b"");
    }
}
