//! Brass games and gifts: `!brass flip <n>`, `!brass slots`, `!brass give <nick> <n>`, and
//! `!brass history`.
//!
//! The games keep a small house edge (a flip wins 48% and pays double; the slots return about 94%)
//! and a per-person daily net-loss cap, so nobody loses a week of fishing in an evening. Gifts have
//! a daily cap too. Both caps live in one small `wager:` record per person, reset each UTC day.
//! History reads the host's brass ledger, which keeps each person's recent transactions.

use super::*;

pub(super) const DEFAULT_MAX_BET: i64 = 100;
pub(super) const DEFAULT_SLOTS_COST: i64 = 5;
pub(super) const DEFAULT_DAILY_LOSS_LIMIT: i64 = 200;
pub(super) const DEFAULT_DAILY_GIFT_LIMIT: i64 = 200;
/// Per mille a flip wins. It pays double, so the house keeps 4%.
const FLIP_WIN: u64 = 480;
pub(super) const DAY: i64 = 86_400;
const HISTORY_SHOWN: usize = 5;

/// Today's wagering and giving for one person.
#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
pub(super) struct Wagers {
    #[serde(default)]
    day: i64,
    /// Stakes lost minus winnings, today. Negative after a lucky day.
    #[serde(default)]
    pub(super) net_lost: i64,
    #[serde(default)]
    given: u64,
}

pub(super) fn wager_key(server: &str, profile_id: &str) -> String {
    format!("wager:{server}:{profile_id}")
}

pub(super) fn load_wagers(server: &str, profile_id: &str, today: i64) -> Result<Wagers, Error> {
    let raw = kv_load(&wager_key(server, profile_id))?;
    let wagers: Wagers = if raw.trim().is_empty() {
        Wagers::default()
    } else {
        serde_json::from_str(&raw)?
    };
    Ok(if wagers.day == today {
        wagers
    } else {
        Wagers {
            day: today,
            ..Wagers::default()
        }
    })
}

pub(super) fn save_wagers(server: &str, profile_id: &str, wagers: &Wagers) -> Result<(), Error> {
    kv_save(
        &wager_key(server, profile_id),
        &serde_json::to_string(wagers)?,
    )
}

pub(super) fn setting_i64(key: &str, server: &str, channel: &str, fallback: i64) -> i64 {
    setting_string(key, server, channel, &fallback.to_string())
        .parse()
        .unwrap_or(fallback)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Symbol {
    Cog,
    Key,
    Bell,
    Crown,
}

/// One reel's symbols and weights, out of 16.
const REEL: [(Symbol, u64); 4] = [
    (Symbol::Cog, 6),
    (Symbol::Key, 5),
    (Symbol::Bell, 3),
    (Symbol::Crown, 2),
];
const REEL_WEIGHT: u64 = 16;

impl Symbol {
    fn glyph(self) -> &'static str {
        match self {
            Symbol::Cog => "⚙",
            Symbol::Key => "🗝",
            Symbol::Bell => "🔔",
            Symbol::Crown => "👑",
        }
    }
}

fn symbol_for(roll: u64) -> Symbol {
    let mut edge = 0;
    for (symbol, weight) in REEL {
        edge += weight;
        if roll < edge {
            return symbol;
        }
    }
    Symbol::Cog
}

/// What a spin pays, as a multiple of its cost.
fn slots_multiplier(reels: [Symbol; 3]) -> u64 {
    let count = |symbol| reels.iter().filter(|reel| **reel == symbol).count();
    match (
        count(Symbol::Crown),
        count(Symbol::Bell),
        count(Symbol::Key),
        count(Symbol::Cog),
    ) {
        (3, _, _, _) => 50,
        (_, 3, _, _) => 12,
        (_, _, 3, _) => 6,
        (_, _, _, 3) => 4,
        (2, _, _, _) => 2,
        (_, 2, _, _) | (_, _, 2, _) => 1,
        _ => 0,
    }
}

/// A stake that has left the caller's brass.
pub(super) struct Placed {
    game: &'static str,
    pub(super) event_id: String,
    wagers: Wagers,
    stake: u64,
    balance: u64,
}

