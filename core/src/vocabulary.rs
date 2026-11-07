//! User vocabulary: terms that bias the decoder, and repairs for its output.
//!
//! Two mechanisms, deliberately separate:
//!
//! - **Terms** are appended to whisper's initial prompt. That is a hint, not a
//!   constraint: it raises the odds of a name or an acronym without changing the
//!   decoder's shape. It must stay short — the prompt shares the model's context
//!   — so [`Vocabulary::prompt`] drops terms past its budget.
//! - **Replacements** are applied to the transcript text after ASR. They are
//!   deterministic, testable, and cannot make recognition worse; they only fix
//!   spellings the model produced.
//!
//! The file lives beside `settings.json` so the GUI and the worker read the same
//! one, and so changing it never requires a restart.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::license;

/// Longest prompt the decoder is given. Whisper's prompt is a context prefix,
/// not an unlimited glossary, and a long one makes it repeat itself.
pub const PROMPT_BUDGET: usize = 600;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replacement {
    /// Text as the model produced it.
    pub from: String,
    /// Text the transcript should show instead.
    pub to: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Vocabulary {
    pub terms: Vec<String>,
    pub replacements: Vec<Replacement>,
}

impl Vocabulary {
    /// Path of the persisted vocabulary.
    pub fn path() -> Option<PathBuf> {
        license::config_dir().map(|dir| dir.join("vocabulary.json"))
    }

    /// Read the vocabulary, or an empty one when there is none yet.
    ///
    /// A malformed file is treated as empty rather than failing a run: losing a
    /// hint is recoverable, refusing to transcribe is not.
    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        let Ok(text) = fs::read_to_string(path) else {
            return Self::default();
        };
        serde_json::from_str(&text).unwrap_or_default()
    }

    /// Persist the vocabulary, creating the config directory if needed.
    pub fn save(&self) -> io::Result<()> {
        static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

        let Some(path) = Self::path() else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no configuration directory",
            ));
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "vocabulary.json".to_string());
        let temporary = path.with_file_name(format!(
            ".{name}.{}.{}.tmp",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&temporary, text)?;
        match fs::rename(&temporary, &path) {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                Err(error)
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty() && self.replacements.is_empty()
    }

    /// The decoder prompt, or `None` when there are no terms.
    ///
    /// Terms past `budget` bytes are dropped rather than truncated, so the
    /// prompt never ends mid-word.
    pub fn prompt(&self, budget: usize) -> Option<String> {
        let mut terms: Vec<&str> = Vec::new();
        for term in &self.terms {
            let term = term.trim();
            if term.is_empty() {
                continue;
            }
            if !terms.iter().any(|kept| kept.eq_ignore_ascii_case(term)) {
                terms.push(term);
            }
        }
        if terms.is_empty() {
            return None;
        }

        let mut prompt = String::from("Names and terms that may appear: ");
        for term in terms {
            let separator = if prompt.ends_with(": ") { 0 } else { 2 };
            if prompt.len() + separator + term.len() > budget {
                break;
            }
            if separator == 2 {
                prompt.push_str(", ");
            }
            prompt.push_str(term);
        }
        if prompt.ends_with(": ") {
            None
        } else {
            Some(prompt)
        }
    }

    /// Apply every replacement to `text`.
    ///
    /// Matching is case-insensitive for ASCII and respects word boundaries, so
    /// `cat` does not rewrite `category`. Longer phrases are applied first, so
    /// `New York` wins over `York`.
    pub fn apply(&self, text: &str) -> String {
        if self.replacements.is_empty() {
            return text.to_string();
        }
        let mut pairs: Vec<&Replacement> = self
            .replacements
            .iter()
            .filter(|pair| !pair.from.trim().is_empty())
            .collect();
        pairs.sort_by_key(|pair| std::cmp::Reverse(pair.from.trim().len()));

        let mut out = text.to_string();
        for pair in pairs {
            out = replace_bounded(&out, pair.from.trim(), &pair.to);
        }
        out
    }

    /// Build from the two dialog fields.
    ///
    /// Terms are separated by commas or newlines. Replacements are `wrong =
    /// right` pairs, separated the same way.
    pub fn from_fields(terms: &str, replacements: &str) -> Self {
        let terms = terms
            .split([',', '\n'])
            .map(str::trim)
            .filter(|term| !term.is_empty())
            .map(str::to_string)
            .collect();

        let replacements = replacements
            .split(['\n', ','])
            .filter_map(|pair| {
                let (from, to) = pair.split_once('=')?;
                let from = from.trim();
                if from.is_empty() {
                    return None;
                }
                Some(Replacement {
                    from: from.to_string(),
                    to: to.trim().to_string(),
                })
            })
            .collect();

        Self {
            terms,
            replacements,
        }
    }

    /// The dialog's two fields, in the shape [`Self::from_fields`] reads.
    pub fn to_fields(&self) -> (String, String) {
        let terms = self.terms.join(", ");
        let replacements = self
            .replacements
            .iter()
            .map(|pair| format!("{} = {}", pair.from, pair.to))
            .collect::<Vec<_>>()
            .join(", ");
        (terms, replacements)
    }
}

