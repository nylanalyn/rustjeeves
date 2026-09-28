//! Calculator, unit, currency, and crypto module for rustjeeves.
//!
//! - `!calc <expr>` — safe arithmetic (see [`expr`]): `^`, constants, functions, factorials,
//!   implicit multiplication, and `ans` for the caller's previous result.
//! - `!convert <amount> <unit> to <unit>` — physical units (see [`units`]), plus currencies and
//!   cryptocurrencies resolved by the host's `money` capability.
//! - `!crypto <coin> [coin…]` — current prices with 24-hour change.
//!
//! No KV and no personal state: `ans` lives in memory for the life of the plugin instance.

mod expr;
mod units;

use extism_pdk::*;
use jeeves_abi::{
    AchievementManifest, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandSpec, CryptoQuoteRequest, CryptoQuoteResponse, Event, EventEnvelope, MessagePayload,
    MoneyConvertRequest, MoneyConvertResponse, SendMessage, SettingGet, SettingKind, SettingScope,
    SettingSpec, SettingsManifest, StatIncrement, ThemeReq, ACHIEVEMENT_MANIFEST_VERSION,
    COMMAND_MANIFEST_VERSION, SETTINGS_MANIFEST_VERSION,
};
use std::cell::RefCell;
use std::collections::BTreeMap;

const MAX_INPUT_CHARS: usize = 200;
const MAX_CRYPTO_SYMBOLS: usize = 3;
const MAX_REMEMBERED_ANSWERS: usize = 2_000;

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn theme(input: String) -> String;
    fn award_stats(input: String) -> String;
    fn setting_get(input: String) -> String;
    fn money_convert(input: String) -> String;
    fn crypto_quote(input: String) -> String;
}

thread_local! {
    /// Each caller's last `!calc` result for `ans`, keyed by (server, profile).
    static ANSWERS: RefCell<BTreeMap<(String, String), f64>> = const { RefCell::new(BTreeMap::new()) };
}

// ── manifests ───────────────────────────────────────────────────────────────

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    let spec =
        |id: &str, name: &str, description: String, stat: &str, threshold, optional, secret| {
            AchievementSpec {
                id: id.into(),
                name: name.into(),
                description,
                stat: stat.into(),
                threshold,
                optional,
                secret,
            }
        };
    let mut achievements = [
        ("back_of_envelope", "Back of the Envelope", 1),
        ("figures_in_order", "Figures in Order", 25),
        ("human_abacus", "Human Abacus", 100),
    ]
    .into_iter()
    .map(|(id, name, threshold)| {
        spec(
            id,
            name,
            format!("Complete {threshold} successful calculations or conversions."),
            "successes",
            threshold,
            false,
            false,
        )
    })
    .collect::<Vec<_>>();
    achievements.push(spec(
        "apples_oranges",
        "Apples and Oranges",
        "Successfully use both calculation and conversion modes.".into(),
        "distinct_modes",
        2,
        false,
        false,
    ));
    achievements.push(spec(
        "bureau_de_change",
        "Bureau de Change",
        "Convert between currencies 10 times.".into(),
        "currency_conversions",
        10,
        true,
        false,
    ));
    achievements.push(spec(
        "to_the_moon",
        "To the Moon",
        "Check a cryptocurrency's price.".into(),
        "crypto_quotes",
        1,
        true,
        true,
    ));
    Ok(serde_json::to_string(&AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        catalog_version: 2,
        stats: [
            ("successes", "Successful calculations and conversions"),
            (
                "distinct_modes",
                "Distinct calculator modes used successfully",
            ),
            ("currency_conversions", "Currency conversions"),
            ("crypto_quotes", "Cryptocurrency price checks"),
        ]
        .into_iter()
        .map(|(id, description)| AchievementStat {
            id: id.into(),
            description: description.into(),
        })
        .collect(),
        achievements,
        prestige: Vec::new(),
    })?)
}

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![
            CommandSpec {
                name: "calc".into(),
                aliases: vec!["calculate".into()],
                description: "Evaluate arithmetic: + - * / % ^ !, pi, e, sqrt, log, ln, sin, round, min, max, avg, gcd, ans, and more.".into(),
                usage: "!calc <expression>".into(),
                ..Default::default()
            },
            CommandSpec {
                name: "convert".into(),
                aliases: Vec::new(),
                description: "Convert units (5 ft 10 in to cm, 30 psi to bar), currencies (50 usd to gbp), or crypto (0.5 btc to eur).".into(),
                usage: "!convert <amount> <unit> [<amount> <unit>…] to <unit>".into(),
                ..Default::default()
            },
            CommandSpec {
                name: "crypto".into(),
                aliases: Vec::new(),
                description: "Current cryptocurrency prices with 24-hour change.".into(),
                usage: "!crypto <coin> [coin] [coin]".into(),
                ..Default::default()
            },
        ],
    })?)
}

