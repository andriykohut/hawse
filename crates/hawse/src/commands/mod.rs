pub mod check;
pub mod client;
pub mod join;
pub mod keygen;
pub mod server;

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use notify::{RecommendedWatcher, RecursiveMode, Watcher as _};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Whether hawse runs on a terminal, where a SIGHUP says the terminal has gone and ends it, as it
/// ends anything run on one. Elsewhere it is the request to reload that a unit sends.
// ponytail: stdin and stderr both, since `nohup` leaves stdin on the terminal and moves stderr
// off it. So `hawse client 2> log` reloads on a hangup and outlives its window. Asking whether
// SIGHUP came in ignored would tell the two apart, and that is `sigaction`, which is unsafe.
fn on_a_terminal() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Resolves on SIGINT or SIGTERM, and on a terminal on SIGHUP, and cancels the token. The
/// handlers are in place when this returns and not when the future first runs, so a signal sent
/// once the caller has gone on is one hawse answers.
pub fn shutdown_signal(cancel: CancellationToken) -> impl Future<Output = ()> {
    let mut term = signal(SignalKind::terminate()).expect("sigterm handler");
    let mut hup = on_a_terminal().then(|| signal(SignalKind::hangup()).expect("sighup handler"));
    async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
            Some(()) = async { hup.as_mut()?.recv().await } => {}
        }
        tracing::info!("shutting down");
        cancel.cancel();
    }
}

/// How long a changed config has to hold still before it is read: a save is often several writes.
const SETTLE: Duration = Duration::from_millis(250);

type Stamp = (SystemTime, u64);

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// The directory a config that is a link points into, when it is not the link's own: the config is
/// then edited there, where a watch on the link's directory sees nothing.
fn linked_dir(path: &Path, dir: &Path) -> Option<PathBuf> {
    let target = std::fs::canonicalize(path).ok()?;
    let target = target.parent()?;
    (std::fs::canonicalize(dir).ok()? != target).then(|| target.to_owned())
}

/// When to read the config again: when the file has changed, and off a terminal on SIGHUP.
pub struct Reloads {
    /// None on a terminal, where a SIGHUP stops hawse.
    hup: Option<Signal>,
    path: PathBuf,
    read: Option<Stamp>,
    /// One for anything that happens in a watched directory, sent from the watcher's thread.
    events: mpsc::Receiver<()>,
    /// Held for its watches, which end when it drops.
    _watcher: Option<RecommendedWatcher>,
}

impl Reloads {
    /// Made before the config is first read, so no change after that read goes unseen.
    ///
    /// The directory is watched and not the file: a save that writes a new file and renames it
    /// over the old one would leave a watch on the file behind with the old one. Where a
    /// directory cannot be watched, SIGHUP still reloads.
    pub fn new(path: PathBuf) -> Self {
        let (tx, events) = mpsc::channel(1);
        let dir = match path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir,
            _ => Path::new("."),
        };
        let watching = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            // Reading the config is itself an access. A full channel already holds the wakeup.
            if !event.is_ok_and(|event| event.kind.is_access()) {
                let _ = tx.try_send(());
            }
        })
        .and_then(|mut watcher| {
            watcher.watch(dir, RecursiveMode::NonRecursive)?;
            // Not a reason to give up the watch above, which still sees the link replaced.
            if let Some(target) = linked_dir(&path, dir)
                && let Err(err) = watcher.watch(&target, RecursiveMode::NonRecursive)
            {
                tracing::warn!(
                    dir = %target.display(),
                    %err,
                    "cannot watch the directory the config links into, so an edit there is read on SIGHUP only"
                );
            }
            Ok(watcher)
        });
        if let Err(err) = &watching {
            tracing::warn!(
                dir = %dir.display(),
                %err,
                "cannot watch the config's directory, so the config is read again on SIGHUP only"
            );
        }
        Self {
            hup: (!on_a_terminal()).then(|| signal(SignalKind::hangup()).expect("sighup handler")),
            read: stamp(&path),
            path,
            events,
            _watcher: watching.ok(),
        }
    }

    /// An event only says when to look at the file. It is read once it differs from what was
    /// last read and has looked the same for `SETTLE`, whatever else goes on in its directory.
    ///
    /// A file that has gone is not read: a save that removes it and writes it anew would
    /// otherwise reload a server with no config, which authorizes nobody.
    pub async fn next(&mut self) {
        // When to look at the file next, and what it looked like at the last look.
        let mut look: Option<(Instant, Option<Stamp>)> = None;
        loop {
            let at = look.map_or_else(Instant::now, |(at, _)| at);
            tokio::select! {
                Some(()) = async { self.hup.as_mut()?.recv().await } => break,
                // Left waiting while a look is due: it would say nothing that look does not.
                Some(()) = self.events.recv(), if look.is_none() => {
                    look = Some((Instant::now() + SETTLE, stamp(&self.path)));
                }
                () = tokio::time::sleep_until(at), if look.is_some() => {
                    let now = stamp(&self.path);
                    if now.is_none() || now == self.read {
                        look = None;
                    } else if look.is_some_and(|(_, last)| last == now) {
                        break;
                    } else {
                        look = Some((Instant::now() + SETTLE, now));
                    }
                }
            }
        }
        self.read = stamp(&self.path);
    }
}

/// A config that does not load leaves the running one in force.
pub fn not_reloaded(report: &miette::Report) {
    tracing::error!(
        "config not reloaded, and the running one stays in force until it loads\n{report:?}"
    );
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    async fn seen(reloads: &mut Reloads, what: &str) {
        tokio::time::timeout(Duration::from_secs(5), reloads.next())
            .await
            .unwrap_or_else(|_| panic!("{what} was not seen within 5 s"));
    }

    #[tokio::test]
    async fn an_edit_where_a_linked_config_points_is_seen() {
        let (links, targets) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let target = targets.path().join("client.toml");
        let link = links.path().join("client.toml");
        fs::write(&target, "before").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let mut reloads = Reloads::new(link);

        fs::write(&target, "after, and longer").unwrap();
        seen(&mut reloads, "an edit to the file the config links to").await;
    }

    #[tokio::test]
    async fn a_directory_that_is_never_quiet_does_not_hold_a_reload_back() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("server.toml");
        fs::write(&config, "before").unwrap();
        let mut reloads = Reloads::new(config.clone());
        let noise = dir.path().join("noise");
        let busy = tokio::spawn(async move {
            loop {
                fs::write(&noise, "noise").unwrap();
                tokio::time::sleep(SETTLE / 5).await;
            }
        });
        // Long enough for the noise alone to have started a wait.
        tokio::time::sleep(SETTLE).await;

        fs::write(&config, "after, and longer").unwrap();
        seen(&mut reloads, "an edit to a config in a busy directory").await;
        busy.abort();
    }
}
