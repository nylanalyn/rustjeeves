//! Six-letter Wordle game for rustjeeves.
//!
//! One game per channel at a time. Word list is bundled at compile time and cycled sequentially
//! so every word gets used before repeating. Stats (wins/losses/streak/guess distribution) are
//! tracked per nick per server.
//!
//! Commands: !wordle  !guess <word>  !wordlestats (alias !wstats)

use extism_pdk::*;
use jeeves_abi::{
    CommandManifest, CommandSpec, Event, EventEnvelope, KvGet, KvSet, SendMessage,
    COMMAND_MANIFEST_VERSION,
};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn now(input: String) -> String;
}

// ── commands manifest ────────────────────────────────────────────────────────

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    let c = |name: &str, desc: &str, usage: &str| CommandSpec {
        name: name.into(),
        description: desc.into(),
        usage: usage.into(),
        ..Default::default()
    };
    let mut wstats = c("wordlestats", "Show your Wordle stats.", "!wordlestats");
    wstats.aliases = vec!["wstats".into()];
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![
            c("wordle", "Start a 6-letter Wordle game in this channel.", "!wordle"),
            c("guess", "Guess a word in the active Wordle.", "!guess <word>"),
            wstats,
        ],
    })?)
}

// ── host helpers ─────────────────────────────────────────────────────────────

fn reply(server: &str, target: &str, text: &str) -> Result<(), Error> {
    unsafe {
        send_message(serde_json::to_string(&SendMessage {
            server: server.into(),
            target: target.into(),
            text: text.into(),
        })?)?;
    }
    Ok(())
}

fn kv_load(key: &str) -> Result<String, Error> {
    Ok(unsafe { kv_get(serde_json::to_string(&KvGet { key: key.into() })?)? })
}

fn kv_save(key: &str, value: &str) -> Result<(), Error> {
    unsafe {
        kv_set(serde_json::to_string(&KvSet {
            key: key.into(),
            value: value.into(),
        })?)?;
    }
    Ok(())
}

fn now_secs() -> i64 {
    unsafe { now(String::new()) }
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

// ── word list ─────────────────────────────────────────────────────────────────

const WORDS_RAW: &str = include_str!("../../../wordle-six-letter-words.txt");

fn words() -> &'static [&'static str] {
    static WORDS: OnceLock<Vec<&'static str>> = OnceLock::new();
    WORDS.get_or_init(|| WORDS_RAW.lines().filter(|l| l.len() == 6).collect())
}

fn is_valid_word(w: &str) -> bool {
    // Word list is alphabetically sorted; binary search is O(log n).
    words().binary_search_by(|probe| probe.cmp(&w)).is_ok()
}

fn pick_word() -> Result<String, Error> {
    let n = words().len() as u64;
    let raw = kv_load("word_idx")?;
    let idx: u64 = if raw.is_empty() {
        // Seed the first game's position from the current time so it's not always "aahing".
        (now_secs() as u64) % n
    } else {
        raw.trim().parse::<u64>().unwrap_or(0)
    };
    let word = words()[(idx % n) as usize].to_string();
    kv_save("word_idx", &((idx + 1) % n).to_string())?;
    Ok(word)
}

// ── game state ────────────────────────────────────────────────────────────────

const MAX_GUESSES: usize = 6;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct GameState {
    word: String,
    guesses: Vec<String>,
    started_by: String,
}

fn game_key(server: &str, channel: &str) -> String {
    format!("game:{server}:{channel}")
}

fn load_game(server: &str, channel: &str) -> Result<Option<GameState>, Error> {
    let raw = kv_load(&game_key(server, channel))?;
    if raw.is_empty() {
        Ok(None)
    } else {
        Ok(serde_json::from_str(&raw).ok())
    }
}

fn save_game(server: &str, channel: &str, state: &GameState) -> Result<(), Error> {
    kv_save(&game_key(server, channel), &serde_json::to_string(state)?)
}

fn clear_game(server: &str, channel: &str) -> Result<(), Error> {
    kv_save(&game_key(server, channel), "")
}

// ── stats ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Stats {
    wins: u32,
    losses: u32,
    #[serde(default)]
    guess_dist: Vec<u32>, // index 0 = won in 1 guess, ..., index 5 = won in 6 guesses
    #[serde(default)]
    streak: u32,
    #[serde(default)]
    best_streak: u32,
}

impl Stats {
    fn record_win(&mut self, guess_count: usize) {
        self.wins += 1;
        self.streak += 1;
        if self.streak > self.best_streak {
            self.best_streak = self.streak;
        }
        let idx = guess_count.saturating_sub(1);
        if self.guess_dist.len() <= idx {
            self.guess_dist.resize(idx + 1, 0);
        }
        self.guess_dist[idx] += 1;
    }