#[plugin_fn]
pub fn settings(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&SettingsManifest {
        version: SETTINGS_MANIFEST_VERSION,
        settings: vec![SettingSpec {
            key: "pint_system".into(),
            description:
                "What plain pint, quart, gallon, and fl oz mean in !convert (US or UK imperial). \
                          'uk pint' and 'us gallon' always work."
                    .into(),
            default: "us".into(),
            kind: SettingKind::Choice {
                options: vec!["us".into(), "uk".into()],
            },
            scopes: vec![
                SettingScope::Global,
                SettingScope::Network,
                SettingScope::Channel,
            ],
            applies_immediately: true,
        }],
    })?)
}

// ── host helpers ────────────────────────────────────────────────────────────

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

fn themed(key: &str, defaults: &[&str], vars: &[(&str, &str)]) -> Result<String, Error> {
    let req = ThemeReq {
        key: key.into(),
        default: defaults.iter().map(|s| s.to_string()).collect(),
        vars: vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    };
    Ok(unsafe { theme(serde_json::to_string(&req)?)? })
}

fn say(
    server: &str,
    dest: &str,
    key: &str,
    default: &str,
    vars: &[(&str, &str)],
) -> Result<(), Error> {
    reply(server, dest, &themed(key, &[default], vars)?)
}

/// Record a success: the running total, the distinct mode, and any mode-specific counter.
fn award(ctx: &Ctx, mode: &str, extra: Option<&str>) -> Result<(), Error> {
    if ctx.profile_id.is_empty() {
        return Ok(());
    }
    let send = |increments: Vec<(&str, u64)>, dedup: Option<String>| -> Result<(), Error> {
        unsafe {
            award_stats(serde_json::to_string(&AwardStatsRequest {
                server: ctx.server.into(),
                profile_id: ctx.profile_id.into(),
                display_name: ctx.caller.into(),
                target: ctx.dest.into(),
                increments: increments
                    .into_iter()
                    .map(|(stat, amount)| StatIncrement {
                        stat: stat.into(),
                        amount,
                    })
                    .collect(),
                deduplication_id: dedup,
            })?)?;
        }
        Ok(())
    };
    let mut increments = vec![("successes", 1)];
    if let Some(stat) = extra {
        increments.push((stat, 1));
    }
    send(increments, None)?;
    send(vec![("distinct_modes", 1)], Some(format!("mode:{mode}")))
}

fn pint_system(server: &str, channel: Option<&str>) -> Result<units::PintSystem, Error> {
    let raw = unsafe {
        setting_get(serde_json::to_string(&SettingGet {
            key: "pint_system".into(),
            server: Some(server.into()),
            channel: channel.map(str::to_string),
        })?)?
    };
    Ok(if raw.trim() == "uk" {
        units::PintSystem::Uk
    } else {
        units::PintSystem::Us
    })
}

struct Ctx<'a> {
    server: &'a str,
    dest: &'a str,
    caller: &'a str,
    profile_id: &'a str,
    channel: Option<&'a str>,
}

// ── dispatch ────────────────────────────────────────────────────────────────

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    let text = msg.text.trim();
    let (cmd, arg) = text
        .split_once(char::is_whitespace)
        .map(|(cmd, arg)| (cmd, arg.trim()))
        .unwrap_or((text, ""));
    let ctx = Ctx {
        server: &env.server,
        dest: if msg.is_private {
            &msg.nick
        } else {
            &msg.target
        },
        caller: display(&msg),
        profile_id: &msg.user_id,
        channel: (!msg.is_private).then_some(msg.target.as_str()),
    };
    // The host rewrites the `!calculate` alias to `!calc`.
    match cmd {
        "!calc" => handle_calc(&ctx, arg)?,
        "!convert" => handle_convert(&ctx, arg)?,
        "!crypto" => handle_crypto(&ctx, arg)?,
        _ => {}
    }
    Ok(())
}

