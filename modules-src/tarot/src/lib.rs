//! Three-card tarot readings backed by host-owned AI.
//!
//! `!tarot` draws a mind/body/spirit spread. `!tarot <question>` draws a
//! problem/cause/solution spread. Card choice is local and random; interpretation is constrained
//! to short IRC-safe lines.

use extism_pdk::*;
use jeeves_abi::{
    AchievementManifest, AchievementSpec, AchievementStat, AiChatRequest, AiChatResponse,
    AwardStatsRequest, CommandManifest, CommandSpec, Event, EventEnvelope, RandomBytesRequest,
    RandomBytesResponse, SendMessage, SettingGet, SettingKind, SettingScope, SettingSpec,
    SettingsManifest, StatIncrement, ThemeReq, ACHIEVEMENT_MANIFEST_VERSION,
    COMMAND_MANIFEST_VERSION, SETTINGS_MANIFEST_VERSION,
};
use std::cell::RefCell;
use std::collections::BTreeMap;

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn theme(input: String) -> String;
    fn random_bytes(input: String) -> String;
    fn ai_chat(input: String) -> String;
    fn setting_get(input: String) -> String;
    fn award_stats(input: String) -> String;
    fn now(input: String) -> String;
}

const DEFAULT_LINE_BYTES: usize = 390;
/// Maximum IRC lines for a reading. The host AI function caps output at ~420 bytes, which is
/// roughly one full line plus a partial second at this line width — so two lines is the honest
/// ceiling. Promising three would show a cut-off second line.
const DEFAULT_MAX_LINES: usize = 2;
const MAX_QUESTION_CHARS: usize = 180;

thread_local! {
    static COOLDOWNS: RefCell<BTreeMap<String, i64>> = const { RefCell::new(BTreeMap::new()) };
}

#[derive(Clone, Copy)]
struct Card {
    name: &'static str,
    meaning: &'static str,
}

#[derive(Clone, Copy)]
struct DrawnCard {
    position: &'static str,
    card: Card,
    reversed: bool,
}

