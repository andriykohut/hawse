//! `hawse status`: asks the server and the client running on this host how they stand, over
//! the sockets they answer on, and prints what they say.

use std::io::{self, BufRead as _, BufReader, IsTerminal as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::ExitCode;

use miette::IntoDiagnostic as _;
use serde::Deserialize;

use crate::paths::Role;
use crate::socket::{self, Env, REQUEST_WAIT, Refusal, Request};
use crate::status::Status;
use crate::terminal::{self, Style};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Deserialize)]
#[serde(untagged)]
enum Reply {
    Refused(Refusal),
    Status(Status),
}

/// Asks `role`, or the server and then the client, and prints each reply to stdout: drawn, cut
/// to stdout's width when it is a terminal, or with `json` as the line it came as. Succeeds when
/// any process answered; otherwise says why there may be none on stderr and fails.
pub fn run(role: Option<Role>, json: bool, style: Style) -> miette::Result<ExitCode> {
    let env = Env::current();
    let stdout = io::stdout();
    let width = rustix::termios::tcgetwinsize(&stdout).map_or(0, |size| usize::from(size.ws_col));
    // A link is an escape that a terminal shows as its text and a file keeps as it is.
    let style = Style {
        links: stdout.is_terminal(),
        ..style
    };
    let roles = role.map_or_else(|| vec![Role::Server, Role::Client], |role| vec![role]);
    let mut replies = Vec::new();
    let mut denied = false;
    for role in roles {
        for path in socket::candidates(role, &env) {
            match ask(&path) {
                Ok(line) => {
                    replies.push(if json {
                        line
                    } else {
                        draw(role, &line, style, width)
                    });
                    break;
                }
                Err(err) => match err.kind() {
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {}
                    io::ErrorKind::PermissionDenied => denied = true,
                    _ => {
                        let path = path.display();
                        tracing::warn!(%path, %err, "no answer on the status socket");
                    }
                },
            }
        }
    }
    if replies.is_empty() {
        eprintln!("{}", nothing_answered(denied));
        return Ok(ExitCode::FAILURE);
    }
    let between = if json { "\n" } else { "\n\n" };
    // One write, which a pipe takes whole even when its reader stops at the first line it wants.
    let text = replies.join(between) + "\n";
    stdout.lock().write_all(text.as_bytes()).into_diagnostic()?;
    Ok(ExitCode::SUCCESS)
}

/// The reply line of the process answering at `path`, without its newline. A process that has
/// not answered within [`REQUEST_WAIT`] is an error, and so is one that closes without a line.
fn ask(path: &Path) -> io::Result<String> {
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(REQUEST_WAIT))?;
    stream.set_write_timeout(Some(REQUEST_WAIT))?;
    let request = serde_json::to_string(&Request::Status).expect("a unit variant serializes");
    stream.write_all(format!("{request}\n").as_bytes())?;
    let mut line = String::new();
    if BufReader::new(stream).read_line(&mut line)? == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(line.trim_end().to_owned())
}

/// `line` as `role` answered it: its status drawn, or, from a process that runs another version
/// of hawse than this one, why it could not say.
fn draw(role: Role, line: &str, style: Style, width: usize) -> String {
    let reply = serde_json::from_str(line).unwrap_or_else(|err| {
        Reply::Refused(Refusal {
            error: format!("its reply does not read as a status: {err}"),
        })
    });
    match reply {
        Reply::Status(status) => terminal::status_lines(&status, style, width).join("\n"),
        Reply::Refused(Refusal { error }) => {
            let name = role.name();
            format!(
                "the {name} cannot answer this hawse: {error}\n\
                 restart the {name} after an upgrade, so that it runs hawse {VERSION} as this does"
            )
        }
    }
}

/// What to say when no process answered: that none did, which is not that none runs, and the
/// likeliest reason.
fn nothing_answered(denied: bool) -> String {
    let why = if denied {
        "run it as root, or as the user the service runs as"
    } else {
        "one started from a unit hawse 0.10.0 wrote has no socket until hawse service install is \
         run again"
    };
    format!("no hawse server or client answered here\n{why}")
}

#[cfg(test)]
mod tests {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt as _;
    use std::os::unix::net::UnixListener;

    use super::*;

    #[test]
    fn nothing_answered_says_what_to_do() {
        assert!(nothing_answered(true).contains("run it as root"));
        assert!(nothing_answered(false).contains("hawse service install"));
    }

    #[test]
    fn a_socket_out_of_reach_is_a_permission_denied() {
        // Root reaches any socket, whatever its mode.
        if crate::paths::is_root() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.sock");
        let _listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, Permissions::from_mode(0o000)).unwrap();
        let err = ask(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
    }
}