fn display(msg: &MessagePayload) -> &str {
    if msg.display.is_empty() {
        &msg.nick
    } else {
        &msg.display
    }
}

fn handle_calc(ctx: &Ctx, arg: &str) -> Result<(), Error> {
    if arg.is_empty() {
        return say(ctx.server, ctx.dest, "calc.usage", "Usage: !calc <expression>. Supports + - * / % ^ !, pi, e, sqrt, log, ln, sin, round, min, max, avg, gcd, and ans.", &[("user", ctx.caller)]);
    }
    if arg.chars().count() > MAX_INPUT_CHARS {
        return say(
            ctx.server,
            ctx.dest,
            "calc.too_long",
            "{user}, that expression is too long.",
            &[("user", ctx.caller)],
        );
    }
    let key = (ctx.server.to_string(), ctx.profile_id.to_string());
    let previous = ANSWERS.with(|answers| answers.borrow().get(&key).copied());
    match expr::evaluate(arg, previous) {
        Ok(value) => {
            if !ctx.profile_id.is_empty() {
                ANSWERS.with(|answers| {
                    let mut answers = answers.borrow_mut();
                    if answers.len() >= MAX_REMEMBERED_ANSWERS && !answers.contains_key(&key) {
                        answers.clear();
                    }
                    answers.insert(key, value);
                });
            }
            say(
                ctx.server,
                ctx.dest,
                "calc.result",
                "{user}: {expr} = {result}",
                &[
                    ("user", ctx.caller),
                    ("expr", arg),
                    ("result", &format_number(value)),
                ],
            )?;
            award(ctx, "calc", None)
        }
        Err(error) => say(
            ctx.server,
            ctx.dest,
            "calc.error",
            "{user}, I couldn't parse that: {error}",
            &[("user", ctx.caller), ("error", error.0)],
        ),
    }
}

fn handle_convert(ctx: &Ctx, arg: &str) -> Result<(), Error> {
    if arg.is_empty() {
        return say(ctx.server, ctx.dest, "convert.usage", "Usage: !convert <amount> <unit> to <unit>, e.g. !convert 5 ft 10 in to cm, !convert 30 psi to bar, !convert 50 usd to gbp", &[("user", ctx.caller)]);
    }
    if arg.chars().count() > MAX_INPUT_CHARS {
        return say(
            ctx.server,
            ctx.dest,
            "convert.too_long",
            "{user}, that conversion is too long.",
            &[("user", ctx.caller)],
        );
    }
    let pints = pint_system(ctx.server, ctx.channel)?;
    match units::plan(arg, pints) {
        Ok(units::Plan::Physical { from, result, to }) => {
            // The amounts carry their own units ("5 ft 10 in"), so this reply has its own key.
            say(
                ctx.server,
                ctx.dest,
                "convert.physical_result",
                "{user}: {amounts} = {result} {to}",
                &[
                    ("user", ctx.caller),
                    ("amounts", &from),
                    ("result", &format_number(result)),
                    ("to", &to),
                ],
            )?;
            award(ctx, "convert", None)
        }
        Ok(units::Plan::Money { amount, from, to }) => convert_money(ctx, amount, &from, &to),
        Err(error) => say(
            ctx.server,
            ctx.dest,
            "convert.error",
            "{user}, {error}",
            &[("user", ctx.caller), ("error", &error)],
        ),
    }
}

