//! Slash-command completion (`CONTEXT.md` §6, §17).
//!
//! A lightweight overlay, sibling to the `@`-file picker, that suggests slash
//! commands and — one level deeper — `/config` subcommands and their values
//! (backends, families, config keys). Tab completes the highlighted item and the
//! overlay re-computes for the next argument, so `/config` → `conductor` → `groq`
//! is a three-Tab flow with hints at every step.

use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher};

/// Top-level slash commands, shown when completing the command name.
const SLASH_COMMANDS: &[(&str, &str, &str)] = &[
    ("config",  "[sub …]",                       "view or edit configuration"),
    ("connect", "<backend> [key] [--model <id>]", "connect a harness or API provider"),
    ("mcp",     "[sub …]",                       "connect and manage MCP servers"),
    ("skill",   "[sub …]",                       "install and manage Agent Skills"),
    ("workers", "",                              "list workers + connected APIs"),
    ("models",  "<provider>",                   "list an API provider's models"),
    ("forget",  "<provider>",                   "delete a stored API key"),
    ("plan",    "",                              "show the current plan"),
    ("steer",   "<text>",                        "guide a parked node"),
    ("approve", "",                              "accept the current state"),
    ("reject",  "",                              "throw out the approach and replan"),
    ("pause",   "",                              "pause the running loop"),
    ("resume",  "",                              "resume a paused run"),
    ("budget",  "<min>",                         "adjust the time budget"),
    ("route",   "<node> <worker>",               "pin a node to a worker"),
    ("logs",    "",                              "where logs are written"),
    ("clear",   "",                              "wipe the screen and reset the session"),
    ("new",     "",                              "alias for /clear"),
    ("help",    "",                              "command reference"),
    ("quit",    "",                              "exit"),
];

/// `/config` subcommands.
const CONFIG_SUBCOMMANDS: &[(&str, &str, &str)] = &[
    ("show",      "", "print resolved config + sources"),
    ("conductor", "", "set planner backend [+ model] [--key <token>]"),
    ("key",       "", "store the current conductor's token (keychain)"),
    ("prefer",    "", "routing family: harness|api|balanced"),
    ("role",      "", "pin a role to a worker"),
    ("model",     "", "default model for a worker"),
    ("set",       "", "set any field: <key> <value>"),
    ("unset",     "", "remove an override"),
    ("init",      "", "scaffold a commented config file"),
    ("edit",      "", "open the config file in your editor"),
    ("path",      "", "show config file paths"),
];

/// `/mcp` subcommands.
const MCP_SUBCOMMANDS: &[(&str, &str, &str)] = &[
    ("add",    "<name> --stdio \"<cmd>\" | --http <url>", "connect a server"),
    ("list",   "", "list connected servers"),
    ("tools",  "<name>", "list a server's tools"),
    ("test",   "<name>", "check a server is reachable"),
    ("remove", "<name>", "disconnect a server"),
];

/// `/skill` subcommands.
const SKILL_SUBCOMMANDS: &[(&str, &str, &str)] = &[
    ("install", "<path> [--project]", "install a skill folder"),
    ("list",    "", "list installed skills"),
    ("show",    "<name>", "print a skill's instructions"),
    ("remove",  "<name>", "uninstall a skill"),
];

/// Backends accepted by `/config conductor <backend>`.
const CONDUCTOR_BACKENDS: &[(&str, &str, &str)] = &[
    ("cloudflare", "", "Workers AI (needs account_id)"),
    ("groq",       "", "fast + cheap"),
    ("nvidia",     "", "NIM endpoint"),
    ("local",      "", "Ollama, fully offline"),
    ("harness",    "", "use the cheapest installed harness"),
];

/// Families accepted by `/config prefer <family>`.
const PREFER_FAMILIES: &[(&str, &str, &str)] = &[
    ("harness",  "", "prefer CLI harnesses"),
    ("api",      "", "prefer API workers"),
    ("balanced", "", "mix by suitability"),
];

/// Roles accepted by `/config role <role> <worker>`.
const ROLES: &[(&str, &str, &str)] = &[
    ("planner",     "", "decomposes the goal into the DAG"),
    ("generator",   "", "produces the work"),
    ("evaluator",   "", "grades the work"),
    ("synthesizer", "", "merges parallel results"),
    ("fixer",       "", "addresses critique"),
];