pub(super) enum Stake {
    Placed(Placed),
    Refused(String),
}

/// Checks the switch and the daily cap, then takes the stake from the caller's brass.
pub(super) fn place_stake(
    server: &str,
    msg: &MessagePayload,
    game: &'static str,
    amount: u64,
) -> Result<Stake, Error> {
    let channel = msg.target.as_str();
    if setting_string("gambling_enabled", server, channel, "true") != "true" {
        return Ok(Stake::Refused(say(
            msg,
            "gacha.gamble_off",
            "The tables are closed here, {honorific}.",
            &[],
        )?));
    }
    let today = now_secs()?.div_euclid(DAY);
    let wagers = load_wagers(server, &msg.user_id, today)?;
    let limit = setting_i64(
        "daily_loss_limit",
        server,
        channel,
        DEFAULT_DAILY_LOSS_LIMIT,
    )
    .max(0);
    if wagers.net_lost.saturating_add(amount as i64) > limit {
        return Ok(Stake::Refused(say(
            msg,
            "gacha.gamble_cap",
            "That's enough wagering for today, {honorific}; the tables reopen at midnight UTC.",
            &[],
        )?));
    }
    let event_id = format!("gacha:{game}:{}:{}", msg.user_id, random_token()?);
    let result = spend(
        server,
        &msg.user_id,
        amount,
        &event_id,
        &format!("{game}_stake"),
    )?;
    if !result.applied {
        return Ok(Stake::Refused(cannot_afford(msg, result.balance)?));
    }
    Ok(Stake::Placed(Placed {
        game,
        event_id,
        wagers,
        stake: amount,
        balance: result.balance,
    }))
}

/// Pays any winnings and records the day's net loss. Returns the new balance.
fn settle(server: &str, msg: &MessagePayload, placed: Placed, payout: u64) -> Result<u64, Error> {
    let Placed {
        game,
        event_id,
        mut wagers,
        stake,
        balance,
    } = placed;
    let balance = if payout > 0 {
        award_brass(
            server,
            &msg.user_id,
            payout,
            &format!("{event_id}:win"),
            &format!("{game}_win"),
        )?
        .balance
    } else {
        balance
    };
    wagers.net_lost = wagers
        .net_lost
        .saturating_add(stake as i64)
        .saturating_sub(payout as i64);
    save_wagers(server, &msg.user_id, &wagers)?;
    Ok(balance)
}

pub(super) fn flip(server: &str, msg: &MessagePayload, argument: &str) -> Result<String, Error> {
    let max = setting_i64("max_bet", server, &msg.target, DEFAULT_MAX_BET).max(1) as u64;
    let Some(amount) = argument.trim().parse::<u64>().ok().filter(|n| *n > 0) else {
        return say(
            msg,
            "gacha.flip_usage",
            "Bet brass on a coin with !brass flip <amount>, up to {max}, {honorific}.",
            &[("max", &max.to_string())],
        );
    };
    if amount > max {
        return say(
            msg,
            "gacha.bet_too_big",
            "The house takes bets of up to {max} brass, {honorific}.",
            &[("max", &max.to_string())],
        );
    }
    let placed = match place_stake(server, msg, "flip", amount)? {
        Stake::Placed(placed) => placed,
        Stake::Refused(text) => return Ok(text),
    };
    let won = random_index(1000)? < FLIP_WIN as usize;
    let payout = if won { amount * 2 } else { 0 };
    let balance = settle(server, msg, placed, payout)?;
    let vars = [
        ("amount", amount.to_string()),
        ("balance", balance.to_string()),
    ];
    let vars = vars
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect::<Vec<_>>();
    if won {
        say(
            msg,
            "gacha.flip_win",
            "🪙 Heads! {user} wins {amount} brass and now has {balance}.",
            &vars,
        )
    } else {
        say(
            msg,
            "gacha.flip_lose",
            "🪙 Tails. {user} loses {amount} brass; {balance} left.",
            &vars,
        )
    }
}