const CARDS: &[Card] = &[
    Card {
        name: "The Fool",
        meaning: "beginnings, trust, open roads",
    },
    Card {
        name: "The Magician",
        meaning: "skill, will, useful tools",
    },
    Card {
        name: "The High Priestess",
        meaning: "intuition, secrets, quiet knowledge",
    },
    Card {
        name: "The Empress",
        meaning: "care, growth, embodied comfort",
    },
    Card {
        name: "The Emperor",
        meaning: "structure, authority, boundaries",
    },
    Card {
        name: "The Hierophant",
        meaning: "tradition, teaching, shared ritual",
    },
    Card {
        name: "The Lovers",
        meaning: "choice, alignment, honest bonds",
    },
    Card {
        name: "The Chariot",
        meaning: "focus, momentum, disciplined drive",
    },
    Card {
        name: "Strength",
        meaning: "patience, courage, gentle control",
    },
    Card {
        name: "The Hermit",
        meaning: "reflection, solitude, inner guidance",
    },
    Card {
        name: "Wheel of Fortune",
        meaning: "change, cycles, a turning point",
    },
    Card {
        name: "Justice",
        meaning: "truth, balance, consequences",
    },
    Card {
        name: "The Hanged Man",
        meaning: "pause, surrender, changed perspective",
    },
    Card {
        name: "Death",
        meaning: "ending, release, necessary transformation",
    },
    Card {
        name: "Temperance",
        meaning: "moderation, blending, patient repair",
    },
    Card {
        name: "The Devil",
        meaning: "attachment, temptation, hidden bargains",
    },
    Card {
        name: "The Tower",
        meaning: "disruption, revelation, collapse of pretense",
    },
    Card {
        name: "The Star",
        meaning: "hope, healing, a clear signal",
    },
    Card {
        name: "The Moon",
        meaning: "uncertainty, dreams, misleading shadows",
    },
    Card {
        name: "The Sun",
        meaning: "joy, clarity, generous success",
    },
    Card {
        name: "Judgement",
        meaning: "reckoning, renewal, answering the call",
    },
    Card {
        name: "The World",
        meaning: "completion, integration, arrival",
    },
    Card {
        name: "Ace of Wands",
        meaning: "spark, invention, fresh energy",
    },
    Card {
        name: "Two of Wands",
        meaning: "planning, distance, choosing a path",
    },
    Card {
        name: "Three of Wands",
        meaning: "expansion, waiting, wider horizons",
    },
    Card {
        name: "Four of Wands",
        meaning: "homecoming, celebration, stable joy",
    },
    Card {
        name: "Five of Wands",
        meaning: "friction, competition, lively disagreement",
    },
    Card {
        name: "Six of Wands",
        meaning: "recognition, progress, public success",
    },
    Card {
        name: "Seven of Wands",
        meaning: "defense, conviction, holding ground",
    },
    Card {
        name: "Eight of Wands",
        meaning: "speed, messages, sudden movement",
    },
    Card {
        name: "Nine of Wands",
        meaning: "stamina, caution, hard-won resilience",
    },
    Card {
        name: "Ten of Wands",
        meaning: "burden, overcommitment, duty",
    },
    Card {
        name: "Page of Wands",
        meaning: "curiosity, experiment, brave news",
    },
    Card {
        name: "Knight of Wands",
        meaning: "adventure, haste, bold pursuit",
    },
    Card {
        name: "Queen of Wands",
        meaning: "warmth, confidence, magnetic leadership",
    },
    Card {
        name: "King of Wands",
        meaning: "vision, command, entrepreneurial fire",
    },
    Card {
        name: "Ace of Cups",
        meaning: "feeling, compassion, emotional opening",
    },
    Card {
        name: "Two of Cups",
        meaning: "partnership, repair, mutual regard",
    },
    Card {
        name: "Three of Cups",
        meaning: "friendship, support, shared delight",
    },
    Card {
        name: "Four of Cups",
        meaning: "restlessness, refusal, inward attention",
    },
    Card {
        name: "Five of Cups",
        meaning: "regret, grief, what remains",
    },
    Card {
        name: "Six of Cups",
        meaning: "memory, kindness, old comforts",
    },
    Card {
        name: "Seven of Cups",
        meaning: "fantasy, options, unclear desire",
    },
    Card {
        name: "Eight of Cups",
        meaning: "departure, search, leaving enough behind",
    },
    Card {
        name: "Nine of Cups",
        meaning: "contentment, wish, earned pleasure",
    },
    Card {
        name: "Ten of Cups",
        meaning: "belonging, harmony, emotional plenty",
    },
    Card {
        name: "Page of Cups",
        meaning: "tender news, imagination, sincerity",
    },
    Card {
        name: "Knight of Cups",
        meaning: "romance, invitation, idealism in motion",
    },
    Card {
        name: "Queen of Cups",
        meaning: "empathy, intuition, emotional steadiness",
    },
    Card {
        name: "King of Cups",
        meaning: "composure, counsel, mature feeling",
    },
    Card {
        name: "Ace of Swords",
        meaning: "clarity, decision, clean truth",
    },
    Card {
        name: "Two of Swords",
        meaning: "stalemate, guarded choice, blocked vision",
    },
    Card {
        name: "Three of Swords",
        meaning: "hurt, honesty, necessary sorrow",
    },
    Card {
        name: "Four of Swords",
        meaning: "rest, recovery, strategic quiet",
    },
    Card {
        name: "Five of Swords",
        meaning: "conflict, cost, hollow victory",
    },
    Card {
        name: "Six of Swords",
        meaning: "transition, passage, calmer waters",
    },
    Card {
        name: "Seven of Swords",
        meaning: "strategy, secrecy, partial truth",
    },
    Card {
        name: "Eight of Swords",
        meaning: "restriction, fear, imagined limits",
    },
    Card {
        name: "Nine of Swords",
        meaning: "anxiety, rumination, midnight worries",
    },
    Card {
        name: "Ten of Swords",
        meaning: "finality, exhaustion, the worst behind",
    },
    Card {
        name: "Page of Swords",
        meaning: "watchfulness, questions, sharp learning",
    },
    Card {
        name: "Knight of Swords",
        meaning: "urgency, argument, decisive charge",
    },
    Card {
        name: "Queen of Swords",
        meaning: "discernment, candor, clear boundaries",
    },
    Card {
        name: "King of Swords",
        meaning: "judgement, strategy, intellectual command",
    },
    Card {
        name: "Ace of Pentacles",
        meaning: "opportunity, resources, practical seed",
    },
    Card {
        name: "Two of Pentacles",
        meaning: "juggling, adaptation, shifting priorities",
    },
    Card {
        name: "Three of Pentacles",
        meaning: "craft, teamwork, useful feedback",
    },
    Card {
        name: "Four of Pentacles",
        meaning: "security, holding tight, guarded value",
    },
    Card {
        name: "Five of Pentacles",
        meaning: "scarcity, exclusion, asking for help",
    },
    Card {
        name: "Six of Pentacles",
        meaning: "generosity, exchange, fair support",
    },
    Card {
        name: "Seven of Pentacles",
        meaning: "patience, assessment, slow growth",
    },
    Card {
        name: "Eight of Pentacles",
        meaning: "practice, diligence, skilled work",
    },
    Card {
        name: "Nine of Pentacles",
        meaning: "self-sufficiency, refinement, earned ease",
    },
    Card {
        name: "Ten of Pentacles",
        meaning: "legacy, stability, shared prosperity",
    },
    Card {
        name: "Page of Pentacles",
        meaning: "study, promise, practical curiosity",
    },
    Card {
        name: "Knight of Pentacles",
        meaning: "reliability, routine, steady progress",
    },
    Card {
        name: "Queen of Pentacles",
        meaning: "nurture, practicality, domestic wisdom",
    },
    Card {
        name: "King of Pentacles",
        meaning: "stewardship, abundance, grounded command",
    },
];

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![CommandSpec {
            name: "tarot".into(),
            aliases: vec!["cards".into()],
            description: "Draw a concise three-card reading.".into(),
            usage: "!tarot [question]".into(),
        }],
    })?)
}

