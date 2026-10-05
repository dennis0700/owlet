use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::Context;
use owlet::{Config, Hub};
use parking_lot::Mutex;
use serde::Serialize;
use tauri::async_runtime::{JoinHandle, spawn};
use tokio::{net::TcpListener, sync::oneshot, time};

/// How long [`ServerState::stop`] waits for a graceful shutdown before aborting.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Called after every status change.
type Notify = Arc<dyn Fn(&ServerStatus) + Send + Sync>;

/// Lifecycle of the embedded owlet server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ServerStatus {
    Stopped,
    Starting,
    /// Accepting connections on `listen`.
    Running {
        listen: String,
    },
    /// Could not start, or stopped unexpectedly.
    Failed {
        error: String,
    },
}

struct Inner {
    status: ServerStatus,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

/// Owns the owlet server running inside the UI process.
pub struct ServerState {
    inner: Arc<Mutex<Inner>>,
    notify: Notify,
}

impl ServerState {
    /// Creates a stopped server; `notify` is called on every status change.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let server = ServerState::new(|status| println!("{status:?}"));
    /// ```
    pub fn new(notify: impl Fn(&ServerStatus) + Send + Sync + 'static) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                status: ServerStatus::Stopped,
                stop: None,
                task: None,
            })),
            notify: Arc::new(notify),
        }
    }

    /// Current status.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let status = server.status();
    /// ```
    pub fn status(&self) -> ServerStatus {
        self.inner.lock().status.clone()
    }

    /// Starts the server on the config file at `path` unless it is already
    /// starting or running. Failures are reported through the status.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// server.start(PathBuf::from("owlet.toml"));
    /// ```
    pub fn start(&self, path: PathBuf) -> ServerStatus {
        let (stop_tx, stop_rx) = oneshot::channel();
        {
            let mut inner = self.inner.lock();
            if matches!(
                inner.status,
                ServerStatus::Starting | ServerStatus::Running { .. }
            ) {
                return inner.status.clone();
            }
            inner.status = ServerStatus::Starting;
            inner.stop = Some(stop_tx);
        }
        (self.notify)(&ServerStatus::Starting);

        let inner = Arc::clone(&self.inner);
        let notify = Arc::clone(&self.notify);
        let task = spawn(async move {
            let last = match serve(path, &inner, &notify, stop_rx).await {
                Ok(()) => ServerStatus::Stopped,
                Err(e) => ServerStatus::Failed {
                    error: format!("{e:#}"),
                },
            };
            set_status(&inner, &notify, last);
        });
        self.inner.lock().task = Some(task);
        self.status()
    }

    /// Stops the server and its upstream processes; aborts after a timeout.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// server.stop().await;
    /// ```
    pub async fn stop(&self) {
        let (stop, task) = {
            let mut inner = self.inner.lock();
            (inner.stop.take(), inner.task.take())
        };
        if let Some(stop) = stop {
            let _ = stop.send(());
        }
        let Some(mut task) = task else {
            return;
        };
        if time::timeout(STOP_TIMEOUT, &mut task).await.is_err() {
            task.abort();
            set_status(&self.inner, &self.notify, ServerStatus::Stopped);
        }
    }
}

fn set_status(inner: &Mutex<Inner>, notify: &Notify, status: ServerStatus) {
    inner.lock().status = status.clone();
    notify(&status);
}

async fn serve(
    path: PathBuf,
    inner: &Mutex<Inner>,
    notify: &Notify,
    stop: oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    let cfg = Config::load(&path)?;
    let listener = TcpListener::bind(cfg.listen)
        .await
        .with_context(|| format!("bind {}", cfg.listen))?;
    let listen = listener.local_addr().context("read listen address")?;

    let min_idle = cfg
        .servers
        .values()
        .filter_map(|s| s.idle_timeout)
        .fold(cfg.idle_timeout, u64::min);
    let reap_every = Duration::from_secs((min_idle / 2).clamp(1, 15));
    let hub = Arc::new(Hub::new(&cfg));
    let reaper = spawn(Arc::clone(&hub).reap_loop(reap_every));

    set_status(
        inner,
        notify,
        ServerStatus::Running {
            listen: listen.to_string(),
        },
    );
    let result = axum::serve(listener, owlet::router(Arc::clone(&hub), cfg.token, path))
        .with_graceful_shutdown(async {
            let _ = stop.await;
        })
        .await;

    reaper.abort();
    hub.shutdown().await;
    result.context("serve")
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn config(listen: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("create temp config");
        writeln!(file, "listen = \"{listen}\"").expect("write temp config");
        file
    }

    async fn wait_for(server: &ServerState, done: impl Fn(&ServerStatus) -> bool) -> ServerStatus {
        for _ in 0..250 {
            let status = server.status();
            if done(&status) {
                return status;
            }
            time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timed out, last status: {:?}", server.status());
    }

    #[tokio::test]
    async fn runs_then_stops() {
        let file = config("127.0.0.1:0");
        let events = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&events);
        let server = ServerState::new(move |s| log.lock().push(s.clone()));

        server.start(file.path().to_path_buf());
        let status = wait_for(&server, |s| matches!(s, ServerStatus::Running { .. })).await;
        let ServerStatus::Running { listen } = status else {
            panic!("not running");
        };
        assert!(listen.starts_with("127.0.0.1:"));
        assert!(
            tokio::net::TcpStream::connect(&listen).await.is_ok(),
            "port must accept connections"
        );

        server.stop().await;
        wait_for(&server, |s| *s == ServerStatus::Stopped).await;
        assert_eq!(events.lock().first(), Some(&ServerStatus::Starting));
        assert_eq!(events.lock().last(), Some(&ServerStatus::Stopped));
    }

    #[tokio::test]
    async fn restarts_after_stop() {
        let file = config("127.0.0.1:0");
        let server = ServerState::new(|_| {});
        for _ in 0..2 {
            server.start(file.path().to_path_buf());
            wait_for(&server, |s| matches!(s, ServerStatus::Running { .. })).await;
            server.stop().await;
            wait_for(&server, |s| *s == ServerStatus::Stopped).await;
        }
    }

    #[tokio::test]
    async fn reports_bind_failure() {
        let taken = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe");
        let addr = taken.local_addr().expect("probe address");
        let file = config(&addr.to_string());
        let server = ServerState::new(|_| {});

        server.start(file.path().to_path_buf());
        let status = wait_for(&server, |s| matches!(s, ServerStatus::Failed { .. })).await;
        let ServerStatus::Failed { error } = status else {
            panic!("not failed");
        };
        assert!(error.contains("bind"), "unexpected error: {error}");
    }

    #[tokio::test]
    async fn reports_missing_config() {
        let server = ServerState::new(|_| {});
        server.start(PathBuf::from("/nonexistent/owlet.toml"));
        wait_for(&server, |s| matches!(s, ServerStatus::Failed { .. })).await;
    }
}
