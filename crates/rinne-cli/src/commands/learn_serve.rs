//! `rinne learn serve` — browse every generated explainer in one place.
//!
//! A tiny read-only HTTP server (built directly on `tokio`, no web framework)
//! that lists `.rinne/learn/*.html` in a left sidebar and loads the selected
//! topic into an `<iframe>`. Topic pages are served from disk; legacy light-theme
//! docs get a small compatibility stylesheet so near-black prose stays readable
//! against the dark shell iframe. The server never rewrites modern pages.
//!
//! ## Single-owner port lock
//!
//! Default port `7420` is shared across projects. A small lock file under the
//! system temp dir records which **pid / project** currently owns the port so:
//! - starting serve in the **same** project reuses the live server (opens the
//!   browser, no second bind);
//! - starting serve in a **different** project takes the port over (stops the
//!   old owner, binds the new cwd) so you never browse ghost docs from another
//!   repo without noticing;
//! - `/serve stop` and `rinne learn serve --stop` can kill a ghost owner even
//!   when this process didn't start it.
//!
//! Routes:
//!   GET /            → the sidebar+iframe shell
//!   GET /p/<topic>   → the raw topic HTML from disk (re-read per request, so
//!                      regenerating a page shows on reload)
//!   *                → 404
//!
//! Only topics discovered on disk are servable; the request path is matched
//! against that set rather than joined onto the filesystem, so `..` traversal
//! is impossible.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rinne_core::Blackboard;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

/// Discover topic names from `<learn_dir>/*.html`, sorted, excluding the shell.
fn discover_topics(learn_dir: &Path) -> Vec<String> {
    let mut topics: Vec<String> = match std::fs::read_dir(learn_dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) == Some("html") {
                    p.file_stem().and_then(|s| s.to_str()).map(String::from)
                } else {
                    None
                }
            })
            .collect(),
        Err(_) => vec![],
    };
    topics.sort();
    topics.dedup();
    topics
}

/// Escape the HTML-significant characters. Local copy so the module is
/// self-contained (matches the escaper in `learn/render.rs`).
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Folder name of the project that owns `learn_dir` (`.rinne/learn` → repo root).
fn project_label(learn_dir: &Path) -> String {
    learn_dir
        .parent() // .rinne
        .and_then(|p| p.parent()) // project root
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("project")
        .to_string()
}

