//! What stats keeps, and the pure arithmetic over it: counting a line, days and weeks in the
//! channel's timezone, periods, streaks, and rankings. No host calls, so it is tested natively.

use serde::{Deserialize, Serialize};

pub(crate) const DAY: i64 = 86_400;
/// Daily counts kept per person: enough for today, week, and month boards.
pub(crate) const PERSON_DAYS: usize = 35;
/// Daily totals kept per channel: a year and a bit, for "busiest day ever".
pub(crate) const CHANNEL_DAYS: usize = 400;

/// Tallies of one kind of talk. Never the words themselves.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Counts {
    #[serde(default)]
    pub(crate) lines: u64,
    #[serde(default)]
    pub(crate) words: u64,
    #[serde(default)]
    pub(crate) questions: u64,
    #[serde(default)]
    pub(crate) exclamations: u64,
    /// Lines in capitals ("SHOUTING").
    #[serde(default)]
    pub(crate) shouts: u64,
    #[serde(default)]
    pub(crate) links: u64,
    /// `/me` actions.
    #[serde(default)]
    pub(crate) actions: u64,
}

impl Counts {
    pub(crate) fn add(&mut self, text: &str, is_action: bool) {
        let text = text.trim();
        self.lines += 1;
        self.words += text.split_whitespace().count() as u64;
        if is_action {
            self.actions += 1;
        }
        if text.ends_with('?') {
            self.questions += 1;
        }
        if text.ends_with('!') {
            self.exclamations += 1;
        }
        let letters = text.chars().filter(|ch| ch.is_alphabetic()).count();
        if letters >= 5
            && text
                .chars()
                .filter(|ch| ch.is_alphabetic())
                .all(char::is_uppercase)
        {
            self.shouts += 1;
        }
        let lower = text.to_ascii_lowercase();
        if lower.contains("http://") || lower.contains("https://") {
            self.links += 1;
        }
    }
}

/// One Monday-to-Sunday week of counts, for weekly awards and the digest.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Week {
    #[serde(default)]
    pub(crate) week: i64,
    #[serde(default)]
    pub(crate) counts: Counts,
    /// Lines between midnight and five.
    #[serde(default)]
    pub(crate) night: u64,
}

/// One person in one channel.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Person {
    /// How they were last seen named, for boards.
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) total: Counts,
    /// Lines by local hour.
    #[serde(default)]
    pub(crate) hours: Vec<u64>,
    /// `(local day, lines)`, oldest first, at most [`PERSON_DAYS`].
    #[serde(default)]
    pub(crate) days: Vec<(i64, u64)>,
    #[serde(default)]
    pub(crate) first_day: i64,
    #[serde(default)]
    pub(crate) last_day: i64,
    #[serde(default)]
    pub(crate) streak: u32,
    #[serde(default)]
    pub(crate) best_streak: u32,
    #[serde(default)]
    pub(crate) this_week: Week,
    #[serde(default)]
    pub(crate) last_week: Week,
}

/// What one line changed for achievements.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct LineOutcome {
    pub(crate) night: bool,
    pub(crate) morning: bool,
    /// The streak just reached a multiple of seven days.
    pub(crate) week_streak: bool,
}

impl Person {
    pub(crate) fn record(
        &mut self,
        name: &str,
        text: &str,
        is_action: bool,
        at: LocalTime,
    ) -> LineOutcome {
        let mut outcome = LineOutcome::default();
        self.name = name.chars().take(64).collect();
        self.total.add(text, is_action);
        if self.hours.len() != 24 {
            self.hours.resize(24, 0);
        }
        self.hours[at.hour as usize] += 1;
        bump_day(&mut self.days, at.day, PERSON_DAYS);
        if self.first_day == 0 {
            self.first_day = at.day;
        }
        if self.last_day != at.day {
            self.streak = if self.last_day == at.day - 1 {
                self.streak + 1
            } else {
                1
            };
            self.best_streak = self.best_streak.max(self.streak);
            self.last_day = at.day;
            outcome.week_streak = self.streak.is_multiple_of(7);
        }
        let week = week_of(at.day);
        if self.this_week.week != week {
            self.last_week = if self.this_week.week == week - 1 {
                std::mem::take(&mut self.this_week)
            } else {
                Week::default()
            };
            self.this_week = Week {
                week,
                ..Week::default()
            };
        }
        self.this_week.counts.add(text, is_action);
        outcome.night = at.hour < 5;
        outcome.morning = (5..9).contains(&at.hour);
        if outcome.night {
            self.this_week.night += 1;
        }
        outcome
    }