pub(super) fn slots(server: &str, msg: &MessagePayload) -> Result<String, Error> {
    let cost = setting_i64("slots_cost", server, &msg.target, DEFAULT_SLOTS_COST).max(1) as u64;
    let placed = match place_stake(server, msg, "slots", cost)? {
        Stake::Placed(placed) => placed,
        Stake::Refused(text) => return Ok(text),
    };
    let reels = [
        symbol_for(random_index(REEL_WEIGHT as usize)? as u64),
        symbol_for(random_index(REEL_WEIGHT as usize)? as u64),
        symbol_for(random_index(REEL_WEIGHT as usize)? as u64),
    ];
    let multiplier = slots_multiplier(reels);
    let payout = cost * multiplier;
    let event_id = placed.event_id.clone();
    let balance = settle(server, msg, placed, payout)?;
    if multiplier == 50 {
        award(server, msg, "jackpots", &event_id)?;
    }
    let shown = reels.map(Symbol::glyph).join(" ");
    let vars = [
        ("reels", shown),
        ("payout", payout.to_string()),
        ("cost", cost.to_string()),
        ("balance", balance.to_string()),
    ];
    let vars = vars
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect::<Vec<_>>();
    match multiplier {
        50 => say(
            msg,
            "gacha.slots_jackpot",
            "🎰 [ {reels} ] JACKPOT! {user} wins {payout} brass. ({balance} brass)",
            &vars,
        ),
        0 => say(
            msg,
            "gacha.slots_lose",
            "🎰 [ {reels} ] Nothing this time, {user}. ({balance} brass)",
            &vars,
        ),
        _ => say(
            msg,
            "gacha.slots_win",
            "🎰 [ {reels} ] {user} wins {payout} brass. ({balance} brass)",
            &vars,
        ),
    }
}

pub(super) fn give(server: &str, msg: &MessagePayload, argument: &str) -> Result<String, Error> {
    let mut words = argument.split_whitespace();
    let (Some(nick), Some(amount), None) = (words.next(), words.next(), words.next()) else {
        return give_usage(msg);
    };
    let Some(amount) = amount.parse::<u64>().ok().filter(|n| *n > 0) else {
        return give_usage(msg);
    };
    let Some(recipient) = profile_for_nick(server, nick)? else {
        return say(
            msg,
            "gacha.give_unknown",
            "I don't know {nick}, {honorific}; they'll need to have been about first.",
            &[("nick", nick)],
        );
    };
    if recipient.id == msg.user_id {
        return say(
            msg,
            "gacha.give_self",
            "Giving brass to yourself is merely holding it, {honorific}.",
            &[],
        );
    }
    let channel = msg.target.as_str();
    let limit = setting_i64(
        "daily_gift_limit",
        server,
        channel,
        DEFAULT_DAILY_GIFT_LIMIT,
    )
    .max(0) as u64;
    let today = now_secs()?.div_euclid(DAY);
    let mut wagers = load_wagers(server, &msg.user_id, today)?;
    let left = limit.saturating_sub(wagers.given);
    if amount > left {
        return say(
            msg,
            "gacha.give_cap",
            "You can give {left} more brass today, {honorific}.",
            &[("left", &left.to_string())],
        );
    }
    let event_id = format!("gacha:give:{}:{}", msg.user_id, random_token()?);
    let sent = spend(server, &msg.user_id, amount, &event_id, "gift_sent")?;
    if !sent.applied {
        return cannot_afford(msg, sent.balance);
    }
    // Both halves are idempotent under the event id, and the recipient's profile was just read.
    award_brass(
        server,
        &recipient.id,
        amount,
        &format!("{event_id}:to"),
        "gift_received",
    )?;
    wagers.given += amount;
    save_wagers(server, &msg.user_id, &wagers)?;
    award(server, msg, "gifts", &event_id)?;
    say(
        msg,
        "gacha.gave",
        "{user} gives {amount} brass to {nick}. ({balance} brass left)",
        &[
            ("nick", nick),
            ("amount", &amount.to_string()),
            ("balance", &sent.balance.to_string()),
        ],
    )
}

