//! Live subscription / rate-limit probes for connected workers.
//!
//! v1: Claude Code (Pro/Max) via Anthropic's OAuth usage endpoint. Other
//! harnesses report `unknown` until a stable probe exists. API workers are
//! listed separately when present in the doctor report.

use std::collections::HashMap;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::probe::{AuthMode, WorkerFamily, WorkerProbe, WorkerStatus};
use crate::model::LimitsConfig;

/// Default thresholds that fire one-shot alerts when crossed upward.
pub const DEFAULT_ALERT_THRESHOLDS: &[u8] = &[50, 75, 90, 100];

/// One rolling window (e.g. 5-hour or weekly) for a single worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitWindow {
    /// Short label: `5h`, `week`, `opus-week`, …
    pub label: String,
    /// Percent of the window already used (0–100+).
    pub used_pct: f64,
    /// ISO-8601 reset time when known.
    #[serde(default)]
    pub resets_at: Option<String>,
}

/// How much we know about a worker's limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LimitKnowledge {
    /// Live numbers from a provider probe.
    Known,
    /// Worker is available but no live probe exists yet.
    Unknown,
    /// Worker is not installed / not authenticated / probe failed.
    Unavailable,
}

/// One worker's limit snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerLimit {
    pub name: String,
    pub family: WorkerFamily,
    pub auth_mode: AuthMode,
    pub knowledge: LimitKnowledge,
    #[serde(default)]
    pub windows: Vec<LimitWindow>,
    /// Human note (why unknown, login hint, error).
    #[serde(default)]
    pub note: Option<String>,
}

impl WorkerLimit {
    /// Worst (highest) used % across windows, if any.
    pub fn max_used_pct(&self) -> Option<f64> {
        self.windows
            .iter()
            .map(|w| w.used_pct)
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
    }
}

/// Full probe report for the pool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitReport {
    pub workers: Vec<WorkerLimit>,
    /// Unix seconds when this report was built.
    pub probed_at: u64,
}

impl LimitReport {
    /// Max used % among workers with live data (best chip signal).
    pub fn max_used_pct(&self) -> Option<f64> {
        self.max_used_pct_for(&[])
    }

    /// Average used % among workers with live data (known only).
    pub fn avg_used_pct(&self) -> Option<f64> {
        self.avg_used_pct_for(&[])
    }

    /// Max used % restricted to `names` when non-empty; otherwise the full pool.
    pub fn max_used_pct_for(&self, names: &[&str]) -> Option<f64> {
        self.known_pcts(names)
            .into_iter()
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
    }

    /// Average used % restricted to `names` when non-empty; otherwise the full pool.
    pub fn avg_used_pct_for(&self, names: &[&str]) -> Option<f64> {
        let vals = self.known_pcts(names);
        if vals.is_empty() {
            None
        } else {
            Some(vals.iter().sum::<f64>() / vals.len() as f64)
        }
    }

    /// Known window max-% values, optionally filtered to worker names.
    fn known_pcts(&self, names: &[&str]) -> Vec<f64> {
        self.workers
            .iter()
            .filter(|w| w.knowledge == LimitKnowledge::Known)
            .filter(|w| names.is_empty() || names.iter().any(|n| *n == w.name))
            .filter_map(|w| w.max_used_pct())
            .collect()
    }

    /// Compact status-line chip, e.g. `limits 42%` or `limits n/a`.
    ///
    /// Uses the **pool max** (worst window) — good idle overview. During a run,
    /// prefer [`Self::run_status_chip`]; after a run, prefer [`Self::breakdown_chip`].
    pub fn status_chip(&self) -> String {
        match self.max_used_pct() {
            Some(p) => format!("limits {:.0}%", p.clamp(0.0, 999.0)),
            None => "limits n/a".into(),
        }
    }

