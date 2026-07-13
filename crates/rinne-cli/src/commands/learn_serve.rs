//! `rinne learn serve` — browse every generated explainer in one place.
//!
//! A tiny read-only HTTP server (built directly on `tokio`, no web framework)
//! that lists `.rinne/learn/*.html` in a left sidebar and loads the selected
//! topic into an `<iframe>`. The topic pages are served verbatim — the server
//! never touches `render.rs`; it only frames the existing self-contained files.
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

use anyhow::{Context, Result};
use rinne_core::Blackboard;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

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

/// The sidebar + iframe shell. Shares the pages' terminal/amber design language
/// so the frame and the content read as one tool. The first topic loads by
/// default; clicking a topic swaps the iframe and the URL hash.
fn shell_html(topics: &[String]) -> String {
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
<title>rinne learn</title>
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
  color: var(--amber); padding: 0 1.25rem 1.25rem; margin: 0;
  border-bottom: 1px solid var(--hairline);
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

    let out = if path == "/" {
        response("200 OK", "text/html; charset=utf-8", shell_html(&topics).as_bytes())
    } else if let Some(name) = path.strip_prefix("/p/") {
        // Match the requested name against the known set — never join user input
        // onto the filesystem, so `..`/absolute paths cannot escape learn_dir.
        let decoded = percent_decode(name);
        if topics.iter().any(|t| t == &decoded) {
            let file = learn_dir.join(format!("{decoded}.html"));
            match std::fs::read(&file) {
                Ok(bytes) => response("200 OK", "text/html; charset=utf-8", &bytes),
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

use tokio_util::sync::CancellationToken;

/// Open `url` in the default browser (best-effort, non-blocking).
fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let _ = std::process::Command::new(opener).arg(url).spawn();
}

/// A learn-docs server running in the background (TUI `/serve`).
///
/// Drop or call [`BackgroundServe::stop`] to shut the accept loop down.
pub struct BackgroundServe {
    pub url: String,
    pub port: u16,
    pub topics: Vec<String>,
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
}

/// Prepare the listener + topic list for a serve session.
async fn prepare(cwd: &Path, port: u16) -> Result<(PathBuf, Vec<String>, String, TcpListener)> {
    let bb = Blackboard::open_with(cwd, true)?;
    let learn_dir = bb.root().join("learn");
    let topics = discover_topics(&learn_dir);
    let addr = format!("127.0.0.1:{port}");
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("failed to bind {addr} (is the port already in use?)"))?;
    let url = format!("http://{addr}");
    Ok((learn_dir, topics, url, listener))
}

/// Status lines describing a serve session (CLI print / TUI feed).
pub fn status_lines(url: &str, topics: &[String]) -> Vec<String> {
    let mut lines = vec![format!(
        "serving {} explainer(s) at {url}",
        topics.len()
    )];
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
/// The accept loop is spawned onto the current Tokio runtime; it stops when
/// [`BackgroundServe::stop`] is called (or the handle's cancel token fires).
pub async fn start_background(cwd: PathBuf, port: u16, open: bool) -> Result<BackgroundServe> {
    let (learn_dir, topics, url, listener) = prepare(&cwd, port).await?;
    if open {
        open_browser(&url);
    }
    let cancel = CancellationToken::new();
    let loop_cancel = cancel.clone();
    tokio::spawn(async move {
        serve_loop(learn_dir, listener, loop_cancel).await;
    });
    Ok(BackgroundServe {
        url,
        port,
        topics,
        cancel,
    })
}

/// Entry point for `rinne learn serve` (blocks until Ctrl-C).
pub async fn run(cwd: PathBuf, port: u16, open: bool) -> Result<()> {
    let (learn_dir, topics, url, listener) = prepare(&cwd, port).await?;
    for line in status_lines(&url, &topics) {
        println!("{line}");
    }
    println!("press Ctrl-C to stop");

    if open {
        open_browser(&url);
    }

    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    // Map Ctrl-C onto the same cancel path the TUI uses, so one accept loop.
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        stop.cancel();
    });
    serve_loop(learn_dir, listener, cancel).await;
    Ok(())
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
        let html = shell_html(&["harness".to_string(), "engine".to_string()]);
        assert!(html.contains("data-topic=\"harness\""), "topic link missing");
        assert!(html.contains("data-topic=\"engine\""));
        // First topic is framed by default.
        assert!(html.contains("src=\"/p/harness\""), "first topic not framed");
        assert!(html.contains("rinne learn"), "brand missing");
    }

    #[test]
    fn shell_empty_state_guides_generation() {
        let html = shell_html(&[]);
        assert!(html.contains("No explainers yet"), "empty state missing");
        assert!(html.contains("rinne learn explain"), "no generation hint");
        assert!(!html.contains("<iframe"), "should not frame anything when empty");
    }

    #[test]
    fn shell_escapes_topic_names() {
        let html = shell_html(&["a<b>&c".to_string()]);
        assert!(!html.contains("a<b>&c"), "raw topic leaked unescaped");
        assert!(html.contains("a&lt;b&gt;&amp;c"), "topic not escaped");
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
}