fn convert_money(ctx: &Ctx, amount: f64, from: &str, to: &str) -> Result<(), Error> {
    let raw = unsafe {
        money_convert(serde_json::to_string(&MoneyConvertRequest {
            amount,
            from: from.into(),
            to: to.into(),
        })?)?
    };
    let response: MoneyConvertResponse = serde_json::from_str(&raw)?;
    let (Some(result), Some(from_code), Some(to_code), Some(rate)) = (
        response.result,
        response.from.as_deref(),
        response.to.as_deref(),
        response.rate,
    ) else {
        let (key, default, subject) = match response.error.as_deref() {
            Some("unknown_from") => (
                "convert.unknown",
                "{user}, I don't know '{unit}' as a unit or currency.",
                from,
            ),
            Some("unknown_to") => (
                "convert.unknown",
                "{user}, I don't know '{unit}' as a unit or currency.",
                to,
            ),
            Some("rate_limited") => (
                "convert.money_busy",
                "{user}, the exchange is rather busy; do try again in a minute.",
                "",
            ),
            _ => (
                "convert.money_unavailable",
                "{user}, I can't reach the exchange rates just now.",
                "",
            ),
        };
        return say(
            ctx.server,
            ctx.dest,
            key,
            default,
            &[("user", ctx.caller), ("unit", subject)],
        );
    };
    let source = match response.as_of.as_deref() {
        Some(date) => format!("{} {date}", response.sources.join(" + ")),
        None => response.sources.join(" + "),
    };
    say(
        ctx.server,
        ctx.dest,
        "convert.money_result",
        "{user}: {amount} {from} = {result} {to} (1 {from} = {rate} {to} · {source})",
        &[
            ("user", ctx.caller),
            ("amount", &format_money(amount)),
            ("from", from_code),
            ("result", &format_money(result)),
            ("to", to_code),
            ("rate", &format_significant(rate, 5)),
            ("source", &source),
        ],
    )?;
    award(ctx, "money", Some("currency_conversions"))
}

fn handle_crypto(ctx: &Ctx, arg: &str) -> Result<(), Error> {
    let symbols = arg
        .split_whitespace()
        .take(MAX_CRYPTO_SYMBOLS)
        .collect::<Vec<_>>();
    if symbols.is_empty() {
        return say(
            ctx.server,
            ctx.dest,
            "crypto.usage",
            "Usage: !crypto <coin> [coin] [coin], e.g. !crypto btc eth doge",
            &[("user", ctx.caller)],
        );
    }
    let mut quotes = Vec::new();
    let mut failures = Vec::new();
    for symbol in symbols {
        let raw = unsafe {
            crypto_quote(serde_json::to_string(&CryptoQuoteRequest {
                symbol: symbol.into(),
            })?)?
        };
        let quote: CryptoQuoteResponse = serde_json::from_str(&raw)?;
        match describe_quote(&quote) {
            Some(text) => quotes.push(text),
            None => failures.push((symbol, quote.error.unwrap_or_default())),
        }
    }
    if quotes.is_empty() {
        let (symbol, error) = failures.first().cloned().unwrap_or_default();
        return match error.as_str() {
            "unknown" => say(
                ctx.server,
                ctx.dest,
                "crypto.unknown",
                "{user}, I can't find a coin called '{coin}'.",
                &[("user", ctx.caller), ("coin", symbol)],
            ),
            "rate_limited" => say(
                ctx.server,
                ctx.dest,
                "crypto.busy",
                "{user}, the crypto markets are rate-limiting me; try again in a minute.",
                &[("user", ctx.caller)],
            ),
            _ => say(
                ctx.server,
                ctx.dest,
                "crypto.unavailable",
                "{user}, I can't reach the crypto prices just now.",
                &[("user", ctx.caller)],
            ),
        };
    }
    say(
        ctx.server,
        ctx.dest,
        "crypto.result",
        "{user}: {quotes} · CoinGecko",
        &[("user", ctx.caller), ("quotes", &quotes.join(" | "))],
    )?;
    award(ctx, "crypto", Some("crypto_quotes"))
}

/// "BTC (Bitcoin) $83,799 · £63,250 · €73,655 · 24h −1.1%".
fn describe_quote(quote: &CryptoQuoteResponse) -> Option<String> {
    let symbol = quote.symbol.as_deref()?;
    let usd = quote.price_usd?;
    let mut parts = vec![format!(
        "{symbol} ({}) ${}",
        quote.name.as_deref().unwrap_or(symbol),
        format_money(usd)
    )];
    if let Some(gbp) = quote.price_gbp {
        parts.push(format!("£{}", format_money(gbp)));
    }
    if let Some(eur) = quote.price_eur {
        parts.push(format!("€{}", format_money(eur)));
    }
    if let Some(change) = quote.change_24h.filter(|change| change.is_finite()) {
        let sign = if change >= 0.0 { "+" } else { "−" };
        parts.push(format!("24h {sign}{:.1}%", change.abs()));
    }
    Some(parts.join(" · "))
}