    /// Live run chip: **average** limit % across harness workers used so far.
    ///
    /// `names` are harness worker ids (e.g. `claude-code`, `codex`) that have
    /// already participated — not model ids like `sonnet`/`opus`. Empty `names`
    /// falls back to the pool-wide chip (no participants yet).
    pub fn run_status_chip(&self, names: &[&str]) -> String {
        if names.is_empty() {
            return self.status_chip();
        }
        match self.avg_used_pct_for(names) {
            Some(p) => format!("limits {:.0}%", p.clamp(0.0, 999.0)),
            None => "limits n/a".into(),
        }
    }

    /// Post-task chip: per-harness-worker limit breakdown for participants.
    ///
    /// e.g. `claude-code 62% · codex n/a`. Empty `names` falls back to the pool chip.
    pub fn breakdown_chip(&self, names: &[&str]) -> String {
        if names.is_empty() {
            return self.status_chip();
        }
        let parts: Vec<String> = names
            .iter()
            .map(|name| match self.workers.iter().find(|w| w.name == *name) {
                Some(w) if w.knowledge == LimitKnowledge::Known => match w.max_used_pct() {
                    Some(p) => format!("{name} {:.0}%", p.clamp(0.0, 999.0)),
                    None => format!("{name} n/a"),
                },
                Some(_) | None => format!("{name} n/a"),
            })
            .collect();
        parts.join(" · ")
    }

    /// Color hint for the chip: green / yellow / orange / red by max %.
    pub fn chip_severity(&self) -> ChipSeverity {
        self.chip_severity_for(&[])
    }

    /// Severity for a participant set (max among them); empty = full pool.
    pub fn chip_severity_for(&self, names: &[&str]) -> ChipSeverity {
        match self.max_used_pct_for(names) {
            None => ChipSeverity::Unknown,
            Some(p) if p >= 90.0 => ChipSeverity::Critical,
            Some(p) if p >= 75.0 => ChipSeverity::High,
            Some(p) if p >= 50.0 => ChipSeverity::Warn,
            Some(_) => ChipSeverity::Ok,
        }
    }

    /// Multi-line human report for `/limit-usage`.
    pub fn format_lines(&self) -> Vec<String> {
        let mut out = vec!["limit usage".into(), String::new()];

        let harnesses: Vec<_> = self
            .workers
            .iter()
            .filter(|w| w.family == WorkerFamily::Harness)
            .collect();
        let apis: Vec<_> = self
            .workers
            .iter()
            .filter(|w| w.family == WorkerFamily::Api)
            .collect();

        out.push("  harnesses".into());
        if harnesses.is_empty() {
            out.push("    (none available)".into());
        } else {
            for w in harnesses {
                out.extend(format_worker_block(w));
            }
        }

        if !apis.is_empty() {
            out.push(String::new());
            out.push("  api workers".into());
            for w in apis {
                out.extend(format_worker_block(w));
            }
        }

        out.push(String::new());
        match (self.max_used_pct(), self.avg_used_pct()) {
            (Some(max), Some(avg)) => {
                let known = self
                    .workers
                    .iter()
                    .filter(|w| w.knowledge == LimitKnowledge::Known)
                    .count();
                out.push(format!(
                    "  summary  pool max {max:.0}% · avg (known, {known}) {avg:.0}%"
                ));
            }
            _ => out.push(
                "  summary  no live limit data yet — log into a subscription harness \
                 (e.g. `claude`) and re-run"
                    .into(),
            ),
        }
        out
    }

