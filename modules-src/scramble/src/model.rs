//! Scramble's pure parts: word lists, scrambling, judging, hints, and careers. No host calls.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::OnceLock;

/// Seconds or less that count as a quick solve.
pub(crate) const QUICK_SECONDS: i64 = 3;

fn lines(raw: &'static str, length: usize) -> Vec<&'static str> {
    raw.lines()
        .map(str::trim)
        .filter(|word| word.len() == length && word.bytes().all(|byte| byte.is_ascii_lowercase()))
        .collect()
}

/// Common words to set, by length (wordle's curated answer lists).
pub(crate) fn answers(length: usize) -> &'static [&'static str] {
    static FIVE: OnceLock<Vec<&str>> = OnceLock::new();
    static SIX: OnceLock<Vec<&str>> = OnceLock::new();
    static SEVEN: OnceLock<Vec<&str>> = OnceLock::new();
    match length {
        5 => FIVE.get_or_init(|| lines(include_str!("../../../wordle-5-letter-answers.txt"), 5)),
        6 => SIX.get_or_init(|| lines(include_str!("../../../wordle-six-letter-answers.txt"), 6)),
        _ => SEVEN.get_or_init(|| lines(include_str!("../../../wordle-7-letter-answers.txt"), 7)),
    }
}

/// Every word accepted as a solution, by length (wordle's full guess lists plus the answers).
pub(crate) fn valid(length: usize) -> &'static HashSet<&'static str> {
    static FIVE: OnceLock<HashSet<&str>> = OnceLock::new();
    static SIX: OnceLock<HashSet<&str>> = OnceLock::new();
    static SEVEN: OnceLock<HashSet<&str>> = OnceLock::new();
    let build = |raw: &'static str| {
        let mut set = lines(raw, length).into_iter().collect::<HashSet<_>>();
        set.extend(answers(length));
        set
    };
    match length {
        5 => FIVE.get_or_init(|| build(include_str!("../../../wordle-5-letter-words.txt"))),
        6 => SIX.get_or_init(|| build(include_str!("../../../wordle-six-letter-words.txt"))),
        _ => SEVEN.get_or_init(|| build(include_str!("../../../wordle-7-letter-words.txt"))),
    }
}

fn sorted(word: &str) -> Vec<u8> {
    let mut letters = word.bytes().collect::<Vec<_>>();
    letters.sort_unstable();
    letters
}

/// Shuffles `word` with `draw` (uniform in `0..n`) until the result is neither the word nor any
/// other real word. `None` if no such order turns up (only for words with very few distinct
/// letters).
pub(crate) fn scramble(word: &str, draw: &mut dyn FnMut(u32) -> u32) -> Option<String> {
    let valid = valid(word.len());
    let mut letters = word.bytes().collect::<Vec<_>>();
    for _ in 0..50 {
        for index in (1..letters.len()).rev() {
            let other = draw(index as u32 + 1) as usize;
            letters.swap(index, other);
        }
        let candidate = String::from_utf8(letters.clone()).ok()?;
        if candidate != word && !valid.contains(candidate.as_str()) {
            return Some(candidate);
        }
    }
    None
}

/// Whether a line solves the scramble: the same letters, making a real word.
pub(crate) fn solves(word: &str, line: &str) -> bool {
    let guess = line
        .trim()
        .trim_end_matches(['!', '.', '?'])
        .to_ascii_lowercase();
    guess.len() == word.len()
        && guess.bytes().all(|byte| byte.is_ascii_lowercase())
        && sorted(&guess) == sorted(word)
        && (guess == word || valid(word.len()).contains(guess.as_str()))
}

