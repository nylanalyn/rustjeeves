//! The static fishing database and the roll tables over it.
//!
//! Types here mirror the shape of the bundled `fish_database.json` (locations, fish, void tiers,
//! artifacts, event definitions) and are deserialized once into [`crate::data`]. The functions are
//! the pure roll logic — pick a rarity, pick a fish, roll a weight — and take an explicit [`Rng`]
//! so they stay deterministic and testable.

use serde::{Deserialize, Serialize};

use crate::{data, Rng, OPTIMAL_WAIT_HOURS};

/// `serde(default)` for event multipliers: an absent multiplier means "no scaling".
fn one() -> f64 {
    1.0
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct Location {
    pub(super) name: String,
    pub(super) level: i64,
    pub(super) max_distance: f64,
    #[serde(rename = "type")]
    pub(super) kind: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct Fish {
    pub(super) name: String,
    pub(super) min_weight: f64,
    pub(super) max_weight: f64,
    pub(super) rarity: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct VoidTier {
    pub(super) name: String,
    pub(super) color: String,
    pub(super) level: i64,
    pub(super) max_distance: f64,
    pub(super) weight_multiplier: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct VoidExpansion {
    pub(super) tiers: Vec<VoidTier>,
    pub(super) fish: Vec<Fish>,
}

/// A fishing artifact: bundled in the DB, and also stored on a player once found.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Artifact {
    pub(super) name: String,
    pub(super) cast_text: String,
    pub(super) float_text: String,
    pub(super) bonus_type: String,
    pub(super) bonus_value: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct EventDef {
    pub(super) name: String,
    pub(super) description: String,
    #[serde(default)]
    pub(super) effect: Option<String>,
    #[serde(default = "one")]
    pub(super) multiplier: f64,
    pub(super) duration_minutes: i64,
    #[serde(default)]
    pub(super) locations: Option<Vec<String>>,
}

pub(super) fn select_rarity(
    rng: &mut Rng,
    wait_hours: f64,
    event_rare_mult: f64,
    rarity_boost: f64,
) -> String {
    let mut weights: Vec<(String, i64)> = data().rarity_weights.clone();
    let set = |w: &mut Vec<(String, i64)>, name: &str, val: i64| {
        if let Some(e) = w.iter_mut().find(|(k, _)| k == name) {
            e.1 = val;
        }
    };
    let get = |w: &[(String, i64)], name: &str| {
        w.iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| *v)
            .unwrap_or(0)
    };
    if wait_hours < 6.0 {
        set(&mut weights, "uncommon", 5);
        set(&mut weights, "rare", 0);
        set(&mut weights, "legendary", 0);
    } else if wait_hours < 12.0 {
        set(&mut weights, "rare", 2);
        set(&mut weights, "legendary", 0);
    } else if wait_hours < 18.0 {
        set(&mut weights, "legendary", 0);
    }
    if event_rare_mult > 1.0 {
        let r = (get(&weights, "rare") as f64 * event_rare_mult) as i64;
        let l = (get(&weights, "legendary") as f64 * event_rare_mult) as i64;
        set(&mut weights, "rare", r);
        set(&mut weights, "legendary", l);
    }
    if rarity_boost > 0.0 {
        let common = get(&weights, "common") as f64;
        let reduction = common * rarity_boost;
        let rare = get(&weights, "rare") + (reduction * 0.6) as i64;
        let legendary = get(&weights, "legendary") + (reduction * 0.4) as i64;
        set(&mut weights, "common", (common - reduction).max(1.0) as i64);
        set(&mut weights, "rare", rare);
        set(&mut weights, "legendary", legendary);
    }
    let total: i64 = weights.iter().map(|(_, w)| *w).sum();
    if total <= 0 {
        return "common".into();
    }
    let mut roll = (rng.below(total as usize) + 1) as i64;
    for (rarity, w) in &weights {
        roll -= w;
        if roll <= 0 {
            return rarity.clone();
        }
    }
    "common".into()
}

pub(super) fn select_fish<'a>(
    rng: &mut Rng,
    location: &str,
    rarity: &str,
    eligible: &[String],
    allow_fallback: bool,
) -> Option<&'a Fish> {
    let d = data();
    let pool: Vec<&Fish> = if eligible.is_empty() {
        d.fish_by_location
            .get(location)
            .map(|v| v.iter().collect())
            .unwrap_or_default()
    } else {
        eligible
            .iter()
            .filter_map(|l| d.fish_by_location.get(l))
            .flat_map(|v| v.iter())
            .collect()
    };
    let matching: Vec<&Fish> = pool
        .iter()
        .copied()
        .filter(|f| f.rarity == rarity)
        .collect();
    if matching.is_empty() {
        if !allow_fallback {
            return None;
        }
        let commons: Vec<&Fish> = pool
            .iter()
            .copied()
            .filter(|f| f.rarity == "common")
            .collect();
        rng.choice(&commons).copied()
    } else {
        rng.choice(&matching).copied()
    }
}

pub(super) fn calc_weight(rng: &mut Rng, fish: &Fish, wait_hours: f64) -> f64 {
    let (min_w, max_w) = (fish.min_weight, fish.max_weight);
    let time_factor = (wait_hours / OPTIMAL_WAIT_HOURS).min(1.0);
    let base = min_w + (max_w - min_w) * time_factor;
    let variance = (max_w - min_w) * 0.2;
    let w = base + rng.range(-variance, variance);
    round2(w.clamp(min_w, max_w))
}

pub(super) fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}
pub(super) fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rarity_respects_wait_gates() {
        let mut rng = Rng(123456789);
        // Under 6h: never rare/legendary.
        for _ in 0..500 {
            let r = select_rarity(&mut rng, 3.0, 1.0, 0.0);
            assert!(r == "common" || r == "uncommon", "got {r} at 3h");
        }
        // 20h: full table — at least one rare/legendary should appear.
        let mut seen_rare = false;
        for _ in 0..2000 {
            let r = select_rarity(&mut rng, 20.0, 1.0, 0.0);
            if r == "rare" || r == "legendary" {
                seen_rare = true;
                break;
            }
        }
        assert!(
            seen_rare,
            "expected a rare/legendary at 20h over many rolls"
        );
    }

    #[test]
    fn weight_stays_in_range_and_scales() {
        let mut rng = Rng(42);
        let fish = Fish {
            name: "Test".into(),
            min_weight: 2.0,
            max_weight: 10.0,
            rarity: "common".into(),
        };
        for _ in 0..200 {
            let w = calc_weight(&mut rng, &fish, 24.0);
            assert!((2.0..=10.0).contains(&w), "w={w}");
        }
        // Long waits trend heavier than very short ones (averaged).
        let avg = |hours: f64| {
            let mut r = Rng(7);
            let mut s = 0.0;
            for _ in 0..500 {
                s += calc_weight(&mut r, &fish, hours);
            }
            s / 500.0
        };
        assert!(avg(24.0) > avg(1.0));
    }
}
