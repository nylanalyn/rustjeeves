//! Building definitions: costs, effects, and daily upkeep (PLAN-PIRATE.md §8). Pure tables and
//! lookups — no host calls.

use crate::model::Buildings;

#[derive(Clone, Copy)]
pub(crate) struct BuildingDef {
    pub key: &'static str,
    pub name: &'static str,
    /// Highest level reachable.
    pub max_level: u8,
    /// Gold cost per level, indexed by the level being bought (level 1 = costs[0]).
    pub costs: &'static [i64],
    /// Daily upkeep per level, indexed by current level (L1 = upkeep[0]).
    pub upkeep: &'static [i64],
    pub effect: &'static str,
}

pub(crate) const BUILDINGS: &[BuildingDef] = &[
    BuildingDef {
        key: "vault",
        name: "Vault",
        max_level: 2,
        costs: &[200, 400],
        upkeep: &[10, 20],
        effect: "protects 50%/75% of gold from raids",
    },
    BuildingDef {
        key: "cove",
        name: "Cove",
        max_level: 2,
        costs: &[300, 600],
        upkeep: &[15, 30],
        effect: "hides 2/4 crew from scouts and !here",
    },
    BuildingDef {
        key: "walls",
        name: "Walls",
        max_level: 2,
        costs: &[250, 500],
        upkeep: &[10, 20],
        effect: "+15/+30 defense power",
    },
    BuildingDef {
        key: "shipyard",
        name: "Shipyard",
        max_level: 2,
        costs: &[200, 400],
        upkeep: &[10, 20],
        effect: "voyages return 20%/35% faster",
    },
    BuildingDef {
        key: "tavern",
        name: "Tavern",
        max_level: 1,
        costs: &[200],
        upkeep: &[10],
        effect: "no desertion on missed payday; +5 defense",
    },
];

pub(crate) fn building_def(key: &str) -> Option<&'static BuildingDef> {
    BUILDINGS.iter().find(|def| def.key == key)
}

pub(crate) fn level(buildings: &Buildings, key: &str) -> u8 {
    match key {
        "vault" => buildings.vault,
        "cove" => buildings.cove,
        "walls" => buildings.walls,
        "shipyard" => buildings.shipyard,
        "tavern" => buildings.tavern,
        _ => 0,
    }
}

pub(crate) fn set_level(buildings: &mut Buildings, key: &str, value: u8) {
    match key {
        "vault" => buildings.vault = value,
        "cove" => buildings.cove = value,
        "walls" => buildings.walls = value,
        "shipyard" => buildings.shipyard = value,
        "tavern" => buildings.tavern = value,
        _ => {}
    }
}

/// Gold cost of the next level, or `None` when the building is maxed.
pub(crate) fn next_cost(buildings: &Buildings, def: &BuildingDef) -> Option<i64> {
    let current = level(buildings, def.key);
    if current >= def.max_level {
        None
    } else {
        Some(def.costs[current as usize])
    }
}

/// Daily upkeep for one building at its current level.
pub(crate) fn upkeep_for(buildings: &Buildings, def: &BuildingDef) -> i64 {
    let current = level(buildings, def.key);
    if current == 0 {
        0
    } else {
        def.upkeep[(current - 1) as usize]
    }
}

/// Total daily building upkeep.
pub(crate) fn total_upkeep(buildings: &Buildings) -> i64 {
    BUILDINGS.iter().map(|def| upkeep_for(buildings, def)).sum()
}

/// Fraction of gold protected from raids (Vault L1 50%, L2 75%).
pub(crate) fn vault_protection(buildings: &Buildings) -> f64 {
    match buildings.vault {
        0 => 0.0,
        1 => 0.50,
        _ => 0.75,
    }
}

/// Crew hidden from scouts and `!here` (Cove L1 2, L2 4).
pub(crate) fn cove_hides(buildings: &Buildings) -> i64 {
    match buildings.cove {
        0 => 0,
        1 => 2,
        _ => 4,
    }
}

