//! Trivia's pure parts: questions, answer matching, hints, scoring, and careers. No host calls,
//! so everything here is tested natively.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Points for a correct answer before the hint, and after it.
pub(crate) const POINTS_EARLY: u64 = 10;
pub(crate) const POINTS_LATE: u64 = 5;
/// Extra points per answer already in a streak, and the most a streak adds.
const STREAK_STEP: u64 = 2;
const STREAK_CAP: u64 = 6;
/// Questions nobody answers in a row before a round stops by itself.
pub(crate) const IDLE_LIMIT: u32 = 3;
/// Streak length that counts as "on fire".
pub(crate) const HOT_STREAK: u32 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Kind {
    /// Typed answers; `accepted` lists every acceptable form.
    Free,
    /// Lettered options; the answer is a letter or the option's text.
    Choice,
    /// True or false.
    TrueFalse,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Question {
    /// Stable identity for "recently asked": a pack id, or a hash of fetched text.
    pub(crate) id: String,
    pub(crate) category: String,
    pub(crate) text: String,
    pub(crate) kind: Kind,
    /// The answer as shown when revealed.
    pub(crate) answer: String,
    pub(crate) accepted: Vec<String>,
    /// Choice options in display order (A, B, …); empty otherwise.
    #[serde(default)]
    pub(crate) options: Vec<String>,
    /// Fetched from Open Trivia DB (credited when asked).
    #[serde(default)]
    pub(crate) fetched: bool,
}

/// One entry of the bundled pack.
#[derive(Deserialize)]
pub(crate) struct PackEntry {
    pub(crate) id: String,
    pub(crate) c: String,
    pub(crate) q: String,
    pub(crate) a: String,
    #[serde(default)]
    pub(crate) alt: Vec<String>,
}

impl PackEntry {
    pub(crate) fn question(&self) -> Question {
        let mut accepted = vec![self.a.clone()];
        accepted.extend(self.alt.iter().cloned());
        Question {
            id: self.id.clone(),
            category: self.c.clone(),
            text: self.q.clone(),
            kind: Kind::Free,
            answer: self.a.clone(),
            accepted,
            options: Vec::new(),
            fetched: false,
        }
    }
}

/// A fetched multiple-choice or true/false question, options placed by `position` (the correct
/// answer's slot among the choices).
pub(crate) fn fetched_question(
    category: &str,
    text: &str,
    kind: &str,
    correct: &str,
    incorrect: &[String],
    position: usize,
) -> Option<Question> {
    let id = format!("otdb:{:016x}", fnv(text));
    if kind == "boolean" {
        let answer = if correct.eq_ignore_ascii_case("true") {
            "True"
        } else {
            "False"
        };
        return Some(Question {
            id,
            category: category.into(),
            text: text.into(),
            kind: Kind::TrueFalse,
            answer: answer.into(),
            accepted: vec![answer.into()],
            options: vec!["True".into(), "False".into()],
            fetched: true,
        });
    }
    if incorrect.is_empty() || incorrect.len() > 5 {
        return None;
    }
    let mut options = incorrect.to_vec();
    let slot = position % (options.len() + 1);
    options.insert(slot, correct.into());
    Some(Question {
        id,
        category: category.into(),
        text: text.into(),
        kind: Kind::Choice,
        answer: format!("{}) {correct}", letter(slot)),
        accepted: vec![correct.into()],
        options,
        fetched: true,
    })
}

pub(crate) fn letter(index: usize) -> char {
    (b'A' + index as u8) as char
}

fn fnv(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Lowercase, accents folded, punctuation dropped, spaces collapsed, and a leading "the", "a",
/// or "an" removed.
pub(crate) fn normalize(text: &str) -> String {
    let folded = text
        .chars()
        .flat_map(char::to_lowercase)
        .map(|ch| match ch {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => 'a',
            'è' | 'é' | 'ê' | 'ë' => 'e',
            'ì' | 'í' | 'î' | 'ï' => 'i',
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' => 'o',
            'ù' | 'ú' | 'û' | 'ü' => 'u',
            'ñ' => 'n',
            'ç' => 'c',
            'ý' | 'ÿ' => 'y',
            other => other,
        })
        .map(|ch| if ch.is_alphanumeric() { ch } else { ' ' })
        .collect::<String>();
    // Keep "26.2" and "3,600" whole rather than splitting them at the separator.
    let joined = join_number_separators(text, &folded);
    let words = joined.split_whitespace().collect::<Vec<_>>();
    let words = match words.first() {
        Some(&("the" | "a" | "an")) if words.len() > 1 => &words[1..],
        _ => &words[..],
    };
    words.join(" ")
}

fn join_number_separators(original: &str, folded: &str) -> String {
    let original = original
        .chars()
        .flat_map(char::to_lowercase)
        .collect::<Vec<_>>();
    let folded = folded.chars().collect::<Vec<_>>();
    if original.len() != folded.len() {
        return folded.into_iter().collect();
    }
    let mut out = String::new();
    for (index, ch) in folded.iter().enumerate() {
        let between_digits = index > 0
            && index + 1 < folded.len()
            && folded[index - 1].is_ascii_digit()
            && folded[index + 1].is_ascii_digit();
        match original[index] {
            // "3,600" is 3600, but "26.2" keeps its point.
            ',' if between_digits => continue,
            '.' if between_digits => out.push('.'),
            _ => out.push(*ch),
        }
    }
    out
}

fn edit_distance(a: &str, b: &str) -> usize {
    let a = a.chars().collect::<Vec<_>>();
    let b = b.chars().collect::<Vec<_>>();
    let mut previous = (0..=b.len()).collect::<Vec<_>>();
    for (i, ca) in a.iter().enumerate() {
        let mut current = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let substitute = previous[j] + usize::from(ca != cb);
            current.push(substitute.min(previous[j + 1] + 1).min(current[j] + 1));
        }
        previous = current;
    }
    previous[b.len()]
}

/// What a line means for the current question.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Guess {
    Correct,
    /// A wrong pick on a choice or true/false question: that guesser is out for this question.
    Wrong,
    /// Ordinary chat, or a wrong typed answer (typed answers can be tried freely).
    Ignored,
}

