pub mod check;
pub mod client;
pub mod keygen;
pub mod server;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use notify::{RecommendedWatcher, RecursiveMode, Watcher as _};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Resolves on SIGINT or SIGTERM and cancels the token.
pub async fn shutdown_signal(cancel: CancellationToken) {
    let mut term = signal(SignalKind::terminate()).expect("sigterm handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
    cancel.cancel();
}

/// How long a changed config has to hold still before it is read: a save is often several writes.
const SETTLE: Duration = Duration::from_millis(250);

type Stamp = (SystemTime, u64);

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// When to read the config again: on SIGHUP, and when the file has changed.
pub struct Reloads {
    hup: Signal,
    path: PathBuf,
    read: Option<Stamp>,
    /// One for anything that happens in the config's directory, sent from the watcher's thread.
    events: mpsc::Receiver<()>,
    /// Held for its watch, which ends when it drops.
    _watcher: Option<RecommendedWatcher>,
}

impl Reloads {
    /// The directory is watched and not the file: a save that writes a new file and renames it
    /// over the old one would leave a watch on the file behind with the old one. Where the
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
            hup: signal(SignalKind::hangup()).expect("sighup handler"),
            read: stamp(&path),
            path,
            events,
            _watcher: watching.ok(),
        }
    }

    /// A file that has gone is not read: a save that removes it and writes it anew would
    /// otherwise reload a server with no config, which authorizes nobody.
    pub async fn next(&mut self) {
        loop {
            tokio::select! {
                _ = self.hup.recv() => break,
                Some(()) = self.events.recv() => {
                    while let Ok(Some(())) = tokio::time::timeout(SETTLE, self.events.recv()).await {}
                    let now = stamp(&self.path);
                    if now.is_some() && now != self.read {
                        break;
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