/// "L T E R B U".
pub(crate) fn spaced(letters: &str) -> String {
    letters
        .chars()
        .map(|ch| ch.to_ascii_uppercase().to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

/// "B _ _ _ _ R": the first and last letters.
pub(crate) fn hint(word: &str) -> String {
    let last = word.len().saturating_sub(1);
    word.chars()
        .enumerate()
        .map(|(index, ch)| {
            if index == 0 || index == last {
                ch.to_ascii_uppercase().to_string()
            } else {
                "_".into()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A player's scrambles in one channel.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Career {
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) solved: u64,
    /// Fastest solve in seconds; `None` until the first.
    #[serde(default)]
    pub(crate) best_seconds: Option<i64>,
    #[serde(default)]
    pub(crate) week: i64,
    #[serde(default)]
    pub(crate) week_solved: u64,
}

impl Career {
    /// Records a solve; true if it was a personal best.
    pub(crate) fn add(&mut self, name: &str, seconds: i64, now: i64) -> bool {
        let week = week_of(now);
        if self.week != week {
            self.week = week;
            self.week_solved = 0;
        }
        self.name = name.chars().take(64).collect();
        self.solved += 1;
        self.week_solved += 1;
        let best = self.best_seconds.is_none_or(|best| seconds < best);
        if best {
            self.best_seconds = Some(seconds);
        }
        best
    }

    pub(crate) fn solved_this_week(&self, now: i64) -> u64 {
        if self.week == week_of(now) {
            self.week_solved
        } else {
            0
        }
    }
}

/// Monday-based week number in UTC.
pub(crate) fn week_of(unix: i64) -> i64 {
    (unix.div_euclid(86_400) + 3).div_euclid(7)
}

/// Whether a channel is lively enough for a pop-up: at least `lines` lines from at least two
/// people within the window.
pub(crate) fn lively(recent: &[(i64, String)], now: i64, window: i64, lines: usize) -> bool {
    let within = recent
        .iter()
        .filter(|(at, _)| now - at <= window)
        .collect::<Vec<_>>();
    let people = within.iter().map(|(_, who)| who).collect::<HashSet<_>>();
    within.len() >= lines && people.len() >= 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_load_and_answers_are_valid() {
        for length in 5..=7 {
            assert!(answers(length).len() >= 800, "{length}");
            assert!(answers(length)
                .iter()
                .all(|word| valid(length).contains(word)));
        }
    }

    #[test]
    fn scrambles_are_never_a_real_word() {
        let mut state = 7u32;
        let mut draw = |n: u32| {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (state >> 8) % n
        };
        for word in ["butler", "listen", "stone", "earnest", "silent"] {
            let scrambled = scramble(word, &mut draw).unwrap();
            assert_ne!(scrambled, word);
            assert!(
                !valid(word.len()).contains(scrambled.as_str()),
                "{word} → {scrambled}"
            );
            assert_eq!(sorted(&scrambled), sorted(word));
        }
    }

    #[test]
    fn any_real_anagram_solves_it() {
        assert!(solves("listen", "LISTEN"));
        assert!(
            solves("listen", "silent!"),
            "another real word from the same letters"
        );
        assert!(!solves("listen", "tinsel s"));
        assert!(!solves("listen", "nilest"), "same letters, not a word");
        assert!(!solves("listen", "listens"));
        assert!(!solves("stone", "stone the crows"));
    }

    #[test]
    fn hints_careers_and_liveliness() {
        assert_eq!(spaced("ltrebu"), "L T R E B U");
        assert_eq!(hint("butler"), "B _ _ _ _ R");
        let monday = 20_360 * 86_400;
        let mut career = Career::default();
        assert!(career.add("ann", 9, monday));
        assert!(!career.add("ann", 12, monday));
        assert!(career.add("ann", 4, monday));
        assert_eq!((career.solved, career.best_seconds), (3, Some(4)));
        assert_eq!(career.solved_this_week(monday + 7 * 86_400), 0);
        let recent = vec![
            (100, "a".to_string()),
            (110, "b".to_string()),
            (120, "a".to_string()),
            (130, "a".to_string()),
            (140, "b".to_string()),
        ];
        assert!(lively(&recent, 150, 600, 5));
        assert!(!lively(&recent, 150, 600, 6));
        assert!(
            !lively(&recent[..1], 150, 600, 1),
            "one person isn't a crowd"
        );
        assert!(!lively(&recent, 900, 600, 5), "too long ago");
    }
}
