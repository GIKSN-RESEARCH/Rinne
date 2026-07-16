//! PTY-backed harness transport for visible Harness Stage sessions (`plan.md`).
//!
//! Spawns the same harness CLI as the headless subprocess transport, but under a
//! pseudo-TTY so tools that check for interactive terminals behave more like
//! their native UX. Output is still line-parsed via [`LineMapper`] for the loop.

use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use tokio_util::sync::CancellationToken;

use rinne_core::worker::{emit, EventSink, ExecStatus, WorkerEvent};
use rinne_core::Result;

use super::subprocess::{LineMapper, SubprocessOutput, SubprocessSpec};

const MAX_CAPTURE_BYTES: usize = 8 * 1024 * 1024;

enum PtyMsg {
    Chunk(Vec<u8>),
    Exit { success: bool, timed_out: bool },
    Err(String),
}

/// Run `spec` under a PTY, streaming lines through `mapper` and capturing output.
pub async fn run(
    spec: SubprocessSpec,
    events: &EventSink,
    cancel: &CancellationToken,
    mapper: LineMapper,
) -> Result<SubprocessOutput> {
    let started = Instant::now();
    let (tx, rx) = mpsc::channel::<PtyMsg>();

    let program = spec.program.clone();
    let args = spec.args.clone();
    let workspace = spec.workspace.clone();
    let stdin = spec.stdin.clone();
    let env = spec.env.clone();
    let timeout = spec.timeout;

    let join = tokio::task::spawn_blocking(move || {
        run_pty_blocking(program, args, workspace, stdin, env, timeout, tx)
    });

    let mut captured = String::new();
    let mut truncated = false;
    let mut line_buf = String::new();
    let mut terminal: Option<ExecStatus> = None;
    let mut exit_success = false;

    loop {
        if cancel.is_cancelled() {
            terminal = Some(ExecStatus::Cancelled);
            break;
        }
        match rx.recv_timeout(Duration::from_millis(40)) {
            Ok(PtyMsg::Chunk(bytes)) => {
                let chunk = String::from_utf8_lossy(&bytes);
                for ch in chunk.chars() {
                    if ch == '\n' {
                        flush_line(
                            &mut line_buf,
                            &mut captured,
                            &mut truncated,
                            mapper,
                            events,
                        );
                    } else if ch != '\r' {
                        line_buf.push(ch);
                    }
                }
            }
            Ok(PtyMsg::Exit {
                success,
                timed_out,
            }) => {
                if !line_buf.is_empty() {
                    flush_line(
                        &mut line_buf,
                        &mut captured,
                        &mut truncated,
                        mapper,
                        events,
                    );
                }
                exit_success = success;
                if timed_out {
                    terminal = Some(ExecStatus::TimedOut);
                }
                break;
            }
            Ok(PtyMsg::Err(e)) => {
                terminal = Some(ExecStatus::Failed(e));
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if join.is_finished() {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // Ensure the blocking task finishes.
    let _ = join.await;

    let status = terminal.unwrap_or_else(|| {
        if exit_success {
            ExecStatus::Success
        } else {
            ExecStatus::Failed("exited non-zero".into())
        }
    });

    emit(events, WorkerEvent::Done);
    Ok(SubprocessOutput {
        stdout: captured,
        stderr: String::new(),
        exit_code: if exit_success { Some(0) } else { Some(1) },
        status,
        wall_ms: started.elapsed().as_millis() as u64,
    })
}

fn flush_line(
    line_buf: &mut String,
    captured: &mut String,
    truncated: &mut bool,
    mapper: LineMapper,
    events: &EventSink,
) {
    let line = std::mem::take(line_buf);
    if captured.len() < MAX_CAPTURE_BYTES {
        captured.push_str(&line);
        captured.push('\n');
    } else if !*truncated {
        *truncated = true;
        captured.push_str("\n…[output truncated]\n");
    }
    for ev in mapper(&line) {
        emit(events, ev);
    }
}

fn run_pty_blocking(
    program: String,
    args: Vec<String>,
    workspace: std::path::PathBuf,
    stdin: Option<String>,
    env: Vec<(String, String)>,
    timeout: Option<Duration>,
    tx: mpsc::Sender<PtyMsg>,
) {
    let pty_system = native_pty_system();
    let pair = match pty_system.openpty(PtySize {
        rows: 40,
        cols: 120,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(e) => {
            let _ = tx.send(PtyMsg::Err(format!("openpty failed: {e}")));
            return;
        }
    };

    let mut cmd = CommandBuilder::new(&program);
    cmd.args(args.iter().map(|s| s.as_str()));
    cmd.cwd(&workspace);
    for (k, v) in &env {
        cmd.env(k, v);
    }

    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(PtyMsg::Err(format!("failed to spawn {program}: {e}")));
            return;
        }
    };
    drop(pair.slave);

    if let Some(input) = stdin {
        match pair.master.take_writer() {
            Ok(mut writer) => {
                let _ = writer.write_all(input.as_bytes());
                drop(writer);
            }
            Err(e) => {
                let _ = tx.send(PtyMsg::Err(format!("pty stdin: {e}")));
                let _ = child.kill();
                return;
            }
        }
    }

    let mut reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => {
            let _ = tx.send(PtyMsg::Err(format!("pty reader: {e}")));
            let _ = child.kill();
            return;
        }
    };

    let deadline = timeout.map(|d| Instant::now() + d);
    let mut buf = [0u8; 4096];
    let mut timed_out = false;
    loop {
        if let Some(dl) = deadline {
            if Instant::now() >= dl {
                timed_out = true;
                let _ = child.kill();
                break;
            }
        }
        // Non-blocking-ish: portable-pty reader may block; rely on timeout kill
        // from another approach — for now read with short lifetime via blocking.
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if tx.send(PtyMsg::Chunk(buf[..n].to_vec())).is_err() {
                    let _ = child.kill();
                    return;
                }
            }
            Err(_) => break,
        }
        if child.try_wait().ok().flatten().is_some() {
            // Drain remaining
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let _ = tx.send(PtyMsg::Chunk(buf[..n].to_vec()));
            }
            break;
        }
    }

    let success = if timed_out {
        false
    } else {
        match child.wait() {
            Ok(status) => status.success(),
            Err(_) => false,
        }
    };
    let _ = tx.send(PtyMsg::Exit {
        success: success && !timed_out,
        timed_out,
    });
    // Hold master until child is fully reaped.
    drop(pair.master);
}
