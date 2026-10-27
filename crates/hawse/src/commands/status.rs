//! `hawse status`: asks the server and the client running on this host how they stand, over
//! the sockets they answer on, and prints what they say, once or, with `--watch`, every second.

use std::collections::BTreeMap;
use std::io::{self, BufRead as _, BufReader, IsTerminal as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use miette::IntoDiagnostic as _;
use serde::Deserialize;
use tokio::time::MissedTickBehavior;

use crate::panel::per_second;
use crate::paths::Role;
use crate::screen;
use crate::socket::{self, Env, REQUEST_WAIT, Refusal, Request};
use crate::status::{ClientStatus, Status};
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
    let stdout = io::stdout();
    let width = rustix::termios::tcgetwinsize(&stdout).map_or(0, |size| usize::from(size.ws_col));
    // A link is an escape that a terminal shows as its text and a file keeps as it is.
    let style = Style {
        links: stdout.is_terminal(),
        ..style
    };
    let (answers, denied) = ask_each(role);
    let replies: Vec<String> = answers
        .into_iter()
        .map(|(role, line)| {
            if json {
                line
            } else {
                draw(role, &line, None, style, width)
            }
        })
        .collect();
    if replies.is_empty() {
        eprintln!("{}", nothing_answered(!denied.is_empty()));
        return Ok(ExitCode::FAILURE);
    }
    for role in denied {
        eprintln!("{}", denied_alongside_answer(role));
    }
    let between = if json { "\n" } else { "\n\n" };
    // One write, which a pipe takes whole even when its reader stops at the first line it wants.
    let text = replies.join(between) + "\n";
    stdout.lock().write_all(text.as_bytes()).into_diagnostic()?;
    Ok(ExitCode::SUCCESS)
}

/// Asks as [`run`] does once a second and redraws what is said in place on stderr, as the panel
/// is drawn, until Ctrl-C. A client's services show their bytes per second since the client last
/// answered. A role that does not answer is left out, and when none does the frame says so and
/// the asking goes on.
pub async fn watch(role: Option<Role>, style: Style) -> ExitCode {
    let mut sampled: Option<(Instant, ClientStatus)> = None;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let interrupted = tokio::signal::ctrl_c();
    tokio::pin!(interrupted);
    loop {
        tokio::select! {
            _ = &mut interrupted => break,
            _ = tick.tick() => {}
        }
        // shortcut: asked on this task, so a hung process holds off Ctrl-C for up to
        // REQUEST_WAIT; ask from a blocking task if that bites.
        let (answers, denied) = ask_each(role);
        let asked = Instant::now();
        let (width, height) = screen::size();
        let mut blocks = Vec::new();
        for (role, line) in answers {
            let rates = match serde_json::from_str(&line) {
                Ok(Status::Client(now)) => {
                    let rates = sampled
                        .as_ref()
                        .map(|(at, then)| rates(then, &now, asked - *at))
                        .unwrap_or_default();
                    sampled = Some((asked, now));
                    Some(rates)
                }
                _ => None,
            };
            blocks.push(draw(role, &line, rates.as_ref(), style, width));
        }
        if blocks.is_empty() {
            blocks.push(nothing_answered(!denied.is_empty()));
        }
        blocks.extend(denied.into_iter().map(denied_alongside_answer));
        let lines = blocks.join("\n\n").split('\n').map(str::to_owned).collect();
        screen::draw(
            terminal::fold(lines, height, style, width.saturating_sub(1)),
            width,
        );
    }
    screen::release();
    ExitCode::SUCCESS
}

/// Each of `now`'s services' bytes per second each way since `then`, `elapsed` before, by name.
/// A service `then` lacks reads as none, and so does every service when `now` has run for less
/// time than `then`: it is a client started again, whose totals count from nothing.
fn rates(
    then: &ClientStatus,
    now: &ClientStatus,
    elapsed: Duration,
) -> BTreeMap<String, (u64, u64)> {
    now.services
        .iter()
        .filter(|_| now.up_s >= then.up_s)
        .filter_map(|service| {
            let before = then.services.iter().find(|s| s.name == service.name)?;
            let down = per_second(service.down, before.down, elapsed);
            let up = per_second(service.up, before.up, elapsed);
            Some((service.name.clone(), (down, up)))
        })
        .collect()
}