/// The sidebar + iframe shell. Shares the pages' terminal/amber design language
/// so the frame and the content read as one tool. The first topic loads by
/// default; clicking a topic swaps the iframe and the URL hash.
///
/// `project` is shown in the brand so a leftover server from another repo is
/// obvious in the browser chrome.
fn shell_html(topics: &[String], project: &str) -> String {
    let mut nav = String::new();
    for t in topics {
        nav.push_str(&format!(
            "<a class=\"topic\" href=\"#{t}\" data-topic=\"{t}\">{label}</a>\n",
            t = esc(t),
            label = esc(t),
        ));
    }
    let first = topics.first().cloned().unwrap_or_default();
    let empty = topics.is_empty();

    let main = if empty {
        "<div class=\"empty\"><p>No explainers yet.</p>\
         <p class=\"hint\">Generate one with <code>rinne learn explain &lt;topic&gt;</code>, \
         then reload.</p></div>"
            .to_string()
    } else {
        format!("<iframe id=\"view\" src=\"/p/{}\" title=\"explainer\"></iframe>", esc(&first))
    };

    format!(
        r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>rinne learn · {project}</title>
<style>
:root {{
  --bg: #0f1620; --panel: #131c27; --prose: #e6e4de; --muted: #9aa5b1;
  --faint: #5f6b78; --hairline: #26313f; --amber: #d8a657; --amber-dim: #7a6438;
  --mono: ui-monospace, "SF Mono", SFMono-Regular, Menlo, "Cascadia Mono", Consolas, monospace;
}}
* {{ box-sizing: border-box; }}
html, body {{ height: 100%; margin: 0; }}
body {{ display: flex; background: var(--bg); color: var(--prose); font-family: var(--mono); }}

aside {{
  width: 240px; flex: 0 0 240px; height: 100vh; overflow-y: auto;
  border-right: 1px solid var(--hairline); background: var(--panel);
  padding: 1.5rem 0; position: relative;
}}
.brand {{
  font-size: .72rem; letter-spacing: .24em; text-transform: uppercase;
  color: var(--amber); padding: 0 1.25rem .35rem; margin: 0;
}}
.project {{
  font-size: .78rem; letter-spacing: .02em; text-transform: none;
  color: var(--muted); padding: 0 1.25rem 1.25rem; margin: 0;
  border-bottom: 1px solid var(--hairline);
  word-break: break-all;
}}
nav {{ padding: .75rem 0; display: flex; flex-direction: column; }}
.topic {{
  display: block; padding: .5rem 1.25rem; color: var(--muted);
  text-decoration: none; font-size: .86rem; border-left: 2px solid transparent;
}}
.topic:hover {{ color: var(--prose); background: rgba(216,166,87,.06); }}
.topic.active {{
  color: var(--amber); border-left-color: var(--amber);
  background: rgba(216,166,87,.09);
}}

main {{ flex: 1; height: 100vh; }}
iframe {{ width: 100%; height: 100%; border: 0; background: var(--bg); }}
.empty {{ padding: 4rem 2rem; color: var(--muted); }}
.empty .hint {{ color: var(--faint); font-size: .85rem; }}
.empty code {{ color: var(--amber); }}

@media (max-width: 640px) {{
  body {{ flex-direction: column; }}
  aside {{ width: 100%; flex-basis: auto; height: auto; border-right: 0; border-bottom: 1px solid var(--hairline); }}
  main {{ height: 70vh; }}
  nav {{ flex-direction: row; flex-wrap: wrap; }}
}}
</style>
</head>
<body>
<aside>
  <p class="brand">rinne learn</p>
  <p class="project">{project}</p>
  <nav>
{nav}  </nav>
</aside>
<main>
{main}
</main>
<script>
(function () {{
  var view = document.getElementById('view');
  var links = Array.prototype.slice.call(document.querySelectorAll('.topic'));
  function select(topic) {{
    if (!topic) return;
    var match = null;
    links.forEach(function (a) {{
      var on = a.getAttribute('data-topic') === topic;
      a.classList.toggle('active', on);
      if (on) match = a;
    }});
    if (match && view) view.src = '/p/' + encodeURIComponent(topic);
  }}
  links.forEach(function (a) {{
    a.addEventListener('click', function () {{
      select(a.getAttribute('data-topic'));
    }});
  }});
  window.addEventListener('hashchange', function () {{
    select(decodeURIComponent(location.hash.replace(/^#/, '')));
  }});
  // Honor a deep link like /#harness on load; else highlight the first.
  var initial = decodeURIComponent(location.hash.replace(/^#/, '')) || {first};
  select(initial);
}})();
</script>
</body>
</html>"##,
        nav = nav,
        main = main,
        project = esc(project),
        first = js_string(&first),
    )
}

/// A JSON string literal for embedding inside an inline `<script>`, with the
/// HTML-significant characters neutralized so a topic name can't break out of
/// the script element (`</script>`) or be misread by the HTML parser.
fn js_string(s: &str) -> String {
    serde_json::to_string(s)
        .unwrap_or_else(|_| "\"\"".into())
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
}

/// Build a minimal HTTP/1.1 response with the given status, content-type, body.
fn response(status: &str, content_type: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n",
        len = body.len(),
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

/// True when a page was rendered by the pre-dark-theme learn HTML (near-black
/// body text, no `--prose` tokens, no painted page background).
///
/// Those files look fine opened alone in a light browser chrome, but inside
/// `learn serve`'s dark iframe the shell background shows through and the
/// near-black prose disappears — which looks like "CSS didn't apply".
fn is_legacy_light_theme(html: &str) -> bool {
    // New renderer always declares the design tokens.
    if html.contains("--prose:") || html.contains("--prose ") {
        return false;
    }
    // Old renderer signature.
    html.contains("color: #1a1a1a")
        || html.contains("color:#1a1a1a")
        || (html.contains("<style>") && html.contains("system-ui, sans-serif"))
}

/// Inject a small compatibility stylesheet so legacy light-theme docs paint
/// their own ground when framed by the dark shell. New docs are returned as-is.
fn prepare_topic_html(bytes: &[u8]) -> Vec<u8> {
    let Ok(html) = std::str::from_utf8(bytes) else {
        return bytes.to_vec();
    };
    if !is_legacy_light_theme(html) {
        return bytes.to_vec();
    }
    // Already patched (e.g. hot-reload of a previously served body) — don't double-inject.
    if html.contains("id=\"rinne-legacy-compat\"") {
        return bytes.to_vec();
    }

    const PATCH: &str = r#"<style id="rinne-legacy-compat">
/* Legacy light-theme learn docs omit a page background. The dark learn-serve
   iframe would otherwise show through and near-black prose becomes unreadable. */
html, body { background: #f7f5f0 !important; }
</style>
"#;

    if let Some(idx) = html.find("</head>") {
        let mut out = String::with_capacity(html.len() + PATCH.len());
        out.push_str(&html[..idx]);
        out.push_str(PATCH);
        out.push_str(&html[idx..]);
        return out.into_bytes();
    }
    // No </head> — prepend the patch so it still takes effect.
    let mut out = PATCH.as_bytes().to_vec();
    out.extend_from_slice(bytes);
    out
}

/// Read the request line, return its decoded path (`GET <path> HTTP/1.1`).
/// Reads only the head (up to the blank line) — enough for GET routing.
async fn read_request_path(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        // Stop once we have the request line (first CRLF) — that's all we need.
        if let Some(pos) = buf.windows(2).position(|w| w == b"\r\n") {
            let line = String::from_utf8_lossy(&buf[..pos]).into_owned();
            let mut parts = line.split_whitespace();
            let method = parts.next()?;
            let path = parts.next()?;
            if method != "GET" {
                return None;
            }
            return Some(path.to_string());
        }
        if buf.len() > 8192 {
            break; // oversized request line — bail
        }
    }
    None
}

/// Route one connection. `learn_dir` is re-scanned per request so newly
/// generated pages appear without a restart.
async fn handle(mut stream: TcpStream, learn_dir: PathBuf) {
    let path = match read_request_path(&mut stream).await {
        Some(p) => p,
        None => {
            let _ = stream.write_all(&response("400 Bad Request", "text/plain", b"bad request")).await;
            return;
        }
    };

    // Strip any query string.
    let path = path.split('?').next().unwrap_or("/");

    let topics = discover_topics(&learn_dir);
    let project = project_label(&learn_dir);

    let out = if path == "/" {
        response(
            "200 OK",
            "text/html; charset=utf-8",
            shell_html(&topics, &project).as_bytes(),
        )
    } else if let Some(name) = path.strip_prefix("/p/") {
        // Match the requested name against the known set — never join user input
        // onto the filesystem, so `..`/absolute paths cannot escape learn_dir.
        let decoded = percent_decode(name);
        if topics.iter().any(|t| t == &decoded) {
            let file = learn_dir.join(format!("{decoded}.html"));
            match std::fs::read(&file) {
                Ok(bytes) => {
                    let body = prepare_topic_html(&bytes);
                    response("200 OK", "text/html; charset=utf-8", &body)
                }
                Err(_) => response("404 Not Found", "text/plain", b"not found"),
            }
        } else {
            response("404 Not Found", "text/plain", b"unknown topic")
        }
    } else {
        response("404 Not Found", "text/plain", b"not found")
    };

    let _ = stream.write_all(&out).await;
    let _ = stream.flush().await;
}

/// Minimal percent-decoding for path segments (`%20` etc.). Good enough for
/// topic names, which are symbol/path fragments; leaves malformed escapes as-is.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Open `url` in the default browser (best-effort, non-blocking).
fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let _ = std::process::Command::new(opener).arg(url).spawn();
}

// ── Port lock (ghost / cross-project collision) ─────────────────────────────

/// Who currently owns a learn-serve port. Written after a successful bind;
/// cleared when the accept loop exits or [`stop_port`] kills the owner.
///
/// `port` is persisted in the lock file for operators inspecting the JSON;
/// runtime code always keys by the port argument passed into the helper.
#[derive(Debug, Clone)]
struct ServeLock {
    pid: u32,
    cwd: PathBuf,
}

fn lock_path(port: u16) -> PathBuf {
    std::env::temp_dir().join(format!("rinne-learn-serve-{port}.json"))
}

fn write_lock(port: u16, cwd: &Path) -> Result<()> {
    let path = lock_path(port);
    let body = serde_json::json!({
        "pid": std::process::id(),
        "port": port,
        "cwd": cwd.to_string_lossy(),
    });
    std::fs::write(&path, body.to_string())
        .with_context(|| format!("write serve lock {}", path.display()))
}

fn read_lock(port: u16) -> Option<ServeLock> {
    let raw = std::fs::read_to_string(lock_path(port)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let pid = v.get("pid")?.as_u64()? as u32;
    let cwd = PathBuf::from(v.get("cwd")?.as_str()?);
    // Lock file also stores `"port"` for operators; runtime keys by path/arg.
    Some(ServeLock { pid, cwd })
}

fn clear_lock(port: u16) {
    let _ = std::fs::remove_file(lock_path(port));
}

/// `true` when `pid` still exists (Unix: `kill -0`).
fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        // Signal 0: existence check, no delivery.
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        // Best-effort on non-Unix: assume alive so we don't clobber blindly.
        let _ = pid;
        true
    }
}

/// Ask a process to exit, wait briefly, then escalate. Used only for pids we
/// recorded in our own lock file — never for a random port occupant.
fn terminate_pid(pid: u32) {
    if pid == 0 || pid == std::process::id() {
        return;
    }
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        for _ in 0..20 {
            if !pid_alive(pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(not(unix))]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// Canonicalize for lock comparison (best-effort; falls back to as-is).
fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Stop whatever learn-serve owns `port` (via lock file). Safe no-op when free.
///
/// Used by `rinne learn serve --stop` and TUI `/serve stop` when this session
/// didn't start the server (ghost from another terminal/project).
pub fn stop_port(port: u16) -> Vec<String> {
    match read_lock(port) {
        None => {
            // No lock — port may still be held by a pre-lock binary. Report only.
            vec![format!(
                "no rinne learn serve lock on :{port} — if something still listens, \
                 run: lsof -nP -iTCP:{port} -sTCP:LISTEN"
            )]
        }
        Some(lock) => {
            if !pid_alive(lock.pid) {
                clear_lock(port);
                return vec![format!(
                    "cleared stale serve lock on :{port} (pid {} already gone)",
                    lock.pid
                )];
            }
            let project = lock
                .cwd
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("project");
            terminate_pid(lock.pid);
            clear_lock(port);
            // Wait for the port to free so a follow-up start can bind.
            for _ in 0..30 {
                if std::net::TcpListener::bind(format!("127.0.0.1:{port}")).is_ok() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            vec![format!(
                "stopped learn serve for {project} (pid {}, :{port})",
                lock.pid
            )]
        }
    }
}

/// Snapshot of a live serve we can reuse without rebinding.
#[derive(Debug, Clone)]
pub struct LiveServe {
    pub url: String,
    pub port: u16,
    pub topics: Vec<String>,
    pub project: String,
}

/// Result of claiming a port for this cwd.
enum Claim {
    /// Fresh listener; caller must write the lock after starting the loop.
    Bound {
        learn_dir: PathBuf,
        topics: Vec<String>,
        url: String,
        listener: TcpListener,
        notes: Vec<String>,
    },
    /// Same project already serving — don't bind again.
    Already(LiveServe),
}

async fn claim_port(cwd: &Path, port: u16) -> Result<Claim> {
    let bb = Blackboard::open_with(cwd, true)?;
    let learn_dir = bb.root().join("learn");
    let topics = discover_topics(&learn_dir);
    let addr = format!("127.0.0.1:{port}");
    let url = format!("http://{addr}");
    let cwd = canon(cwd);

    match TcpListener::bind(&addr).await {
        Ok(listener) => Ok(Claim::Bound {
            learn_dir,
            topics,
            url,
            listener,
            notes: vec![],
        }),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // Same project already up → reuse.
            if let Some(lock) = read_lock(port) {
                if pid_alive(lock.pid) && canon(&lock.cwd) == cwd {
                    return Ok(Claim::Already(LiveServe {
                        url,
                        port,
                        topics,
                        project: project_label(&learn_dir),
                    }));
                }
                // Different project (or dead lock) owned by a rinne we recorded
                // → take over so the user never browses ghost docs silently.
                if pid_alive(lock.pid) {
                    let old = lock
                        .cwd
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("other-project");
                    let note = format!(
                        "taking over :{port} from {old} (pid {})",
                        lock.pid
                    );
                    terminate_pid(lock.pid);
                    clear_lock(port);
                    // Retry bind after the port frees.
                    for _ in 0..40 {
                        match TcpListener::bind(&addr).await {
                            Ok(listener) => {
                                return Ok(Claim::Bound {
                                    learn_dir,
                                    topics,
                                    url,
                                    listener,
                                    notes: vec![note],
                                });
                            }
                            Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
                                tokio::time::sleep(Duration::from_millis(50)).await;
                            }
                            Err(err) => {
                                return Err(err).with_context(|| format!("bind {addr}"));
                            }
                        }
                    }
                    bail!(
                        "port {port} still busy after stopping previous serve for {old}; \
                         try: rinne learn serve --stop --port {port}"
                    );
                }
                // Stale lock — drop and retry once.
                clear_lock(port);
                let listener = TcpListener::bind(&addr)
                    .await
                    .with_context(|| {
                        format!(
                            "failed to bind {addr} (port in use by a non-rinne process? \
                             lsof -nP -iTCP:{port} -sTCP:LISTEN)"
                        )
                    })?;
                return Ok(Claim::Bound {
                    learn_dir,
                    topics,
                    url,
                    listener,
                    notes: vec![format!("cleared stale serve lock on :{port}")],
                });
            }
            // Busy, no lock — pre-lock binary or foreign process.
            bail!(
                "port {port} is already in use and has no rinne serve lock.\n\
                 · stop a ghost:  lsof -nP -iTCP:{port} -sTCP:LISTEN   then kill <pid>\n\
                 · or pick another port:  rinne learn serve --port {}\n\
                 · or after upgrade, use:  rinne learn serve --stop",
                port + 1
            );
        }
        Err(e) => Err(e).with_context(|| format!("failed to bind {addr}")),
    }
}

/// A learn-docs server running in the background (TUI `/serve`).
///
/// The accept loop stops when the held cancel token is fired (TUI `/serve stop`
/// or quit). Dropping this struct does **not** cancel — the TUI keeps only the
/// token so the server outlives the start helper.
pub struct BackgroundServe {
    pub url: String,
    pub port: u16,
    pub topics: Vec<String>,
    /// Human notes from claim (e.g. "taking over from other-project").
    pub notes: Vec<String>,
    /// True when we reused an existing same-project server (no new loop).
    pub reused: bool,
    cancel: CancellationToken,
}

impl BackgroundServe {
    /// Clone of the cancel token — fire it to shut the accept loop down.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }
}

/// Bind `127.0.0.1:port`, optionally open a browser, and run the accept loop
/// until `cancel` is fired. Shared by the CLI (`run`) and the TUI.
async fn serve_loop(
    learn_dir: PathBuf,
    listener: TcpListener,
    cancel: CancellationToken,
    port: u16,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let dir = learn_dir.clone();
                        tokio::spawn(async move { handle(stream, dir).await });
                    }
                    Err(e) => {
                        // Transient accept errors shouldn't kill the server.
                        tracing::debug!("learn serve accept error: {e}");
                        continue;
                    }
                }
            }
        }
    }
    clear_lock(port);
}