#[plugin_fn]
pub fn settings(_: String) -> FnResult<String> {
    let all_scopes = || {
        vec![
            SettingScope::Global,
            SettingScope::Network,
            SettingScope::Channel,
        ]
    };
    Ok(serde_json::to_string(&SettingsManifest {
        version: SETTINGS_MANIFEST_VERSION,
        settings: vec![
            SettingSpec {
                key: "enabled".into(),
                description: "Enable tarot readings.".into(),
                default: "true".into(),
                kind: SettingKind::Boolean,
                scopes: all_scopes(),
                applies_immediately: true,
            },
            SettingSpec {
                key: "cooldown_seconds".into(),
                description: "Per-user delay between tarot readings.".into(),
                default: "60".into(),
                kind: SettingKind::DurationSeconds { min: 0, max: 3_600 },
                scopes: all_scopes(),
                applies_immediately: true,
            },
            SettingSpec {
                key: "temperature_percent".into(),
                description: "Sampling temperature from 0 to 200 (0.0 to 2.0).".into(),
                default: "80".into(),
                kind: SettingKind::Integer { min: 0, max: 200 },
                scopes: all_scopes(),
                applies_immediately: true,
            },
            SettingSpec {
                key: "max_tokens".into(),
                description: "Maximum generated tokens per reading.".into(),
                default: "256".into(),
                kind: SettingKind::Integer { min: 32, max: 512 },
                scopes: all_scopes(),
                applies_immediately: true,
            },
            SettingSpec {
                key: "response_line_bytes".into(),
                description: "Preferred maximum UTF-8 bytes per IRC line.".into(),
                default: DEFAULT_LINE_BYTES.to_string(),
                kind: SettingKind::Integer { min: 120, max: 450 },
                scopes: all_scopes(),
                applies_immediately: true,
            },
        ],
    })?)
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        catalog_version: 1,
        stats: vec![AchievementStat {
            id: "readings".into(),
            description: "Tarot readings completed".into(),
        }],
        achievements: [
            ("first_spread", "The Cards Are Dealt", 1),
            ("regular_at_the_table", "Regular at the Table", 10),
            ("seventy_eight_steps", "Seventy-Eight Steps", 78),
        ]
        .into_iter()
        .map(|(id, name, threshold)| AchievementSpec {
            id: id.into(),
            name: name.into(),
            description: format!("Complete {threshold} tarot readings."),
            stat: "readings".into(),
            threshold,
            optional: false,
            secret: false,
        })
        .collect(),
        prestige: Vec::new(),
    })?)
}

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    let text = msg.text.trim();
    if !text.starts_with('!') {
        return Ok(());
    }

    let mut parts = text.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    if !matches!(cmd, "!tarot" | "!cards") {
        return Ok(());
    }

    let question = parts.next().unwrap_or("").trim();
    let channel = if msg.is_private {
        None
    } else {
        Some(msg.target.as_str())
    };
    let dest = if msg.is_private {
        msg.nick.as_str()
    } else {
        msg.target.as_str()
    };
    let user = if msg.display.is_empty() {
        msg.nick.as_str()
    } else {
        msg.display.as_str()
    };

    if !setting_bool("enabled", &env.server, channel)? {
        reply(
            &env.server,
            dest,
            &themed(
                "tarot.disabled",
                &["The tarot deck is put away for now."],
                &[],
            )?,
        )?;
        return Ok(());
    }
    if question.chars().count() > MAX_QUESTION_CHARS {
        reply(
            &env.server,
            dest,
            &themed(
                "tarot.question_too_long",
                &["A concise question, if you please, {user}."],
                &[("user", user)],
            )?,
        )?;
        return Ok(());
    }

    let cooldown = setting_i64("cooldown_seconds", &env.server, channel, 60).clamp(0, 3_600);
    if cooldown > 0 {
        let current = timestamp()?;
        let key = format!("{}:{}", env.server, msg.user_id);
        let remaining = COOLDOWNS.with(|cooldowns| {
            let mut cooldowns = cooldowns.borrow_mut();
            let last = cooldowns.get(&key).copied().unwrap_or(0);
            let remaining = cooldown - current.saturating_sub(last);
            if remaining <= 0 || remaining > cooldown {
                cooldowns.insert(key, current);
                0
            } else {
                remaining
            }
        });
        if remaining > 0 {
            let seconds = remaining.to_string();
            reply(
                &env.server,
                dest,
                &themed(
                    "tarot.cooldown",
                    &["The cards are still settling, {user}. Try again in {seconds}s."],
                    &[("user", user), ("seconds", &seconds)],
                )?,
            )?;
            return Ok(());
        }
    }

    let spread = if question.is_empty() {
        ["Mind", "Body", "Spirit"]
    } else {
        ["Problem", "Cause", "Solution"]
    };
    let cards = draw_cards(&spread)?;
    // First message: the draw — card names and positions, no meanings.
    reply(&env.server, dest, &draw_line(user, &cards)?)?;
    // Second message: the AI interpretation of those cards.
    let response = reading(&env.server, channel, user, question, &cards)?;
    let rendered = themed(
        "tarot.response",
        &["{response}"],
        &[("response", &response)],
    )?;
    let line_bytes = setting_i64(
        "response_line_bytes",
        &env.server,
        channel,
        DEFAULT_LINE_BYTES as i64,
    )
    .clamp(120, 450) as usize;
    for line in response_lines(&rendered, line_bytes, DEFAULT_MAX_LINES) {
        reply(&env.server, dest, &line)?;
    }
    award(&env.server, &msg.user_id, user, dest)?;
    Ok(())
}