// ── number formatting ───────────────────────────────────────────────────────

/// Format a number for IRC: integers without a trailing `.0`, up to four decimals, significant
/// figures for tiny values, and scientific notation for very large ones.
pub(crate) fn format_number(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf" } else { "-inf" }.into();
    }
    // Avoid "-0".
    if value == 0.0 {
        return "0".into();
    }
    if value.abs() >= 1e15 {
        return scientific(value);
    }
    // Fixed 4-decimal rounding would print tiny results as "0" (1/30000, 5 mg to kg), so keep
    // four significant figures below 0.0001, switching to scientific notation when tiny.
    if value.abs() < 1e-4 {
        if value.abs() < 1e-12 {
            return format!("{value:.3e}");
        }
        return format_significant(value, 4);
    }
    let rounded = (value * 10_000.0).round() / 10_000.0;
    if rounded.fract() == 0.0 {
        return format!("{}", rounded as i64);
    }
    format!("{rounded}")
}

/// "1.152922e18" style: six significant figures, trailing zeros trimmed.
fn scientific(value: f64) -> String {
    let text = format!("{value:.5e}");
    match text.split_once('e') {
        Some((mantissa, exponent)) => {
            let mantissa = mantissa.trim_end_matches('0').trim_end_matches('.');
            format!("{mantissa}e{exponent}")
        }
        None => text,
    }
}

/// `value` to `figures` significant figures, without trailing zeros.
fn format_significant(value: f64, figures: i32) -> String {
    if value == 0.0 || !value.is_finite() {
        return format_number(value);
    }
    let magnitude = value.abs().log10().floor() as i32;
    let decimals = (figures - 1 - magnitude).clamp(0, 12) as usize;
    let text = format!("{value:.decimals$}");
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        text
    }
}

/// Money: two decimals with thousands separators from 1 up, significant figures below 1
/// (so 0.00059 BTC or 0.094 USD per DOGE stay readable).
fn format_money(value: f64) -> String {
    if value.abs() < 1.0 {
        return format_significant(value, 4);
    }
    if value.abs() >= 1e15 {
        return scientific(value);
    }
    let text = format!("{:.2}", value.abs());
    let (whole, cents) = text.split_once('.').unwrap_or((&text, "00"));
    let mut grouped = String::new();
    for (index, digit) in whole.chars().enumerate() {
        if index > 0 && (whole.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    let sign = if value < 0.0 { "-" } else { "" };
    format!("{sign}{grouped}.{cents}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_render_tidily() {
        assert_eq!(format_number(4.0), "4");
        assert_eq!(format_number(-7.0), "-7");
        assert_eq!(format_number(1.23456), "1.2346");
        assert_eq!(format_number(0.5), "0.5");
        assert_eq!(format_number(0.0), "0");
        assert_eq!(format_number(-0.0), "0");
        assert_eq!(format_number(1.0 / 30_000.0), "0.00003333");
        assert_eq!(format_number(0.000005), "0.000005");
        assert_eq!(format_number(2e-15), "2.000e-15");
        assert_eq!(format_number(1_152_921_504_606_846_976.0), "1.15292e18");
        assert_eq!(format_number(123_456_789.0), "123456789");
    }

    #[test]
    fn money_renders_with_separators_and_significant_figures() {
        assert_eq!(format_money(37.697_75), "37.70");
        assert_eq!(format_money(36_827.649_8), "36,827.65");
        assert_eq!(format_money(1_234_567.891), "1,234,567.89");
        assert_eq!(format_money(0.000_596_8), "0.0005968");
        assert_eq!(format_money(-1_500.0), "-1,500.00");
        assert_eq!(format_significant(0.753_955, 5), "0.75396");
        assert_eq!(format_significant(73_655.299, 5), "73655");
    }

    #[test]
    fn quotes_describe_price_and_change() {
        let quote = CryptoQuoteResponse {
            symbol: Some("BTC".into()),
            name: Some("Bitcoin".into()),
            price_usd: Some(83_799.0),
            change_24h: Some(-1.08),
            price_gbp: Some(63_250.0),
            price_eur: None,
            error: None,
        };
        assert_eq!(
            describe_quote(&quote).unwrap(),
            "BTC (Bitcoin) $83,799.00 · £63,250.00 · 24h −1.1%"
        );
    }
}