/// Status lines describing a serve session (CLI print / TUI feed).
pub fn status_lines(url: &str, topics: &[String]) -> Vec<String> {
    status_lines_for(url, topics, None)
}

/// Like [`status_lines`], optionally tagging the project folder name.
pub fn status_lines_for(url: &str, topics: &[String], project: Option<&str>) -> Vec<String> {
    let mut lines = vec![match project {
        Some(p) if !p.is_empty() => format!("serving {} · {} explainer(s) at {url}", p, topics.len()),
        _ => format!("serving {} explainer(s) at {url}", topics.len()),
    }];
    if topics.is_empty() {
        lines.push("  (none yet — /learn <topic> or `rinne learn explain <topic>`)".into());
    } else {
        for t in topics {
            lines.push(format!("  · {t}"));
        }
    }
    lines
}

/// Start serving in the background and return a handle the TUI can stop later.
///
/// Same-project reuse: if this cwd already owns the port, opens the browser and
/// returns `reused = true` without spawning a second loop.
///
/// Cross-project: takes over the port from the previous owner.
pub async fn start_background(cwd: PathBuf, port: u16, open: bool) -> Result<BackgroundServe> {
    match claim_port(&cwd, port).await? {
        Claim::Already(live) => {
            if open {
                open_browser(&live.url);
            }
            Ok(BackgroundServe {
                url: live.url,
                port: live.port,
                topics: live.topics,
                notes: vec![format!(
                    "already serving this project — reusing :{}",
                    live.port
                )],
                reused: true,
                // Dummy token: nothing for the TUI to cancel (owner is external).
                cancel: CancellationToken::new(),
            })
        }
        Claim::Bound {
            learn_dir,
            topics,
            url,
            listener,
            notes,
        } => {
            write_lock(port, &canon(&cwd))?;
            if open {
                open_browser(&url);
            }
            let cancel = CancellationToken::new();
            let loop_cancel = cancel.clone();
            tokio::spawn(async move {
                serve_loop(learn_dir, listener, loop_cancel, port).await;
            });
            Ok(BackgroundServe {
                url,
                port,
                topics,
                notes,
                reused: false,
                cancel,
            })
        }
    }
}

