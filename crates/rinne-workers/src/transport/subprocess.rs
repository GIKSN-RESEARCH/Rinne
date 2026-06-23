//! The `subprocess-json` transport (`CONTEXT.md` §8, §14).
//!
//! Spawns a harness CLI with piped stdio, streams its stdout lines to the event
//! sink as they arrive, captures the full output for the adapter to parse, and
//! enforces a timeout plus cancellation. Adapters build a [`SubprocessSpec`],
//! call [`run`], then map the captured stdout into an `ExecuteResult`.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use rinne_core::worker::{emit, EventSink, ExecStatus, WorkerEvent};
use rinne_core::{Result, RinneError};

/// How to invoke a subprocess worker.
pub struct SubprocessSpec {
    pub program: String,
    pub args: Vec<String>,
    /// Working directory — the repo the worker operates in.
    pub workspace: PathBuf,
    /// Optional text piped to the child's stdin (some CLIs take the prompt this
    /// way); `None` closes stdin.
    pub stdin: Option<String>,
    /// Hard wall-clock timeout. Always set one for beta CLIs (`CONTEXT.md` §21).
    pub timeout: Option<Duration>,
}

/// The captured result of a subprocess run.
pub struct SubprocessOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub status: ExecStatus,
    pub wall_ms: u64,
}

/// Map a raw stdout line to a streamed event. Adapters pass their own mapper to
/// turn structured CLI output into rich events; the default wraps the line raw.
pub type LineMapper = fn(&str) -> Option<WorkerEvent>;

/// Run a subprocess, streaming stdout lines via `mapper` and capturing output.
pub async fn run(
    spec: SubprocessSpec,
    events: &EventSink,
    cancel: &CancellationToken,
    mapper: LineMapper,
) -> Result<SubprocessOutput> {
    let started = Instant::now();

    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .current_dir(&spec.workspace)
        .stdin(if spec.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| RinneError::Worker(format!("failed to spawn {}: {e}", spec.program)))?;

    if let Some(input) = &spec.stdin {
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(input.as_bytes())
                .await
                .map_err(|e| RinneError::Worker(format!("stdin write failed: {e}")))?;
            // Drop closes the pipe so the child sees EOF.
        }
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| RinneError::Worker("no stdout pipe".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| RinneError::Worker("no stderr pipe".into()))?;

    // Drain stderr concurrently so a chatty child can't deadlock on a full pipe.
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        let mut rdr = BufReader::new(stderr);
        let _ = rdr.read_to_string(&mut buf).await;
        buf
    });

    let mut lines = BufReader::new(stdout).lines();
    let mut captured = String::new();

    // A far-future deadline stands in when no timeout is configured, so the
    // select arm is always well-formed.
    let deadline = spec
        .timeout
        .map(|d| Instant::now() + d)
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(60 * 60 * 24 * 365));
    let sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
    tokio::pin!(sleep);

    let mut terminal: Option<ExecStatus> = None;

    loop {
        tokio::select! {
            line = lines.next_line() => {
                match line {
                    Ok(Some(l)) => {
                        captured.push_str(&l);
                        captured.push('\n');
                        if let Some(ev) = mapper(&l) {
                            emit(events, ev);
                        }
                    }
                    Ok(None) => break, // EOF: process closed stdout
                    Err(e) => {
                        terminal = Some(ExecStatus::Failed(format!("stdout read error: {e}")));
                        break;
                    }
                }
            }
            _ = cancel.cancelled() => {
                let _ = child.start_kill();
                terminal = Some(ExecStatus::Cancelled);
                break;
            }
            _ = &mut sleep => {
                let _ = child.start_kill();
                terminal = Some(ExecStatus::TimedOut);
                break;
            }
        }
    }

    let wait_status = child
        .wait()
        .await
        .map_err(|e| RinneError::Worker(format!("wait failed: {e}")))?;
    let stderr_str = stderr_task.await.unwrap_or_default();
    let exit_code = wait_status.code();

    let status = terminal.unwrap_or_else(|| {
        if wait_status.success() {
            ExecStatus::Success
        } else {
            let code = exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into());
            ExecStatus::Failed(format!("exited {code}"))
        }
    });

    emit(events, WorkerEvent::Done);

    Ok(SubprocessOutput {
        stdout: captured,
        stderr: stderr_str,
        exit_code,
        status,
        wall_ms: started.elapsed().as_millis() as u64,
    })
}

/// The default line mapper: surface each line raw to the stream pane.
pub fn raw_lines(line: &str) -> Option<WorkerEvent> {
    if line.trim().is_empty() {
        None
    } else {
        Some(WorkerEvent::Raw(line.to_string()))
    }
}
