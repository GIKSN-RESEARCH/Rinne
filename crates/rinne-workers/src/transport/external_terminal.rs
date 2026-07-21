//! External terminal transport — open a **real** system Terminal window and run
//! the harness there so the user sees the actual product UI (Grok Build, Claude
//! Code, Codex, …).
//!
//! # Why earlier attempts failed
//!
//! 1. **`script(1)`** nested a PTY → mouse-protocol garbage (`35;6M…`).
//! 2. **`tee`** stole the TTY → plain JSON dumps, no product UI.
//! 3. **`kill -9` on the process group** aborted the TUI without disabling mouse
//!    tracking → Terminal kept emitting CSI mouse reports as text after death.
//! 4. **Banners + non-`exec` mess** before the TUI fought alt-screen rendering.
//!
//! # Correct interactive path
//!
//! - Launch a minimal launcher that **resets the TTY**, then runs the harness
//!   as the foreground job on the real Terminal TTY (no script, no tee).
//! - Deliverable is a **result file** the agent writes (Rinne polls it).
//! - On completion / cancel: **SIGTERM first** so the launcher EXIT trap can
//!   reset mouse tracking + alt-screen; SIGKILL only as a last resort.
//! - Capture (plain single-turn) mode still uses `tee` when the user opts out
//!   of the product UI.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::Command as StdCommand;
use std::time::{Duration, Instant};

use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use rinne_core::worker::{emit, EventSink, ExecStatus, WorkerEvent};
use rinne_core::{Result, RinneError};

use super::subprocess::{LineMapper, SubprocessOutput, SubprocessSpec};

const MAX_CAPTURE_BYTES: usize = 8 * 1024 * 1024;
/// Prefix the launcher writes its own status lines with, so they can be told
/// apart from harness output in the shared log (see `is_terminal_noise`).
const LAUNCHER_MARKER: &str = "rinne-launcher:";
const POLL: Duration = Duration::from_millis(100);
/// How long a result file must stay the same size before we accept it.
const RESULT_STABLE: Duration = Duration::from_millis(1200);
/// After SIGTERM, wait this long for a clean EXIT trap before SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(4);

/// How the launcher should treat the child process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalMode {
    /// Single-turn / headless flags: tee stdout so Rinne can parse. No product UI.
    Capture,
    /// Full interactive product UI on a real TTY. Result via `spec.result_file`.
    Interactive,
}

/// Run `spec` inside a newly opened system terminal window (cwd = workspace).
pub async fn run(
    spec: SubprocessSpec,
    events: &EventSink,
    cancel: &CancellationToken,
    mapper: LineMapper,
) -> Result<SubprocessOutput> {
    let mode = if looks_like_single_turn(&spec) && spec.result_file.is_none() {
        TerminalMode::Capture
    } else {
        TerminalMode::Interactive
    };
    run_with_mode(spec, events, cancel, mapper, mode).await
}