    fn record_loss(&mut self) {
        self.losses += 1;
        self.streak = 0;
    }
}

fn stats_key(server: &str, nick: &str) -> String {
    format!("stats:{server}:{nick}")
}

fn load_stats(server: &str, nick: &str) -> Result<Stats, Error> {
    let raw = kv_load(&stats_key(server, nick))?;
    if raw.is_empty() {
        Ok(Stats::default())
    } else {
        Ok(serde_json::from_str(&raw).unwrap_or_default())
    }
}

fn save_stats(server: &str, nick: &str, stats: &Stats) -> Result<(), Error> {
    kv_save(&stats_key(server, nick), &serde_json::to_string(stats)?)
}

// ── scoring and rendering ─────────────────────────────────────────────────────

// Returns one value per position: 2 = correct, 1 = wrong position, 0 = not in word.
fn score_guess(guess: &[char; 6], answer: &[char; 6]) -> [u8; 6] {
    let mut result = [0u8; 6];
    let mut answer_used = [false; 6];
    // First pass: exact matches.
    for i in 0..6 {
        if guess[i] == answer[i] {
            result[i] = 2;
            answer_used[i] = true;
        }
    }
    // Second pass: present-but-wrong-position (consume each answer letter at most once).
    for i in 0..6 {
        if result[i] == 2 {
            continue;
        }
        for j in 0..6 {
            if !answer_used[j] && guess[i] == answer[j] {
                result[i] = 1;
                answer_used[j] = true;
                break;
            }
        }
    }
    result
}

fn to_chars6(s: &str) -> [char; 6] {
    let mut it = s.chars();
    std::array::from_fn(|_| it.next().unwrap_or(' '))
}

// Render one row: IRC-colored uppercase letters + emoji squares.
// Correct  → green  (\x03 03) + 🟩
// Present  → yellow (\x03 08) + 🟨
// Absent   → plain  + ⬛
fn render_row(guess: &str, score: &[u8; 6]) -> String {
    let letters: Vec<char> = guess.chars().collect();
    let mut letters_part = String::new();
    let mut emoji_part = String::new();
    for (i, &s) in score.iter().enumerate() {
        let upper = letters[i].to_ascii_uppercase();
        match s {
            2 => letters_part.push_str(&format!("\x0303{upper}\x03")),
            1 => letters_part.push_str(&format!("\x0308{upper}\x03")),
            _ => letters_part.push(upper),
        }
        emoji_part.push(match s {
            2 => '🟩',
            1 => '🟨',
            _ => '⬛',
        });
    }
    format!("{letters_part} {emoji_part}")
}

// ── event handler ─────────────────────────────────────────────────────────────

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let envelope: EventEnvelope = serde_json::from_str(&input)?;
    let Event::Message(msg) = envelope.event else {
        return Ok(());
    };
    if msg.is_private {
        return Ok(());
    }

    let server = &envelope.server;
    let channel = &msg.target;
    let nick = &msg.nick;
    let text = msg.text.trim();

    // Split the text into command + argument on the first whitespace run.
    let (cmd, arg) = match text.find(|c: char| c.is_whitespace()) {
        Some(i) => (&text[..i], text[i..].trim()),
        None => (text, ""),
    };

    match cmd.to_ascii_lowercase().as_str() {
        "!wordle" => cmd_start(server, channel, nick)?,
        "!guess" => cmd_guess(server, channel, nick, arg)?,
        "!wordlestats" | "!wstats" => cmd_stats(server, channel, nick)?,
        _ => {}
    }

    Ok(())
}

fn cmd_start(server: &str, channel: &str, nick: &str) -> Result<(), Error> {
    if let Some(game) = load_game(server, channel)? {
        reply(
            server,
            channel,
            &format!(
                "A Wordle is already in progress ({}/{} guesses). Use !guess <word>.",
                game.guesses.len(),
                MAX_GUESSES
            ),
        )?;
        return Ok(());
    }
    let word = pick_word()?;
    save_game(
        server,
        channel,
        &GameState {
            word,
            guesses: vec![],
            started_by: nick.to_string(),
        },
    )?;
    reply(
        server,
        channel,
        &format!("🟩 New Wordle started! Guess the 6-letter word. You have {MAX_GUESSES} tries — !guess <word>"),
    )?;
    Ok(())
}