    /// Lines in the last `days` local days, today included.
    pub(crate) fn lines_since(&self, today: i64, days: i64) -> u64 {
        self.days
            .iter()
            .filter(|(day, _)| *day > today - days)
            .map(|(_, lines)| lines)
            .sum()
    }

    pub(crate) fn lines_in(&self, period: Period, today: i64) -> u64 {
        match period {
            Period::Today => self.lines_since(today, 1),
            Period::Week => self.lines_since(today, 7),
            Period::Month => self.lines_since(today, 30),
            Period::All => self.total.lines,
        }
    }

    /// The streak as of `today`: it lapses once a whole day passes in silence.
    pub(crate) fn current_streak(&self, today: i64) -> u32 {
        if self.last_day >= today - 1 {
            self.streak
        } else {
            0
        }
    }
}

/// One channel's totals.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Channel {
    #[serde(default)]
    pub(crate) since_day: i64,
    #[serde(default)]
    pub(crate) lines: u64,
    /// Lines by local weekday (Monday first) and hour.
    #[serde(default)]
    pub(crate) heatmap: Vec<Vec<u64>>,
    /// `(local day, lines)`, oldest first, at most [`CHANNEL_DAYS`].
    #[serde(default)]
    pub(crate) days: Vec<(i64, u64)>,
    /// The last week a digest was posted for, so a redelivered timer doesn't post twice.
    #[serde(default)]
    pub(crate) digest_week: i64,
}

impl Channel {
    pub(crate) fn record(&mut self, at: LocalTime) {
        if self.since_day == 0 {
            self.since_day = at.day;
        }
        self.lines += 1;
        if self.heatmap.len() != 7 || self.heatmap.iter().any(|row| row.len() != 24) {
            self.heatmap = vec![vec![0; 24]; 7];
        }
        self.heatmap[weekday(at.day)][at.hour as usize] += 1;
        bump_day(&mut self.days, at.day, CHANNEL_DAYS);
    }

    /// Lines by hour, all weekdays together.
    pub(crate) fn hours(&self) -> Vec<u64> {
        (0..24)
            .map(|hour| {
                self.heatmap
                    .iter()
                    .map(|row| row.get(hour).copied().unwrap_or(0))
                    .sum()
            })
            .collect()
    }

    /// Whether `day` has more lines than any earlier day, given at least a fortnight of history.
    pub(crate) fn is_record_day(&self, day: i64) -> bool {
        let lines = self.lines_on(day);
        let earlier = self
            .days
            .iter()
            .filter(|(d, _)| *d < day)
            .map(|(_, lines)| *lines)
            .max()
            .unwrap_or(0);
        lines > 0 && lines > earlier && day - self.since_day >= 14
    }