/// Flat defense power from Walls.
pub(crate) fn walls_bonus(buildings: &Buildings) -> f64 {
    match buildings.walls {
        0 => 0.0,
        1 => 15.0,
        _ => 30.0,
    }
}

/// Flat defense power from a Tavern.
pub(crate) fn tavern_bonus(buildings: &Buildings) -> f64 {
    if buildings.tavern > 0 {
        5.0
    } else {
        0.0
    }
}

/// Voyage duration multiplier from the Shipyard (L1 0.80, L2 0.65).
pub(crate) fn shipyard_speed(buildings: &Buildings) -> f64 {
    match buildings.shipyard {
        0 => 1.0,
        1 => 0.80,
        _ => 0.65,
    }
}

/// Degrade the most expensive standing building by one level (unpaid upkeep). Returns its key.
pub(crate) fn degrade_one(buildings: &mut Buildings) -> Option<&'static str> {
    let target = BUILDINGS
        .iter()
        .filter(|def| level(buildings, def.key) > 0)
        .max_by_key(|def| upkeep_for(buildings, def))?;
    let current = level(buildings, target.key);
    set_level(buildings, target.key, current - 1);
    Some(target.key)
}

/// One-line summary of standing buildings, e.g. "Vault L1, Cove L2".
pub(crate) fn describe(buildings: &Buildings) -> String {
    let parts: Vec<String> = BUILDINGS
        .iter()
        .filter_map(|def| {
            let lvl = level(buildings, def.key);
            (lvl > 0).then(|| format!("{} L{}", def.name, lvl))
        })
        .collect();
    if parts.is_empty() {
        "no buildings".into()
    } else {
        parts.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn costs_and_upkeep_follow_the_plan_table() {
        let mut b = Buildings::default(); // cove 1
        assert_eq!(next_cost(&b, building_def("vault").unwrap()), Some(200));
        assert_eq!(next_cost(&b, building_def("cove").unwrap()), Some(600));
        assert_eq!(next_cost(&b, building_def("tavern").unwrap()), Some(200));
        assert_eq!(total_upkeep(&b), 15, "starting cove L1 upkeep");
        b.vault = 2;
        assert_eq!(next_cost(&b, building_def("vault").unwrap()), None);
        assert_eq!(total_upkeep(&b), 15 + 20);
    }

    #[test]
    fn effects_match_the_plan() {
        let mut b = Buildings {
            cove: 0,
            ..Default::default()
        };
        assert_eq!(vault_protection(&b), 0.0);
        b.vault = 1;
        assert_eq!(vault_protection(&b), 0.50);
        b.vault = 2;
        assert_eq!(vault_protection(&b), 0.75);
        assert_eq!(cove_hides(&b), 0);
        b.cove = 2;
        assert_eq!(cove_hides(&b), 4);
        b.walls = 2;
        assert_eq!(walls_bonus(&b), 30.0);
        b.tavern = 1;
        assert_eq!(tavern_bonus(&b), 5.0);
        b.shipyard = 1;
        assert_eq!(shipyard_speed(&b), 0.80);
        b.shipyard = 2;
        assert_eq!(shipyard_speed(&b), 0.65);
    }

    #[test]
    fn degrade_one_picks_the_priciest_standing_building() {
        let mut b = Buildings {
            vault: 1,
            cove: 2,
            walls: 1,
            shipyard: 0,
            tavern: 0,
        };
        // Cove L2 upkeep 30 > vault 10 = walls 10.
        assert_eq!(degrade_one(&mut b), Some("cove"));
        assert_eq!(b.cove, 1);
        assert_eq!(degrade_one(&mut b), Some("cove"));
        assert_eq!(b.cove, 0);
        b.vault = 0;
        b.walls = 0;
        assert_eq!(degrade_one(&mut b), None);
    }
}