    /// Emit one-shot threshold alerts for windows that newly crossed a threshold.
    ///
    /// `fired` maps `"worker:window-label"` → highest threshold already announced.
    pub fn take_alerts(
        &self,
        fired: &mut HashMap<String, u8>,
        thresholds: &[u8],
    ) -> Vec<String> {
        let mut alerts = Vec::new();
        let mut sorted: Vec<u8> = thresholds.to_vec();
        sorted.sort_unstable();
        sorted.dedup();

        for w in &self.workers {
            if w.knowledge != LimitKnowledge::Known {
                continue;
            }
            for win in &w.windows {
                let key = format!("{}:{}", w.name, win.label);
                let prev = fired.get(&key).copied().unwrap_or(0);
                let pct = win.used_pct;
                let mut highest = prev;
                for &t in &sorted {
                    if t <= prev {
                        continue;
                    }
                    if pct + f64::EPSILON >= f64::from(t) {
                        highest = t;
                        let severity = if t >= 90 {
                            "⚠"
                        } else if t >= 75 {
                            "●"
                        } else {
                            "·"
                        };
                        let reset = win
                            .resets_at
                            .as_deref()
                            .map(|r| format!(" · resets {r}"))
                            .unwrap_or_default();
                        alerts.push(format!(
                            "{severity} {name} {label} crossed {t}% (now {pct:.0}%{reset})",
                            name = w.name,
                            label = win.label,
                            pct = pct,
                        ));
                    }
                }
                if highest > prev {
                    fired.insert(key, highest);
                }
            }
        }
        alerts
    }
}

/// Chip color band for the status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipSeverity {
    Ok,
    Warn,
    High,
    Critical,
    Unknown,
}

fn format_worker_block(w: &WorkerLimit) -> Vec<String> {
    let mut lines = Vec::new();
    match w.knowledge {
        LimitKnowledge::Known if !w.windows.is_empty() => {
            let parts: Vec<String> = w
                .windows
                .iter()
                .map(|win| {
                    let reset = win
                        .resets_at
                        .as_deref()
                        .map(short_reset)
                        .map(|r| format!(" (resets {r})"))
                        .unwrap_or_default();
                    format!("{label} {pct:.0}%{reset}", label = win.label, pct = win.used_pct)
                })
                .collect();
            lines.push(format!("    {:<14} {}", w.name, parts.join("  ·  ")));
        }
        LimitKnowledge::Known => {
            lines.push(format!("    {:<14} (no windows reported)", w.name));
        }
        LimitKnowledge::Unknown => {
            let note = w.note.as_deref().unwrap_or("no live probe yet");
            lines.push(format!("    {:<14} n/a — {note}", w.name));
        }
        LimitKnowledge::Unavailable => {
            let note = w.note.as_deref().unwrap_or("unavailable");
            lines.push(format!("    {:<14} — {note}", w.name));
        }
    }
    lines
}

