//! Slash-command completion (`CONTEXT.md` §6, §17).
//!
//! A lightweight overlay, sibling to the `@`-file picker, that suggests slash
//! commands and — one level deeper — `/config` subcommands and their values
//! (backends, families, config keys). Tab completes the highlighted item and the
//! overlay re-computes for the next argument, so `/config` → `conductor` → `groq`
//! is a three-Tab flow with hints at every step.

/// Top-level slash commands, shown when completing the command name.
const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("config", "view or edit configuration"),
    ("connect", "connect a harness or API provider"),
    ("workers", "list workers + connected APIs"),
    ("models", "list an API provider's models"),
    ("forget", "delete a stored API key"),
    ("plan", "show the current plan"),
    ("steer", "guide a parked node"),
    ("approve", "accept the current state"),
    ("reject", "throw out the approach and replan"),
    ("pause", "pause the running loop"),
    ("resume", "resume a paused run"),
    ("budget", "adjust the time budget"),
    ("route", "pin a node to a worker"),
    ("logs", "where logs are written"),
    ("help", "command reference"),
    ("quit", "exit"),
];

/// `/config` subcommands.
const CONFIG_SUBCOMMANDS: &[(&str, &str)] = &[
    ("show", "print resolved config + sources"),
    ("conductor", "set planner backend [+ model] [--key <token>]"),
    ("key", "store the current conductor's token (keychain)"),
    ("prefer", "routing family: harness|api|balanced"),
    ("role", "pin a role to a worker"),
    ("model", "default model for a worker"),
    ("set", "set any field: <key> <value>"),
    ("unset", "remove an override"),
    ("init", "scaffold a commented config file"),
    ("edit", "open the config file in your editor"),
    ("path", "show config file paths"),
];

/// Backends accepted by `/config conductor <backend>`.
const CONDUCTOR_BACKENDS: &[(&str, &str)] = &[
    ("cloudflare", "Workers AI (needs account_id)"),
    ("groq", "fast + cheap"),
    ("nvidia", "NIM endpoint"),
    ("local", "Ollama, fully offline"),
    ("harness", "use the cheapest installed harness"),
];

/// Families accepted by `/config prefer <family>`.
const PREFER_FAMILIES: &[(&str, &str)] = &[
    ("harness", "prefer CLI harnesses"),
    ("api", "prefer API workers"),
    ("balanced", "mix by suitability"),
];

/// Roles accepted by `/config role <role> <worker>`.
const ROLES: &[(&str, &str)] = &[
    ("planner", "decomposes the goal into the DAG"),
    ("generator", "produces the work"),
    ("evaluator", "grades the work"),
    ("synthesizer", "merges parallel results"),
    ("fixer", "addresses critique"),
];

/// Common dotted keys for `/config set|unset <key>`.
const CONFIG_KEYS: &[(&str, &str)] = &[
    ("conductor.backend", "cloudflare|groq|nvidia|local|harness"),
    ("conductor.model", "model id on that backend"),
    ("conductor.base_url", "override the endpoint"),
    ("conductor.account_id", "cloudflare account id"),
    ("loop.max_iterations_per_node", "generator↔evaluator rounds"),
    ("loop.global_budget_minutes", "wall-clock ceiling"),
    ("loop.test_ratchet", "true|false — block test-weakening diffs"),
    ("loop.stuck_loop_threshold", "failures before escalating to you"),
    ("preferences.prefer", "harness|api|balanced"),
];

/// One suggestion: the value to insert plus a short description.
pub struct CompletionItem {
    pub value: String,
    pub desc: String,
}

/// The active completion overlay: candidates, selection, and where in the input
/// the current token starts (so Tab can replace just that token).
pub struct Completion {
    pub items: Vec<CompletionItem>,
    pub selected: usize,
    pub token_start: usize,
    pub label: String,
}

impl Completion {
    pub fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }
    pub fn down(&mut self) {
        if self.selected + 1 < self.items.len() {
            self.selected += 1;
        }
    }
    pub fn selected(&self) -> Option<&CompletionItem> {
        self.items.get(self.selected)
    }
}