fn draw_cards(spread: &[&'static str; 3]) -> Result<Vec<DrawnCard>, Error> {
    let bytes = host_random(16)?;
    let mut seed = 0_u64;
    for byte in bytes.iter().take(8) {
        seed = (seed << 8) | (*byte as u64);
    }
    let mut deck = CARDS.to_vec();
    for i in (1..deck.len()).rev() {
        seed = next_seed(seed);
        deck.swap(i, (seed as usize) % (i + 1));
    }
    Ok(spread
        .iter()
        .enumerate()
        .map(|(index, position)| {
            seed = next_seed(seed);
            DrawnCard {
                position,
                card: deck[index],
                reversed: (seed & 1) == 1,
            }
        })
        .collect())
}

fn next_seed(seed: u64) -> u64 {
    seed.wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407)
}

/// Render the drawn cards as a compact single line: names + positions + reversed, no meanings.
fn draw_line(user: &str, cards: &[DrawnCard]) -> Result<String, Error> {
    let list = cards
        .iter()
        .map(|d| {
            format!(
                "{}: {}{}",
                d.position,
                d.card.name,
                if d.reversed { " (reversed)" } else { "" }
            )
        })
        .collect::<Vec<_>>()
        .join(" — ");
    themed(
        "tarot.draw",
        &["{user} draws: {cards}"],
        &[("user", user), ("cards", &list)],
    )
}