fn cmd_guess(server: &str, channel: &str, nick: &str, raw: &str) -> Result<(), Error> {
    let Some(mut game) = load_game(server, channel)? else {
        reply(server, channel, "No Wordle in progress. Start one with !wordle")?;
        return Ok(());
    };

    let word_lower = raw.to_ascii_lowercase();
    let word = word_lower.trim();

    if word.len() != 6 || !word.chars().all(|c| c.is_ascii_alphabetic()) {
        reply(server, channel, "Guess must be exactly 6 letters.")?;
        return Ok(());
    }
    if !is_valid_word(word) {
        reply(server, channel, &format!("'{word}' isn't in the word list."))?;
        return Ok(());
    }
    if game.guesses.iter().any(|g| g == word) {
        reply(server, channel, &format!("'{word}' was already guessed."))?;
        return Ok(());
    }

    let score = score_guess(&to_chars6(word), &to_chars6(&game.word));
    game.guesses.push(word.to_string());
    let guess_num = game.guesses.len();
    let row = render_row(word, &score);
    let is_win = score.iter().all(|&s| s == 2);

    if is_win {
        clear_game(server, channel)?;
        let mut stats = load_stats(server, nick)?;
        stats.record_win(guess_num);
        save_stats(server, nick, &stats)?;
        reply(server, channel, &format!("{row} ({guess_num}/{MAX_GUESSES})"))?;
        reply(
            server,
            channel,
            &format!(
                "🎉 {} got it in {}/{}! The word was {}.",
                nick,
                guess_num,
                MAX_GUESSES,
                game.word.to_ascii_uppercase()
            ),
        )?;
    } else if guess_num >= MAX_GUESSES {
        clear_game(server, channel)?;
        let mut stats = load_stats(server, nick)?;
        stats.record_loss();
        save_stats(server, nick, &stats)?;
        reply(server, channel, &format!("{row} ({guess_num}/{MAX_GUESSES})"))?;
        reply(
            server,
            channel,
            &format!(
                "Game over! The word was {}.",
                game.word.to_ascii_uppercase()
            ),
        )?;
    } else {
        save_game(server, channel, &game)?;
        let remaining = MAX_GUESSES - guess_num;
        reply(
            server,
            channel,
            &format!(
                "{row} ({guess_num}/{MAX_GUESSES}, {} left)",
                if remaining == 1 { "1 guess".to_string() } else { format!("{remaining} guesses") }
            ),
        )?;
    }

    Ok(())
}

fn cmd_stats(server: &str, channel: &str, nick: &str) -> Result<(), Error> {
    let stats = load_stats(server, nick)?;
    let total = stats.wins + stats.losses;
    if total == 0 {
        reply(server, channel, &format!("{nick}: No Wordle games played yet."))?;
        return Ok(());
    }
    let pct = stats.wins * 100 / total;
    let dist: String = stats
        .guess_dist
        .iter()
        .enumerate()
        .filter(|(_, &c)| c > 0)
        .map(|(i, &c)| format!("{}:{}", i + 1, c))
        .collect::<Vec<_>>()
        .join(" ");
    reply(
        server,
        channel,
        &format!(
            "{nick}: Wordle — {}/{} ({pct}%) | streak {} best {} | {}",
            stats.wins,
            total,
            stats.streak,
            stats.best_streak,
            if dist.is_empty() { "—".to_string() } else { dist }
        ),
    )?;
    Ok(())
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{render_row, score_guess, to_chars6};

    #[test]
    fn score_correct_position() {
        let score = score_guess(&to_chars6("crane"), &to_chars6("crane"));
        assert_eq!(score, [2, 2, 2, 2, 2, 2]);
    }

    #[test]
    fn score_duplicate_letter_exhaustion() {
        // answer = "aaaaab", guess = "bbbbbb"
        // Only one 'b' in the answer (at pos 5); the first pass matches it at pos 5 (green).
        // The remaining 'b's in the guess find no unused 'b' in the answer → all gray.
        let score = score_guess(&to_chars6("bbbbbb"), &to_chars6("aaaaab"));
        assert_eq!(score, [0, 0, 0, 0, 0, 2]);
    }

    #[test]
    fn score_duplicate_letter_in_guess() {
        // guess = "street", answer = "crates"
        // Exact matches first: e[4] in guess matches e[4] in answer → green.
        // Then wrong-position pass:
        //   s[0] → finds s at answer[5] → yellow; answer[5] consumed.
        //   t[1] → finds t at answer[3] → yellow; answer[3] consumed.
        //   r[2] → finds r at answer[1] → yellow; answer[1] consumed.
        //   e[3] → answer[4] already consumed; no other e → gray.
        //   t[5] → answer[3] already consumed; no other t → gray.
        let score = score_guess(&to_chars6("street"), &to_chars6("crates"));
        assert_eq!(score, [1, 1, 1, 0, 2, 0]);
    }

    #[test]
    fn render_row_produces_output() {
        let score = [2u8, 1, 0, 2, 1, 0];
        let row = render_row("puzzle", &score);
        assert!(row.contains('🟩'));
        assert!(row.contains('🟨'));
        assert!(row.contains('⬛'));
    }
}
