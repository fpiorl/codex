//! Bidirectional privacy filter for model traffic.
//!
//! Every string sent to the model is *redacted* (real values such as company
//! domains and names are replaced by configured placeholders) and every string
//! received from the model is *restored* (placeholders are mapped back to the
//! real values). The mapping is deterministic, so prompt caching keeps working,
//! and it is applied at the API client boundary so the model never observes the
//! real values while the user, the shell and the filesystem never observe the
//! placeholders.
//!
//! Matching is case-insensitive for ASCII letters and preserves the case shape
//! of the matched text (`ACME` -> `COMPANY-A`, `Acme` -> `Company-a`). Rules are
//! matched leftmost-longest in a single pass, so a replacement is never
//! re-scanned against other rules.

use std::collections::HashMap;

use codex_protocol::models::ResponseItem;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

/// A single substitution rule: `real` is what exists in the user's world,
/// `placeholder` is what the model sees instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivacyRule {
    pub real: String,
    pub placeholder: String,
}

#[derive(Debug, Clone)]
struct Pattern {
    /// ASCII-lowercased needle.
    needle_lower: Vec<u8>,
    /// Needle exactly as configured.
    needle: String,
    /// Replacement exactly as configured.
    replacement: String,
}

#[derive(Debug, Clone)]
struct Direction {
    /// Sorted longest-first so leftmost-longest wins.
    patterns: Vec<Pattern>,
}

impl Direction {
    fn new(pairs: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut patterns: Vec<Pattern> = pairs
            .into_iter()
            .filter(|(needle, _)| !needle.is_empty())
            .map(|(needle, replacement)| Pattern {
                needle_lower: needle.to_ascii_lowercase().into_bytes(),
                needle,
                replacement,
            })
            .collect();
        patterns.sort_by(|a, b| {
            b.needle_lower
                .len()
                .cmp(&a.needle_lower.len())
                .then_with(|| a.needle_lower.cmp(&b.needle_lower))
        });
        patterns.dedup_by(|a, b| a.needle_lower == b.needle_lower);
        Self { patterns }
    }

    fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    fn max_len(&self) -> usize {
        self.patterns.first().map_or(0, |p| p.needle_lower.len())
    }

    /// Replace all matches in `text`. Returns `None` when nothing matched so
    /// callers can avoid re-allocating untouched strings.
    fn apply(&self, text: &str) -> Option<String> {
        if self.is_empty() || text.is_empty() {
            return None;
        }
        let bytes = text.as_bytes();
        let lower = text.to_ascii_lowercase();
        let lower = lower.as_bytes();
        let mut out: Option<String> = None;
        let mut copied_upto = 0usize;
        let mut i = 0usize;
        while i < bytes.len() {
            let matched = self
                .patterns
                .iter()
                .find(|p| lower[i..].starts_with(&p.needle_lower));
            match matched {
                Some(p) => {
                    let end = i + p.needle_lower.len();
                    let out = out.get_or_insert_with(|| String::with_capacity(text.len()));
                    out.push_str(&text[copied_upto..i]);
                    let matched_text = &text[i..end];
                    out.push_str(&shape_replacement(matched_text, &p.needle, &p.replacement));
                    copied_upto = end;
                    i = end;
                }
                None => {
                    i += 1;
                }
            }
        }
        out.map(|mut s| {
            s.push_str(&text[copied_upto..]);
            s
        })
    }

    /// Length of the longest suffix of `text` that is a strict prefix of some
    /// needle (case-insensitively). Used by the streaming restorer to hold
    /// back bytes that might become a match once more text arrives.
    fn pending_suffix_len(&self, text: &str) -> usize {
        let lower = text.to_ascii_lowercase();
        let lower = lower.as_bytes();
        let max = self.max_len().saturating_sub(1).min(lower.len());
        for k in (1..=max).rev() {
            let suffix = &lower[lower.len() - k..];
            if self
                .patterns
                .iter()
                .any(|p| p.needle_lower.len() > k && p.needle_lower.starts_with(suffix))
            {
                return k;
            }
        }
        0
    }
}