/// Like [`run`], but forces capture vs interactive.
pub async fn run_with_mode(
    spec: SubprocessSpec,
    events: &EventSink,
    cancel: &CancellationToken,
    mapper: LineMapper,
    mode: TerminalMode,
) -> Result<SubprocessOutput> {
    let started = Instant::now();
    let suffix = unique_suffix();
    // Unique title tag so we can always find/close THIS window even if tty
    // matching or Automation permissions fail on the first try.
    let stage_tag = format!("rinne-stage-{}-{}", std::process::id(), suffix);
    let scratch = std::env::temp_dir().join(&stage_tag);
    std::fs::create_dir_all(&scratch)
        .map_err(|e| RinneError::Worker(format!("external terminal scratch dir: {e}")))?;

    let log_path = scratch.join("harness.log");
    let exit_path = scratch.join("exit.code");
    let pid_path = scratch.join("wrapper.pid");
    let tty_path = scratch.join("tty.name");
    let tag_path = scratch.join("stage.tag");
    let win_path = scratch.join("window.id");
    // macOS: `.command` is a recognized executable shell script type.
    let script_path = scratch.join("run-harness.command");

    std::fs::write(&log_path, b"").ok();
    let _ = std::fs::write(&tag_path, &stage_tag);
    let _ = std::fs::remove_file(&exit_path);

    write_launcher(
        &script_path,
        &spec,
        &log_path,
        &exit_path,
        &pid_path,
        &tty_path,
        &stage_tag,
        mode,
    )?;

    emit(
        events,
        WorkerEvent::Message(match mode {
            TerminalMode::Capture => "opening single-turn harness in system Terminal…".into(),
            TerminalMode::Interactive => "opening harness product UI in system Terminal…".into(),
        }),
    );

    open_system_terminal(&script_path, &spec.workspace, &stage_tag, &win_path)?;

    let deadline = spec
        .timeout
        .map(|d| Instant::now() + d)
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));

    wait_for_file(&pid_path, cancel, deadline).await?;

    let mut log_file = std::fs::File::open(&log_path).ok();
    let mut log_pos: u64 = 0;
    let mut captured = String::new();
    let mut truncated = false;
    let mut line_buf = String::new();
    let mut terminal_status: Option<ExecStatus> = None;
    let mut got_result_file = false;
    let mut last_result_size: u64 = 0;
    let mut result_stable_since: Option<Instant> = None;
    // How many result-file lines already streamed into Stage.
    let mut stage_lines_emitted: usize = 0;

    loop {
        if cancel.is_cancelled() {
            soft_stop(&pid_path, &exit_path).await;
            ensure_stage_window_closed(&tty_path, &tag_path, &win_path).await;
            terminal_status = Some(ExecStatus::Cancelled);
            break;
        }
        if Instant::now() >= deadline {
            soft_stop(&pid_path, &exit_path).await;
            ensure_stage_window_closed(&tty_path, &tag_path, &win_path).await;
            terminal_status = Some(ExecStatus::TimedOut);
            break;
        }

        // ── Result-file: stream partial output into Stage as it grows ─────
        if let Some(ref rf) = spec.result_file {
            if let Ok(partial) = std::fs::read_to_string(rf) {
                if !partial.is_empty() {
                    stream_result_to_stage(events, &partial, &mut stage_lines_emitted);
                }
            }
            if let Some(text) = stable_result(rf, &mut last_result_size, &mut result_stable_since) {
                captured = text;
                got_result_file = true;
                // Finish streaming any remaining lines into Stage.
                stream_result_to_stage(events, &captured, &mut stage_lines_emitted);
                emit(
                    events,
                    WorkerEvent::Message("✓ deliverable ready — closing Terminal".into()),
                );
                // SIGTERM → launcher EXIT trap resets TTY + closes the window.
                soft_stop(&pid_path, &exit_path).await;
                ensure_stage_window_closed(&tty_path, &tag_path, &win_path).await;
                terminal_status = Some(ExecStatus::Success);
                break;
            }
        }

        // ── Optional log tail (capture mode) ──────────────────────────────
        if let Some(ref mut lf) = log_file {
            if lf.seek(SeekFrom::Start(log_pos)).is_ok() {
                let mut buf = Vec::new();
                if lf.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
                    log_pos += buf.len() as u64;
                    let chunk = String::from_utf8_lossy(&buf);
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
            }
        }

        if exit_path.exists() {
            if !line_buf.is_empty() {
                flush_line(&mut line_buf, &mut captured, &mut truncated, mapper, events);
            }
            if let Some(ref rf) = spec.result_file {
                if let Ok(text) = std::fs::read_to_string(rf) {
                    let text = text.trim().to_string();
                    if !text.is_empty() {
                        captured = text;
                        got_result_file = true;
                        stream_result_to_stage(events, &captured, &mut stage_lines_emitted);
                    }
                }
            }
            ensure_stage_window_closed(&tty_path, &tag_path, &win_path).await;
            break;
        }

        sleep(POLL).await;
    }

    // Drain remaining log (capture mode only — result-file path already streamed).
    if !got_result_file {
        if let Ok(meta) = std::fs::metadata(&log_path) {
            if meta.len() > log_pos {
                if let Ok(mut f) = std::fs::File::open(&log_path) {
                    let _ = f.seek(SeekFrom::Start(log_pos));
                    let mut rest = String::new();
                    let _ = f.read_to_string(&mut rest);
                    for ch in rest.chars() {
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
                    if !line_buf.is_empty() {
                        flush_line(&mut line_buf, &mut captured, &mut truncated, mapper, events);
                    }
                }
            }
        }
        if let Some(ref rf) = spec.result_file {
            if let Ok(text) = std::fs::read_to_string(rf) {
                let text = text.trim().to_string();
                if !text.is_empty() {
                    captured = text;
                    got_result_file = true;
                    stream_result_to_stage(events, &captured, &mut stage_lines_emitted);
                }
            }
        }
    }

    // Ensure Stage has the final body even if capture mode only filled `captured`.
    if got_result_file || !captured.trim().is_empty() {
        stream_result_to_stage(events, &captured, &mut stage_lines_emitted);
    }

    let exit_code = std::fs::read_to_string(&exit_path)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok());

    let status = terminal_status.unwrap_or_else(|| {
        resolve_status(
            spec.result_file.is_some(),
            got_result_file,
            captured.trim().is_empty(),
            exit_code,
            cancel.is_cancelled(),
        )
    });

    // Final guarantee: multi-strategy close with retries (never leave orphans).
    ensure_stage_window_closed(&tty_path, &tag_path, &win_path).await;

    if matches!(status, ExecStatus::Success) {
        let _ = std::fs::remove_dir_all(&scratch);
    } else {
        emit(
            events,
            WorkerEvent::Message(format!(
                "session artifacts kept at {} (exit {:?})",
                scratch.display(),
                exit_code
            )),
        );
    }

    emit(events, WorkerEvent::Done);
    Ok(SubprocessOutput {
        stdout: captured,
        stderr: String::new(),
        exit_code: if got_result_file { Some(0) } else { exit_code },
        status,
        wall_ms: started.elapsed().as_millis() as u64,
    })
}

/// Push new lines from the result file into Stage (idempotent via `emitted` count).
fn stream_result_to_stage(events: &EventSink, text: &str, emitted: &mut usize) {
    const MAX_STAGE_LINES: usize = 250;
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return;
    }
    if *emitted == 0 {
        emit(events, WorkerEvent::Message("── harness output ──".into()));
    }
    while *emitted < lines.len() && *emitted < MAX_STAGE_LINES {
        let line = lines[*emitted];
        *emitted += 1;
        if line.is_empty() || is_terminal_noise(line) {
            continue;
        }
        // Soft-cap very long single lines for the Stage pane.
        let shown = if line.chars().count() > 400 {
            let take: String = line.chars().take(400).collect();
            format!("{take}…")
        } else {
            line.to_string()
        };
        emit(events, WorkerEvent::Message(shown));
    }
    if lines.len() > MAX_STAGE_LINES && *emitted == MAX_STAGE_LINES {
        emit(
            events,
            WorkerEvent::Message(format!(
                "… ({} more lines truncated in Stage)",
                lines.len() - MAX_STAGE_LINES
            )),
        );
        *emitted = lines.len(); // don't re-emit the truncation notice
    }
}

