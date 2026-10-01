//! Seasonal boundaries, quarter statistics, and champion selection.
//!
//! This module owns the lazy reset policy and its pure date/champion helpers. The reset remains
//! command-triggered by design; durable scheduler delivery is intentionally out of scope here.

use crate::{
    commands::name_of,
    location_for_level,
    model::{Champions, Player, SeasonStats, State},
    xp_for_level, VOID_EXPANSION_START,
};

/// Convert unix seconds to a UTC `(year, month, day)` (Howard Hinnant's civil-from-days).
pub(super) fn civil_from_unix(secs: i64) -> (i64, u32, u32) {
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

/// Inverse: midnight UTC of `(year, month, day)` as unix seconds.
pub(super) fn unix_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let (m, d) = (m as i64, d as i64);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468) * 86_400
}

/// Midnight UTC of the next quarter boundary (Jan/Apr/Jul/Oct 1) strictly after `secs`.
pub(super) fn next_quarter_start(secs: i64) -> i64 {
    let (y, _, _) = civil_from_unix(secs);
    for &qm in &[1u32, 4, 7, 10] {
        let ts = unix_from_civil(y, qm, 1);
        if ts > secs {
            return ts;
        }
    }
    unix_from_civil(y + 1, 1, 1)
}

/// The season label a reset at `secs` concludes (Apr 1 concludes Q1, Jan 1 concludes the prior Q4).
pub(super) fn compute_reset_season(secs: i64) -> String {
    let (y, m, _) = civil_from_unix(secs);
    match m {
        1 => format!("Q4 {}", y - 1),
        4 => format!("Q1 {y}"),
        7 => format!("Q2 {y}"),
        10 => format!("Q3 {y}"),
        _ => format!("Q? {y}"),
    }
}

// ── champions ────────────────────────────────────────────────────────────────

pub(super) fn legacy_season_stats(player: &Player) -> SeasonStats {
    // Before dedicated seasonal counters, every quarter wiped the lifetime fields. A restored old
    // save therefore contains one season's totals. Reconstruct earned XP from progression; XP
    // spent on consumables cannot be recovered, but this preserves the old Traveler ordering as
    // closely as the legacy schema permits.
    let level_xp = (0..player.level).map(xp_for_level).sum::<i64>();
    SeasonStats {
        xp_earned: level_xp.saturating_add(player.xp),
        fish_caught: player.total_fish,
        unique_species: player.catches.keys().cloned().collect(),
        rare_catches: player.rare_total(),
        heaviest_catch: player.biggest_fish,
        furthest_cast: player.furthest_cast,
        start_level: None,
    }
}

pub(super) fn season_stats(player: &Player) -> SeasonStats {
    player
        .season_stats
        .clone()
        .unwrap_or_else(|| legacy_season_stats(player))
}

pub(super) fn season_stats_mut(player: &mut Player) -> &mut SeasonStats {
    if player.season_stats.is_none() {
        player.season_stats = Some(legacy_season_stats(player));
    }
    let level = player.level;
    let stats = player.season_stats.as_mut().unwrap();
    stats.start_level.get_or_insert(level);
    stats
}

/// Levels climbed this season (counted from when the season's start level was first known).
pub(super) fn levels_climbed(player: &Player) -> i64 {
    let start = season_stats(player).start_level.unwrap_or(player.level);
    (player.level - start).max(0)
}

/// Compute the three champions (player keys) from current-quarter counters: the Traveler climbed
/// the most levels, the Caster made the furthest cast, and the Collector landed the most rare and
/// legendary fish. Ties are broken by seasonal XP, then seasonal fish caught, then lifetime fish.
pub(super) fn compute_champions(
    players: &[(&String, &Player)],
) -> (Option<String>, Option<String>, Option<String>) {
    let best = |score: &dyn Fn(&Player, &SeasonStats) -> f64| -> Option<String> {
        players
            .iter()
            .filter(|(_, p)| score(p, &season_stats(p)) > 0.0)
            .max_by(|(_, a), (_, b)| {
                let sa = season_stats(a);
                let sb = season_stats(b);
                score(a, &sa)
                    .partial_cmp(&score(b, &sb))
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(sa.xp_earned.cmp(&sb.xp_earned))
                    .then(sa.fish_caught.cmp(&sb.fish_caught))
                    .then(a.total_fish.cmp(&b.total_fish))
            })
            .map(|(k, _)| (*k).clone())
    };
    (
        best(&|p, _| levels_climbed(p) as f64),
        best(&|_, s| s.furthest_cast),
        best(&|_, s| s.rare_catches as f64),
    )
}

/// Champion title suffix shown within fishing messages (e.g. "the Traveler the Collector").
pub(super) fn champion_titles(state: &State, server: &str, key: &str) -> String {
    let Some(c) = state.champions.get(server) else {
        return String::new();
    };
    let is = |w: &Option<String>| w.as_deref() == Some(key);
    let mut parts = Vec::new();
    if is(&c.traveler) {
        parts.push("the Traveler");
    }
    if is(&c.caster) {
        parts.push("the Caster");
    }
    if is(&c.collector) {
        parts.push("the Collector");
    }
    parts.join(" ")
}