fn reading(
    server: &str,
    channel: Option<&str>,
    user: &str,
    question: &str,
    cards: &[DrawnCard],
) -> Result<String, Error> {
    let temperature =
        setting_i64("temperature_percent", server, channel, 80).clamp(0, 200) as f64 / 100.0;
    let max_tokens = setting_i64("max_tokens", server, channel, 256).clamp(32, 512) as u32;
    let prompt = prompt(user, question, cards);
    let raw = unsafe {
        ai_chat(serde_json::to_string(&AiChatRequest {
            prompt,
            context: Vec::new(),
            temperature,
            max_tokens,
        })?)?
    };
    let response: AiChatResponse = serde_json::from_str(&raw)?;

    if let Some(text) = response.text {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    fallback_reading()
}

fn prompt(user: &str, question: &str, cards: &[DrawnCard]) -> String {
    let spread = cards
        .iter()
        .map(|draw| {
            format!(
                "{}: {}{} ({})",
                draw.position,
                draw.card.name,
                if draw.reversed { " reversed" } else { "" },
                draw.card.meaning
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let question_clause = if question.is_empty() {
        "No specific question was asked; read it as a general mind/body/spirit spread.".to_string()
    } else {
        // The question is user-supplied; pass it verbatim as the thing to read toward.
        // The host labels AI context as untrusted, but this is the prompt itself, so quote it
        // to reduce the chance of injection altering the reading's framing.
        format!("The querent, {user}, asks: \"{question}\".")
    };
    format!(
        "Read this three-card tarot spread for {user}. {question_clause} Cards: {spread}. \
         Offer a brief interpretation in exactly two sentences. Weave the cards together into \
         one reading rather than listing them separately. Finish both sentences completely. \
         No disclaimers, no advice, no medical, legal, or financial guidance."
    )
}

fn fallback_reading() -> Result<String, Error> {
    themed(
        "tarot.fallback",
        &["The cards are drawn, but their interpretation is unavailable right now. Please try again shortly."],
        &[],
    )
}

fn host_random(count: usize) -> Result<Vec<u8>, Error> {
    let raw = unsafe { random_bytes(serde_json::to_string(&RandomBytesRequest { count })?)? };
    Ok(serde_json::from_str::<RandomBytesResponse>(&raw)?.bytes)
}

fn setting(key: &str, server: &str, channel: Option<&str>) -> Result<String, Error> {
    Ok(unsafe {
        setting_get(serde_json::to_string(&SettingGet {
            key: key.into(),
            server: Some(server.into()),
            channel: channel.map(str::to_string),
        })?)?
    })
}

fn setting_bool(key: &str, server: &str, channel: Option<&str>) -> Result<bool, Error> {
    Ok(setting(key, server, channel)? == "true")
}

fn setting_i64(key: &str, server: &str, channel: Option<&str>, fallback: i64) -> i64 {
    setting(key, server, channel)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(fallback)
}

fn timestamp() -> Result<i64, Error> {
    Ok(unsafe { now(String::new())? }.parse().unwrap_or(0))
}

fn themed(key: &str, defaults: &[&str], vars: &[(&str, &str)]) -> Result<String, Error> {
    Ok(unsafe {
        theme(serde_json::to_string(&ThemeReq {
            key: key.into(),
            default: defaults.iter().map(|value| (*value).into()).collect(),
            vars: vars
                .iter()
                .map(|(key, value)| ((*key).into(), (*value).into()))
                .collect(),
        })?)?
    })
}

fn reply(server: &str, target: &str, text: &str) -> Result<(), Error> {
    unsafe {
        send_message(serde_json::to_string(&SendMessage {
            server: server.into(),
            target: target.into(),
            text: text.into(),
        })?)?
    };
    Ok(())
}

fn response_lines(text: &str, max_bytes: usize, max_lines: usize) -> Vec<String> {
    let max_bytes = max_bytes.max(4);
    let mut remaining = text.trim();
    let mut lines = Vec::new();
    while !remaining.is_empty() && lines.len() < max_lines {
        if remaining.len() <= max_bytes {
            lines.push(remaining.to_string());
            break;
        }
        let mut sentence_end = None;
        let mut word_end = None;
        for (byte, ch) in remaining.char_indices() {
            let end = byte + ch.len_utf8();
            if end > max_bytes {
                break;
            }
            if matches!(ch, '.' | '!' | '?') {
                sentence_end = Some(end);
            }
            if ch.is_whitespace() {
                word_end = Some(byte);
            }
        }
        let split = sentence_end.or(word_end).unwrap_or_else(|| {
            let mut end = max_bytes.min(remaining.len());
            while !remaining.is_char_boundary(end) {
                end -= 1;
            }
            end
        });
        let line = remaining[..split].trim();
        if !line.is_empty() {
            lines.push(line.to_string());
        }
        remaining = remaining[split..].trim_start();
    }
    lines
}

fn award(server: &str, profile_id: &str, display_name: &str, target: &str) -> Result<(), Error> {
    if profile_id.is_empty() {
        return Ok(());
    }
    unsafe {
        award_stats(serde_json::to_string(&AwardStatsRequest {
            server: server.into(),
            profile_id: profile_id.into(),
            display_name: display_name.into(),
            target: target.into(),
            increments: vec![StatIncrement {
                stat: "readings".into(),
                amount: 1,
            }],
            deduplication_id: None,
        })?)?;
    }
    Ok(())
}
