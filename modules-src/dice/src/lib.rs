//! Dice, coins, choices, and an eight ball.
//!
//! `!roll` takes dice notation: `!roll` (a d6), `!roll d20`, `!roll 2d6+3`, `!roll 4d6k3` (keep the
//! highest three; `kl` keeps the lowest), `!roll d%`, and sums of those (`!roll 1d8+1d6+2`).
//! Anything after the expression is a label: `!roll d20+5 stealth`. `!coin` (for `!roll coin`)
//! flips a coin. `!choose a | b | c` (or commas, or "a or b") picks one, and `!8ball <question>`
//! (for `!choose 8ball`) answers a yes-or-no question from the theme's list. Stateless: nothing is
//! stored, so there are no data hooks.

use extism_pdk::*;
use jeeves_abi::{
    AchievementManifest, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandShortcut, CommandSpec, Event, EventEnvelope, MessagePayload, StatIncrement,
    ACHIEVEMENT_MANIFEST_VERSION, COMMAND_MANIFEST_VERSION,
};
use jeeves_guest::{display, honorific, reply, themed, Entropy};

const MAX_TERMS: usize = 10;
const MAX_DICE: u32 = 100;
const MAX_SIDES: u32 = 1_000;
const MAX_CONSTANT: i64 = 1_000_000;
/// Rolls with more dice than this show only the total.
const MAX_SHOWN_DICE: usize = 20;
const MAX_OPTIONS: usize = 20;
const MAX_OPTION_CHARS: usize = 200;
const MAX_LABEL_CHARS: usize = 60;

#[host_fn]
extern "ExtismHost" {
    fn award_stats(input: String) -> String;
}

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![
            CommandSpec {
                name: "roll".into(),
                aliases: vec!["dice".into()],
                description: "Roll dice: d20, 2d6+3, 4d6k3 (keep highest 3), d%, or a coin.".into(),
                usage: "!roll [dice] [label] | !roll coin".into(),
                shortcuts: vec![
                    CommandShortcut::new("coin", "coin").described("Flip a coin.", "!coin")
                ],
            },
            CommandSpec {
                name: "choose".into(),
                aliases: Vec::new(),
                description:
                    "Pick one of several options, or ask the eight ball a yes-or-no question."
                        .into(),
                usage: "!choose a | b | c  ·  !choose 8ball <question>".into(),
                shortcuts: vec![CommandShortcut::new("8ball", "8ball")
                    .described("Ask a yes-or-no question.", "!8ball <question>")],
            },
        ],
    })?)
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    let stats = [
        ("rolls", "Dice rolls and coin flips"),
        ("natural_20s", "Natural twenties rolled"),
        ("choices", "Choices made for you"),
        ("questions", "Questions put to the eight ball"),
    ]
    .into_iter()
    .map(|(id, description)| AchievementStat {
        id: id.into(),
        description: description.into(),
    })
    .collect();
    let achievements = [
        (
            "first_roll",
            "The Die Is Cast",
            "Roll the dice.",
            "rolls",
            1,
        ),
        (
            "hundred_rolls",
            "A Sporting Disposition",
            "Roll dice or flip coins 100 times.",
            "rolls",
            100,
        ),
        (
            "natural_twenty",
            "A Natural Twenty",
            "Roll a 20 on a d20.",
            "natural_20s",
            1,
        ),
        (
            "decisive",
            "Decisively Undecided",
            "Have a choice made for you 10 times.",
            "choices",
            10,
        ),
        (
            "oracle",
            "Consulting the Oracle",
            "Ask the eight ball 25 questions.",
            "questions",
            25,
        ),
    ]
    .into_iter()
    .map(|(id, name, description, stat, threshold)| AchievementSpec {
        id: id.into(),
        name: name.into(),
        description: description.into(),
        stat: stat.into(),
        threshold,
        optional: false,
        secret: false,
    })
    .collect();
    Ok(serde_json::to_string(&AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        catalog_version: 1,
        stats,
        achievements,
        prestige: Vec::new(),
    })?)
}

/// A themed line with `{user}` and `{honorific}` always available.
fn say(
    msg: &MessagePayload,
    key: &str,
    defaults: &[&str],
    vars: &[(&str, &str)],
) -> Result<String, Error> {
    let mut all = vec![("user", display(msg)), ("honorific", honorific(msg))];
    all.extend_from_slice(vars);
    themed(key, defaults, &all)
}

