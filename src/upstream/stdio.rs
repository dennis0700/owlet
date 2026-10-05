use std::{
    collections::HashMap, path::Path, process::ExitStatus, process::Stdio, sync::Arc,
    time::Duration,
};

use anyhow::Context;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
    sync::mpsc,
};
use tracing::{debug, info, warn};

use super::{Reader, Wiring};

const KILL_GRACE: Duration = Duration::from_secs(3);

/// Spawns the process and its IO tasks; returns the pid.
pub fn start(
    name: &str,
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
    cwd: Option<&Path>,
    wiring: Wiring,
) -> anyhow::Result<Option<u32>> {
    let mut cmd = Command::new(command);
    cmd.args(args)
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    // Own process group so `npx`/`uvx` wrappers and their children die together.
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd.spawn().with_context(|| format!("spawn `{command}`"))?;
    let stdin = child.stdin.take().context("child stdin unavailable")?;
    let stdout = child.stdout.take().context("child stdout unavailable")?;
    let stderr = child.stderr.take().context("child stderr unavailable")?;
    let pid = child.id();
    info!(server = %name, pid, "spawned `{command}`");

    let Wiring {
        reader,
        outgoing,
        stop,
        exited,
    } = wiring;
    let inner = Arc::clone(&reader.inner);

    tokio::spawn(write_loop(stdin, outgoing));
    tokio::spawn(read_loop(reader, stdout));
    tokio::spawn(stderr_loop(name.to_owned(), stderr));
    tokio::spawn(async move {
        let status = tokio::select! {
            status = child.wait() => {
                // Leader exited on its own; reap leftovers in its group.
                signal_group(pid, true);
                status
            }
            // Fires on explicit shutdown and when the handle is dropped.
            _ = stop => terminate(&mut child, pid).await,
        };
        inner.mark_dead();
        match status {
            Ok(status) => info!(server = %inner.name, pid, "exited: {status}"),
            Err(e) => warn!(server = %inner.name, pid, "wait failed: {e}"),
        }
        let _ = exited.send(true);
    });
    Ok(pid)
}

async fn write_loop(mut stdin: ChildStdin, mut rx: mpsc::UnboundedReceiver<String>) {
    while let Some(mut line) = rx.recv().await {
        line.push('\n');
        if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
            break;
        }
    }
}

async fn read_loop(reader: Reader, stdout: ChildStdout) {
    let mut lines = BufReader::new(stdout).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => reader.dispatch_line(&line),
            Ok(None) => break,
            Err(e) => {
                warn!(server = %reader.inner.name, "stdout read error: {e}");
                break;
            }
        }
    }
    reader.inner.mark_dead();
}

async fn stderr_loop(name: String, stderr: ChildStderr) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        debug!(server = %name, "stderr: {line}");
    }
}

async fn terminate(child: &mut Child, pid: Option<u32>) -> std::io::Result<ExitStatus> {
    signal_group(pid, false);
    match tokio::time::timeout(KILL_GRACE, child.wait()).await {
        Ok(status) => {
            signal_group(pid, true);
            status
        }
        Err(_) => {
            signal_group(pid, true);
            let _ = child.start_kill();
            child.wait().await
        }
    }
}

#[cfg(unix)]
fn signal_group(pid: Option<u32>, force: bool) {
    use nix::{
        sys::signal::{Signal, killpg},
        unistd::Pid,
    };
    let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()) else {
        return;
    };
    let sig = if force {
        Signal::SIGKILL
    } else {
        Signal::SIGTERM
    };
    // ESRCH just means the group is already gone.
    let _ = killpg(Pid::from_raw(pid), sig);
}

#[cfg(not(unix))]
fn signal_group(_pid: Option<u32>, _force: bool) {}
