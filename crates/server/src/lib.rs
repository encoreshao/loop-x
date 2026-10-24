//! HTTP server for LoopX (`loopx serve`): the dashboard backend and the API the
//! Mac app talks to.

pub mod api;
pub mod chat_tool;
pub mod lock;
pub mod oauth_routes;
pub mod snapshot;
pub mod ui;

use api::{router, AppState};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub root: PathBuf,
    /// 0 picks a free port.
    pub port: u16,
    /// Where `loopx.lock` goes (default: next to the config).
    pub lock_path: PathBuf,
}

impl ServeOptions {
    pub fn new(root: PathBuf, port: u16) -> Self {
        Self {
            root,
            port,
            lock_path: lock::default_path(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("cannot listen on 127.0.0.1:{0}: {1}")]
    Bind(u16, std::io::Error),
    #[error("{0}")]
    Lock(#[from] lock::AcquireError),
    #[error("could not watch {0}: {1}")]
    Watch(PathBuf, notify::Error),
    #[error("server error: {0}")]
    Io(#[from] std::io::Error),
}

/// Recompute the snapshot whenever something under `outputs/` changes (and
/// every few seconds, to notice a dead pid or an expired approval), and
/// broadcast it when it differs from the last one.
fn spawn_watcher(state: Arc<AppState>) -> Result<notify::RecommendedWatcher, ServeError> {
    use notify::{RecursiveMode, Watcher};
    let outputs = state.paths.outputs.clone();
    std::fs::create_dir_all(&outputs)?;
    let (ntx, mut nrx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok() {
            let _ = ntx.send(());
        }
    })
    .map_err(|e| ServeError::Watch(outputs.clone(), e))?;
    watcher
        .watch(&outputs, RecursiveMode::Recursive)
        .map_err(|e| ServeError::Watch(outputs.clone(), e))?;
    let events_dir = state.paths.events_dir.clone();
    if !events_dir.starts_with(&outputs) && events_dir.exists() {
        let _ = watcher.watch(&events_dir, RecursiveMode::NonRecursive);
    }
    tokio::spawn(async move {
        let mut last = state.snapshot_text();
        loop {
            tokio::select! {
                got = nrx.recv() => {
                    if got.is_none() { return; }
                    // Debounce a burst of writes into one recompute.
                    tokio::time::sleep(Duration::from_millis(120)).await;
                    while nrx.try_recv().is_ok() {}
                }
                _ = tokio::time::sleep(Duration::from_secs(3)) => {}
            }
            let text = state.snapshot_text();
            if text != last {
                last = text.clone();
                let _ = state.tx.send(Arc::new(text));
            }
        }
    });
    Ok(watcher)
}

/// Serve on an already-bound listener until `shutdown` resolves. The lock file
/// is written (and removed on the way out) by the caller of [`serve`].
pub async fn run_on(
    listener: tokio::net::TcpListener,
    state: Arc<AppState>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ServeError> {
    let _watcher = spawn_watcher(state.clone())?;
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

async fn termination() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

/// `loopx serve`: take the lock and serve until interrupted. `on_ready` receives the bound port.
pub async fn serve(opts: ServeOptions, on_ready: impl FnOnce(u16)) -> Result<(), ServeError> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", opts.port))
        .await
        .map_err(|e| ServeError::Bind(opts.port, e))?;
    let port = listener.local_addr()?.port();
    let token = lock::new_token();
    let info = lock::LockInfo::new(port, loopx_core::VERSION, &token);
    let _lock = lock::acquire(&opts.lock_path, &info)?;
    lock::spawn_keeper(opts.lock_path.clone(), info.clone());

    let state = AppState::new(opts.root.clone(), token);
    on_ready(port);
    run_on(listener, state, termination()).await
}

pub fn crate_name() -> &'static str {
    "loopx-server"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_name_is_stable() {
        assert_eq!(super::crate_name(), "loopx-server");
    }

    #[tokio::test]
    async fn a_status_change_reaches_an_open_event_stream() {
        let d = tempfile::tempdir().unwrap();
        let state = AppState::new(d.path().to_path_buf(), "t".into());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(run_on(listener, state, async move {
            let _ = stop_rx.await;
        }));

        let outputs = d.path().join("outputs");
        let reader = tokio::task::spawn_blocking(move || {
            let resp = ureq::get(&format!("http://127.0.0.1:{port}/api/events"))
                .timeout(Duration::from_secs(20))
                .call()
                .unwrap();
            let mut lines = std::io::BufRead::lines(std::io::BufReader::new(resp.into_reader()));
            let mut states = Vec::new();
            while let Some(Ok(line)) = lines.next() {
                if let Some(data) = line.strip_prefix("data: ") {
                    let v: serde_json::Value = serde_json::from_str(data).unwrap();
                    states.push(
                        v["loops"]["gitlab-loop"]["state"]
                            .as_str()
                            .map(str::to_owned),
                    );
                    if states.len() == 2 {
                        break;
                    }
                }
            }
            states
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        loopx_core::status::write_status(&outputs.join("status.json"), "idle", &[]).unwrap();
        let states = tokio::time::timeout(Duration::from_secs(15), reader)
            .await
            .expect("an update within 15s")
            .unwrap();
        assert_eq!(states, vec![None, Some("idle".to_string())]);
        let _ = stop_tx.send(());
        let _ = server.await;
    }
}