/// Entry point for `rinne learn serve` (blocks until Ctrl-C).
///
/// Pass `stop = true` for `rinne learn serve --stop` (kill owner of port and exit).
pub async fn run(cwd: PathBuf, port: u16, open: bool, stop: bool) -> Result<()> {
    if stop {
        for line in stop_port(port) {
            println!("{line}");
        }
        return Ok(());
    }

    match claim_port(&cwd, port).await? {
        Claim::Already(live) => {
            for line in status_lines_for(
                &live.url,
                &live.topics,
                Some(&live.project),
            ) {
                println!("{line}");
            }
            println!(
                "already running for this project (pid in lock) — opened browser; \
                 stop with: rinne learn serve --stop --port {port}"
            );
            if open {
                open_browser(&live.url);
            }
            Ok(())
        }
        Claim::Bound {
            learn_dir,
            topics,
            url,
            listener,
            notes,
        } => {
            for n in &notes {
                println!("{n}");
            }
            let project = project_label(&learn_dir);
            for line in status_lines_for(&url, &topics, Some(&project)) {
                println!("{line}");
            }
            println!("press Ctrl-C to stop  ·  or: rinne learn serve --stop");

            write_lock(port, &canon(&cwd))?;
            if open {
                open_browser(&url);
            }

            let cancel = CancellationToken::new();
            let stop = cancel.clone();
            tokio::spawn(async move {
                let _ = tokio::signal::ctrl_c().await;
                stop.cancel();
            });
            serve_loop(learn_dir, listener, cancel, port).await;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rinne-serve-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn discover_topics_lists_html_stems_sorted() {
        let d = tmp("list");
        std::fs::write(d.join("harness.html"), "x").unwrap();
        std::fs::write(d.join("conductor.html"), "x").unwrap();
        std::fs::write(d.join("notes.txt"), "x").unwrap(); // ignored
        let topics = discover_topics(&d);
        assert_eq!(topics, vec!["conductor".to_string(), "harness".to_string()]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn discover_topics_empty_dir_is_empty() {
        let d = tmp("empty");
        assert!(discover_topics(&d).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn shell_lists_topics_and_frames_first() {
        let html = shell_html(&["harness".to_string(), "engine".to_string()], "my-app");
        assert!(html.contains("data-topic=\"harness\""), "topic link missing");
        assert!(html.contains("data-topic=\"engine\""));
        // First topic is framed by default.
        assert!(html.contains("src=\"/p/harness\""), "first topic not framed");
        assert!(html.contains("rinne learn"), "brand missing");
        assert!(html.contains("my-app"), "project label missing from shell");
    }

    #[test]
    fn shell_empty_state_guides_generation() {
        let html = shell_html(&[], "demo");
        assert!(html.contains("No explainers yet"), "empty state missing");
        assert!(html.contains("rinne learn explain"), "no generation hint");
        assert!(!html.contains("<iframe"), "should not frame anything when empty");
    }

    #[test]
    fn shell_escapes_topic_names() {
        let html = shell_html(&["a<b>&c".to_string()], "proj");
        assert!(!html.contains("a<b>&c"), "raw topic leaked unescaped");
        assert!(html.contains("a&lt;b&gt;&amp;c"), "topic not escaped");
    }

    #[test]
    fn project_label_from_learn_dir() {
        let learn = PathBuf::from("/Users/me/apps/constructmind-ai-core/.rinne/learn");
        assert_eq!(project_label(&learn), "constructmind-ai-core");
    }

    #[test]
    fn lock_roundtrip() {
        let port = 17_420 + (std::process::id() % 1000) as u16;
        let cwd = tmp("lock-cwd");
        write_lock(port, &cwd).unwrap();
        let lock = read_lock(port).expect("lock written");
        assert_eq!(lock.pid, std::process::id());
        assert_eq!(lock.cwd, cwd);
        clear_lock(port);
        assert!(read_lock(port).is_none());
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn stop_port_clears_stale_lock() {
        let port = 18_420 + (std::process::id() % 1000) as u16;
        let cwd = tmp("stale");
        // Fake a dead pid.
        let path = lock_path(port);
        std::fs::write(
            &path,
            serde_json::json!({"pid": 1u32, "port": port, "cwd": cwd}).to_string(),
        )
        .unwrap();
        // pid 1 may be alive on Unix (init/launchd) — if so, stop_port will try
        // to kill it which is bad. Use a high unlikely-alive pid instead.
        std::fs::write(
            &path,
            serde_json::json!({"pid": 999_999_999u32, "port": port, "cwd": cwd.to_string_lossy()}).to_string(),
        )
        .unwrap();
        let lines = stop_port(port);
        assert!(
            lines.iter().any(|l| l.contains("stale") || l.contains("stopped")),
            "unexpected: {lines:?}"
        );
        assert!(read_lock(port).is_none(), "lock should be cleared");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn percent_decode_handles_escapes_and_plain() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("plain"), "plain");
        // Malformed escape is left intact rather than dropped.
        assert_eq!(percent_decode("a%zz"), "a%zz");
    }

    #[test]
    fn response_has_status_and_content_length() {
        let r = response("200 OK", "text/plain", b"hello");
        let s = String::from_utf8_lossy(&r);
        assert!(s.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(s.contains("Content-Length: 5\r\n"));
        assert!(s.ends_with("hello"));
    }

    #[test]
    fn legacy_light_theme_gets_compat_background() {
        let legacy = r#"<!DOCTYPE html><html><head>
<style>
body { font-family: system-ui, sans-serif; max-width: 900px; margin: 2rem auto; padding: 0 1rem; color: #1a1a1a; line-height: 1.6; }
</style>
</head><body><h1>old doc</h1></body></html>"#;
        let out = String::from_utf8(prepare_topic_html(legacy.as_bytes())).unwrap();
        assert!(
            out.contains("id=\"rinne-legacy-compat\""),
            "compat style not injected: {out}"
        );
        assert!(
            out.contains("background: #f7f5f0"),
            "light ground not forced: {out}"
        );
        // Patch sits before </head> so it overrides nothing structural.
        let head_end = out.find("</head>").unwrap();
        let patch_at = out.find("id=\"rinne-legacy-compat\"").unwrap();
        assert!(patch_at < head_end);
    }

    #[test]
    fn dark_theme_docs_are_left_alone() {
        let modern = r#"<!DOCTYPE html><html><head>
<style>
:root { --prose: #e6e4de; --bg: #0f1620; }
body { background: var(--bg); color: var(--prose); }
</style>
</head><body><p class="section-label">Overview</p></body></html>"#;
        let out = prepare_topic_html(modern.as_bytes());
        assert_eq!(out, modern.as_bytes(), "modern docs must not be rewritten");
    }

    #[test]
    fn legacy_compat_is_idempotent() {
        let legacy = r#"<!DOCTYPE html><html><head>
<style>body { color: #1a1a1a; font-family: system-ui, sans-serif; }</style>
</head><body></body></html>"#;
        let once = prepare_topic_html(legacy.as_bytes());
        let twice = prepare_topic_html(&once);
        assert_eq!(once, twice, "second pass should not double-inject");
        let s = String::from_utf8_lossy(&twice);
        assert_eq!(s.matches("id=\"rinne-legacy-compat\"").count(), 1);
    }
}