    /// One week's totals against the week before, its busiest day, and any record day in it.
    pub(crate) fn week_summary(&self, week: i64) -> WeekSummary {
        let in_week = |w: i64| {
            self.days
                .iter()
                .filter(move |(day, _)| week_of(*day) == w)
                .copied()
        };
        let busiest = in_week(week).max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)));
        WeekSummary {
            lines: in_week(week).map(|(_, lines)| lines).sum(),
            previous: in_week(week - 1).map(|(_, lines)| lines).sum(),
            busiest,
            record_day: in_week(week)
                .map(|(day, _)| day)
                .find(|day| self.is_record_day(*day)),
        }
    }

    pub(crate) fn lines_on(&self, day: i64) -> u64 {
        self.days
            .iter()
            .find(|(d, _)| *d == day)
            .map_or(0, |(_, lines)| *lines)
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct WeekSummary {
    pub(crate) lines: u64,
    pub(crate) previous: u64,
    /// `(day, lines)`; the earliest on ties.
    pub(crate) busiest: Option<(i64, u64)>,
    pub(crate) record_day: Option<i64>,
}

/// The first day (a Monday) of a week.
pub(crate) fn week_start(week: i64) -> i64 {
    week * 7 - 3
}

/// A person's counts for `week`, whichever slot holds it.
pub(crate) fn week_counts(person: &Person, week: i64) -> Option<&Week> {
    [&person.this_week, &person.last_week]
        .into_iter()
        .find(|slot| slot.week == week && slot.counts.lines > 0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AwardKind {
    Chatterbox,
    Inquisitor,
    Excitable,
    CapsLock,
    Librarian,
    NightOwl,
    Theatrical,
    Wordsmith,
}

/// A week's winner of one award: who, and the figure (for Wordsmith, tenths of a word a line).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Award {
    pub(crate) kind: AwardKind,
    pub(crate) name: String,
    pub(crate) value: u64,
}

/// Lines needed in a week to be considered for Wordsmith, so one long line doesn't win it.
const WORDSMITH_MIN_LINES: u64 = 10;

/// The week's superlatives, in a fixed order; an award nobody earned is left out. Ties go to the
/// name first alphabetically, so the result doesn't depend on storage order.
pub(crate) fn awards(people: &[(String, Person)], week: i64) -> Vec<Award> {
    let weeks = people
        .iter()
        .filter_map(|(_, person)| week_counts(person, week).map(|w| (person.name.as_str(), w)))
        .collect::<Vec<_>>();
    let best = |kind: AwardKind, score: &dyn Fn(&Week) -> u64| -> Option<Award> {
        weeks
            .iter()
            .map(|(name, week)| (*name, score(week)))
            .filter(|(_, value)| *value > 0)
            .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(a.0)))
            .map(|(name, value)| Award {
                kind,
                name: name.to_string(),
                value,
            })
    };
    [
        best(AwardKind::Chatterbox, &|w| w.counts.lines),
        best(AwardKind::Inquisitor, &|w| w.counts.questions),
        best(AwardKind::Excitable, &|w| w.counts.exclamations),
        best(AwardKind::CapsLock, &|w| w.counts.shouts),
        best(AwardKind::Librarian, &|w| w.counts.links),
        best(AwardKind::NightOwl, &|w| w.night),
        best(AwardKind::Theatrical, &|w| w.counts.actions),
        best(AwardKind::Wordsmith, &|w| {
            if w.counts.lines < WORDSMITH_MIN_LINES {
                0
            } else {
                w.counts.words * 10 / w.counts.lines
            }
        }),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// People whose first counted day fell in `week`.
pub(crate) fn new_faces(people: &[(String, Person)], week: i64) -> Vec<&str> {
    let mut names = people
        .iter()
        .filter(|(_, person)| person.first_day > 0 && week_of(person.first_day) == week)
        .map(|(_, person)| person.name.as_str())
        .collect::<Vec<_>>();
    names.sort_unstable();
    names
}

/// The next Monday 09:00 in a timezone `offset_seconds` from UTC, as a Unix time.
pub(crate) fn next_digest_at(now: i64, offset_seconds: i64) -> i64 {
    let local = LocalTime::at(now, offset_seconds);
    let mut days_ahead = (7 - weekday(local.day) as i64) % 7;
    if days_ahead == 0 && local.hour >= 9 {
        days_ahead = 7;
    }
    (local.day + days_ahead) * DAY + 9 * 3_600 - offset_seconds
}

pub(crate) fn weekday_name(day: i64) -> &'static str {
    [
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
        "Sunday",
    ][weekday(day)]
}

fn bump_day(days: &mut Vec<(i64, u64)>, day: i64, keep: usize) {
    match days.last_mut() {
        Some((last, lines)) if *last == day => *lines += 1,
        _ => days.push((day, 1)),
    }
    days.retain(|(d, _)| *d > day - keep as i64);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Period {
    Today,
    Week,
    Month,
    All,
}

impl Period {
    pub(crate) fn parse(word: &str) -> Option<Self> {
        match word.to_ascii_lowercase().as_str() {
            "" | "today" | "day" => Some(Self::Today),
            "week" | "7d" => Some(Self::Week),
            "month" | "30d" => Some(Self::Month),
            "all" | "ever" | "alltime" | "all-time" => Some(Self::All),
            _ => None,
        }
    }
}

/// A moment in the channel's timezone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LocalTime {
    /// Days since 1970-01-01, local.
    pub(crate) day: i64,
    pub(crate) hour: u32,
}

impl LocalTime {
    pub(crate) fn at(unix: i64, offset_seconds: i64) -> Self {
        let local = unix + offset_seconds;
        Self {
            day: local.div_euclid(DAY),
            hour: (local.rem_euclid(DAY) / 3_600) as u32,
        }
    }
}

/// Monday-based week number (1970-01-05 was a Monday).
pub(crate) fn week_of(day: i64) -> i64 {
    (day + 3).div_euclid(7)
}

/// 0 = Monday … 6 = Sunday.
pub(crate) fn weekday(day: i64) -> usize {
    (day + 3).rem_euclid(7) as usize
}

/// Days since 1970-01-01 for a civil date (proleptic Gregorian).
pub(crate) fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date of a day number.
pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

/// "29 Sep".
pub(crate) fn short_date(day: i64) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (_, month, date) = civil_from_days(day);
    format!("{date} {}", MONTHS[(month - 1) as usize])
}