/// Accept result file only when non-empty and size-stable.
fn stable_result(
    path: &Path,
    last_size: &mut u64,
    stable_since: &mut Option<Instant>,
) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    let len = meta.len();
    if len == 0 {
        return None;
    }
    if len != *last_size {
        *last_size = len;
        *stable_since = Some(Instant::now());
        return None;
    }
    let since = stable_since.get_or_insert_with(Instant::now);
    if since.elapsed() < RESULT_STABLE {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.trim().to_string();
    if text.is_empty() {
        return None;
    }
    // Reject mouse-noise-only "results".
    if is_terminal_noise(&text) {
        return None;
    }
    Some(text)
}

fn looks_like_single_turn(spec: &SubprocessSpec) -> bool {
    spec.args.iter().any(|a| {
        matches!(
            a.as_str(),
            "-p" | "--print" | "--single" | "--output-format" | "--prompt-file" | "exec" | "run"
        ) || a.starts_with("--output-format=")
            || a.starts_with("--prompt-file=")
    })
}

/// Write the launcher script.
///
/// Interactive mode deliberately has **no banner** before the harness — banners
/// fight alt-screen TUIs. Capture mode may print a short header.
fn write_launcher(
    script_path: &Path,
    spec: &SubprocessSpec,
    log_path: &Path,
    exit_path: &Path,
    pid_path: &Path,
    tty_path: &Path,
    stage_tag: &str,
    mode: TerminalMode,
) -> Result<()> {
    let mut script = String::new();
    script.push_str("#!/bin/bash\n");
    // No `set -e` — we need the EXIT trap even if the harness fails.
    script.push_str("set -u\n");
    // Do NOT use `set -m` — job-control process groups make SIGTERM/SIGKILL
    // racey with Terminal.app and leave mouse tracking enabled on hard kill.

    script.push_str(&format!(
        "cd {} || exit 90\n",
        sh_quote(&spec.workspace.display().to_string())
    ));

    // Clean terminal capabilities for modern TUIs.
    script.push_str("export TERM=\"${TERM:-xterm-256color}\"\n");
    script.push_str("export COLORTERM=\"${COLORTERM:-truecolor}\"\n");
    script.push_str("export DISABLE_AUTO_UPDATE=true\n");
    script.push_str(&format!("export RINNE_STAGE_TAG={}\n", sh_quote(stage_tag)));

    for (k, v) in &spec.env {
        script.push_str(&format!("export {}={}\n", k, sh_quote(v)));
    }

    let pid_q = sh_quote(&pid_path.display().to_string());
    let exit_q = sh_quote(&exit_path.display().to_string());
    let log_q = sh_quote(&log_path.display().to_string());
    let tty_q = sh_quote(&tty_path.display().to_string());
    let tag_q = sh_quote(stage_tag);

    // ── TTY reset (start + end) — critical for mouse-tracking cleanup ─────
    script.push_str("reset_tty() {\n");
    script.push_str("  if [ -w /dev/tty ]; then\n");
    script.push_str("    printf '\\033[?1003l\\033[?1002l\\033[?1000l\\033[?1006l\\033[?1015l\\033[?1005l' >/dev/tty 2>/dev/null || true\n");
    script.push_str("    printf '\\033[?25h\\033[?1049l\\033[0m' >/dev/tty 2>/dev/null || true\n");
    script.push_str("  fi\n");
    script.push_str("}\n");

    // Mark this window with a unique title so Rinne can always find it.
    script.push_str("set_stage_title() {\n");
    script.push_str(&format!("  TAG={tag_q}\n"));
    script.push_str("  # OSC 0/2: icon + window title (Terminal.app / iTerm honor these).\n");
    script.push_str("  if [ -w /dev/tty ]; then\n");
    script.push_str("    printf '\\033]0;%s\\007' \"$TAG\" >/dev/tty 2>/dev/null || true\n");
    script.push_str("    printf '\\033]2;%s\\007' \"$TAG\" >/dev/tty 2>/dev/null || true\n");
    script.push_str("  fi\n");
    script.push_str("  printf '\\033]0;%s\\007' \"$TAG\" 2>/dev/null || true\n");
    script.push_str("  printf '\\033]2;%s\\007' \"$TAG\" 2>/dev/null || true\n");
    script.push_str("}\n");

    // Close THIS window — try every strategy so nothing is left open.
    script.push_str("close_window() {\n");
    script.push_str("  TTY_NAME=$(tty 2>/dev/null | sed 's|^/dev/||' || true)\n");
    script.push_str(&format!("  TAG={tag_q}\n"));
    script.push_str(&format!(
        "  [ -n \"${{TTY_NAME:-}}\" ] && echo \"$TTY_NAME\" > {tty_q} 2>/dev/null || true\n"
    ));
    script.push_str("  if [ \"$(uname -s 2>/dev/null)\" != Darwin ]; then\n");
    // Linux: try wmctrl/xdotool by window title.
    script.push_str("    if command -v wmctrl >/dev/null 2>&1; then\n");
    script.push_str("      wmctrl -c \"$TAG\" 2>/dev/null || true\n");
    script.push_str("    fi\n");
    script.push_str("    if command -v xdotool >/dev/null 2>&1; then\n");
    script.push_str("      xdotool search --name \"$TAG\" windowclose %@ 2>/dev/null || true\n");
    script.push_str("    fi\n");
    script.push_str("    return 0\n");
    script.push_str("  fi\n");
    // --- macOS Terminal.app: by tty ---
    script.push_str("  osascript >/dev/null 2>&1 <<OSA || true\n");
    script.push_str("tell application \"Terminal\"\n");
    script.push_str("  repeat with w in windows\n");
    script.push_str("    try\n");
    script.push_str("      set t to tty of selected tab of w\n");
    script.push_str("      if t is \"$TTY_NAME\" or t is \"/dev/$TTY_NAME\" then\n");
    script.push_str("        close w saving no\n");
    script.push_str("      end if\n");
    script.push_str("    end try\n");
    script.push_str("  end repeat\n");
    script.push_str("end tell\n");
    script.push_str("OSA\n");
    // --- macOS Terminal.app: by unique title tag (reliable fallback) ---
    script.push_str("  osascript >/dev/null 2>&1 <<OSA || true\n");
    script.push_str("tell application \"Terminal\"\n");
    script.push_str("  set wins to windows whose name contains \"$TAG\"\n");
    script.push_str("  repeat with w in wins\n");
    script.push_str("    try\n");
    script.push_str("      close w saving no\n");
    script.push_str("    end try\n");
    script.push_str("  end repeat\n");
    script.push_str("end tell\n");
    script.push_str("OSA\n");
    // --- iTerm2: by tty + by name ---
    script.push_str("  osascript >/dev/null 2>&1 <<OSA || true\n");
    script.push_str("tell application \"iTerm\"\n");
    script.push_str("  repeat with w in windows\n");
    script.push_str("    repeat with t in tabs of w\n");
    script.push_str("      repeat with s in sessions of t\n");
    script.push_str("        try\n");
    script.push_str("          set st to tty of s\n");
    script.push_str("          set nm to name of s\n");
    script.push_str("          if st contains \"$TTY_NAME\" or nm contains \"$TAG\" then\n");
    script.push_str("            close s\n");
    script.push_str("          end if\n");
    script.push_str("        end try\n");
    script.push_str("      end repeat\n");
    script.push_str("    end repeat\n");
    script.push_str("  end repeat\n");
    script.push_str("end tell\n");
    script.push_str("OSA\n");
    // --- System Events: Cmd+W if front window title matches (last resort) ---
    script.push_str("  osascript >/dev/null 2>&1 <<OSA || true\n");
    script.push_str("tell application \"System Events\"\n");
    script.push_str("  if exists process \"Terminal\" then\n");
    script.push_str("    tell process \"Terminal\"\n");
    script.push_str("      repeat with w in windows\n");
    script.push_str("        try\n");
    script.push_str("          if name of w contains \"$TAG\" then\n");
    script.push_str("            set frontmost to true\n");
    script.push_str("            perform action \"AXRaise\" of w\n");
    script.push_str("            keystroke \"w\" using command down\n");
    script.push_str("            delay 0.15\n");
    script.push_str("            -- dismiss \"do you want to terminate\" if any\n");
    script.push_str("            keystroke return\n");
    script.push_str("          end if\n");
    script.push_str("        end try\n");
    script.push_str("      end repeat\n");
    script.push_str("    end tell\n");
    script.push_str("  end if\n");
    script.push_str("end tell\n");
    script.push_str("OSA\n");
    script.push_str("}\n");

    script.push_str("on_exit() {\n");
    script.push_str("  ec=$?\n");
    script.push_str("  reset_tty\n");
    script.push_str("  set_stage_title\n");
    script.push_str(&format!("  echo \"$ec\" > {exit_q}\n"));
    script.push_str(&format!(
        "  echo \"{LAUNCHER_MARKER} harness exit $ec\" >> {log_q} 2>/dev/null || true\n"
    ));
    // Multiple close attempts from inside the session (most reliable).
    script.push_str("  close_window\n");
    script.push_str("  sleep 0.2\n");
    script.push_str("  close_window\n");
    script.push_str("}\n");
    script.push_str("trap on_exit EXIT\n");
    script.push_str("trap 'exit 143' TERM\n");
    script.push_str("trap 'exit 130' INT\n");

    script.push_str(&format!("echo $$ > {pid_q}\n"));
    // Record tty early so Rinne can close the window even if the trap is skipped.
    script.push_str("TTY_NAME=$(tty 2>/dev/null | sed 's|^/dev/||' || true)\n");
    script.push_str(&format!(
        "[ -n \"${{TTY_NAME:-}}\" ] && echo \"$TTY_NAME\" > {tty_q} || true\n"
    ));
    // Title first so every subsequent close-by-name works.
    script.push_str("set_stage_title\n");
    script.push_str("reset_tty\n");
    script.push_str("set_stage_title\n");

    // Build argv.
    script.push_str("cmd=(\n");
    script.push_str(&format!("  {}\n", sh_quote(&spec.program)));
    for a in &spec.args {
        script.push_str(&format!("  {}\n", sh_quote(a)));
    }
    script.push_str(")\n");

    let prompt_file = script_path
        .parent()
        .unwrap_or(Path::new("/tmp"))
        .join("prompt.txt");
    if let Some(ref input) = spec.stdin {
        let _ = std::fs::write(&prompt_file, input);
    }

    match mode {
        TerminalMode::Interactive => {
            // Clean slate, then product UI. No banner — the harness owns the screen.
            script.push_str("if [ -w /dev/tty ]; then\n");
            script.push_str("  printf '\\033[2J\\033[H' >/dev/tty 2>/dev/null || true\n");
            script.push_str("fi\n");
            if spec.stdin.is_some() {
                script.push_str(&format!(
                    "\"${{cmd[@]}}\" < {}\n",
                    sh_quote(&prompt_file.display().to_string())
                ));
            } else {
                // format! → "${cmd[@]}"
                script.push_str("\"${cmd[@]}\"\n");
            }
            // EXIT trap: reset TTY, write exit code, close this Terminal window.
            script.push_str("ec=$?\n");
            script.push_str("exit \"$ec\"\n");
        }
        TerminalMode::Capture => {
            script.push_str("echo \"Rinne single-turn session\" >> ");
            script.push_str(&log_q);
            script.push_str("\n");
            if spec.stdin.is_some() {
                script.push_str(&format!(
                    "\"${{cmd[@]}}\" < {} 2>&1 | tee -a {}\n",
                    sh_quote(&prompt_file.display().to_string()),
                    log_q
                ));
            } else {
                script.push_str(&format!("\"${{cmd[@]}}\" 2>&1 | tee -a {}\n", log_q));
            }
            script.push_str("ec=${PIPESTATUS[0]}\n");
            script.push_str("exit \"$ec\"\n");
        }
    }

    std::fs::write(script_path, script)
        .map_err(|e| RinneError::Worker(format!("write terminal launcher: {e}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(script_path)
            .map_err(|e| RinneError::Worker(format!("stat launcher: {e}")))?
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(script_path, perms)
            .map_err(|e| RinneError::Worker(format!("chmod launcher: {e}")))?;
    }
    Ok(())
}

/// Open a new system terminal window that runs the launcher.
fn open_system_terminal(
    script_path: &Path,
    workspace: &Path,
    stage_tag: &str,
    win_path: &Path,
) -> Result<()> {
    let script = script_path.display().to_string();
    let cwd = workspace.display().to_string();

    let prefer = std::env::var("RINNE_EXTERNAL_TERMINAL")
        .unwrap_or_default()
        .to_ascii_lowercase();

    #[cfg(target_os = "macos")]
    {
        if prefer == "iterm" || prefer == "iterm2" || which("iTerm") || app_exists("iTerm") {
            return open_iterm(&script, &cwd, stage_tag, win_path);
        }
        // Prefer osascript so we can capture the window id for forced close.
        if open_macos_terminal_capture_id(&script, stage_tag, win_path).is_ok() {
            return Ok(());
        }
        // Fallback: open .command file.
        if script_path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e == "command")
        {
            return open_macos_command_file(script_path);
        }
        return open_macos_terminal_capture_id(&script, stage_tag, win_path);
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = (cwd, stage_tag, win_path);
        if prefer == "kitty" || which("kitty") {
            return run_detached("kitty", &["--title", stage_tag, "bash", &script]);
        }
        if prefer == "gnome" || which("gnome-terminal") {
            return run_detached(
                "gnome-terminal",
                &["--title", stage_tag, "--", "bash", &script],
            );
        }
        if which("x-terminal-emulator") {
            return run_detached("x-terminal-emulator", &["-e", "bash", &script]);
        }
        if which("xterm") {
            return run_detached("xterm", &["-T", stage_tag, "-e", "bash", &script]);
        }
        Err(RinneError::Worker(
            "no system terminal found (install gnome-terminal, xterm, or kitty; \
             or set RINNE_EXTERNAL_TERMINAL)"
                .into(),
        ))
    }
}

#[cfg(target_os = "macos")]
fn open_macos_command_file(script_path: &Path) -> Result<()> {
    let status = StdCommand::new("open")
        .arg("-a")
        .arg("Terminal")
        .arg(script_path)
        .status()
        .map_err(|e| RinneError::Worker(format!("open Terminal: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(RinneError::Worker(format!(
            "open Terminal failed with {status}"
        )))
    }
}