/// Shorten an ISO timestamp for display (`2025-11-04T14:22:00Z` → `14:22Z` or date).
fn short_reset(iso: &str) -> String {
    // Keep time-of-day if present; otherwise the date prefix.
    if let Some(t) = iso.split('T').nth(1) {
        let t = t.trim_end_matches('Z');
        let hhmm = t.get(..5).unwrap_or(t);
        return format!("{hhmm}Z");
    }
    iso.chars().take(16).collect()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Probe every available worker from a doctor report (and any enabled-but-missing
/// harnesses we still want to name).
pub async fn probe_limits(workers: &[WorkerProbe], _cfg: &LimitsConfig) -> LimitReport {
    let mut out = Vec::new();

    for w in workers {
        if !w.status.is_available() {
            // Still list installed-but-broken / not-enabled lightly? Skip noise.
            if matches!(w.status, WorkerStatus::NotInstalled) {
                continue;
            }
            out.push(WorkerLimit {
                name: w.name.clone(),
                family: w.family,
                auth_mode: w.auth_mode,
                knowledge: LimitKnowledge::Unavailable,
                windows: Vec::new(),
                note: Some(match &w.status {
                    WorkerStatus::SmokeTestFailed(why) => format!("error: {why}"),
                    _ => "not available".into(),
                }),
            });
            continue;
        }

        let limit = match (w.family, w.name.as_str()) {
            (WorkerFamily::Harness, "claude-code") => probe_claude_code().await,
            (WorkerFamily::Harness, _) => WorkerLimit {
                name: w.name.clone(),
                family: w.family,
                auth_mode: w.auth_mode,
                knowledge: LimitKnowledge::Unknown,
                windows: Vec::new(),
                note: Some("no live limit probe for this harness yet".into()),
            },
            (WorkerFamily::Api, _) => WorkerLimit {
                name: w.name.clone(),
                family: w.family,
                auth_mode: w.auth_mode,
                knowledge: LimitKnowledge::Unknown,
                windows: Vec::new(),
                note: Some("metered API — plan % not published; watch provider dashboards".into()),
            },
        };
        out.push(limit);
    }

    // Stable order: harnesses first, then api; alpha within.
    out.sort_by(|a, b| {
        let fa = matches!(a.family, WorkerFamily::Api) as u8;
        let fb = matches!(b.family, WorkerFamily::Api) as u8;
        fa.cmp(&fb).then_with(|| a.name.cmp(&b.name))
    });

    LimitReport {
        workers: out,
        probed_at: now_secs(),
    }
}

// ----- Claude Code -------------------------------------------------------------

/// Anthropic's OAuth usage endpoint hard-throttles non–`claude-code/*` UAs
/// (persistent 429). Mimic the CLI; prefer the installed version when known.
fn claude_code_user_agent() -> String {
    if let Ok(out) = Command::new("claude").args(["--version"]).output() {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            // e.g. "1.0.55 (Claude Code)" or "claude-code/1.0.55"
            let ver = text
                .split(|c: char| c.is_whitespace() || c == '/')
                .find(|p| p.chars().next().is_some_and(|c| c.is_ascii_digit()));
            if let Some(v) = ver {
                let v = v.trim_matches(|c: char| !c.is_ascii_digit() && c != '.');
                if !v.is_empty() {
                    return format!("claude-code/{v}");
                }
            }
        }
    }
    "claude-code/1.0.0".into()
}

#[derive(Debug, Deserialize)]
struct ClaudeOauthUsage {
    five_hour: Option<ClaudeWindow>,
    seven_day: Option<ClaudeWindow>,
    seven_day_opus: Option<ClaudeWindow>,
    /// Weekly Sonnet window — often the binding limit when Opus is unused.
    #[serde(default)]
    seven_day_sonnet: Option<ClaudeWindow>,
}

#[derive(Debug, Deserialize)]
struct ClaudeWindow {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

/// Outcome of locating a Claude Code OAuth access token.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ClaudeToken {
    Access(String),
    /// Credentials found but `expiresAt` is in the past.
    Expired,
    Missing,
}

async fn probe_claude_code() -> WorkerLimit {
    match claude_oauth_token() {
        ClaudeToken::Missing => {
            return WorkerLimit {
                name: "claude-code".into(),
                family: WorkerFamily::Harness,
                auth_mode: AuthMode::Subscription,
                knowledge: LimitKnowledge::Unavailable,
                windows: Vec::new(),
                note: Some("not logged in — run `claude` and /login".into()),
            };
        }
        ClaudeToken::Expired => {
            return WorkerLimit {
                name: "claude-code".into(),
                family: WorkerFamily::Harness,
                auth_mode: AuthMode::Subscription,
                knowledge: LimitKnowledge::Unavailable,
                windows: Vec::new(),
                note: Some("token expired — run `claude` to refresh login".into()),
            };
        }
        ClaudeToken::Access(token) => probe_claude_usage_with_token(&token).await,
    }
}

async fn probe_claude_usage_with_token(token: &str) -> WorkerLimit {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(12))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return WorkerLimit {
                name: "claude-code".into(),
                family: WorkerFamily::Harness,
                auth_mode: AuthMode::Subscription,
                knowledge: LimitKnowledge::Unavailable,
                windows: Vec::new(),
                note: Some(format!("http client: {e}")),
            };
        }
    };

    let resp = client
        .get("https://api.anthropic.com/api/oauth/usage")
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        // Must look like Claude Code or Anthropic rate-limits the usage API hard.
        .header("User-Agent", claude_code_user_agent())
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => match r.json::<ClaudeOauthUsage>().await {
            Ok(body) => {
                let mut windows = Vec::new();
                push_claude_window(&mut windows, "5h", body.five_hour);
                push_claude_window(&mut windows, "week", body.seven_day);
                push_claude_window(&mut windows, "opus-week", body.seven_day_opus);
                push_claude_window(&mut windows, "sonnet-week", body.seven_day_sonnet);
                if windows.is_empty() {
                    WorkerLimit {
                        name: "claude-code".into(),
                        family: WorkerFamily::Harness,
                        auth_mode: AuthMode::Subscription,
                        knowledge: LimitKnowledge::Unknown,
                        windows,
                        note: Some("usage endpoint returned no windows".into()),
                    }
                } else {
                    WorkerLimit {
                        name: "claude-code".into(),
                        family: WorkerFamily::Harness,
                        auth_mode: AuthMode::Subscription,
                        knowledge: LimitKnowledge::Known,
                        windows,
                        note: None,
                    }
                }
            }
            Err(e) => WorkerLimit {
                name: "claude-code".into(),
                family: WorkerFamily::Harness,
                auth_mode: AuthMode::Subscription,
                knowledge: LimitKnowledge::Unavailable,
                windows: Vec::new(),
                note: Some(format!("bad usage payload: {e}")),
            },
        },
        Ok(r) => WorkerLimit {
            name: "claude-code".into(),
            family: WorkerFamily::Harness,
            auth_mode: AuthMode::Subscription,
            knowledge: LimitKnowledge::Unavailable,
            windows: Vec::new(),
            note: Some(format!(
                "usage probe HTTP {} — re-login with `claude` if this persists",
                r.status()
            )),
        },
        Err(e) => WorkerLimit {
            name: "claude-code".into(),
            family: WorkerFamily::Harness,
            auth_mode: AuthMode::Subscription,
            knowledge: LimitKnowledge::Unavailable,
            windows: Vec::new(),
            note: Some(format!("network: {e}")),
        },
    }
}