/// "1,402".
pub(crate) fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// "▁▂▃▅▇█" scaled to the largest value; all-zero is flat.
pub(crate) fn sparkline(values: &[u64]) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let max = values.iter().copied().max().unwrap_or(0);
    values
        .iter()
        .map(|value| {
            let level = (value * 7 + max / 2).checked_div(max).unwrap_or(0);
            BARS[level.min(7) as usize]
        })
        .collect()
}

/// The index of the largest value (the earliest on ties), if any are non-zero.
pub(crate) fn peak(values: &[u64]) -> Option<usize> {
    let max = *values.iter().max()?;
    (max > 0).then(|| values.iter().position(|value| *value == max).unwrap_or(0))
}

/// "America/New_York" → "New York".
pub(crate) fn zone_label(zone: &str) -> String {
    zone.rsplit('/').next().unwrap_or(zone).replace('_', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(day: i64, hour: u32) -> LocalTime {
        LocalTime { day, hour }
    }

    #[test]
    fn counts_what_kind_of_line_it_was() {
        let mut counts = Counts::default();
        counts.add("is it tea time?", false);
        counts.add("IT IS TEA TIME!", false);
        counts.add("see https://example.com", false);
        counts.add("waves", true);
        counts.add("OK", false);
        assert_eq!(
            counts,
            Counts {
                lines: 5,
                words: 4 + 4 + 2 + 1 + 1,
                questions: 1,
                exclamations: 1,
                shouts: 1,
                links: 1,
                actions: 1,
            }
        );
    }

    #[test]
    fn streaks_days_and_weeks_roll_forward() {
        let mut person = Person::default();
        // 2026-09-28 is a Monday.
        let monday = days_from_civil(2026, 9, 28);
        assert_eq!(weekday(monday), 0);
        for offset in 0..7 {
            let outcome = person.record("ann", "hello", false, at(monday + offset, 3));
            assert_eq!(outcome.week_streak, offset == 6, "day {offset}");
            assert!(outcome.night);
        }
        person.record("ann", "again", false, at(monday + 6, 12));
        assert_eq!((person.streak, person.best_streak), (7, 7));
        assert_eq!(person.this_week.counts.lines, 8);
        assert_eq!(person.this_week.night, 7);
        // Next Monday starts a new week; this one becomes last week.
        person.record("ann", "new week", false, at(monday + 7, 10));
        assert_eq!(person.last_week.counts.lines, 8);
        assert_eq!(person.this_week.counts.lines, 1);
        // A skipped day breaks the streak, and a skipped week empties last week.
        person.record("ann", "back", false, at(monday + 21, 10));
        assert_eq!((person.streak, person.best_streak), (1, 8));
        assert_eq!(person.last_week, Week::default());
        assert_eq!(person.current_streak(monday + 22), 1);
        assert_eq!(person.current_streak(monday + 23), 0);
        assert!(person.days.len() <= PERSON_DAYS);
        assert_eq!(person.days.last(), Some(&(monday + 21, 1)));
    }

    #[test]
    fn periods_add_up_the_right_days() {
        let mut person = Person::default();
        for day in 1..=40 {
            for _ in 0..day {
                person.record("ann", "x", false, at(day, 12));
            }
        }
        assert_eq!(person.lines_in(Period::Today, 40), 40);
        assert_eq!(
            person.lines_in(Period::Week, 40),
            (34..=40).sum::<i64>() as u64
        );
        assert_eq!(
            person.lines_in(Period::Month, 40),
            (11..=40).sum::<i64>() as u64
        );
        assert_eq!(
            person.lines_in(Period::All, 40),
            (1..=40).sum::<i64>() as u64
        );
        assert_eq!(person.lines_in(Period::Today, 41), 0);
    }

    #[test]
    fn channels_keep_a_heatmap_and_daily_totals() {
        let mut channel = Channel::default();
        let monday = days_from_civil(2026, 9, 28);
        channel.record(at(monday, 21));
        channel.record(at(monday + 1, 21));
        channel.record(at(monday + 1, 9));
        assert_eq!(channel.lines, 3);
        assert_eq!(channel.since_day, monday);
        assert_eq!(channel.heatmap[1][21], 1);
        assert_eq!(peak(&channel.hours()), Some(21));
        assert_eq!(channel.lines_on(monday + 1), 2);
    }

    #[test]
    fn awards_go_to_the_week_s_leaders() {
        let monday = days_from_civil(2026, 9, 28);
        let week = week_of(monday);
        let mut ann = Person::default();
        let mut bob = Person::default();
        for _ in 0..12 {
            ann.record(
                "ann",
                "a rather long and thoughtful line here",
                false,
                at(monday, 2),
            );
        }
        for _ in 0..3 {
            bob.record("bob", "WHY THOUGH?", false, at(monday + 1, 14));
        }
        bob.record("bob", "https://example.com!", false, at(monday + 1, 14));
        bob.record("bob", "waves", true, at(monday + 1, 14));
        // Last week's figures belong to last week.
        let mut old = Person::default();
        old.record("old", "hello?", false, at(monday - 3, 12));
        let people = vec![
            ("a".to_string(), ann),
            ("b".to_string(), bob),
            ("o".to_string(), old),
        ];
        let won = awards(&people, week)
            .into_iter()
            .map(|award| (award.kind, award.name, award.value))
            .collect::<Vec<_>>();
        assert_eq!(
            won,
            [
                (AwardKind::Chatterbox, "ann".into(), 12),
                (AwardKind::Inquisitor, "bob".into(), 3),
                (AwardKind::Excitable, "bob".into(), 1),
                (AwardKind::CapsLock, "bob".into(), 3),
                (AwardKind::Librarian, "bob".into(), 1),
                (AwardKind::NightOwl, "ann".into(), 12),
                (AwardKind::Theatrical, "bob".into(), 1),
                (AwardKind::Wordsmith, "ann".into(), 70),
            ]
        );
        assert_eq!(awards(&people, week - 1)[0].name, "old");
        assert_eq!(new_faces(&people, week), ["ann", "bob"]);
    }

    #[test]
    fn weeks_summarise_and_records_need_history() {
        let monday = days_from_civil(2026, 9, 28);
        let mut channel = Channel::default();
        for day in (monday - 21)..(monday + 7) {
            let lines = if day == monday + 2 { 50 } else { 10 };
            for _ in 0..lines {
                channel.record(at(day, 12));
            }
        }
        let summary = channel.week_summary(week_of(monday));
        assert_eq!(summary.lines, 6 * 10 + 50);
        assert_eq!(summary.previous, 70);
        assert_eq!(summary.busiest, Some((monday + 2, 50)));
        assert_eq!(summary.record_day, Some(monday + 2));
        assert_eq!(weekday_name(monday + 2), "Wednesday");
        let young = Channel {
            since_day: monday,
            days: vec![(monday, 5), (monday + 1, 9)],
            ..Channel::default()
        };
        assert!(!young.is_record_day(monday + 1), "too new to have records");
    }

    #[test]
    fn digests_land_on_monday_mornings() {
        let offset = -4 * 3_600; // New York in summer
        let monday = days_from_civil(2026, 9, 28);
        let at_local = |day: i64, hour: i64| day * DAY + hour * 3_600 - offset;
        // Sunday evening → tomorrow 09:00; Monday 08:00 → that morning; Monday 10:00 → next week.
        assert_eq!(
            next_digest_at(at_local(monday - 1, 20), offset),
            at_local(monday, 9)
        );
        assert_eq!(
            next_digest_at(at_local(monday, 8), offset),
            at_local(monday, 9)
        );
        assert_eq!(
            next_digest_at(at_local(monday, 10), offset),
            at_local(monday + 7, 9)
        );
        assert_eq!(week_start(week_of(monday + 4)), monday);
    }

    #[test]
    fn local_time_uses_the_offset() {
        // 2026-09-29 03:30 UTC is 23:30 the day before in New York (UTC-4).
        let unix = days_from_civil(2026, 9, 29) * DAY + 3 * 3_600 + 1_800;
        let local = LocalTime::at(unix, -4 * 3_600);
        assert_eq!(local.day, days_from_civil(2026, 9, 28));
        assert_eq!(local.hour, 23);
    }

    #[test]
    fn dates_numbers_and_sparklines_read_well() {
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
        assert_eq!(short_date(days_from_civil(2026, 9, 29)), "29 Sep");
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(1_402), "1,402");
        assert_eq!(grouped(1_234_567), "1,234,567");
        assert_eq!(sparkline(&[0, 1, 7, 14]), "▁▂▅█");
        assert_eq!(sparkline(&[0, 0]), "▁▁");
        assert_eq!(peak(&[0, 0]), None);
        assert_eq!(zone_label("America/New_York"), "New York");
        assert_eq!(Period::parse("WEEK"), Some(Period::Week));
        assert_eq!(Period::parse("nonsense"), None);
    }
}