/// Common dotted keys for `/config set|unset <key>`.
const CONFIG_KEYS: &[(&str, &str, &str)] = &[
    ("conductor.backend",              "", "cloudflare|groq|nvidia|local|harness"),
    ("conductor.model",                "", "model id on that backend"),
    ("conductor.base_url",             "", "override the endpoint"),
    ("conductor.account_id",           "", "cloudflare account id"),
    ("loop.max_iterations_per_node",   "", "generator↔evaluator rounds"),
    ("loop.global_budget_minutes",     "", "wall-clock ceiling"),
    ("loop.test_ratchet",              "", "true|false — block test-weakening diffs"),
    ("loop.stuck_loop_threshold",      "", "failures before escalating to you"),
    ("preferences.prefer",             "", "harness|api|balanced"),
];

/// One suggestion: the value to insert, its usage hint, plus a short description.
pub struct CompletionItem {
    pub value: String,
    pub usage: String,
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

    // Stage 2+ — argument completion for commands that have subcommands.
    match complete[0] {
        "config" => match complete.len() {
            1 => filter(CONFIG_SUBCOMMANDS, partial, "/config subcommand", token_start),
            2 => match complete[1] {
                "conductor" => filter(CONDUCTOR_BACKENDS, partial, "conductor backend", token_start),
                "prefer" => filter(PREFER_FAMILIES, partial, "prefer family", token_start),
                "role" => filter(ROLES, partial, "role", token_start),
                "set" | "unset" | "clear" => filter(CONFIG_KEYS, partial, "config key", token_start),
                _ => None,
            },
            _ => None,
        },
        "mcp" if complete.len() == 1 => {
            filter(MCP_SUBCOMMANDS, partial, "/mcp subcommand", token_start)
        }
        "skill" if complete.len() == 1 => {
            filter(SKILL_SUBCOMMANDS, partial, "/skill subcommand", token_start)
        }
        _ => None,
    }
}

/// Filter a candidate table by `partial` using nucleo fuzzy matching.
///
/// For an empty partial, returns all candidates in declared order. Otherwise
/// ranks by nucleo score (best first) and truncates to 12.
fn filter(
    cands: &[(&str, &str, &str)],
    partial: &str,
    label: &str,
    token_start: usize,
) -> Option<Completion> {
    let mut items: Vec<CompletionItem> = if partial.is_empty() {
        cands
            .iter()
            .map(|(v, u, d)| CompletionItem {
                value: v.to_string(),
                usage: u.to_string(),
                desc: d.to_string(),
            })
            .collect()
    } else {
        let mut matcher = Matcher::new(Config::DEFAULT);
        let pattern = Pattern::parse(partial, CaseMatching::Ignore, Normalization::Smart);
        let names: Vec<&str> = cands.iter().map(|(name, _, _)| *name).collect();
        let scored = pattern.match_list(names.iter().copied(), &mut matcher);
        scored
            .into_iter()
            .map(|(matched_name, _score)| {
                let (v, u, d) = cands
                    .iter()
                    .find(|(name, _, _)| *name == matched_name)
                    .copied()
                    .expect("match_list returned a name not in cands");
                CompletionItem {
                    value: v.to_string(),
                    usage: u.to_string(),
                    desc: d.to_string(),
                }
            })
            .collect()
    };
    if items.is_empty() {
        return None;
    }
    items.truncate(12);
    Some(Completion { items, selected: 0, token_start, label: label.to_string() })
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

    #[test]
    fn fuzzy_matches_subsequence() {
        let v = values("/cfg");
        assert!(v.contains(&"config".to_string()), "{v:?}");
        let v2 = values("/wk");
        assert!(v2.contains(&"workers".to_string()), "{v2:?}");
    }

    #[test]
    fn items_carry_usage_hints() {
        let c = suggest("/connect").unwrap();
        let it = c.items.iter().find(|i| i.value == "connect").unwrap();
        assert!(it.usage.contains("<backend>"), "{:?}", it.usage);
    }
}