pub(crate) fn judge(question: &Question, line: &str) -> Guess {
    let said = normalize(line);
    if said.is_empty() || said.chars().count() > 80 {
        return Guess::Ignored;
    }
    match question.kind {
        Kind::Free => {
            let hit = question.accepted.iter().any(|accepted| {
                let accepted = normalize(accepted);
                let length = accepted.chars().count();
                // Typos are forgiven only in longer answers, and never in numbers.
                let allowed = if accepted.chars().any(|ch| ch.is_ascii_digit()) {
                    0
                } else if length >= 9 {
                    2
                } else if length >= 5 {
                    1
                } else {
                    0
                };
                said == accepted || (allowed > 0 && edit_distance(&said, &accepted) <= allowed)
            });
            if hit {
                Guess::Correct
            } else {
                Guess::Ignored
            }
        }
        Kind::TrueFalse => {
            let pick = match said.as_str() {
                "true" | "t" | "yes" | "y" => "true",
                "false" | "f" | "no" | "n" => "false",
                _ => return Guess::Ignored,
            };
            if pick == normalize(&question.answer) {
                Guess::Correct
            } else {
                Guess::Wrong
            }
        }
        Kind::Choice => {
            let picked = if said.chars().count() == 1 {
                let index = (said.chars().next().unwrap_or(' ') as u8).wrapping_sub(b'a') as usize;
                if index >= question.options.len() {
                    return Guess::Ignored;
                }
                index
            } else {
                match question
                    .options
                    .iter()
                    .position(|option| normalize(option) == said)
                {
                    Some(index) => index,
                    None => return Guess::Ignored,
                }
            };
            let correct = question
                .options
                .iter()
                .position(|option| option == &question.accepted[0]);
            if Some(picked) == correct {
                Guess::Correct
            } else {
                Guess::Wrong
            }
        }
    }
}