/// Reproduce the case shape of `matched` onto `replacement`.
fn shape_replacement(matched: &str, needle: &str, replacement: &str) -> String {
    if matched == needle {
        return replacement.to_string();
    }
    let letters: Vec<char> = matched.chars().filter(|c| c.is_alphabetic()).collect();
    if letters.is_empty() {
        return replacement.to_string();
    }
    if letters.iter().all(|c| c.is_uppercase()) && letters.len() > 1 {
        return replacement.to_uppercase();
    }
    if letters.iter().all(|c| c.is_lowercase()) {
        return replacement.to_lowercase();
    }
    let first_is_upper = letters[0].is_uppercase();
    let rest_lower = letters[1..].iter().all(|c| c.is_lowercase());
    if first_is_upper && rest_lower {
        let mut chars = replacement.chars();
        return match chars.next() {
            Some(c) => c.to_uppercase().chain(chars).collect(),
            None => String::new(),
        };
    }
    replacement.to_string()
}

/// The compiled bidirectional filter.
#[derive(Debug, Clone)]
pub struct PrivacyFilter {
    redact: Direction,
    restore: Direction,
}

impl PrivacyFilter {
    pub fn new(rules: impl IntoIterator<Item = PrivacyRule>) -> Self {
        let rules: Vec<PrivacyRule> = rules
            .into_iter()
            .filter(|r| !r.real.is_empty() && !r.placeholder.is_empty())
            .collect();
        let redact = Direction::new(
            rules
                .iter()
                .map(|r| (r.real.clone(), r.placeholder.clone())),
        );
        let restore = Direction::new(
            rules
                .iter()
                .map(|r| (r.placeholder.clone(), r.real.clone())),
        );
        Self { redact, restore }
    }

    pub fn is_empty(&self) -> bool {
        self.redact.is_empty()
    }

    /// Real values -> placeholders (outbound to the model).
    pub fn redact(&self, text: &str) -> String {
        self.redact.apply(text).unwrap_or_else(|| text.to_string())
    }

    /// Placeholders -> real values (inbound from the model).
    pub fn restore(&self, text: &str) -> String {
        self.restore.apply(text).unwrap_or_else(|| text.to_string())
    }

    pub fn redact_in_place(&self, text: &mut String) -> bool {
        match self.redact.apply(text) {
            Some(replaced) => {
                *text = replaced;
                true
            }
            None => false,
        }
    }

    pub fn restore_in_place(&self, text: &mut String) -> bool {
        match self.restore.apply(text) {
            Some(replaced) => {
                *text = replaced;
                true
            }
            None => false,
        }
    }

    /// Redact every string leaf in a JSON value. Returns whether anything changed.
    pub fn redact_value(&self, value: &mut Value) -> bool {
        walk_strings(value, &|s| self.redact.apply(s))
    }

    /// Restore every string leaf in a JSON value. Returns whether anything changed.
    pub fn restore_value(&self, value: &mut Value) -> bool {
        walk_strings(value, &|s| self.restore.apply(s))
    }

    pub fn redact_item(&self, item: &mut ResponseItem) {
        rewrite_item(item, |v| self.redact_value(v));
    }

    pub fn restore_item(&self, item: &mut ResponseItem) {
        rewrite_item(item, |v| self.restore_value(v));
    }

    pub fn redact_items(&self, items: &mut [ResponseItem]) {
        for item in items {
            self.redact_item(item);
        }
    }

    /// Create a streaming restorer for a sequence of text deltas.
    pub fn stream_restorer(&self) -> StreamRestorer {
        StreamRestorer {
            direction: self.restore.clone(),
            buffer: String::new(),
        }
    }
}

fn walk_strings(value: &mut Value, f: &dyn Fn(&str) -> Option<String>) -> bool {
    match value {
        Value::String(s) => match f(s) {
            Some(replaced) => {
                *s = replaced;
                true
            }
            None => false,
        },
        Value::Array(items) => {
            let mut changed = false;
            for item in items {
                changed |= walk_strings(item, f);
            }
            changed
        }
        Value::Object(map) => {
            let mut changed = false;
            for (_, item) in map.iter_mut() {
                changed |= walk_strings(item, f);
            }
            changed
        }
        _ => false,
    }
}