fn push_claude_window(out: &mut Vec<LimitWindow>, label: &str, w: Option<ClaudeWindow>) {
    let Some(w) = w else { return };
    let Some(util) = w.utilization else { return };
    out.push(LimitWindow {
        label: label.into(),
        used_pct: util,
        resets_at: w.resets_at,
    });
}

/// Locate Claude Code's OAuth access token.
///
/// Order: macOS Keychain → keyring → `~/.claude/.credentials.json` →
/// `CLAUDE_CODE_OAUTH_TOKEN`. Honors `expiresAt` when present.
fn claude_oauth_token() -> ClaudeToken {
    // Prefer structured credential blobs (keychain / file) so we can honor
    // expiresAt; fall through to the env token last.
    if let Some(raw) = read_claude_credentials_raw() {
        return token_from_credentials_json(&raw);
    }
    if let Some(env) = std::env::var("CLAUDE_CODE_OAUTH_TOKEN")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        // Long-lived setup-token is a bare string; some tools dump JSON.
        if env.starts_with('{') {
            return token_from_credentials_json(&env);
        }
        return ClaudeToken::Access(env);
    }
    ClaudeToken::Missing
}

/// Parse a Claude credentials JSON blob (keychain payload or `.credentials.json`).
fn token_from_credentials_json(raw: &str) -> ClaudeToken {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return ClaudeToken::Missing;
    };
    // Full shape: { "claudeAiOauth": { "accessToken", "expiresAt", … } }
    // Also accept a bare oauth object when env was set to that JSON alone.
    let oauth = v
        .get("claudeAiOauth")
        .cloned()
        .or_else(|| {
            if v.get("accessToken").is_some() {
                Some(v.clone())
            } else {
                None
            }
        });
    let Some(oauth) = oauth else {
        return ClaudeToken::Missing;
    };
    let Some(token) = oauth
        .get("accessToken")
        .and_then(|t| t.as_str())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
    else {
        return ClaudeToken::Missing;
    };
    if oauth_expires_at_past(&oauth) {
        return ClaudeToken::Expired;
    }
    ClaudeToken::Access(token)
}