/// Lazy quarterly reset for `ctx.server`. First sight schedules the boundary without resetting; once
/// `now` passes a boundary, crowns champions, clears only seasonal counters, advances the
/// boundary, and returns `(crowned, state_changed)`, one [`Champions`] per elapsed boundary
/// (usually none). `state_changed` is deliberately separate from the crownings: first sight of a
/// server only persists its initial boundary and has nothing to announce.
pub(super) fn maybe_seasonal_reset(
    server: &str,
    state: &mut State,
    now: i64,
) -> (Vec<Champions>, bool) {
    let mut crowned = Vec::new();
    let mut state_changed = false;
    if !matches!(state.next_reset.get(server), Some(&b) if b != 0) {
        let prefix = format!("{server}/");
        let has_existing_season = state.players.keys().any(|key| key.starts_with(&prefix));
        // The original scheduler failed to persist its initial boundary. Existing seasons that
        // encounter the fixed module after the Q3 expansion must still receive the missed July 1
        // reset; empty/new servers can safely begin at the next boundary.
        let boundary = if has_existing_season && now >= VOID_EXPANSION_START {
            VOID_EXPANSION_START
        } else {
            next_quarter_start(now)
        };
        state.next_reset.insert(server.to_string(), boundary);
        state_changed = true;
        if boundary > now {
            return (crowned, state_changed);
        }
    }
    while let Some(&boundary) = state.next_reset.get(server) {
        if boundary == 0 || now < boundary {
            break;
        }
        let season = compute_reset_season(boundary);
        crowned.push(run_season_reset(state, server, &season));
        state
            .next_reset
            .insert(server.to_string(), next_quarter_start(boundary));
        state_changed = true;
    }
    (crowned, state_changed)
}