#[cfg(target_os = "macos")]
fn open_macos_terminal_capture_id(script: &str, stage_tag: &str, win_path: &Path) -> Result<()> {
    // Launch, set custom title via AppleScript, and record window id for kill.
    let apple = format!(
        r#"tell application "Terminal"
  activate
  set newTab to do script "exec bash {script}"
  delay 0.35
  try
    set custom title of newTab to "{tag}"
  end try
  try
    set wid to id of front window
    return wid as text
  on error
    return ""
  end try
end tell"#,
        script = sh_quote(script).replace('\\', "\\\\"),
        tag = stage_tag.replace('\\', "\\\\").replace('"', "\\\"")
    );
    let output = StdCommand::new("osascript")
        .arg("-e")
        .arg(&apple)
        .output()
        .map_err(|e| RinneError::Worker(format!("osascript Terminal: {e}")))?;
    if !output.status.success() {
        return Err(RinneError::Worker(format!(
            "osascript Terminal failed with {}",
            output.status
        )));
    }
    let wid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !wid.is_empty() {
        let _ = std::fs::write(win_path, &wid);
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn open_iterm(script: &str, _cwd: &str, stage_tag: &str, win_path: &Path) -> Result<()> {
    let apple = format!(
        r#"tell application "iTerm"
  activate
  try
    tell current window
      create tab with default profile
      tell current session
        write text "exec bash {script}"
        set name to "{tag}"
      end tell
      try
        set wid to id
        return wid as text
      end try
    end tell
  on error
    create window with default profile
    tell current session of current window
      write text "exec bash {script}"
      set name to "{tag}"
    end tell
    try
      set wid to id of current window
      return wid as text
    end try
  end try
  return ""
end tell"#,
        script = sh_quote(script),
        tag = stage_tag.replace('\\', "\\\\").replace('"', "\\\"")
    );
    let output = StdCommand::new("osascript")
        .arg("-e")
        .arg(&apple)
        .output()
        .map_err(|e| RinneError::Worker(format!("osascript iTerm: {e}")))?;
    if output.status.success() {
        let wid = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !wid.is_empty() {
            let _ = std::fs::write(win_path, &wid);
        }
        Ok(())
    } else {
        open_macos_terminal_capture_id(script, stage_tag, win_path)
    }
}

#[cfg(target_os = "macos")]
fn app_exists(name: &str) -> bool {
    Path::new(&format!("/Applications/{name}.app")).exists()
        || Path::new(&format!(
            "{}/Applications/{name}.app",
            std::env::var("HOME").unwrap_or_default()
        ))
        .exists()
}

fn which(bin: &str) -> bool {
    StdCommand::new("which")
        .arg(bin)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
fn run_detached(program: &str, args: &[&str]) -> Result<()> {
    StdCommand::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| RinneError::Worker(format!("spawn {program}: {e}")))?;
    Ok(())
}

async fn wait_for_file(path: &Path, cancel: &CancellationToken, deadline: Instant) -> Result<()> {
    loop {
        if cancel.is_cancelled() {
            return Err(RinneError::Worker(
                "cancelled before terminal started".into(),
            ));
        }
        if Instant::now() >= deadline {
            return Err(RinneError::Worker(
                "timeout waiting for external terminal to start".into(),
            ));
        }
        if path.exists() {
            return Ok(());
        }
        sleep(POLL).await;
    }
}

/// Multi-strategy close with retries. Goal: **never** leave a Stage Terminal open.
///
/// Order:
/// 1. by window id (captured at open)
/// 2. by unique title tag (`rinne-stage-…`)
/// 3. by tty name
/// 4. System Events Cmd+W by title
/// 5. Linux wmctrl/xdotool by title
async fn ensure_stage_window_closed(tty_path: &Path, tag_path: &Path, win_path: &Path) {
    let tag = std::fs::read_to_string(tag_path)
        .unwrap_or_default()
        .trim()
        .to_string();
    let tty = std::fs::read_to_string(tty_path)
        .unwrap_or_default()
        .trim()
        .trim_start_matches("/dev/")
        .to_string();
    let win_id = std::fs::read_to_string(win_path)
        .unwrap_or_default()
        .trim()
        .to_string();

    // Several passes: Terminal.app sometimes ignores the first close while the
    // shell is still dying.
    for attempt in 0..6 {
        close_stage_window_once(&tty, &tag, &win_id);
        // If we still see a matching window, wait and retry.
        if !stage_window_still_open(&tag, &win_id) {
            return;
        }
        sleep(Duration::from_millis(150 + attempt * 100)).await;
    }
    // Last desperate pass.
    close_stage_window_once(&tty, &tag, &win_id);
    close_stage_window_once(&tty, &tag, &win_id);
}

fn close_stage_window_once(tty: &str, tag: &str, win_id: &str) {
    #[cfg(target_os = "macos")]
    {
        // 1) Window id
        if !win_id.is_empty() && win_id.chars().all(|c| c.is_ascii_digit()) {
            let script = format!(
                r#"tell application "Terminal"
  try
    close (every window whose id is {win_id}) saving no
  end try
end tell"#
            );
            run_osascript(&script);
            let script = format!(
                r#"tell application "iTerm"
  try
    repeat with w in windows
      try
        if (id of w as text) is "{win_id}" then close w
      end try
    end repeat
  end try
end tell"#
            );
            run_osascript(&script);
        }

        // 2) Unique title tag (most reliable across open methods)
        if !tag.is_empty() {
            let tag_esc = tag.replace('\\', "\\\\").replace('"', "\\\"");
            let script = format!(
                r#"tell application "Terminal"
  try
    set wins to windows whose name contains "{tag_esc}"
    repeat with w in wins
      try
        close w saving no
      end try
    end repeat
  end try
end tell"#
            );
            run_osascript(&script);

            let script = format!(
                r#"tell application "iTerm"
  try
    repeat with w in windows
      repeat with t in tabs of w
        repeat with s in sessions of t
          try
            if name of s contains "{tag_esc}" then close s
          end try
        end repeat
      end repeat
      try
        if name of w contains "{tag_esc}" then close w
      end try
    end repeat
  end try
end tell"#
            );
            run_osascript(&script);

            // System Events (works even when Terminal Apple Events are flaky)
            let script = format!(
                r#"tell application "System Events"
  if exists process "Terminal" then
    tell process "Terminal"
      set frontmost to true
      repeat with w in windows
        try
          if name of w contains "{tag_esc}" then
            perform action "AXRaise" of w
            keystroke "w" using command down
            delay 0.12
            keystroke return
          end if
        end try
      end repeat
    end tell
  end if
  if exists process "iTerm2" then
    tell process "iTerm2"
      set frontmost to true
      repeat with w in windows
        try
          if name of w contains "{tag_esc}" then
            perform action "AXRaise" of w
            keystroke "w" using command down
            delay 0.12
            keystroke return
          end if
        end try
      end repeat
    end tell
  end if
end tell"#
            );
            run_osascript(&script);
        }

        // 3) TTY match
        if !tty.is_empty() {
            let script = format!(
                r#"tell application "Terminal"
  repeat with w in windows
    try
      set t to tty of selected tab of w
      if t is "{tty}" or t is "/dev/{tty}" then
        close w saving no
      end if
    end try
  end repeat
end tell"#
            );
            run_osascript(&script);

            let script = format!(
                r#"tell application "iTerm"
  repeat with w in windows
    repeat with t in tabs of w
      repeat with s in sessions of t
        try
          if tty of s contains "{tty}" then close s
        end try
      end repeat
    end repeat
  end repeat
end tell"#
            );
            run_osascript(&script);
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        if !tag.is_empty() {
            let _ = StdCommand::new("wmctrl")
                .args(["-c", tag])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            let _ = StdCommand::new("xdotool")
                .args(["search", "--name", tag, "windowclose", "%@"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        let _ = tty;
        let _ = win_id;
    }
}

fn stage_window_still_open(tag: &str, win_id: &str) -> bool {
    if tag.is_empty() && win_id.is_empty() {
        return false;
    }
    #[cfg(target_os = "macos")]
    {
        let tag_esc = tag.replace('\\', "\\\\").replace('"', "\\\"");
        let mut checks = String::new();
        if !tag.is_empty() {
            checks.push_str(&format!(
                r#"
tell application "Terminal"
  try
    set n to count of (windows whose name contains "{tag_esc}")
    if n > 0 then return "yes"
  end try
end tell
"#
            ));
        }
        if !win_id.is_empty() && win_id.chars().all(|c| c.is_ascii_digit()) {
            checks.push_str(&format!(
                r#"
tell application "Terminal"
  try
    set n to count of (windows whose id is {win_id})
    if n > 0 then return "yes"
  end try
end tell
"#
            ));
        }
        checks.push_str(r#"return "no""#);
        let output = StdCommand::new("osascript").arg("-e").arg(&checks).output();
        if let Ok(out) = output {
            let s = String::from_utf8_lossy(&out.stdout);
            return s.contains("yes");
        }
        // If we can't query, assume it might still be open so we keep retrying.
        return true;
    }
    #[cfg(not(target_os = "macos"))]
    {
        if tag.is_empty() {
            return false;
        }
        if let Ok(out) = StdCommand::new("wmctrl").args(["-l"]).output() {
            let s = String::from_utf8_lossy(&out.stdout);
            return s.contains(tag);
        }
        false
    }
}

fn run_osascript(script: &str) {
    let _ = StdCommand::new("osascript")
        .arg("-e")
        .arg(script)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// SIGTERM the launcher (so EXIT trap resets the TTY + closes the window),
/// wait for exit.code, only then SIGKILL if still alive.
async fn soft_stop(pid_path: &Path, exit_path: &Path) {
    let Some(pid) = read_pid(pid_path) else {
        return;
    };

    #[cfg(unix)]
    {
        unsafe {
            // TERM the process only (not -pgid): launcher is a simple bash
            // script without set -m; children are in the same group by default
            // on macOS Terminal so we also signal the group gently.
            libc::kill(pid, libc::SIGTERM);
            libc::kill(-pid, libc::SIGTERM);
        }

        let deadline = Instant::now() + TERM_GRACE;
        while Instant::now() < deadline {
            if exit_path.exists() {
                return;
            }
            // Process gone?
            #[cfg(unix)]
            {
                let alive = unsafe { libc::kill(pid, 0) == 0 };
                if !alive && exit_path.exists() {
                    return;
                }
                if !alive {
                    // Write a synthetic exit if trap did not run (hard crash).
                    let _ = std::fs::write(exit_path, b"143");
                    return;
                }
            }
            sleep(Duration::from_millis(100)).await;
        }

        // Last resort.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            libc::kill(-pid, libc::SIGKILL);
        }
        if !exit_path.exists() {
            let _ = std::fs::write(exit_path, b"137");
        }
    }

    #[cfg(not(unix))]
    {
        let _ = (pid, exit_path);
    }
}

fn read_pid(pid_path: &Path) -> Option<i32> {
    std::fs::read_to_string(pid_path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

fn flush_line(
    line_buf: &mut String,
    captured: &mut String,
    truncated: &mut bool,
    mapper: LineMapper,
    events: &EventSink,
) {
    let line = std::mem::take(line_buf);
    if is_terminal_noise(&line) {
        return;
    }
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

/// Decide a session's terminal status from what the launcher left behind.
///
/// `wanted_result` means the kickoff told the harness to write its deliverable
/// to a result file, so that file — not the exit code — is the real signal.
fn resolve_status(
    wanted_result: bool,
    got_result_file: bool,
    captured_empty: bool,
    exit_code: Option<i32>,
    cancelled: bool,
) -> ExecStatus {
    if got_result_file && !captured_empty {
        return ExecStatus::Success;
    }
    match exit_code {
        Some(c) if c != 0 => return ExecStatus::Failed(format!("exited {c}")),
        None if cancelled => return ExecStatus::Cancelled,
        _ => {}
    }
    // Exit 0 but no deliverable: the user closed the window, or the harness
    // never did the work. Trusting the exit code here silently drops the node's
    // output and marks it succeeded.
    if wanted_result {
        return ExecStatus::Failed(
            "harness produced no deliverable — result file was never written".into(),
        );
    }
    match exit_code {
        Some(_) => ExecStatus::Success,
        None => ExecStatus::Failed("harness terminal closed without exit code".into()),
    }
}

fn is_terminal_noise(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    // Rinne's own launcher bookkeeping shares the log the Stage tails. It is
    // not harness output and must never stand in for the deliverable.
    if t.starts_with(LAUNCHER_MARKER) {
        return true;
    }
    // SGR mouse reports: digits/semicolons ending in M/m, often concatenated.
    if t.len() >= 4
        && t.chars()
            .all(|c| c.is_ascii_digit() || matches!(c, ';' | 'M' | 'm'))
        && t.contains(';')
        && (t.contains('M') || t.ends_with('m'))
    {
        return true;
    }
    false
}

fn sh_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    let mut out = String::from("'");
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_deliverable_fails_even_when_the_harness_exits_clean() {
        // An interactive kickoff tells the harness to write its deliverable to
        // a result file. Exiting 0 without one means the user closed the window
        // or the harness never did the work — the node contributed nothing and
        // must not be reported as succeeded.
        let status = resolve_status(true, false, true, Some(0), false);
        assert!(
            matches!(status, ExecStatus::Failed(_)),
            "expected Failed, got {status:?}"
        );
    }

    #[test]
    fn a_produced_deliverable_succeeds() {
        assert!(matches!(
            resolve_status(true, true, false, Some(0), false),
            ExecStatus::Success
        ));
    }

    #[test]
    fn capture_sessions_still_succeed_on_a_clean_exit() {
        // No result file was ever demanded, so the exit code is the only signal.
        assert!(matches!(
            resolve_status(false, false, false, Some(0), false),
            ExecStatus::Success
        ));
    }

    #[test]
    fn launcher_bookkeeping_is_not_harness_output() {
        // The launcher writes its own exit marker into the same log the Stage
        // tails, so without filtering it is shown as harness output and can end
        // up standing in for the deliverable.
        assert!(is_terminal_noise("rinne-launcher: harness exit 0"));
        assert!(is_terminal_noise("  rinne-launcher: harness exit 143  "));
        assert!(!is_terminal_noise("editing src/main.rs"));
        assert!(!is_terminal_noise(
            "the launcher script is described in rinne-launcher docs"
        ));
    }

    #[test]
    fn sh_quote_handles_spaces_and_quotes() {
        assert_eq!(sh_quote("hello"), "'hello'");
        assert_eq!(sh_quote("a b"), "'a b'");
        assert!(sh_quote("it's").contains("\\'"));
    }

    #[test]
    fn interactive_launcher_resets_tty_and_avoids_script_tee() {
        let dir = std::env::temp_dir().join(format!("rinne-ext-int-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("run.command");
        let log = dir.join("h.log");
        let exit = dir.join("e.code");
        let pid = dir.join("w.pid");
        let result = dir.join("result.txt");
        let spec = SubprocessSpec {
            program: "grok".into(),
            args: vec![
                "--fullscreen".into(),
                "--always-approve".into(),
                "hello".into(),
            ],
            workspace: dir.clone(),
            stdin: None,
            timeout: None,
            env: vec![],
            result_file: Some(result),
        };
        let tty = dir.join("tty.name");
        write_launcher(
            &script,
            &spec,
            &log,
            &exit,
            &pid,
            &tty,
            "rinne-stage-test-tag",
            TerminalMode::Interactive,
        )
        .unwrap();
        let body = std::fs::read_to_string(&script).unwrap();
        assert!(body.contains("reset_tty"), "must reset mouse tracking");
        assert!(body.contains("1000l"), "must disable mouse mode 1000");
        assert!(body.contains("close_window"), "must close Terminal on exit");
        assert!(
            body.contains("set_stage_title"),
            "must title window for close-by-name"
        );
        assert!(body.contains("rinne-stage-test-tag"));
        assert!(body.contains("trap on_exit EXIT"));
        assert!(!body.contains("script -q"), "must not use script(1)");
        assert!(
            !body.contains("tee -a") || body.contains("Capture"),
            "interactive must not tee"
        );
        assert!(!body.contains("set -m"), "no job-control pgid races");
        assert!(!body.contains("Rinne → harness session"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_launcher_uses_tee_not_script() {
        let dir = std::env::temp_dir().join(format!("rinne-ext-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("run.command");
        let log = dir.join("h.log");
        let exit = dir.join("e.code");
        let pid = dir.join("w.pid");
        let tty = dir.join("tty.name");
        let spec = SubprocessSpec {
            program: "echo".into(),
            args: vec!["-p".into(), "hi".into()],
            workspace: dir.clone(),
            stdin: None,
            timeout: Some(Duration::from_secs(5)),
            env: vec![],
            result_file: None,
        };
        write_launcher(
            &script,
            &spec,
            &log,
            &exit,
            &pid,
            &tty,
            "rinne-stage-cap-tag",
            TerminalMode::Capture,
        )
        .unwrap();
        let body = std::fs::read_to_string(&script).unwrap();
        assert!(body.contains("tee"));
        assert!(!body.contains("script -q"));
        assert!(body.contains("close_window"));
        assert!(body.contains("rinne-stage-cap-tag"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mouse_noise_filtered() {
        assert!(is_terminal_noise("35;6M35;35;7M65;35;7M"));
        assert!(is_terminal_noise("36;18M35;37;18M"));
        assert!(!is_terminal_noise("planning complete"));
        assert!(!is_terminal_noise(r#"{"goal":"x"}"#));
    }
}