fn rewrite_item(item: &mut ResponseItem, rewrite: impl FnOnce(&mut Value) -> bool) {
    let Ok(mut value) = serde_json::to_value(&*item) else {
        return;
    };
    if !rewrite(&mut value) {
        return;
    }
    if let Ok(rewritten) = serde_json::from_value::<ResponseItem>(value) {
        *item = rewritten;
    }
}

/// Restores placeholders across streamed text deltas.
///
/// A placeholder can be split across two deltas, so the restorer holds back
/// any trailing bytes that could still be the beginning of a placeholder and
/// releases them once the next delta disambiguates them, or on [`flush`].
///
/// [`flush`]: StreamRestorer::flush
#[derive(Debug, Clone)]
pub struct StreamRestorer {
    direction: Direction,
    buffer: String,
}

impl StreamRestorer {
    /// Feed a delta; returns the text that is safe to emit now.
    pub fn push(&mut self, delta: &str) -> String {
        if self.direction.is_empty() {
            return delta.to_string();
        }
        self.buffer.push_str(delta);
        let hold = self.direction.pending_suffix_len(&self.buffer);
        let cut = self.buffer.len() - hold;
        let ready = self.buffer[..cut].to_string();
        self.buffer.drain(..cut);
        self.direction.apply(&ready).unwrap_or(ready)
    }

    /// Emit whatever is still held back.
    pub fn flush(&mut self) -> String {
        let held = std::mem::take(&mut self.buffer);
        self.direction.apply(&held).unwrap_or(held)
    }

    pub fn has_pending(&self) -> bool {
        !self.buffer.is_empty()
    }
}

/// Convenience map of per-item streaming restorers, keyed by item id.
#[derive(Debug, Default)]
pub struct StreamRestorers {
    by_key: HashMap<String, StreamRestorer>,
}

impl StreamRestorers {
    pub fn push(&mut self, filter: &PrivacyFilter, key: &str, delta: &str) -> String {
        self.by_key
            .entry(key.to_string())
            .or_insert_with(|| filter.stream_restorer())
            .push(delta)
    }

    pub fn flush(&mut self, key: &str) -> Option<String> {
        let mut restorer = self.by_key.remove(key)?;
        let text = restorer.flush();
        if text.is_empty() { None } else { Some(text) }
    }

    pub fn flush_all(&mut self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .by_key
            .drain()
            .filter_map(|(key, mut restorer)| {
                let text = restorer.flush();
                if text.is_empty() {
                    None
                } else {
                    Some((key, text))
                }
            })
            .collect();
        out.sort();
        out
    }
}

/// Neutral words used to build random placeholders.
const PLACEHOLDER_WORDS: &[&str] = &[
    "amber", "birch", "cedar", "delta", "ember", "fjord", "gale", "harbor", "iris", "jade",
    "kestrel", "lumen", "maple", "nova", "opal", "pearl", "quartz", "ridge", "sable", "tidal",
    "umber", "vale", "willow", "xenon", "yarrow", "zephyr", "aspen", "basalt", "cobalt", "dune",
    "echo", "flint", "granite", "heron", "indigo", "juniper", "koral", "lotus", "meadow", "nimbus",
];

/// Generate a random placeholder for `real`.
///
/// Values that look like a domain (contain a dot, no whitespace) become a
/// fake `<word>-<word>.example` domain so the model still treats them as a
/// host. Everything else becomes a CamelCase fake name. A random suffix keeps
/// two sessions from ever picking the same placeholder.
pub fn generate_placeholder(real: &str) -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::BuildHasher;

    let seed = RandomState::new().hash_one(real)
        ^ RandomState::new().hash_one(std::time::SystemTime::now());
    let pick = |shift: u32| PLACEHOLDER_WORDS[((seed >> shift) as usize) % PLACEHOLDER_WORDS.len()];
    let suffix = (seed >> 48) % 900 + 100;
    let looks_like_domain = real.contains('.') && !real.chars().any(char::is_whitespace);
    if looks_like_domain {
        format!("{}-{}{}.example", pick(0), pick(16), suffix)
    } else {
        let capitalize = |word: &str| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
                None => String::new(),
            }
        };
        format!("{}{}{}", capitalize(pick(0)), capitalize(pick(16)), suffix)
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
