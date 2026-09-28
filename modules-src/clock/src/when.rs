//! Pure parsing and calendar arithmetic for `!time` conversions and `!until`.

/// A civil date.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Date {
    pub year: i32,
    pub month: u32,
    pub day: u32,
}

/// Days since 1970-01-01 (Hinnant's days_from_civil).
pub fn days_from_civil(date: Date) -> i64 {
    let year = i64::from(date.year) - i64::from(date.month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = i64::from(date.month);
    let day_of_year =
        (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(date.day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The civil date `days` after 1970-01-01.
pub fn civil_from_days(days: i64) -> Date {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = (year_of_era + era * 400 + i64::from(month <= 2)) as i32;
    Date { year, month, day }
}

pub fn add_days(date: Date, days: i64) -> Date {
    civil_from_days(days_from_civil(date) + days)
}

/// 0 = Monday … 6 = Sunday.
pub fn weekday_index(date: Date) -> u32 {
    // 1970-01-01 was a Thursday (index 3).
    ((days_from_civil(date) + 3).rem_euclid(7)) as u32
}

pub const WEEKDAYS: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];

pub const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

pub fn valid(date: Date) -> bool {
    (1..=12).contains(&date.month)
        && date.day >= 1
        && civil_from_days(days_from_civil(date)) == date
}

fn month_number(word: &str) -> Option<u32> {
    let word = word.trim_end_matches('.').to_lowercase();
    if word.len() < 3 {
        return None;
    }
    MONTHS
        .iter()
        .position(|month| month.to_lowercase().starts_with(&word))
        .map(|index| index as u32 + 1)
}

fn weekday_number(word: &str) -> Option<u32> {
    let word = word.to_lowercase();
    if word.len() < 3 {
        return None;
    }
    WEEKDAYS
        .iter()
        .position(|day| day.to_lowercase().starts_with(&word))
        .map(|index| index as u32)
}

/// Parse a clock time at the start of `input`: "3pm", "3:30 pm", "15:00", "noon", "midnight".
/// Returns (hour, minute, rest of input).
pub fn parse_clock(input: &str) -> Option<(u32, u32, &str)> {
    let input = input.trim_start();
    let lower = input.to_lowercase();
    for (word, hour) in [("noon", 12), ("midday", 12), ("midnight", 0)] {
        if lower.starts_with(word)
            && !lower[word.len()..]
                .chars()
                .next()
                .is_some_and(char::is_alphanumeric)
        {
            return Some((hour, 0, &input[word.len()..]));
        }
    }
    let digits = input.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || digits > 2 {
        return None;
    }
    let mut hour: u32 = input[..digits].parse().ok()?;
    let mut rest = &input[digits..];
    let mut minute = 0;
    let mut has_minutes = false;
    if let Some(after) = rest.strip_prefix(':').or_else(|| rest.strip_prefix('.')) {
        let minute_digits = after.chars().take_while(char::is_ascii_digit).count();
        if minute_digits != 2 {
            return None;
        }
        minute = after[..2].parse().ok()?;
        rest = &after[2..];
        has_minutes = true;
    }
    let trimmed = rest.trim_start();
    let lower = trimmed.to_lowercase();
    let meridiem = ["am", "a.m.", "pm", "p.m."]
        .iter()
        .find(|suffix| {
            lower.starts_with(**suffix)
                && !lower[suffix.len()..]
                    .chars()
                    .next()
                    .is_some_and(char::is_alphanumeric)
        })
        .copied();
    if let Some(suffix) = meridiem {
        if !(1..=12).contains(&hour) {
            return None;
        }
        let pm = suffix.starts_with('p');
        hour = match (hour, pm) {
            (12, false) => 0,
            (12, true) => 12,
            (hour, true) => hour + 12,
            (hour, false) => hour,
        };
        rest = &trimmed[suffix.len()..];
    } else if !has_minutes {
        // A bare number isn't a time ("!time 5 things"); require minutes or am/pm.
        return None;
    }
    if hour > 23 || minute > 59 {
        return None;
    }
    // The time must end at a word boundary.
    if rest.chars().next().is_some_and(char::is_alphanumeric) {
        return None;
    }
    Some((hour, minute, rest))
}

/// What `!until` is counting down to.
#[derive(Debug, PartialEq, Eq)]
pub struct Target {
    pub date: Date,
    pub hour: u32,
    pub minute: u32,
    /// A friendly name for named events ("Christmas").
    pub name: Option<&'static str>,
}

/// Named yearly events: (names, month, day, display).
const EVENTS: &[(&[&str], u32, u32, &str)] = &[
    (&["christmas", "xmas", "christmas day"], 12, 25, "Christmas"),
    (&["christmas eve"], 12, 24, "Christmas Eve"),
    (&["boxing day"], 12, 26, "Boxing Day"),
    (
        &[
            "new year",
            "new years",
            "new year's",
            "new years day",
            "new year's day",
        ],
        1,
        1,
        "New Year's Day",
    ),
    (
        &["new years eve", "new year's eve", "nye"],
        12,
        31,
        "New Year's Eve",
    ),
    (&["halloween"], 10, 31, "Halloween"),
    (
        &[
            "valentines",
            "valentine's",
            "valentines day",
            "valentine's day",
        ],
        2,
        14,
        "Valentine's Day",
    ),
    (&["bonfire night", "guy fawkes"], 11, 5, "Bonfire Night"),
    (
        &[
            "st patricks",
            "st patrick's",
            "st patricks day",
            "st patrick's day",
        ],
        3,
        17,
        "St Patrick's Day",
    ),
    (&["april fools", "april fool's"], 4, 1, "April Fools' Day"),
    (
        &["independence day", "fourth of july", "4th of july"],
        7,
        4,
        "Independence Day",
    ),
];

/// Parse an `!until` target relative to `today` and the current local clock time. Dates without
/// a year roll to next year once passed; weekdays mean the next such day.
pub fn parse_target(input: &str, today: Date, now_minutes: u32) -> Option<Target> {
    let input = input.trim().trim_end_matches(['!', '?', '.']);
    let lower = input.to_lowercase();
    let lower = lower.trim();
    let (lower, clock) = split_trailing_clock(lower);
    let (hour, minute) = clock.unwrap_or((0, 0));
    let next_occurrence = |month: u32, day: u32| {
        let this_year = Date {
            year: today.year,
            month,
            day,
        };
        let passed = this_year < today || (this_year == today && hour * 60 + minute <= now_minutes);
        let date = if passed {
            Date {
                year: today.year + 1,
                ..this_year
            }
        } else {
            this_year
        };
        valid(date).then_some(date)
    };
    if let Some((_, month, day, name)) = EVENTS
        .iter()
        .find(|(names, ..)| names.contains(&lower.as_str()))
    {
        let date = next_occurrence(*month, *day)?;
        return Some(Target {
            date,
            hour,
            minute,
            name: Some(name),
        });
    }
    if lower.is_empty() {
        // Just a time: today if still ahead, otherwise tomorrow.
        let (hour, minute) = clock?;
        let date = if hour * 60 + minute > now_minutes {
            today
        } else {
            add_days(today, 1)
        };
        return Some(Target {
            date,
            hour,
            minute,
            name: None,
        });
    }
    if lower == "tomorrow" {
        return Some(Target {
            date: add_days(today, 1),
            hour,
            minute,
            name: None,
        });
    }
    // "friday", "next friday": the next such day (today only if the time is still ahead).
    let weekday_word = lower.strip_prefix("next ").unwrap_or(&lower);
    if let Some(weekday) = weekday_number(weekday_word) {
        let mut offset = (i64::from(weekday) - i64::from(weekday_index(today))).rem_euclid(7);
        if offset == 0 && (clock.is_none() || hour * 60 + minute <= now_minutes) {
            offset = 7;
        }
        return Some(Target {
            date: add_days(today, offset),
            hour,
            minute,
            name: None,
        });
    }
    // ISO: 2026-12-25.
    let parts = lower.split(['-', '/']).collect::<Vec<_>>();
    if let [year, month, day] = parts.as_slice() {
        if year.len() == 4 {
            let date = Date {
                year: year.parse().ok()?,
                month: month.parse().ok()?,
                day: day.parse().ok()?,
            };
            return valid(date).then_some(Target {
                date,
                hour,
                minute,
                name: None,
            });
        }
    }
    // "dec 25", "25 december", "december 25 2026", "25th dec".
    let words = lower
        .split(|ch: char| ch.is_whitespace() || ch == ',')
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    let mut month = None;
    let mut day = None;
    let mut year = None;
    for word in &words {
        if let Some(number) = month_number(word) {
            month = Some(number);
            continue;
        }
        let digits = word.trim_end_matches(|ch: char| ch.is_alphabetic());
        let Ok(number) = digits.parse::<u32>() else {
            return None;
        };
        if digits.len() == 4 {
            year = Some(number as i32);
        } else if day.is_none() && (1..=31).contains(&number) {
            day = Some(number);
        } else {
            return None;
        }
    }
    let (month, day) = (month?, day?);
    let date = match year {
        Some(year) => {
            let date = Date { year, month, day };
            valid(date).then_some(date)?
        }
        None => next_occurrence(month, day)?,
    };
    Some(Target {
        date,
        hour,
        minute,
        name: None,
    })
}

/// Split a trailing "at 8pm" / "20:00" off a lowercase target.
fn split_trailing_clock(lower: &str) -> (String, Option<(u32, u32)>) {
    // Try each suffix that starts at a word boundary, longest first.
    let starts = std::iter::once(0)
        .chain(lower.match_indices(' ').map(|(index, _)| index + 1))
        .collect::<Vec<_>>();
    for start in starts {
        let candidate = &lower[start..];
        if let Some((hour, minute, rest)) = parse_clock(candidate) {
            if rest.trim().is_empty() {
                let head = lower[..start].trim_end();
                let head = head.strip_suffix(" at").unwrap_or(head);
                let head = if head == "at" { "" } else { head };
                return (head.trim().to_string(), Some((hour, minute)));
            }
        }
    }
    (lower.to_string(), None)
}

/// "87 days, 4 hours", "3 hours, 12 minutes", "5 minutes".
pub fn describe_duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let units = [
        (365 * 86_400, "year"),
        (86_400, "day"),
        (3_600, "hour"),
        (60, "minute"),
    ];
    let mut parts = Vec::new();
    let mut remaining = seconds;
    for (size, name) in units {
        let value = remaining / size;
        if value > 0 {
            parts.push(format!(
                "{value} {name}{}",
                if value == 1 { "" } else { "s" }
            ));
            remaining %= size;
        }
        if parts.len() == 2 {
            break;
        }
    }
    if parts.is_empty() {
        "less than a minute".into()
    } else {
        parts.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(year: i32, month: u32, day: u32) -> Date {
        Date { year, month, day }
    }

    #[test]
    fn civil_arithmetic_round_trips() {
        assert_eq!(days_from_civil(date(1970, 1, 1)), 0);
        assert_eq!(civil_from_days(0), date(1970, 1, 1));
        assert_eq!(add_days(date(2026, 12, 31), 1), date(2027, 1, 1));
        assert_eq!(add_days(date(2024, 2, 28), 1), date(2024, 2, 29));
        assert_eq!(
            WEEKDAYS[weekday_index(date(2026, 9, 28)) as usize],
            "Monday"
        );
        assert!(!valid(date(2026, 2, 29)));
        assert!(valid(date(2028, 2, 29)));
    }

    #[test]
    fn clock_times_parse_in_common_forms() {
        assert_eq!(parse_clock("3pm PST"), Some((15, 0, " PST")));
        assert_eq!(
            parse_clock("3:30 pm in London"),
            Some((15, 30, " in London"))
        );
        assert_eq!(parse_clock("12am"), Some((0, 0, "")));
        assert_eq!(parse_clock("12 pm"), Some((12, 0, "")));
        assert_eq!(parse_clock("15:00 UTC"), Some((15, 0, " UTC")));
        assert_eq!(parse_clock("noon"), Some((12, 0, "")));
        assert_eq!(parse_clock("5 things"), None);
        assert_eq!(parse_clock("25:00"), None);
        assert_eq!(parse_clock("13pm"), None);
        assert_eq!(parse_clock("paris"), None);
    }

    #[test]
    fn until_targets_roll_forward() {
        let today = date(2026, 9, 28); // a Monday
        let noon = 12 * 60;
        let christmas = parse_target("christmas", today, noon).unwrap();
        assert_eq!(christmas.date, date(2026, 12, 25));
        assert_eq!(christmas.name, Some("Christmas"));
        assert_eq!(
            parse_target("new year", today, noon).unwrap().date,
            date(2027, 1, 1)
        );
        assert_eq!(
            parse_target("dec 25", today, noon).unwrap().date,
            date(2026, 12, 25)
        );
        assert_eq!(
            parse_target("25th December", today, noon).unwrap().date,
            date(2026, 12, 25)
        );
        assert_eq!(
            parse_target("jan 5", today, noon).unwrap().date,
            date(2027, 1, 5)
        );
        assert_eq!(
            parse_target("2027-03-01", today, noon).unwrap().date,
            date(2027, 3, 1)
        );
        let friday = parse_target("friday 8pm", today, noon).unwrap();
        assert_eq!((friday.date, friday.hour), (date(2026, 10, 2), 20));
        assert_eq!(
            parse_target("monday", today, noon).unwrap().date,
            date(2026, 10, 5)
        );
        assert_eq!(
            parse_target("monday at 6pm", today, noon).unwrap().date,
            today
        );
        assert_eq!(parse_target("20:00", today, noon).unwrap().date, today);
        assert_eq!(
            parse_target("9am", today, noon).unwrap().date,
            date(2026, 9, 29)
        );
        assert_eq!(
            parse_target("tomorrow at noon", today, noon).unwrap().hour,
            12
        );
        assert!(parse_target("feb 30", today, noon).is_none());
        assert!(parse_target("whenever", today, noon).is_none());
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(
            describe_duration(87 * 86_400 + 4 * 3600 + 59),
            "87 days, 4 hours"
        );
        assert_eq!(describe_duration(3 * 3600 + 12 * 60), "3 hours, 12 minutes");
        assert_eq!(describe_duration(30), "less than a minute");
    }
}