/// A hint for a typed answer: first letters and blanks, or the length for very short answers.
pub(crate) fn letters_hint(answer: &str) -> String {
    let letters = answer.chars().filter(|ch| ch.is_alphanumeric()).count();
    if letters <= 2 {
        return format!("{letters} character{}", if letters == 1 { "" } else { "s" });
    }
    answer
        .split_whitespace()
        .map(|word| {
            word.chars()
                .enumerate()
                .map(|(index, ch)| {
                    if !ch.is_alphanumeric() {
                        ch.to_string()
                    } else if index == 0 {
                        ch.to_uppercase().to_string()
                    } else {
                        "_".into()
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("   ")
}

/// For a choice question, two wrong options to rule out (or one, with only three options).
pub(crate) fn ruled_out(question: &Question, pick: usize) -> Vec<char> {
    let wrong = question
        .options
        .iter()
        .enumerate()
        .filter(|(_, option)| **option != question.accepted[0])
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let take = if question.options.len() >= 4 { 2 } else { 1 };
    let start = pick % wrong.len().max(1);
    let mut chosen = (0..take)
        .filter_map(|offset| wrong.get((start + offset) % wrong.len().max(1)).copied())
        .collect::<Vec<_>>();
    chosen.sort_unstable();
    chosen.dedup();
    chosen.into_iter().map(letter).collect()
}

/// Points for a correct answer: faster before the hint, plus the streak bonus.
pub(crate) fn points(hinted: bool, streak: u32) -> u64 {
    let base = if hinted { POINTS_LATE } else { POINTS_EARLY };
    base + (u64::from(streak.saturating_sub(1)) * STREAK_STEP).min(STREAK_CAP)
}

/// One player's part in a round.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Entry {
    pub(crate) name: String,
    pub(crate) points: u64,
    pub(crate) correct: u32,
}

/// The round's winners (everyone tied on the top score), best first by name.
pub(crate) fn winners(scores: &BTreeMap<String, Entry>) -> Vec<(&String, &Entry)> {
    let best = scores.values().map(|entry| entry.points).max().unwrap_or(0);
    if best == 0 {
        return Vec::new();
    }
    let mut top = scores
        .iter()
        .filter(|(_, entry)| entry.points == best)
        .collect::<Vec<_>>();
    top.sort_by(|a, b| a.1.name.cmp(&b.1.name));
    top
}

/// Scores in order, best first.
pub(crate) fn standings(scores: &BTreeMap<String, Entry>) -> Vec<(&String, &Entry)> {
    let mut all = scores.iter().collect::<Vec<_>>();
    all.sort_by(|a, b| {
        b.1.points
            .cmp(&a.1.points)
            .then_with(|| a.1.name.cmp(&b.1.name))
    });
    all
}

/// A player's lifetime trivia in one channel.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Career {
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) points: u64,
    #[serde(default)]
    pub(crate) correct: u64,
    #[serde(default)]
    pub(crate) rounds_won: u64,
    #[serde(default)]
    pub(crate) best_streak: u32,
    /// Monday-based UTC week of `week_points`.
    #[serde(default)]
    pub(crate) week: i64,
    #[serde(default)]
    pub(crate) week_points: u64,
}

impl Career {
    pub(crate) fn add(&mut self, name: &str, points: u64, streak: u32, now: i64) {
        let week = week_of(now);
        if self.week != week {
            self.week = week;
            self.week_points = 0;
        }
        self.name = name.chars().take(64).collect();
        self.points += points;
        self.week_points += points;
        self.correct += 1;
        self.best_streak = self.best_streak.max(streak);
    }

    pub(crate) fn points_this_week(&self, now: i64) -> u64 {
        if self.week == week_of(now) {
            self.week_points
        } else {
            0
        }
    }
}

/// Monday-based week number in UTC.
pub(crate) fn week_of(unix: i64) -> i64 {
    (unix.div_euclid(86_400) + 3).div_euclid(7)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn free(answer: &str, alt: &[&str]) -> Question {
        PackEntry {
            id: "t".into(),
            c: "Test".into(),
            q: "?".into(),
            a: answer.into(),
            alt: alt.iter().map(|a| (*a).into()).collect(),
        }
        .question()
    }

    #[test]
    fn the_bundled_pack_parses_and_every_answer_is_accepted() {
        let pack: Vec<PackEntry> = serde_json::from_str(include_str!("../questions.json")).unwrap();
        assert!(pack.len() >= 300);
        let mut ids = std::collections::HashSet::new();
        for entry in &pack {
            assert!(ids.insert(entry.id.clone()), "duplicate {}", entry.id);
            let question = entry.question();
            assert!(!normalize(&entry.a).is_empty(), "{}", entry.id);
            assert_eq!(judge(&question, &entry.a), Guess::Correct, "{}", entry.id);
            for alt in &entry.alt {
                assert_eq!(judge(&question, alt), Guess::Correct, "{} {alt}", entry.id);
            }
        }
    }

    #[test]
    fn answers_match_forgivingly_but_not_loosely() {
        let q = free("The Sun", &["sun"]);
        assert_eq!(judge(&q, "the sun!"), Guess::Correct);
        assert_eq!(judge(&q, "Sun"), Guess::Correct);
        assert_eq!(judge(&q, "moon"), Guess::Ignored);
        let q = free("Tutankhamun", &[]);
        assert_eq!(
            judge(&q, "tutankamun"),
            Guess::Correct,
            "one slip in a long answer"
        );
        assert_eq!(judge(&q, "tootankamoon"), Guess::Ignored);
        let q = free("1945", &[]);
        assert_eq!(judge(&q, "1946"), Guess::Ignored, "numbers must be exact");
        let q = free("26.2", &[]);
        assert_eq!(judge(&q, "26.2"), Guess::Correct);
        assert_eq!(judge(&q, "262"), Guess::Ignored);
        let q = free("3600", &["3,600"]);
        assert_eq!(judge(&q, "3,600"), Guess::Correct);
        let q = free("Bjorn Borg", &["Borg"]);
        assert_eq!(judge(&q, "Björn Borg"), Guess::Correct, "accents fold");
        let q = free("Au", &[]);
        assert_eq!(judge(&q, "Ag"), Guess::Ignored, "no typos in short answers");
        assert_eq!(judge(&q, "a"), Guess::Ignored);
    }

    #[test]
    fn choices_take_a_letter_or_the_text_and_one_guess() {
        let q = fetched_question(
            "Science",
            "What does CPU stand for?",
            "multiple",
            "Central Processing Unit",
            &[
                "Computer Personal Unit".into(),
                "Central Process Unit".into(),
                "Core Power Unit".into(),
            ],
            1,
        )
        .unwrap();
        assert_eq!(q.options[1], "Central Processing Unit");
        assert_eq!(q.answer, "B) Central Processing Unit");
        assert_eq!(judge(&q, "b"), Guess::Correct);
        assert_eq!(judge(&q, "B!"), Guess::Correct);
        assert_eq!(judge(&q, "central processing unit"), Guess::Correct);
        assert_eq!(judge(&q, "a"), Guess::Wrong);
        assert_eq!(judge(&q, "Core Power Unit"), Guess::Wrong);
        assert_eq!(judge(&q, "e"), Guess::Ignored, "no option E");
        assert_eq!(judge(&q, "hmm, tricky"), Guess::Ignored);
        let ruled = ruled_out(&q, 5);
        assert_eq!(ruled.len(), 2);
        assert!(!ruled.contains(&'B'));
        let q = fetched_question(
            "History",
            "The sky is green.",
            "boolean",
            "False",
            &["True".into()],
            0,
        )
        .unwrap();
        assert_eq!(judge(&q, "false"), Guess::Correct);
        assert_eq!(judge(&q, "no"), Guess::Correct);
        assert_eq!(judge(&q, "true"), Guess::Wrong);
        assert_eq!(judge(&q, "maybe"), Guess::Ignored);
    }

    #[test]
    fn hints_scoring_and_winners() {
        assert_eq!(letters_hint("Tutankhamun"), "T _ _ _ _ _ _ _ _ _ _");
        assert_eq!(letters_hint("Big Ben"), "B _ _   B _ _");
        assert_eq!(letters_hint("Au"), "2 characters");
        assert_eq!(letters_hint("Rubik's Cube"), "R _ _ _ _ ' _   C _ _ _");
        assert_eq!(points(false, 1), 10);
        assert_eq!(points(true, 1), 5);
        assert_eq!(points(false, 3), 14);
        assert_eq!(points(false, 9), 16, "streak bonus is capped");
        let mut scores = BTreeMap::new();
        scores.insert(
            "a".to_string(),
            Entry {
                name: "ann".into(),
                points: 20,
                correct: 2,
            },
        );
        scores.insert(
            "b".to_string(),
            Entry {
                name: "bob".into(),
                points: 20,
                correct: 2,
            },
        );
        scores.insert(
            "c".to_string(),
            Entry {
                name: "cy".into(),
                points: 5,
                correct: 1,
            },
        );
        let won = winners(&scores);
        assert_eq!(
            won.iter().map(|(_, e)| e.name.as_str()).collect::<Vec<_>>(),
            ["ann", "bob"]
        );
        assert_eq!(standings(&scores)[2].1.name, "cy");
        assert!(winners(&BTreeMap::new()).is_empty());
    }

    #[test]
    fn careers_keep_the_week_apart() {
        let monday = 20_360 * 86_400; // 2025-09-29, a Monday
        let mut career = Career::default();
        career.add("ann", 10, 1, monday);
        career.add("ann", 12, 2, monday + 86_400);
        assert_eq!(
            (career.points, career.correct, career.best_streak),
            (22, 2, 2)
        );
        assert_eq!(career.points_this_week(monday + 2 * 86_400), 22);
        assert_eq!(career.points_this_week(monday + 7 * 86_400), 0);
        career.add("ann", 5, 1, monday + 7 * 86_400);
        assert_eq!((career.points, career.week_points), (27, 5));
    }
}