/// True when `expiresAt` is present and strictly in the past.
///
/// Claude Code stores ms-since-epoch numbers; some dumps use seconds or ISO-8601.
fn oauth_expires_at_past(oauth: &serde_json::Value) -> bool {
    let Some(exp) = oauth.get("expiresAt") else {
        return false;
    };
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let exp_ms = if let Some(n) = exp.as_u64() {
        // Heuristic: values before year ~2286 in seconds are treated as seconds.
        if n < 10_000_000_000 {
            n.saturating_mul(1000)
        } else {
            n
        }
    } else if let Some(n) = exp.as_i64() {
        let n = n.max(0) as u64;
        if n < 10_000_000_000 {
            n.saturating_mul(1000)
        } else {
            n
        }
    } else if let Some(s) = exp.as_str() {
        if let Ok(n) = s.trim().parse::<u64>() {
            if n < 10_000_000_000 {
                n.saturating_mul(1000)
            } else {
                n
            }
        } else {
            // ISO-8601 — best-effort via chrono-less parse of common forms.
            // If unparseable, don't treat as expired (probe will 401 if dead).
            return false;
        }
    } else {
        return false;
    };
    now_ms >= exp_ms
}

fn read_claude_credentials_raw() -> Option<String> {
    // Opt out of credential-store reads. A freshly built binary is not in the
    // Keychain item's ACL, so macOS prompts for a password on every rebuild —
    // unusable in test and CI runs. Set NO_KEYCHAIN_RINNE=1 to skip.
    //
    // Deliberately not `RINNE_`-prefixed: figment maps every `RINNE_*` var onto
    // a config key, so that prefix would be parsed as one and fail validation.
    if std::env::var_os("NO_KEYCHAIN_RINNE").is_some() {
        return None;
    }

    // macOS Keychain (Claude Code's documented store on macOS).
    if cfg!(target_os = "macos") {
        if let Ok(out) = Command::new("security")
            .args(["find-generic-password", "-s", "Claude Code-credentials", "-w"])
            .output()
        {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !s.is_empty() {
                    return Some(s);
                }
            }
        }
    }

    // keyring crate fallback (Linux Secret Service / Windows Credential Manager
    // when Claude Code uses a compatible service name).
    if let Ok(entry) = keyring::Entry::new("Claude Code-credentials", "Claude Code") {
        if let Ok(s) = entry.get_password() {
            if !s.trim().is_empty() {
                return Some(s);
            }
        }
    }
    if let Ok(entry) = keyring::Entry::new("Claude Code-credentials", "claude-code") {
        if let Ok(s) = entry.get_password() {
            if !s.trim().is_empty() {
                return Some(s);
            }
        }
    }

    // File store used on Linux/Windows and some mac installs.
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let path = std::path::PathBuf::from(home)
            .join(".claude")
            .join(".credentials.json");
        if let Ok(s) = std::fs::read_to_string(&path) {
            if !s.trim().is_empty() {
                return Some(s);
            }
        }
    }

    None
}

// ----- Alert state persistence (session-friendly) ------------------------------

/// Tracks which thresholds have already been announced so we only fire on
/// upward crossings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AlertState {
    /// `"worker:window"` → highest threshold already fired.
    pub fired: HashMap<String, u8>,
}