/// Compute completions for the current input, or `None` if nothing applies.
///
/// Only fires for slash-command lines. Completes the command name, then (for
/// `/config`) the subcommand, then its first value where the value set is known.
pub fn suggest(input: &str) -> Option<Completion> {
    let body = input.strip_prefix('/')?;
    let trailing_ws = input.ends_with(char::is_whitespace);
    let parts: Vec<&str> = body.split_whitespace().collect();
    let partial = if trailing_ws { "" } else { parts.last().copied().unwrap_or("") };
    let complete_len = if trailing_ws { parts.len() } else { parts.len().saturating_sub(1) };
    let complete = &parts[..complete_len];
    // `partial` is the trailing run of non-space chars, so it is an ASCII suffix
    // of `input` and this byte arithmetic lands on a char boundary.
    let token_start = input.len() - partial.len();

    // Stage 1 — the command name itself.
    if complete.is_empty() {
        return filter(SLASH_COMMANDS, partial, "/command", token_start);
    }

    // Stage 2+ — only `/config` exposes argument completion for now.
    if complete[0] != "config" {
        return None;
    }
    match complete.len() {
        1 => filter(CONFIG_SUBCOMMANDS, partial, "/config subcommand", token_start),
        2 => match complete[1] {
            "conductor" => filter(CONDUCTOR_BACKENDS, partial, "conductor backend", token_start),
            "prefer" => filter(PREFER_FAMILIES, partial, "prefer family", token_start),
            "role" => filter(ROLES, partial, "role", token_start),
            "set" | "unset" | "clear" => filter(CONFIG_KEYS, partial, "config key", token_start),
            _ => None,
        },
        _ => None,
    }
}

/// Filter a candidate table by `partial` (prefix first, then substring), build a
/// [`Completion`], or `None` if nothing matches.
fn filter(
    cands: &[(&str, &str)],
    partial: &str,
    label: &str,
    token_start: usize,
) -> Option<Completion> {
    let p = partial.to_lowercase();
    let mut items: Vec<CompletionItem> = cands
        .iter()
        .filter(|(v, _)| p.is_empty() || v.to_lowercase().starts_with(&p))
        .map(|(v, d)| CompletionItem { value: v.to_string(), desc: d.to_string() })
        .collect();
    if items.is_empty() && !p.is_empty() {
        items = cands
            .iter()
            .filter(|(v, _)| v.to_lowercase().contains(&p))
            .map(|(v, d)| CompletionItem { value: v.to_string(), desc: d.to_string() })
            .collect();
    }
    if items.is_empty() {
        return None;
    }
    Some(Completion {
        items,
        selected: 0,
        token_start,
        label: label.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(input: &str) -> Vec<String> {
        suggest(input)
            .map(|c| c.items.into_iter().map(|i| i.value).collect())
            .unwrap_or_default()
    }

    #[test]
    fn completes_command_name() {
        let v = values("/conf");
        assert!(v.contains(&"config".to_string()), "{v:?}");
    }

    #[test]
    fn completes_config_subcommands() {
        let v = values("/config ");
        assert!(v.contains(&"conductor".to_string()) && v.contains(&"set".to_string()), "{v:?}");
        // Partial narrows it.
        let v2 = values("/config co");
        assert_eq!(v2, vec!["conductor".to_string()]);
    }

    #[test]
    fn completes_conductor_backends() {
        let v = values("/config conductor ");
        assert!(v.contains(&"groq".to_string()) && v.contains(&"local".to_string()), "{v:?}");
        assert_eq!(values("/config conductor g"), vec!["groq".to_string()]);
    }

    #[test]
    fn completes_config_keys_for_set() {
        let v = values("/config set loop.");
        assert!(v.iter().all(|k| k.starts_with("loop.")), "{v:?}");
        assert!(v.contains(&"loop.max_iterations_per_node".to_string()), "{v:?}");
    }

    #[test]
    fn no_completion_past_known_values() {
        // typing a free value (the model id) has no candidate set
        assert!(suggest("/config conductor groq ").is_none());
        // non-/config commands get no argument completion
        assert!(suggest("/connect deep").is_none());
        // not a slash line at all
        assert!(suggest("summarise this").is_none());
    }

    #[test]
    fn token_start_targets_trailing_word() {
        let c = suggest("/config co").unwrap();
        assert_eq!(&"/config co"[c.token_start..], "co");
        let c2 = suggest("/config ").unwrap();
        assert_eq!(c2.token_start, "/config ".len());
    }
}
