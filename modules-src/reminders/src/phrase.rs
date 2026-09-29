//! Forgiving reminder phrasing, parsed without side effects.
//!
//! Who, when, and what may come in either order:
//! `me in 10 minutes to X`, `me to X in 10 minutes`, `at 5:30pm next tuesday to X`,
//! `tomorrow at 9 to X`, `on dec 25 to X`, `sally at 10 to X`, `me every monday at 18:00 to X`.
//! Calendar work is shared with the clock module's `when.rs`.

use crate::when::{self, Date};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecurDays {
    Daily,
    /// Monday to Friday.
    Weekdays,
    /// 0 = Monday … 6 = Sunday.
    Weekly(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Recurrence {
    pub days: RecurDays,
    pub hour: u32,
    pub minute: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum When {
    /// Seconds from now.
    In(i64),
    /// A local wall-clock time in the owner's timezone.
    At {
        date: Date,
        hour: u32,
        minute: u32,
    },
    Every(Recurrence),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Who {
    Me,
    Nick(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parsed {
    pub who: Who,
    pub when: When,
    pub text: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    /// Nothing usable; show the usage line.
    Usage,
    /// There's a message but no time we understood.
    NoTime,
}

/// The owner's local "now", for resolving "at 5" and "tomorrow".
#[derive(Clone, Copy, Debug)]
pub struct LocalNow {
    pub today: Date,
    /// Minutes since local midnight.
    pub minutes: u32,
}

/// Words that begin a time phrase, so `!remind in 5m to X` needs no "me".
const TIME_STARTS: &[&str] = &[
    "in", "at", "on", "to", "tomorrow", "today", "tonight", "next", "every", "daily",
];

pub fn parse(arg: &str, now: LocalNow) -> Result<Parsed, ParseError> {
    let words = arg.split_whitespace().collect::<Vec<_>>();
    let Some(first) = words.first() else {
        return Err(ParseError::Usage);
    };
    let first_lower = first.to_lowercase();
    let (who, rest) = if first_lower == "me" {
        (Who::Me, &words[1..])
    } else if TIME_STARTS.contains(&first_lower.as_str())
        || when::weekday_number(&first_lower).is_some()
        || parse_when(&words.join(" "), now).is_some()
    {
        (Who::Me, &words[..])
    } else {
        (
            Who::Nick(first.trim_end_matches([',', ':']).to_string()),
            &words[1..],
        )
    };
    if rest.is_empty() {
        return Err(ParseError::Usage);
    }
    let (when, text) = split_when_and_text(rest, now).ok_or(ParseError::NoTime)?;
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err(ParseError::Usage);
    }
    Ok(Parsed { who, when, text })
}

/// Find the time phrase and the message in either order.
fn split_when_and_text(words: &[&str], now: LocalNow) -> Option<(When, String)> {
    let lower = words
        .iter()
        .map(|word| word.to_lowercase())
        .collect::<Vec<_>>();
    let join = |range: &[&str]| range.join(" ");
    if lower[0] == "to" {
        // `to X <when>`: the longest trailing phrase that is a time.
        for start in 2..words.len() {
            if let Some(when) = parse_when(&join(&words[start..]), now) {
                return Some((when, join(&words[1..start])));
            }
        }
        return None;
    }
    // `<when> to X`: the first " to " whose prefix is a time.
    for (index, word) in lower.iter().enumerate().skip(1) {
        if word == "to" {
            if let Some(when) = parse_when(&join(&words[..index]), now) {
                return Some((when, join(&words[index + 1..])));
            }
        }
    }
    // `<when> X` without "to": the longest leading phrase that is a time.
    for end in (1..words.len()).rev() {
        if let Some(when) = parse_when(&join(&words[..end]), now) {
            return Some((when, join(&words[end..])));
        }
    }
    None
}

/// One time phrase: `in 2 hours`, `at 5`, `tomorrow at 9`, `every weekday at 8:30`, `dec 25`.
pub fn parse_when(phrase: &str, now: LocalNow) -> Option<When> {
    let lower = phrase.trim().to_lowercase();
    let lower = lower.trim_end_matches(['.', '!', ',']).trim();
    if lower.is_empty() {
        return None;
    }
    if let Some(duration) = lower.strip_prefix("in ") {
        return parse_duration(duration).map(When::In);
    }
    if let Some(recurrence) = parse_recurrence(lower) {
        return Some(When::Every(recurrence));
    }
    parse_moment(lower, now)
}

fn parse_recurrence(lower: &str) -> Option<Recurrence> {
    let rest = lower
        .strip_prefix("every ")
        .map(str::trim)
        .or_else(|| lower.strip_prefix("daily").map(|rest| rest.trim()))?;
    let is_daily = lower.starts_with("daily");
    let (days, rest) = if is_daily {
        (RecurDays::Daily, rest)
    } else {
        let (word, rest) = rest.split_once(' ').unwrap_or((rest, ""));
        let days = match word {
            "day" => RecurDays::Daily,
            "weekday" => RecurDays::Weekdays,
            other => RecurDays::Weekly(when::weekday_number(other.trim_end_matches('s'))?),
        };
        (days, rest.trim())
    };
    let clock = rest.strip_prefix("at ").unwrap_or(rest).trim();
    let (hour, minute, _) = clock_time(clock)?;
    Some(Recurrence { days, hour, minute })
}

/// A clock time: "5pm", "17:30", "noon", or a bare "5" / "530" / "5:30" (flagged ambiguous when
/// it could be morning or evening).
fn clock_time(text: &str) -> Option<(u32, u32, bool)> {
    let text = text.trim();
    if let Some((hour, minute, rest)) = when::parse_clock(text) {
        if rest.trim().is_empty() {
            let explicit = text.contains(['a', 'p', 'n']) || hour > 12 || text.starts_with('0');
            return Some((hour, minute, !explicit && (1..=12).contains(&hour)));
        }
        return None;
    }
    let digits = text.replace(':', "");
    if !digits.chars().all(|ch| ch.is_ascii_digit()) || digits.is_empty() || digits.len() > 4 {
        return None;
    }
    let (hour, minute) = if digits.len() <= 2 {
        (digits.parse::<u32>().ok()?, 0)
    } else {
        let split = digits.len() - 2;
        (digits[..split].parse().ok()?, digits[split..].parse().ok()?)
    };
    if hour > 23 || minute > 59 {
        return None;
    }
    let ambiguous = (1..=12).contains(&hour) && !digits.starts_with('0');
    Some((hour, minute, ambiguous))
}

/// A one-off moment. Accepts the time before or after the date ("at 5 tomorrow",
/// "tomorrow at 5"), "today", and a leading "on".
fn parse_moment(lower: &str, now: LocalNow) -> Option<When> {
    let lower = lower.strip_prefix("on ").unwrap_or(lower);
    let words = lower.split_whitespace().collect::<Vec<_>>();
    // Pull out an "at <time>" (or a trailing bare clock like "5pm") wherever it is.
    let mut clock = None;
    let mut date_words = Vec::new();
    let mut index = 0;
    while index < words.len() {
        let word = words[index];
        if word == "at" && index + 1 < words.len() {
            // "5 pm" is two words.
            let two = words
                .get(index + 2)
                .map(|next| format!("{} {next}", words[index + 1]));
            if let Some(found) = two.as_deref().and_then(clock_time) {
                clock = Some(found);
                index += 3;
                continue;
            }
            clock = Some(clock_time(words[index + 1])?);
            index += 2;
            continue;
        }
        if clock.is_none() && when::parse_clock(word).is_some_and(|(_, _, rest)| rest.is_empty()) {
            clock = clock_time(word);
            index += 1;
            continue;
        }
        date_words.push(word);
        index += 1;
    }
    let date_text = date_words.join(" ");
    let date_text = match date_text.as_str() {
        "today" | "tonight" => String::new(),
        _ => date_text,
    };
    if date_text.is_empty() && clock.is_none() {
        return None;
    }
    let resolve = |hour: u32, minute: u32| {
        when::parse_target(
            &format!("{date_text} {hour:02}:{minute:02}"),
            now.today,
            now.minutes,
        )
    };
    let (hour, minute) = match clock {
        None => (9, 0), // a date alone means that morning
        Some((hour, minute, false)) => (hour, minute),
        Some((hour, minute, true)) => {
            let evening = if hour == 12 { 0 } else { hour + 12 };
            if date_text.is_empty() {
                // "at 5" means the next five o'clock, morning or evening.
                let soonest = [hour % 12 + if hour == 12 { 12 } else { 0 }, evening]
                    .into_iter()
                    .map(|candidate| candidate % 24)
                    .min_by_key(|candidate| {
                        (i64::from(candidate * 60 + minute) - i64::from(now.minutes) - 1)
                            .rem_euclid(24 * 60)
                    })?;
                (soonest, minute)
            } else if (1..=7).contains(&hour) {
                (hour + 12, minute)
            } else {
                (hour % 12 + if hour == 12 { 12 } else { 0 }, minute)
            }
        }
    };
    let target = resolve(hour, minute)?;
    // parse_target counts named events and plain dates; a lone word like "tonight" is handled.
    Some(When::At {
        date: target.date,
        hour: target.hour,
        minute: target.minute,
    })
}

/// "10 minutes", "1h30m", "an hour", "a week", "2 days and 3 hours", "half an hour".
pub fn parse_duration(input: &str) -> Option<i64> {
    let text = input
        .trim()
        .to_lowercase()
        .replace(" and ", " ")
        .replace(',', " ");
    if text == "half an hour" || text == "half hour" {
        return Some(30 * 60);
    }
    let chars = text.chars().collect::<Vec<_>>();
    let mut index = 0;
    let mut total = 0i64;
    let mut parts = 0;
    while index < chars.len() {
        while index < chars.len() && chars[index].is_whitespace() {
            index += 1;
        }
        if index >= chars.len() {
            break;
        }
        let number_start = index;
        while index < chars.len() && chars[index].is_ascii_digit() {
            index += 1;
        }
        let number = if number_start == index {
            // "a"/"an" count as one.
            let word_start = index;
            while index < chars.len() && chars[index].is_alphabetic() {
                index += 1;
            }
            match chars[word_start..index].iter().collect::<String>().as_str() {
                "a" | "an" | "one" => 1,
                _ => return None,
            }
        } else {
            chars[number_start..index]
                .iter()
                .collect::<String>()
                .parse::<i64>()
                .ok()?
        };
        while index < chars.len() && chars[index].is_whitespace() {
            index += 1;
        }
        let unit_start = index;
        while index < chars.len() && chars[index].is_alphabetic() {
            index += 1;
        }
        let unit = chars[unit_start..index].iter().collect::<String>();
        let multiplier = match unit.as_str() {
            "s" | "sec" | "secs" | "second" | "seconds" => 1,
            "m" | "min" | "mins" | "minute" | "minutes" => 60,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3_600,
            "d" | "day" | "days" => 86_400,
            "w" | "wk" | "wks" | "week" | "weeks" => 7 * 86_400,
            _ => return None,
        };
        total = total.checked_add(number.checked_mul(multiplier)?)?;
        parts += 1;
    }
    (parts > 0 && total > 0).then_some(total)
}

/// The next occurrence of a recurrence strictly after local `now`, as (date, hour, minute).
pub fn next_occurrence(recurrence: Recurrence, now: LocalNow) -> (Date, u32, u32) {
    let at = recurrence.hour * 60 + recurrence.minute;
    for offset in 0..8 {
        let date = when::add_days(now.today, offset);
        if offset == 0 && at <= now.minutes {
            continue;
        }
        let weekday = when::weekday_index(date);
        let matches = match recurrence.days {
            RecurDays::Daily => true,
            RecurDays::Weekdays => weekday < 5,
            RecurDays::Weekly(day) => weekday == day,
        };
        if matches {
            return (date, recurrence.hour, recurrence.minute);
        }
    }
    (
        when::add_days(now.today, 7),
        recurrence.hour,
        recurrence.minute,
    )
}

/// "every day at 09:00", "every weekday at 08:30", "every Monday at 18:00".
pub fn describe_recurrence(recurrence: Recurrence) -> String {
    let days = match recurrence.days {
        RecurDays::Daily => "every day".to_string(),
        RecurDays::Weekdays => "every weekday".to_string(),
        RecurDays::Weekly(day) => format!("every {}", when::WEEKDAYS[day as usize]),
    };
    format!("{days} at {:02}:{:02}", recurrence.hour, recurrence.minute)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Monday 2026-09-28, 14:00 local.
    fn now() -> LocalNow {
        LocalNow {
            today: Date {
                year: 2026,
                month: 9,
                day: 28,
            },
            minutes: 14 * 60,
        }
    }

    fn day(day: u32) -> Date {
        Date {
            year: 2026,
            month: if day >= 28 { 9 } else { 10 },
            day,
        }
    }

    fn at(date: Date, hour: u32, minute: u32) -> When {
        When::At { date, hour, minute }
    }

    fn parsed(arg: &str) -> (Who, When, String) {
        let parsed = parse(arg, now()).unwrap_or_else(|error| panic!("{arg}: {error:?}"));
        (parsed.who, parsed.when, parsed.text)
    }

    #[test]
    fn durations_are_forgiving() {
        assert_eq!(parse_duration("10 minutes"), Some(600));
        assert_eq!(parse_duration("1h30m"), Some(5_400));
        assert_eq!(parse_duration("an hour"), Some(3_600));
        assert_eq!(parse_duration("a week"), Some(604_800));
        assert_eq!(parse_duration("2 days and 3 hours"), Some(183_600));
        assert_eq!(parse_duration("half an hour"), Some(1_800));
        assert_eq!(parse_duration("0 minutes"), None);
        assert_eq!(parse_duration("tomorrow"), None);
    }

    #[test]
    fn either_order_and_optional_me() {
        let expected = (Who::Me, When::In(600), "check the oven".to_string());
        assert_eq!(parsed("me in 10 minutes to check the oven"), expected);
        assert_eq!(parsed("me to check the oven in 10 minutes"), expected);
        assert_eq!(parsed("in 10 minutes to check the oven"), expected);
        assert_eq!(parsed("to check the oven in 10 minutes"), expected);
        assert_eq!(parsed("me in 10 minutes check the oven"), expected);
        assert_eq!(
            parsed("ME IN 2 Hours TO stretch").1,
            When::In(7_200),
            "case doesn't matter"
        );
        assert_eq!(
            parsed("me to take the 5 train in an hour"),
            (Who::Me, When::In(3_600), "take the 5 train".into())
        );
    }

    #[test]
    fn clock_times_dates_and_weekdays() {
        assert_eq!(parsed("me at 5pm to call mum").1, at(day(28), 17, 0));
        assert_eq!(parsed("me at 5:30 to call mum").1, at(day(28), 17, 30));
        assert_eq!(parsed("me at 530 to call mum").1, at(day(28), 17, 30));
        assert_eq!(
            parsed("me at 9 to call mum").1,
            at(day(28), 21, 0),
            "a bare 9 at 2pm means 9pm"
        );
        assert_eq!(parsed("me at 15:15 to eat").1, at(day(28), 15, 15));
        assert_eq!(
            parsed("me at 13:15 to eat").1,
            at(day(29), 13, 15),
            "a passed time today means tomorrow"
        );
        assert_eq!(parsed("me tomorrow at 9 to call mum").1, at(day(29), 9, 0));
        assert_eq!(
            parsed("me at 9am tomorrow to call mum").1,
            at(day(29), 9, 0)
        );
        assert_eq!(
            parsed("me at 530 next tuesday to call mum").1,
            at(day(29), 17, 30),
            "evening for small bare hours on a named day"
        );
        assert_eq!(parsed("me on friday to pay rent").1, at(day(2), 9, 0));
        assert_eq!(
            parsed("me on dec 25 at 8pm to call").1,
            at(
                Date {
                    year: 2026,
                    month: 12,
                    day: 25
                },
                20,
                0
            )
        );
        assert_eq!(
            parsed("me to call mum tomorrow at 5 pm").1,
            at(day(29), 17, 0)
        );
    }

    #[test]
    fn recurrences_and_other_people() {
        let (who, when, text) = parsed("me every monday at 18:00 to take the bins out");
        assert_eq!(who, Who::Me);
        assert_eq!(
            when,
            When::Every(Recurrence {
                days: RecurDays::Weekly(0),
                hour: 18,
                minute: 0
            })
        );
        assert_eq!(text, "take the bins out");
        assert_eq!(
            parsed("me every weekday at 8:30 to stand up").1,
            When::Every(Recurrence {
                days: RecurDays::Weekdays,
                hour: 8,
                minute: 30
            })
        );
        assert_eq!(
            parsed("sally at 10 to eat cheese"),
            (
                Who::Nick("sally".into()),
                at(day(28), 22, 0),
                "eat cheese".into()
            )
        );
        assert_eq!(parse("", now()), Err(ParseError::Usage));
        assert_eq!(parse("me to do things", now()), Err(ParseError::NoTime));
    }

    #[test]
    fn recurrences_find_their_next_day() {
        let weekly = Recurrence {
            days: RecurDays::Weekly(0),
            hour: 9,
            minute: 0,
        };
        // Monday 14:00: Monday 09:00 has passed, so next week.
        assert_eq!(next_occurrence(weekly, now()).0, day(5));
        let daily = Recurrence {
            days: RecurDays::Daily,
            hour: 18,
            minute: 0,
        };
        assert_eq!(next_occurrence(daily, now()).0, day(28));
        assert_eq!(describe_recurrence(weekly), "every Monday at 09:00");
    }
}