impl AlertState {
    pub fn load(path: &std::path::Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &std::path::Path) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(name: &str, windows: Vec<(&str, f64)>) -> WorkerLimit {
        WorkerLimit {
            name: name.into(),
            family: WorkerFamily::Harness,
            auth_mode: AuthMode::Subscription,
            knowledge: LimitKnowledge::Known,
            windows: windows
                .into_iter()
                .map(|(l, p)| LimitWindow {
                    label: l.into(),
                    used_pct: p,
                    resets_at: None,
                })
                .collect(),
            note: None,
        }
    }

    #[test]
    fn chip_uses_max_across_workers() {
        let report = LimitReport {
            workers: vec![
                known("claude-code", vec![("5h", 40.0), ("week", 62.0)]),
                known("other", vec![("5h", 10.0)]),
            ],
            probed_at: 0,
        };
        assert!((report.max_used_pct().unwrap() - 62.0).abs() < 0.01);
        assert_eq!(report.status_chip(), "limits 62%");
        assert_eq!(report.chip_severity(), ChipSeverity::Warn);
    }

    #[test]
    fn avg_ignores_unknown() {
        let report = LimitReport {
            workers: vec![
                known("claude-code", vec![("5h", 40.0)]),
                WorkerLimit {
                    name: "codex".into(),
                    family: WorkerFamily::Harness,
                    auth_mode: AuthMode::Subscription,
                    knowledge: LimitKnowledge::Unknown,
                    windows: Vec::new(),
                    note: None,
                },
            ],
            probed_at: 0,
        };
        assert!((report.avg_used_pct().unwrap() - 40.0).abs() < 0.01);
    }

    #[test]
    fn run_chip_averages_participants_only() {
        let report = LimitReport {
            workers: vec![
                known("claude-code", vec![("5h", 40.0), ("week", 60.0)]),
                known("cursor", vec![("day", 20.0)]),
                known("idle-harness", vec![("5h", 99.0)]),
            ],
            probed_at: 0,
        };
        // Avg of each participant's max window: (60 + 20) / 2 = 40.
        assert_eq!(
            report.run_status_chip(&["claude-code", "cursor"]),
            "limits 40%"
        );
        // Empty participants → pool-wide max chip.
        assert_eq!(report.run_status_chip(&[]), "limits 99%");
    }

    #[test]
    fn breakdown_chip_lists_each_participant() {
        let report = LimitReport {
            workers: vec![
                known("claude-code", vec![("5h", 42.0), ("week", 55.0)]),
                WorkerLimit {
                    name: "codex".into(),
                    family: WorkerFamily::Harness,
                    auth_mode: AuthMode::Subscription,
                    knowledge: LimitKnowledge::Unknown,
                    windows: Vec::new(),
                    note: None,
                },
            ],
            probed_at: 0,
        };
        let chip = report.breakdown_chip(&["claude-code", "codex"]);
        assert_eq!(chip, "claude-code 55% · codex n/a");
        assert_eq!(report.chip_severity_for(&["claude-code"]), ChipSeverity::Warn);
        assert_eq!(report.chip_severity_for(&["codex"]), ChipSeverity::Unknown);
    }

    #[test]
    fn threshold_alerts_fire_once_per_crossing() {
        let report = LimitReport {
            workers: vec![known("claude-code", vec![("5h", 76.0)])],
            probed_at: 0,
        };
        let mut fired = HashMap::new();
        let a1 = report.take_alerts(&mut fired, DEFAULT_ALERT_THRESHOLDS);
        assert_eq!(a1.len(), 2, "50 and 75 should fire: {a1:?}");
        assert!(a1.iter().any(|s| s.contains("50%")));
        assert!(a1.iter().any(|s| s.contains("75%")));

        let a2 = report.take_alerts(&mut fired, DEFAULT_ALERT_THRESHOLDS);
        assert!(a2.is_empty(), "no re-fire: {a2:?}");

        let report2 = LimitReport {
            workers: vec![known("claude-code", vec![("5h", 91.0)])],
            probed_at: 1,
        };
        let a3 = report2.take_alerts(&mut fired, DEFAULT_ALERT_THRESHOLDS);
        assert_eq!(a3.len(), 1);
        assert!(a3[0].contains("90%"));
    }