fn give_usage(msg: &MessagePayload) -> Result<String, Error> {
    say(
        msg,
        "gacha.give_usage",
        "Give brass with !brass give <nick> <amount>, {honorific}.",
        &[],
    )
}

#[derive(Deserialize)]
struct LedgerEntry {
    amount: u64,
    direction: String,
    #[serde(default)]
    reason: String,
    /// Missing on entries written before the ledger kept times; those aren't shown.
    #[serde(default)]
    at: Option<i64>,
}

/// The most recent timestamped entries, newest first, as "+15 fishing catch".
fn recent_ledger(entries: &[jeeves_abi::ModuleKvEntry]) -> Vec<String> {
    let mut parsed = entries
        .iter()
        .filter_map(|entry| serde_json::from_str::<LedgerEntry>(&entry.value).ok())
        .filter_map(|entry| entry.at.map(|at| (at, entry)))
        .collect::<Vec<_>>();
    parsed.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    parsed
        .into_iter()
        .take(HISTORY_SHOWN)
        .map(|(_, entry)| {
            let sign = if entry.direction == "spend" {
                "−"
            } else {
                "+"
            };
            let reason = entry.reason.replace('_', " ");
            format!("{sign}{} {}", entry.amount, reason.trim())
                .trim()
                .to_string()
        })
        .collect()
}

pub(super) fn history(server: &str, msg: &MessagePayload) -> Result<String, Error> {
    let entries = kv_list_prefix(&format!("economy:ledger:{server}:{}:", msg.user_id))?;
    let recent = recent_ledger(&entries);
    if recent.is_empty() {
        return say(
            msg,
            "gacha.history_empty",
            "No brass has changed hands for you lately, {honorific}.",
            &[],
        );
    }
    say(
        msg,
        "gacha.history",
        "{user}'s recent brass: {entries}. Balance: {balance}.",
        &[
            ("entries", &recent.join(" · ")),
            ("balance", &balance(server, &msg.user_id)?.to_string()),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_return_a_little_less_than_they_take() {
        let mut expected = 0.0;
        for a in 0..REEL_WEIGHT {
            for b in 0..REEL_WEIGHT {
                for c in 0..REEL_WEIGHT {
                    let reels = [symbol_for(a), symbol_for(b), symbol_for(c)];
                    expected += slots_multiplier(reels) as f64;
                }
            }
        }
        let rtp = expected / (REEL_WEIGHT.pow(3) as f64);
        assert!((0.92..0.96).contains(&rtp), "return to player {rtp}");
        assert_eq!(slots_multiplier([Symbol::Crown; 3]), 50);
        assert_eq!(
            slots_multiplier([Symbol::Cog, Symbol::Key, Symbol::Bell]),
            0
        );
        assert_eq!(
            slots_multiplier([Symbol::Cog, Symbol::Cog, Symbol::Bell]),
            0,
            "a pair of cogs pays nothing"
        );
        assert_eq!(
            slots_multiplier([Symbol::Crown, Symbol::Bell, Symbol::Crown]),
            2
        );
        assert!(
            (FLIP_WIN as f64 * 2.0 / 1000.0) < 1.0,
            "flips favour the house"
        );
    }

    #[test]
    fn history_shows_the_newest_timestamped_entries() {
        let entry = |key: &str, value: &str| jeeves_abi::ModuleKvEntry {
            key: key.into(),
            value: value.into(),
        };
        let mut entries = vec![entry(
            "old",
            r#"{"amount":9,"direction":"award","reason":"before_times"}"#,
        )];
        for at in 0..7 {
            entries.push(entry(
                &format!("e{at}"),
                &format!(
                    r#"{{"amount":{at},"direction":"{}","reason":"slots_stake","at":{at}}}"#,
                    if at % 2 == 0 { "spend" } else { "award" }
                ),
            ));
        }
        assert_eq!(
            recent_ledger(&entries),
            [
                "−6 slots stake",
                "+5 slots stake",
                "−4 slots stake",
                "+3 slots stake",
                "−2 slots stake"
            ]
        );
    }
}