/// Asks `role`, or the server and then the client: the reply line of each that answered, with
/// its role, and the roles that did not answer because their socket refused permission.
fn ask_each(role: Option<Role>) -> (Vec<(Role, String)>, Vec<Role>) {
    let env = Env::current();
    let roles = role.map_or_else(|| vec![Role::Server, Role::Client], |role| vec![role]);
    let mut replies = Vec::new();
    let mut denied = Vec::new();
    for role in roles {
        let mut answered = false;
        let mut refused = false;
        for path in socket::candidates(role, &env) {
            match ask(&path) {
                Ok(line) => {
                    replies.push((role, line));
                    answered = true;
                    break;
                }
                Err(err) => match err.kind() {
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {}
                    io::ErrorKind::PermissionDenied => refused = true,
                    _ => {
                        let path = path.display();
                        tracing::warn!(%path, %err, "no answer on the status socket");
                    }
                },
            }
        }
        if refused && !answered {
            denied.push(role);
        }
    }
    (replies, denied)
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

/// `line` as `role` answered it: its status drawn, with `rates` as
/// [`terminal::status_lines`] takes them, or, from a process that runs another version of hawse
/// than this one, why it could not say.
fn draw(
    role: Role,
    line: &str,
    rates: Option<&BTreeMap<String, (u64, u64)>>,
    style: Style,
    width: usize,
) -> String {
    let reply = serde_json::from_str(line).unwrap_or_else(|err| {
        Reply::Refused(Refusal {
            error: format!("its reply does not read as a status: {err}"),
        })
    });
    match reply {
        Reply::Status(status) => terminal::status_lines(&status, rates, style, width).join("\n"),
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
        "one started from a unit hawse 0.10.0 wrote has no socket until `sudo hawse service \
         install` is run again, with the --config and --user it was installed with"
    };
    format!("no hawse server or client answered here\n{why}")
}

/// What to say on stderr when `role`'s socket refused permission while the other role answered.
fn denied_alongside_answer(role: Role) -> String {
    format!(
        "the hawse {} here: permission denied; run it as root, or as the user the service runs as",
        role.name()
    )
}

#[cfg(test)]
mod tests {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt as _;
    use std::os::unix::net::UnixListener;

    use super::*;
    use crate::status::{Answer, LinkStatus, ServiceStatus};

    fn client(services: &[(&str, u64, u64)]) -> ClientStatus {
        let service = |&(name, down, up): &(&str, u64, u64)| ServiceStatus {
            name: name.to_owned(),
            local: String::new(),
            port: String::new(),
            answer: Answer::Pending,
            open: 0,
            visitors: 0,
            down,
            up,
            datagrams_down: 0,
            datagrams_up: 0,
            dropped: 0,
        };
        ClientStatus {
            version: VERSION.to_owned(),
            server: String::new(),
            up_s: 0,
            link: LinkStatus::Connecting,
            services: services.iter().map(service).collect(),
        }
    }

    #[test]
    fn a_rate_counts_from_the_last_answer_and_a_fall_or_a_new_service_reads_as_none() {
        let then = client(&[("web", 1000, 500), ("db", 9000, 0)]);
        let now = client(&[("web", 3000, 1500), ("db", 100, 0), ("new", 4000, 0)]);
        assert_eq!(
            rates(&then, &now, Duration::from_secs(2)),
            BTreeMap::from([("web".to_owned(), (1000, 500)), ("db".to_owned(), (0, 0)),])
        );
    }

    #[test]
    fn a_client_started_again_has_no_rates_even_past_the_last_total() {
        let mut then = client(&[("web", 1000, 0)]);
        then.up_s = 600;
        let restarted = client(&[("web", 3000, 0)]);
        assert!(rates(&then, &restarted, Duration::from_secs(1)).is_empty());
    }

    #[test]
    fn nothing_answered_says_what_to_do() {
        assert!(nothing_answered(true).contains("run it as root"));
        assert!(nothing_answered(false).contains("hawse service install"));
        assert!(nothing_answered(false).contains("--config and --user"));
    }

    #[test]
    fn a_denied_role_is_named_beside_an_answer() {
        let hint = denied_alongside_answer(Role::Client);
        assert!(
            hint.starts_with("the hawse client here: permission denied"),
            "{hint}"
        );
        assert!(hint.contains("run it as root"), "{hint}");
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