    #[test]
    fn format_lines_includes_summary() {
        let report = LimitReport {
            workers: vec![known("claude-code", vec![("5h", 12.0), ("week", 34.0)])],
            probed_at: 0,
        };
        let text = report.format_lines().join("\n");
        assert!(text.contains("claude-code"));
        assert!(text.contains("5h"));
        assert!(text.contains("summary"));
        assert!(text.contains("pool max"));
    }

    #[test]
    fn sonnet_week_window_drives_max_when_highest() {
        // Live OAuth payloads can omit opus and bind on seven_day_sonnet.
        let report = LimitReport {
            workers: vec![known(
                "claude-code",
                vec![("5h", 20.0), ("week", 30.0), ("sonnet-week", 88.0)],
            )],
            probed_at: 0,
        };
        assert!((report.max_used_pct().unwrap() - 88.0).abs() < 0.01);
        assert_eq!(report.status_chip(), "limits 88%");
        assert_eq!(report.chip_severity(), ChipSeverity::High);
        assert_eq!(report.breakdown_chip(&["claude-code"]), "claude-code 88%");
    }

    #[test]
    fn oauth_usage_deserializes_sonnet_window() {
        let json = r#"{
            "five_hour": { "utilization": 10.0, "resets_at": "2026-01-01T00:00:00Z" },
            "seven_day": { "utilization": 20.0 },
            "seven_day_sonnet": { "utilization": 77.5 },
            "seven_day_opus": null
        }"#;
        let body: ClaudeOauthUsage = serde_json::from_str(json).unwrap();
        let mut windows = Vec::new();
        push_claude_window(&mut windows, "5h", body.five_hour);
        push_claude_window(&mut windows, "week", body.seven_day);
        push_claude_window(&mut windows, "opus-week", body.seven_day_opus);
        push_claude_window(&mut windows, "sonnet-week", body.seven_day_sonnet);
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[2].label, "sonnet-week");
        assert!((windows[2].used_pct - 77.5).abs() < 0.01);
        let max = windows
            .iter()
            .map(|w| w.used_pct)
            .max_by(|a, b| a.partial_cmp(b).unwrap())
            .unwrap();
        assert!((max - 77.5).abs() < 0.01);
    }

    #[test]
    fn token_from_credentials_honors_expires_at() {
        let future_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 3_600_000;
        let past_ms = 1_000_000_u64;

        let valid = format!(
            r#"{{"claudeAiOauth":{{"accessToken":"sk-ant-oat01-live","expiresAt":{future_ms}}}}}"#
        );
        assert_eq!(
            token_from_credentials_json(&valid),
            ClaudeToken::Access("sk-ant-oat01-live".into())
        );

        let expired = format!(
            r#"{{"claudeAiOauth":{{"accessToken":"sk-ant-oat01-dead","expiresAt":{past_ms}}}}}"#
        );
        assert_eq!(token_from_credentials_json(&expired), ClaudeToken::Expired);

        let no_exp = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-ok"}}"#;
        assert_eq!(
            token_from_credentials_json(no_exp),
            ClaudeToken::Access("sk-ant-oat01-ok".into())
        );

        let bare = r#"{"accessToken":"sk-ant-oat01-bare","expiresAt":9999999999999}"#;
        assert_eq!(
            token_from_credentials_json(bare),
            ClaudeToken::Access("sk-ant-oat01-bare".into())
        );

        assert_eq!(token_from_credentials_json("{}"), ClaudeToken::Missing);
        assert_eq!(token_from_credentials_json("not-json"), ClaudeToken::Missing);
    }

    #[test]
    fn claude_user_agent_is_claude_code_shaped() {
        let ua = claude_code_user_agent();
        assert!(
            ua.starts_with("claude-code/"),
            "usage endpoint throttles non-cli UAs: {ua}"
        );
    }
}