/// Counts toward achievements; only for callers with a stable profile.
fn award(server: &str, msg: &MessagePayload, increments: &[(&str, u64)]) -> Result<(), Error> {
    let increments = increments
        .iter()
        .filter(|(_, amount)| *amount > 0)
        .map(|(stat, amount)| StatIncrement {
            stat: (*stat).into(),
            amount: *amount,
        })
        .collect::<Vec<_>>();
    if msg.user_id.is_empty() || increments.is_empty() {
        return Ok(());
    }
    unsafe {
        award_stats(serde_json::to_string(&AwardStatsRequest {
            server: server.into(),
            profile_id: msg.user_id.clone(),
            display_name: display(msg).into(),
            target: msg.target.clone(),
            increments,
            deduplication_id: None,
        })?)?
    };
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Keep {
    All,
    Highest(u32),
    Lowest(u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Term {
    Dice { count: u32, sides: u32, keep: Keep },
    Constant(i64),
}

/// A parsed expression: signed terms, in order.
#[derive(Debug, PartialEq, Eq)]
struct Expression {
    terms: Vec<(i64, Term)>,
}

impl Expression {
    fn dice(&self) -> u32 {
        self.terms
            .iter()
            .map(|(_, term)| match term {
                Term::Dice { count, .. } => *count,
                Term::Constant(_) => 0,
            })
            .sum()
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ParseError {
    Invalid,
    TooMany,
}

fn parse_number(text: &str) -> Result<u32, ParseError> {
    if text.is_empty() || text.len() > 7 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ParseError::Invalid);
    }
    text.parse().map_err(|_| ParseError::Invalid)
}

fn parse_term(text: &str) -> Result<Term, ParseError> {
    let Some((count, rest)) = text.split_once('d') else {
        let value = parse_number(text)? as i64;
        return if value > MAX_CONSTANT {
            Err(ParseError::TooMany)
        } else {
            Ok(Term::Constant(value))
        };
    };
    let count = if count.is_empty() {
        1
    } else {
        parse_number(count)?
    };
    let (sides, keep) = if let Some((sides, kept)) = rest.split_once("kl") {
        (sides, Keep::Lowest(parse_number(kept)?))
    } else if let Some((sides, kept)) = rest.split_once("kh") {
        (sides, Keep::Highest(parse_number(kept)?))
    } else if let Some((sides, kept)) = rest.split_once('k') {
        (sides, Keep::Highest(parse_number(kept)?))
    } else {
        (rest, Keep::All)
    };
    let sides = if sides == "%" {
        100
    } else {
        parse_number(sides)?
    };
    if count == 0 || sides == 0 {
        return Err(ParseError::Invalid);
    }
    if let Keep::Highest(kept) | Keep::Lowest(kept) = keep {
        if kept == 0 || kept > count {
            return Err(ParseError::Invalid);
        }
    }
    if count > MAX_DICE || sides > MAX_SIDES {
        return Err(ParseError::TooMany);
    }
    Ok(Term::Dice { count, sides, keep })
}

/// `2d6+1d4-1`, case-insensitive, without spaces.
fn parse_expression(text: &str) -> Result<Expression, ParseError> {
    let text = text.to_ascii_lowercase();
    if text.is_empty() {
        return Err(ParseError::Invalid);
    }
    let mut terms = Vec::new();
    let mut sign = 1;
    let mut start = 0;
    let bytes = text.as_bytes();
    for index in 0..=bytes.len() {
        let at_operator = index < bytes.len() && (bytes[index] == b'+' || bytes[index] == b'-');
        if index == bytes.len() || at_operator {
            let piece = &text[start..index];
            if piece.is_empty() {
                // Only a leading sign may stand alone ("-1d4" is fine, "1d4++2" is not).
                if index != 0 || !at_operator {
                    return Err(ParseError::Invalid);
                }
            } else {
                terms.push((sign, parse_term(piece)?));
            }
            if at_operator {
                sign = if bytes[index] == b'-' { -1 } else { 1 };
                start = index + 1;
            }
        }
    }
    let expression = Expression { terms };
    if expression.terms.len() > MAX_TERMS || expression.dice() > MAX_DICE {
        return Err(ParseError::TooMany);
    }
    Ok(expression)
}

/// Splits `!roll` arguments into the expression and a trailing label: the expression is every
/// leading word made only of dice characters.
fn split_label(argument: &str) -> (String, String) {
    let dice_word = |word: &str| {
        word.chars()
            .all(|ch| ch.is_ascii_digit() || "dDkKlLhH%+-".contains(ch))
    };
    let mut words = argument.split_whitespace().peekable();
    let mut expression = String::new();
    while let Some(word) = words.next_if(|word| dice_word(word)) {
        expression.push_str(word);
    }
    let label = words.collect::<Vec<_>>().join(" ");
    (expression, label)
}

struct RolledTerm {
    sign: i64,
    term: Term,
    /// Each die, and whether it counts.
    dice: Vec<(u32, bool)>,
}

struct Roll {
    terms: Vec<RolledTerm>,
    total: i64,
    natural_20s: u64,
}

fn roll(
    expression: &Expression,
    draw: &mut dyn FnMut(u32) -> Result<u32, Error>,
) -> Result<Roll, Error> {
    let mut total = 0i64;
    let mut natural_20s = 0;
    let mut terms = Vec::new();
    for (sign, term) in &expression.terms {
        let mut dice = Vec::new();
        match term {
            Term::Constant(value) => total += sign * value,
            Term::Dice { count, sides, keep } => {
                let values = (0..*count)
                    .map(|_| draw(*sides).map(|value| value + 1))
                    .collect::<Result<Vec<_>, _>>()?;
                let mut order = (0..values.len()).collect::<Vec<_>>();
                // Stable ordering: ties keep the earlier die.
                order.sort_by_key(|index| std::cmp::Reverse(values[*index]));
                let kept = match keep {
                    Keep::All => order,
                    Keep::Highest(kept) => order.into_iter().take(*kept as usize).collect(),
                    Keep::Lowest(kept) => order.into_iter().rev().take(*kept as usize).collect(),
                };
                for (index, value) in values.iter().enumerate() {
                    let counts = kept.contains(&index);
                    if counts {
                        total += sign * i64::from(*value);
                        if *sides == 20 && *value == 20 {
                            natural_20s += 1;
                        }
                    }
                    dice.push((*value, counts));
                }
            }
        }
        terms.push(RolledTerm {
            sign: *sign,
            term: term.clone(),
            dice,
        });
    }
    Ok(Roll {
        terms,
        total,
        natural_20s,
    })
}

/// "[4, 2] + 3", with dropped dice in parentheses.
fn breakdown(roll: &Roll) -> String {
    let mut out = String::new();
    for (index, term) in roll.terms.iter().enumerate() {
        if index > 0 || term.sign < 0 {
            out.push_str(if term.sign < 0 { " − " } else { " + " });
        }
        match term.term {
            Term::Constant(value) => out.push_str(&value.to_string()),
            Term::Dice { .. } => {
                let dice = term
                    .dice
                    .iter()
                    .map(|(value, counts)| {
                        if *counts {
                            value.to_string()
                        } else {
                            format!("({value})")
                        }
                    })
                    .collect::<Vec<_>>();
                out.push('[');
                out.push_str(&dice.join(", "));
                out.push(']');
            }
        }
    }
    out.trim_start().to_string()
}

fn cmd_roll(server: &str, msg: &MessagePayload, argument: &str) -> Result<String, Error> {
    let mut entropy = Entropy::default();
    if argument.eq_ignore_ascii_case("coin") {
        let heads = entropy.below(2)? == 0;
        award(server, msg, &[("rolls", 1)])?;
        return if heads {
            say(
                msg,
                "dice.coin_heads",
                &["🪙 {user} flips a coin: heads."],
                &[],
            )
        } else {
            say(
                msg,
                "dice.coin_tails",
                &["🪙 {user} flips a coin: tails."],
                &[],
            )
        };
    }
    let (text, label) = split_label(argument);
    let text = if text.is_empty() {
        "d6".to_string()
    } else {
        text
    };
    let expression = match parse_expression(&text) {
        Ok(expression) => expression,
        Err(ParseError::TooMany) => {
            return say(
                msg,
                "dice.too_many",
                &["That's rather a lot of dice, {honorific}; I can manage {max} at most, of up to {sides} sides."],
                &[
                    ("max", &MAX_DICE.to_string()),
                    ("sides", &MAX_SIDES.to_string()),
                ],
            )
        }
        Err(ParseError::Invalid) => {
            return say(
                msg,
                "dice.usage",
                &["I roll things like d20, 2d6+3, 4d6k3, or d%, {honorific}. !roll coin flips a coin."],
                &[],
            )
        }
    };
    let rolled = roll(&expression, &mut |sides| entropy.below(sides))?;
    award(
        server,
        msg,
        &[("rolls", 1), ("natural_20s", rolled.natural_20s)],
    )?;
    let label: String = label
        .chars()
        .filter(|ch| !ch.is_control())
        .take(MAX_LABEL_CHARS)
        .collect();
    let shown = if label.is_empty() {
        text
    } else {
        format!("{text} ({label})")
    };
    let total = rolled.total.to_string();
    if expression.dice() as usize > MAX_SHOWN_DICE {
        return say(
            msg,
            "dice.roll_total",
            &["🎲 {user} rolls {dice}: {total}"],
            &[("dice", &shown), ("total", &total)],
        );
    }
    say(
        msg,
        "dice.roll",
        &["🎲 {user} rolls {dice}: {rolls} = {total}"],
        &[
            ("dice", &shown),
            ("rolls", &breakdown(&rolled)),
            ("total", &total),
        ],
    )
}

/// "a | b | c", "a, b, c", or "a or b".
fn options(argument: &str) -> Vec<String> {
    let argument = argument.trim().trim_end_matches('?');
    let pieces: Vec<&str> = if argument.contains('|') {
        argument.split('|').collect()
    } else if argument.contains(',') {
        argument.split(',').collect()
    } else {
        let lower = argument.to_lowercase();
        // Split on " or " by position in the lowercased copy; ASCII lowercasing keeps offsets.
        if lower.len() == argument.len() {
            let mut pieces = Vec::new();
            let mut start = 0;
            for (index, _) in lower.match_indices(" or ") {
                pieces.push(&argument[start..index]);
                start = index + 4;
            }
            pieces.push(&argument[start..]);
            pieces
        } else {
            argument.split(" or ").collect()
        }
    };
    pieces
        .into_iter()
        .map(|piece| {
            piece
                .trim()
                .trim_start_matches(|ch: char| ch == '!' || ch.is_whitespace())
                .chars()
                .filter(|ch| !ch.is_control())
                .take(MAX_OPTION_CHARS)
                .collect::<String>()
        })
        .filter(|piece| !piece.is_empty())
        .collect()
}

const EIGHT_BALL: &[&str] = &[
    "Most assuredly, {honorific}.",
    "I should think so, {honorific}.",
    "Without a doubt, {honorific}.",
    "All the signs point that way, {honorific}.",
    "I would put money on it, {honorific}.",
    "Yes, {honorific}, and I say so with some confidence.",
    "It would appear so, {honorific}.",
    "In all likelihood, {honorific}.",
    "It is too early to say, {honorific}.",
    "I would rather not commit myself, {honorific}.",
    "Ask me again after tea, {honorific}.",
    "The matter remains obscure, {honorific}.",
    "I fear not, {honorific}.",
    "I would not count on it, {honorific}.",
    "Most unlikely, {honorific}.",
    "No, {honorific}, and I say it with regret.",
];

fn cmd_choose(server: &str, msg: &MessagePayload, argument: &str) -> Result<String, Error> {
    let (first, rest) = argument
        .split_once(char::is_whitespace)
        .map(|(first, rest)| (first, rest.trim()))
        .unwrap_or((argument, ""));
    if first.eq_ignore_ascii_case("8ball") {
        if rest.is_empty() {
            return say(
                msg,
                "dice.8ball_usage",
                &["Put a yes-or-no question to me, {honorific}: !8ball <question>"],
                &[],
            );
        }
        award(server, msg, &[("questions", 1)])?;
        return say(msg, "dice.8ball", EIGHT_BALL, &[]);
    }
    let options = options(argument);
    if options.len() < 2 {
        return say(
            msg,
            "dice.choose_usage",
            &["Give me at least two things to choose between, {honorific}: !choose tea | coffee"],
            &[],
        );
    }
    if options.len() > MAX_OPTIONS {
        return say(
            msg,
            "dice.choose_too_many",
            &["That is too many to weigh, {honorific}; {max} at most."],
            &[("max", &MAX_OPTIONS.to_string())],
        );
    }
    let choice = &options[Entropy::default().below(options.len() as u32)? as usize];
    award(server, msg, &[("choices", 1)])?;
    say(
        msg,
        "dice.choice",
        &["{user}: I would go with {choice}."],
        &[("choice", choice)],
    )
}

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    let text = msg.text.trim();
    let (command, argument) = text
        .split_once(char::is_whitespace)
        .map(|(command, argument)| (command.to_ascii_lowercase(), argument.trim()))
        .unwrap_or((text.to_ascii_lowercase(), ""));
    let destination = if msg.is_private {
        msg.nick.as_str()
    } else {
        msg.target.as_str()
    };
    let text = match command.as_str() {
        "!roll" => cmd_roll(&env.server, &msg, argument)?,
        "!choose" => cmd_choose(&env.server, &msg, argument)?,
        _ => return Ok(()),
    };
    reply(&env.server, destination, &text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dice(count: u32, sides: u32, keep: Keep) -> Term {
        Term::Dice { count, sides, keep }
    }

    #[test]
    fn parses_dice_notation() {
        assert_eq!(
            parse_expression("2d6+3").unwrap().terms,
            [(1, dice(2, 6, Keep::All)), (1, Term::Constant(3))]
        );
        assert_eq!(
            parse_expression("D20").unwrap().terms,
            [(1, dice(1, 20, Keep::All))]
        );
        assert_eq!(
            parse_expression("4d6k3-1d4").unwrap().terms,
            [
                (1, dice(4, 6, Keep::Highest(3))),
                (-1, dice(1, 4, Keep::All))
            ]
        );
        assert_eq!(
            parse_expression("2d20kl1").unwrap().terms,
            [(1, dice(2, 20, Keep::Lowest(1)))]
        );
        assert_eq!(
            parse_expression("d%").unwrap().terms,
            [(1, dice(1, 100, Keep::All))]
        );
        assert_eq!(
            parse_expression("-1d4").unwrap().terms,
            [(-1, dice(1, 4, Keep::All))]
        );
        for bad in [
            "", "d", "0d6", "2d0", "1d6++2", "1d6+", "4d6k5", "4d6k0", "abc", "1d6k",
        ] {
            assert_eq!(parse_expression(bad), Err(ParseError::Invalid), "{bad}");
        }
        for huge in ["101d6", "1d1001", "60d6+60d6", "2000000"] {
            assert_eq!(parse_expression(huge), Err(ParseError::TooMany), "{huge}");
        }
    }

    #[test]
    fn labels_follow_the_expression() {
        assert_eq!(
            split_label("d20 + 5 stealth check"),
            ("d20+5".into(), "stealth check".into())
        );
        assert_eq!(split_label("for luck"), (String::new(), "for luck".into()));
        assert_eq!(split_label(""), (String::new(), String::new()));
    }

    #[test]
    fn keeps_the_right_dice_and_counts_natural_twenties() {
        let rolls = [6u32, 1, 4, 3];
        let mut next = rolls.iter().copied();
        let expression = parse_expression("4d6k3+2").unwrap();
        let rolled = roll(&expression, &mut |_| Ok(next.next().unwrap() - 1)).unwrap();
        assert_eq!(rolled.total, 6 + 4 + 3 + 2);
        assert_eq!(breakdown(&rolled), "[6, (1), 4, 3] + 2");

        let mut next = [20u32, 20].into_iter();
        let expression = parse_expression("2d20kl1").unwrap();
        let rolled = roll(&expression, &mut |_| Ok(next.next().unwrap() - 1)).unwrap();
        assert_eq!(rolled.total, 20);
        assert_eq!(rolled.natural_20s, 1, "only the kept twenty counts");
        assert_eq!(breakdown(&rolled), "[(20), 20]");

        let mut next = [3u32].into_iter();
        let rolled = roll(&parse_expression("-1d4").unwrap(), &mut |_| {
            Ok(next.next().unwrap() - 1)
        })
        .unwrap();
        assert_eq!((rolled.total, breakdown(&rolled)), (-3, "− [3]".into()));
    }

    #[test]
    fn options_split_on_pipes_commas_or_or() {
        assert_eq!(options("tea | coffee | !cocoa"), ["tea", "coffee", "cocoa"]);
        assert_eq!(options("tea, coffee"), ["tea", "coffee"]);
        assert_eq!(options("Tea OR coffee?"), ["Tea", "coffee"]);
        assert_eq!(options("just tea"), ["just tea"]);
        assert!(options(" | ").is_empty());
    }
}
