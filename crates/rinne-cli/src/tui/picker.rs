//! The `@`-file picker (Power 1, `CONTEXT.md` §6, §14).
//!
//! A `nucleo` fuzzy matcher over the file index, shown as an overlay when the
//! current prompt token starts with `@`. Selecting a match completes the token
//! to an explicit file reference (never a pasted file body).

use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher};

use super::index::FileIndex;

/// How many matches to show in the overlay.
const MAX_MATCHES: usize = 12;

/// Picker state: the live query and its current matches.
pub struct Picker {
    pub query: String,
    pub matches: Vec<String>,
    pub selected: usize,
    matcher: Matcher,
}

impl Picker {
    pub fn new() -> Self {
        Self {
            query: String::new(),
            matches: Vec::new(),
            selected: 0,
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
        }
    }

    /// Recompute matches for `query` against the index.
    pub fn update(&mut self, query: &str, index: &FileIndex) {
        self.query = query.to_string();
        self.matches = if query.is_empty() {
            index.files().iter().take(MAX_MATCHES).cloned().collect()
        } else {
            let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
            let mut scored = pattern.match_list(index.files().iter(), &mut self.matcher);
            scored.truncate(MAX_MATCHES);
            scored.into_iter().map(|(s, _)| s.clone()).collect()
        };
        if self.selected >= self.matches.len() {
            self.selected = self.matches.len().saturating_sub(1);
        }
    }

    pub fn selected(&self) -> Option<&String> {
        self.matches.get(self.selected)
    }

    pub fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn down(&mut self) {
        if self.selected + 1 < self.matches.len() {
            self.selected += 1;
        }
    }
}