/// Replace every whole-word, ASCII-case-insensitive occurrence of `needle`.
fn replace_bounded(haystack: &str, needle: &str, replacement: &str) -> String {
    let needle = needle.as_bytes();
    if needle.is_empty() {
        return haystack.to_string();
    }
    let hay = haystack.as_bytes();
    let mut out = String::with_capacity(haystack.len());
    let mut index = 0;
    while index < hay.len() {
        let end = index + needle.len();
        let matches = end <= hay.len()
            && hay[index..end].eq_ignore_ascii_case(needle)
            && boundary_before(haystack, index)
            && boundary_after(haystack, end);
        if matches {
            out.push_str(replacement);
            index = end;
        } else {
            let character = haystack[index..].chars().next().unwrap_or('\u{fffd}');
            out.push(character);
            index += character.len_utf8();
        }
    }
    out
}

fn boundary_before(text: &str, index: usize) -> bool {
    text[..index]
        .chars()
        .next_back()
        .is_none_or(|character| !character.is_alphanumeric())
}

fn boundary_after(text: &str, index: usize) -> bool {
    text[index..]
        .chars()
        .next()
        .is_none_or(|character| !character.is_alphanumeric())
}

/// Where the vocabulary file sits, for the diagnostics table.
pub fn describe_path() -> Option<PathBuf> {
    Vocabulary::path()
}

/// Read the vocabulary from an explicit path, for tests and tooling.
pub fn load_from(path: &Path) -> Vocabulary {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vocabulary(terms: &[&str], replacements: &[(&str, &str)]) -> Vocabulary {
        Vocabulary {
            terms: terms.iter().map(|term| term.to_string()).collect(),
            replacements: replacements
                .iter()
                .map(|(from, to)| Replacement {
                    from: from.to_string(),
                    to: to.to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn replacements_respect_word_boundaries() {
        let vocab = vocabulary(&[], &[("cat", "dog")]);
        assert_eq!(vocab.apply("the cat sat"), "the dog sat");
        assert_eq!(vocab.apply("category"), "category");
        assert_eq!(vocab.apply("Cat."), "dog.");
    }

    #[test]
    fn replacements_are_case_insensitive_and_keep_the_replacement_case() {
        let vocab = vocabulary(&[], &[("acme corp", "ACME Corp")]);
        assert_eq!(
            vocab.apply("Acme Corp and ACME CORP"),
            "ACME Corp and ACME Corp"
        );
    }

    #[test]
    fn longer_phrases_win_over_shorter_ones() {
        let vocab = vocabulary(&[], &[("york", "York"), ("new york", "New York")]);
        assert_eq!(vocab.apply("new york city"), "New York city");
    }

    #[test]
    fn punctuation_inside_a_term_still_matches() {
        let vocab = vocabulary(&[], &[("c++", "C++")]);
        assert_eq!(vocab.apply("we use c++ here"), "we use C++ here");
    }

    #[test]
    fn prompt_dedupes_and_skips_blank_terms() {
        let vocab = vocabulary(&["Kubernetes", " kubernetes ", "", "Siobhán"], &[]);
        let prompt = vocab.prompt(PROMPT_BUDGET).unwrap();
        assert_eq!(prompt.matches("Kubernetes").count(), 1);
        assert!(prompt.contains("Siobhán"));
    }

    #[test]
    fn prompt_stays_inside_its_budget() {
        let terms: Vec<String> = (0..200).map(|index| format!("term{index}")).collect();
        let vocab = Vocabulary {
            terms,
            replacements: Vec::new(),
        };
        let prompt = vocab.prompt(120).unwrap();
        assert!(prompt.len() <= 120, "{prompt}");
        assert!(prompt.ends_with("term9") || prompt.len() < 120);
    }

    #[test]
    fn no_terms_means_no_prompt() {
        assert_eq!(vocabulary(&["", " "], &[]).prompt(PROMPT_BUDGET), None);
    }

    #[test]
    fn fields_round_trip() {
        let vocab = Vocabulary::from_fields(
            "Kubernetes, Siobhán\nACME",
            "pie torch = PyTorch, acme = ACME Corp",
        );
        assert_eq!(vocab.terms, vec!["Kubernetes", "Siobhán", "ACME"]);
        assert_eq!(vocab.replacements.len(), 2);
        assert_eq!(vocab.replacements[1].to, "ACME Corp");
        let (terms, replacements) = vocab.to_fields();
        assert_eq!(Vocabulary::from_fields(&terms, &replacements), vocab);
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = std::env::temp_dir().join(format!(
            "hippo-vocabulary-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("vocabulary.json");
        let vocab = vocabulary(&["Kubernetes"], &[("pie torch", "PyTorch")]);
        let text = serde_json::to_string_pretty(&vocab).unwrap();
        std::fs::write(&path, text).unwrap();

        assert_eq!(load_from(&path), vocab);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