/// Crowns this season's champions, resets the seasonal counters, and returns the crowning.
pub(super) fn run_season_reset(state: &mut State, server: &str, season: &str) -> Champions {
    let prefix = format!("{server}/");
    let players: Vec<(&String, &Player)> = state
        .players
        .iter()
        .filter(|(k, _)| k.starts_with(&prefix))
        .collect();
    let (traveler, caster, collector) = compute_champions(&players);
    drop(players);

    let mut champ = Champions {
        season: season.to_string(),
        ..Default::default()
    };
    champ.traveler_name = traveler
        .as_ref()
        .and_then(|k| state.players.get(k))
        .map(name_of)
        .unwrap_or_default();
    champ.caster_name = caster
        .as_ref()
        .and_then(|k| state.players.get(k))
        .map(name_of)
        .unwrap_or_default();
    champ.collector_name = collector
        .as_ref()
        .and_then(|k| state.players.get(k))
        .map(name_of)
        .unwrap_or_default();
    if let Some(p) = traveler.as_ref().and_then(|k| state.players.get(k)) {
        champ.traveler_xp = season_stats(p).xp_earned;
        champ.traveler_levels = levels_climbed(p);
        champ.traveler_level = p.level;
        champ.traveler_location = location_for_level(p.level).name.clone();
    }
    if let Some(p) = caster.as_ref().and_then(|k| state.players.get(k)) {
        champ.caster_distance = season_stats(p).furthest_cast;
    }
    if let Some(p) = collector.as_ref().and_then(|k| state.players.get(k)) {
        champ.collector_count = season_stats(p).rare_catches;
    }

    champ.traveler = traveler;
    champ.caster = caster;
    champ.collector = collector;
    state.champions.insert(server.to_string(), champ.clone());

    // Only competition counters reset. Career progress and in-flight gameplay are permanent.
    for (key, player) in &mut state.players {
        if key.starts_with(&prefix) {
            player.season_stats = Some(SeasonStats {
                start_level: Some(player.level),
                ..SeasonStats::default()
            });
        }
    }
    champ
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_date_round_trip() {
        assert_eq!(unix_from_civil(2026, 6, 26), 1_782_432_000);
        assert_eq!(civil_from_unix(1_782_432_000), (2026, 6, 26));
        assert_eq!(civil_from_unix(0), (1970, 1, 1));
        let ts = unix_from_civil(2024, 2, 29);
        assert_eq!(civil_from_unix(ts), (2024, 2, 29));
    }

    #[test]
    fn quarter_boundaries_and_seasons() {
        let jun = unix_from_civil(2026, 6, 26);
        let next = next_quarter_start(jun);
        assert_eq!(civil_from_unix(next), (2026, 7, 1));
        assert_eq!(compute_reset_season(next), "Q2 2026");
        let jul = unix_from_civil(2026, 7, 1);
        assert_eq!(civil_from_unix(next_quarter_start(jul)), (2026, 10, 1));
        let jan = unix_from_civil(2027, 1, 1);
        assert_eq!(compute_reset_season(jan), "Q4 2026");
    }

    #[test]
    fn champions_pick_leaders_with_tiebreak() {
        // A high-level angler with lots of XP but little climbing this season…
        let a = Player {
            level: 40,
            total_fish: 50,
            season_stats: Some(SeasonStats {
                start_level: Some(39),
                xp_earned: 90_000,
                fish_caught: 5,
                furthest_cast: 10.0,
                ..Default::default()
            }),
            ..Default::default()
        };
        // …and a newer one who climbed further, with a longer cast and a rare catch.
        let mut b = Player {
            level: 6,
            total_fish: 9,
            season_stats: Some(SeasonStats {
                start_level: Some(2),
                xp_earned: 400,
                fish_caught: 9,
                rare_catches: 1,
                furthest_cast: 50.0,
                ..Default::default()
            }),
            ..Default::default()
        };
        b.rare_catches.push(crate::model::RareCatch {
            name: "x".into(),
            weight: 1.0,
            rarity: "rare".into(),
            location: "Puddle".into(),
            caught_at: 0,
        });
        let (ka, kb) = ("s/a".to_string(), "s/b".to_string());
        let players = vec![(&ka, &a), (&kb, &b)];
        let (traveler, caster, collector) = compute_champions(&players);
        assert_eq!(
            traveler.as_deref(),
            Some("s/b"),
            "levels climbed, not XP, makes the Traveler"
        );
        assert_eq!(caster.as_deref(), Some("s/b"));
        assert_eq!(collector.as_deref(), Some("s/b"));
        // Equal climbs fall back to seasonal XP.
        let mut c = b.clone();
        c.season_stats.as_mut().unwrap().xp_earned = 500;
        let kc = "s/c".to_string();
        let players = vec![(&kb, &b), (&kc, &c)];
        assert_eq!(compute_champions(&players).0.as_deref(), Some("s/c"));
        // Nobody climbing means no Traveler.
        let still = Player {
            level: 5,
            season_stats: Some(SeasonStats {
                start_level: Some(5),
                xp_earned: 50,
                ..Default::default()
            }),
            ..Default::default()
        };
        let players = vec![(&ka, &still)];
        assert_eq!(compute_champions(&players).0, None);
    }

    #[test]
    fn seasonal_reset_preserves_career_and_clears_only_season_stats() {
        let mut st = State::default();
        st.players.insert(
            "s/a".into(),
            Player {
                level: 3,
                furthest_cast: 20.0,
                total_fish: 4,
                season_stats: Some(SeasonStats {
                    start_level: Some(1),
                    xp_earned: 900,
                    fish_caught: 4,
                    furthest_cast: 20.0,
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        let jun = unix_from_civil(2026, 6, 26);
        let (crowned, state_changed) = maybe_seasonal_reset("s", &mut st, jun);
        assert!(crowned.is_empty());
        assert!(state_changed);
        assert!(st.players.contains_key("s/a"));
        assert_eq!(st.next_reset.get("s"), Some(&unix_from_civil(2026, 7, 1)));
        let (crowned, state_changed) = maybe_seasonal_reset("s", &mut st, jun + 1);
        assert!(crowned.is_empty());
        assert!(!state_changed);
        let aug = unix_from_civil(2026, 8, 1);
        let (crowned, state_changed) = maybe_seasonal_reset("s", &mut st, aug);
        assert_eq!(crowned.len(), 1);
        assert!(state_changed);
        let player = st.players.get("s/a").unwrap();
        assert_eq!(player.level, 3);
        assert_eq!(player.total_fish, 4);
        assert_eq!(player.season_stats.as_ref().unwrap().fish_caught, 0);
        let champ = st.champions.get("s").unwrap();
        assert_eq!(champ.traveler.as_deref(), Some("s/a"));
        assert_eq!(champ.season, "Q2 2026");
        assert_eq!(champ.traveler_xp, 900);
        assert_eq!(champ.traveler_levels, 2);
        assert_eq!(
            player.season_stats.as_ref().unwrap().start_level,
            Some(3),
            "the new season counts from today's level"
        );
    }

    #[test]
    fn missing_schedule_catches_up_the_q3_reset_for_an_existing_season() {
        let mut st = State::default();
        st.players.insert(
            "s/a".into(),
            Player {
                level: 3,
                ..Default::default()
            },
        );
        let after_boundary = unix_from_civil(2026, 7, 1) + 1;
        let (lines, state_changed) = maybe_seasonal_reset("s", &mut st, after_boundary);
        assert!(state_changed);
        assert!(!lines.is_empty());
        assert!(st.players.contains_key("s/a"));
        assert_eq!(
            st.players["s/a"].season_stats.as_ref().unwrap().xp_earned,
            0
        );
        assert_eq!(st.champions.get("s").unwrap().season, "Q2 2026");
        assert_eq!(st.next_reset.get("s"), Some(&unix_from_civil(2026, 10, 1)));
    }
}
