//! The #games brass economy, deliberately silly egg pulls, and the wardrobe of cosmetics some
//! eggs contain.
//!
//! `!egg` is the module's noun: `!egg` (buy), `!egg hatch`, `!egg pull` (buy and hatch at once),
//! `!egg recycle`, `!egg odds`, and `!egg shelf`, each also a top-level shortcut. `!wardrobe`
//! lists owned badges and flourishes; `!wear` puts one on. Cosmetics live in the host store, so
//! other modules can show a badge beside a name or a flourish after a win.

use extism_pdk::*;
use jeeves_abi::{
    AchievementManifest, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandShortcut, CommandSpec, Cosmetic, CosmeticGrantRequest, CosmeticGrantResponse,
    CosmeticInventory, CosmeticKind, CosmeticListRequest, CosmeticWearRequest,
    CosmeticWearResponse, DataSubject, EconomyBalanceRequest, EconomyBalanceResponse,
    EconomyTransactionRequest, EconomyTransactionResponse, Event, EventEnvelope, KvGet, KvList,
    KvSet, MessagePayload, ModuleDataDeletePlan, ModuleDataRequest, ModuleDataResponse,
    ModuleKvMutation, Profile, ProfileKey, RandomBytesRequest, RandomBytesResponse, SendMessage,
    SettingGet, SettingKind, SettingScope, SettingSpec, SettingsManifest, StatIncrement, ThemeReq,
    ACHIEVEMENT_MANIFEST_VERSION, COMMAND_MANIFEST_VERSION, DATA_LIFECYCLE_VERSION,
    SETTINGS_MANIFEST_VERSION,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const DEFAULT_GAME_ROOM: &str = "#games";
const DEFAULT_ANNOUNCEMENT_ROOM: &str = "#transience";
const EGG_COST: u64 = 50;
const TRASH_BUNDLE: u64 = 100;
const TRASH_VALUE: u64 = 10;
const SHELF_SIZE: usize = 3;
const GLOBAL_SHELF_SIZE: usize = 10;
/// Per mille: 850 common, 110 rare, 35 legendary, 5 mythic.
const RARITY_ODDS: [(Rarity, u64); 4] = [
    (Rarity::Common, 850),
    (Rarity::Rare, 110),
    (Rarity::Legendary, 35),
    (Rarity::Mythic, 5),
];
/// Per mille of eggs that hold a cosmetic instead of an item.
const COSMETIC_CHANCE: u64 = 80;
/// Cosmetic tiers, per mille of cosmetic eggs.
const COSMETIC_ODDS: [(Rarity, u64); 3] = [
    (Rarity::Common, 700),
    (Rarity::Rare, 250),
    (Rarity::Legendary, 50),
];
/// Brass paid out for a cosmetic you already own.
const DUPLICATE_REFUND: u64 = 20;
const COSMETIC_PREFIX: &str = "cosmetic:";

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Rarity {
    Common,
    Rare,
    Legendary,
    Mythic,
}

impl Rarity {
    fn label(self) -> &'static str {
        match self {
            Self::Common => "common",
            Self::Rare => "rare",
            Self::Legendary => "legendary",
            Self::Mythic => "mythic",
        }
    }
}

struct ItemDef {
    id: &'static str,
    name: &'static str,
    rarity: Rarity,
}

struct CosmeticDef {
    id: &'static str,
    kind: CosmeticKind,
    name: &'static str,
    value: &'static str,
    rarity: Rarity,
}

const fn badge(
    id: &'static str,
    name: &'static str,
    value: &'static str,
    rarity: Rarity,
) -> CosmeticDef {
    CosmeticDef {
        id,
        kind: CosmeticKind::Badge,
        name,
        value,
        rarity,
    }
}

const fn flourish(
    id: &'static str,
    name: &'static str,
    value: &'static str,
    rarity: Rarity,
) -> CosmeticDef {
    CosmeticDef {
        id,
        kind: CosmeticKind::Flourish,
        name,
        value,
        rarity,
    }
}

/// Badges sit beside a name (`!whoami`, leaderboards); flourishes follow a win.
const COSMETICS: &[CosmeticDef] = &[
    badge("teacup", "teacup badge", "☕", Rarity::Common),
    badge("biscuit", "biscuit badge", "🍪", Rarity::Common),
    badge("duck", "rubber duck badge", "🦆", Rarity::Common),
    badge("snail", "snail badge", "🐌", Rarity::Common),
    badge("mushroom", "mushroom badge", "🍄", Rarity::Common),
    badge("owl", "owl badge", "🦉", Rarity::Rare),
    badge("top_hat", "top hat badge", "🎩", Rarity::Rare),
    badge("octopus", "octopus badge", "🐙", Rarity::Rare),
    badge("comet", "comet badge", "☄️", Rarity::Rare),
    badge("crown", "crown badge", "👑", Rarity::Legendary),
    badge("dragon", "dragon badge", "🐉", Rarity::Legendary),
    flourish(
        "polite_applause",
        "polite applause",
        "(polite applause)",
        Rarity::Common,
    ),
    flourish("ta_da", "ta-da", "✨ ta-da!", Rarity::Common),
    flourish("hat_tip", "tip of the hat", "*tips hat*", Rarity::Common),
    flourish("fanfare", "fanfare", "🎺 fanfare!", Rarity::Rare),
    flourish("confetti", "confetti", "🎉🎉", Rarity::Rare),
    flourish(
        "mild_crowd",
        "a mild crowd",
        "(the crowd goes mild)",
        Rarity::Rare,
    ),
    flourish(
        "double_rainbow",
        "double rainbow",
        "🌈🌈 what does it mean",
        Rarity::Legendary,
    ),
    flourish(
        "ovation",
        "standing ovation",
        "👏 bravo, encore!",
        Rarity::Legendary,
    ),
];

const COMMON: &[ItemDef] = &[
    ItemDef {
        id: "melted_spoon",
        name: "a melted spoon",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "french_fry",
        name: "a French fry",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "spiderman_photo",
        name: "a photo of Spider-Man",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "haunted_thimble",
        name: "a slightly haunted thimble",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "single_shoelace",
        name: "a single shoelace",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "blank_domino",
        name: "a single domino (the blank one)",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "empty_teabag",
        name: "an empty teabag",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "left_sock",
        name: "the left half of a sock",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "unreadable_note",
        name: "an unreadable note",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "crumbs",
        name: "three suspicious crumbs",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "bent_fork",
        name: "a bent fork",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "cold_chip",
        name: "a cold chip",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "lint_ball",
        name: "a lint ball with ambitions",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "mystery_key",
        name: "a key to nowhere obvious",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "broken_pencil",
        name: "a broken pencil",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "umbrella_handle",
        name: "an umbrella handle, no umbrella",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "stale_cracker",
        name: "a stale cracker",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "single_grape",
        name: "a single grape",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "rubber_band",
        name: "a tired rubber band",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "paperclip",
        name: "a bent paperclip",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "empty_matchbox",
        name: "an empty matchbox",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "crumpled_menu",
        name: "a crumpled menu",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "unclaimed_receipt",
        name: "a receipt for something called lunch",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "tiny_stone",
        name: "a tiny stone",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "chewed_pencil",
        name: "a chewed pencil",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "soggy_coaster",
        name: "a soggy coaster",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "mismatched_cufflink",
        name: "a mismatched cufflink",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "old_ticket",
        name: "an expired ticket",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "mystery_button",
        name: "a button labelled IMPORTANT",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "dull_coin",
        name: "a coin too dull to identify",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "paper_crown",
        name: "a paper crown",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "one_glove",
        name: "one glove",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "biscuit_shadow",
        name: "the shadow of a biscuit",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "small_feather",
        name: "a small feather",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "questionable_stamp",
        name: "a questionable stamp",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "empty_inkwell",
        name: "an empty inkwell",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "nowhere_postcard",
        name: "a postcard from nowhere in particular",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "cold_teaspoon",
        name: "a cold teaspoon",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "weary_spoon",
        name: "a spoon that has seen things",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "lost_label",
        name: "a label marked LOST",
        rarity: Rarity::Common,
    },
];

const RARE: &[ItemDef] = &[
    ItemDef {
        id: "impossible_key",
        name: "a key to a room that does not exist",
        rarity: Rarity::Rare,
    },
    ItemDef {
        id: "pigeon_apology",
        name: "a signed apology from a pigeon",
        rarity: Rarity::Rare,
    },
    ItemDef {
        id: "haunted_receipt",
        name: "a receipt that remembers you",
        rarity: Rarity::Rare,
    },
    ItemDef {
        id: "silver_button",
        name: "a suspiciously silver button",
        rarity: Rarity::Rare,
    },
    ItemDef {
        id: "tea_map",
        name: "a map of the ideal tea temperature",
        rarity: Rarity::Rare,
    },
];

const LEGENDARY: &[ItemDef] = &[
    ItemDef {
        id: "judging_monocle",
        name: "a monocle that judges you",
        rarity: Rarity::Legendary,
    },
    ItemDef {
        id: "tea_recipe",
        name: "the original household tea recipe",
        rarity: Rarity::Legendary,
    },
    ItemDef {
        id: "royal_biscuit_tin",
        name: "the royal biscuit tin",
        rarity: Rarity::Legendary,
    },
    ItemDef {
        id: "pigeon_crown",
        name: "the Pigeon King's crown",
        rarity: Rarity::Legendary,
    },
];

const MYTHIC: &[ItemDef] = &[ItemDef {
    id: "last_biscuit",
    name: "The Last Biscuit",
    rarity: Rarity::Mythic,
}];

#[cfg(not(test))]
#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn theme(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_list(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn random_bytes(input: String) -> String;
    fn now(input: String) -> String;
    fn setting_get(input: String) -> String;
    fn profile_get(input: String) -> String;
    fn award_stats(input: String) -> String;
    fn economy_balance(input: String) -> String;
    fn economy_award(input: String) -> String;
    fn economy_spend(input: String) -> String;
    fn cosmetic_grant(input: String) -> String;
    fn cosmetic_list(input: String) -> String;
    fn cosmetic_wear(input: String) -> String;
}

#[cfg(test)]
unsafe fn send_message(_: String) -> Result<String, Error> {
    Ok(String::new())
}
#[cfg(test)]
unsafe fn theme(input: String) -> Result<String, Error> {
    Ok(input)
}
#[cfg(test)]
unsafe fn kv_get(_: String) -> Result<String, Error> {
    Ok(String::new())
}
#[cfg(test)]
unsafe fn kv_list(_: String) -> Result<String, Error> {
    Ok("[]".into())
}
#[cfg(test)]
unsafe fn kv_set(_: String) -> Result<(), Error> {
    Ok(())
}
#[cfg(test)]
unsafe fn now(_: String) -> Result<String, Error> {
    Ok("0".into())
}
#[cfg(test)]
unsafe fn setting_get(_: String) -> Result<String, Error> {
    Ok(String::new())
}
#[cfg(test)]
unsafe fn profile_get(_: String) -> Result<String, Error> {
    Ok(String::new())
}
#[cfg(test)]
unsafe fn award_stats(_: String) -> Result<String, Error> {
    Ok(String::new())
}
#[cfg(test)]
unsafe fn economy_balance(_: String) -> Result<String, Error> {
    Ok(serde_json::to_string(&EconomyBalanceResponse { balance: 0 }).unwrap())
}
#[cfg(test)]
unsafe fn economy_award(_: String) -> Result<String, Error> {
    Ok(serde_json::to_string(&EconomyTransactionResponse {
        balance: 0,
        applied: true,
        duplicate: false,
    })
    .unwrap())
}
#[cfg(test)]
unsafe fn economy_spend(_: String) -> Result<String, Error> {
    Ok(serde_json::to_string(&EconomyTransactionResponse {
        balance: 0,
        applied: false,
        duplicate: false,
    })
    .unwrap())
}
#[cfg(test)]
unsafe fn random_bytes(input: String) -> Result<String, Error> {
    let request: RandomBytesRequest = serde_json::from_str(&input)?;
    let bytes = (0..request.count).map(|index| index as u8).collect();
    Ok(serde_json::to_string(&RandomBytesResponse { bytes })?)
}
#[cfg(test)]
unsafe fn cosmetic_grant(_: String) -> Result<String, Error> {
    Ok(serde_json::to_string(&CosmeticGrantResponse::default()).unwrap())
}
#[cfg(test)]
unsafe fn cosmetic_list(_: String) -> Result<String, Error> {
    Ok(serde_json::to_string(&CosmeticInventory::default()).unwrap())
}
#[cfg(test)]
unsafe fn cosmetic_wear(_: String) -> Result<String, Error> {
    Ok(serde_json::to_string(&CosmeticWearResponse::default()).unwrap())
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct OwnedItem {
    name: String,
    rarity: String,
    count: u64,
    first_found: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Pending {
    kind: String,
    event_id: String,
    /// An item id, or `cosmetic:<id>` for a cosmetic egg.
    item_id: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Collection {
    display: String,
    eggs: u64,
    items: BTreeMap<String, OwnedItem>,
    #[serde(default)]
    pending: Option<Pending>,
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    let stats = [
        ("eggs_bought", "Eggs bought"),
        ("hatches", "Eggs hatched"),
        ("rare_pulls", "Rare items pulled"),
        ("legendary_pulls", "Legendary items pulled"),
        ("mythic_pulls", "Mythic items pulled"),
        ("trades", "Junk bundles recycled"),
        ("cosmetics", "Cosmetics found"),
    ]
    .into_iter()
    .map(|(id, description)| AchievementStat {
        id: id.into(),
        description: description.into(),
    })
    .collect();
    let achievements = vec![
        achievement(
            "first_hatch",
            "A New Hope",
            "Hatch your first egg.",
            "hatches",
            1,
            false,
        ),
        achievement(
            "rare_pull",
            "Something Better",
            "Pull a rare item.",
            "rare_pulls",
            1,
            false,
        ),
        achievement(
            "legendary_pull",
            "Remarkably Good Rubbish",
            "Pull a legendary item.",
            "legendary_pulls",
            1,
            false,
        ),
        achievement(
            "mythic_pull",
            "The Impossible Shelf",
            "Pull a mythic item.",
            "mythic_pulls",
            1,
            true,
        ),
        achievement(
            "junk_trader",
            "The Recycling Magnate",
            "Recycle 10 bundles of common junk.",
            "trades",
            10,
            false,
        ),
        achievement(
            "dressed_for_dinner",
            "Dressed for Dinner",
            "Find something to wear in an egg.",
            "cosmetics",
            1,
            false,
        ),
    ];
    Ok(serde_json::to_string(&AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        catalog_version: 1,
        stats,
        achievements,
        prestige: Vec::new(),
    })?)
}

fn achievement(
    id: &str,
    name: &str,
    description: &str,
    stat: &str,
    threshold: u64,
    secret: bool,
) -> AchievementSpec {
    AchievementSpec {
        id: id.into(),
        name: name.into(),
        description: description.into(),
        stat: stat.into(),
        threshold,
        // Legendary pulls (3.5%) are hard but fair and count toward completion. The secret mythic
        // (0.5%) is optional: finishing the collection must never hinge on one lucky roll.
        optional: secret,
        secret,
    }
}

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    let shortcut = |name: &str, expands: &str, description: &str, usage: &str| {
        CommandShortcut::new(name, expands).described(description, usage)
    };
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![
            CommandSpec {
                name: "brass".into(),
                aliases: vec!["wallet".into()],
                description: "Show your brass balance.".into(),
                usage: "!brass".into(),
                ..Default::default()
            },
            CommandSpec {
                name: "egg".into(),
                aliases: vec!["eggs".into()],
                description: "Buy, hatch, and recycle gacha eggs, and admire the shelves.".into(),
                usage: "!egg [buy | hatch | pull | recycle | odds | shelf [nick|top]]".into(),
                shortcuts: vec![
                    shortcut("hatch", "hatch", "Hatch one egg you own.", "!hatch"),
                    shortcut(
                        "pull",
                        "pull",
                        "Buy an egg and hatch it at once (50 brass).",
                        "!pull",
                    ),
                    shortcut(
                        "recycle",
                        "recycle",
                        "Turn 100 common junk items into 10 brass.",
                        "!recycle",
                    ),
                    shortcut("odds", "odds", "Show the egg odds.", "!odds"),
                    shortcut(
                        "shelf",
                        "shelf",
                        "Show your best pulls, someone else's, or the room's finest.",
                        "!shelf [nick | top]",
                    ),
                ],
            },
            CommandSpec {
                name: "wardrobe".into(),
                aliases: Vec::new(),
                description: "The badges and flourishes you've found in eggs, and what you wear."
                    .into(),
                usage: "!wardrobe [wear <name> | remove <badge|flourish>]".into(),
                shortcuts: vec![shortcut(
                    "wear",
                    "wear",
                    "Wear a badge or flourish you own.",
                    "!wear <name>",
                )],
            },
        ],
    })?)
}

#[plugin_fn]
pub fn settings(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&SettingsManifest {
        version: SETTINGS_MANIFEST_VERSION,
        settings: vec![
            SettingSpec {
                key: "game_room".into(),
                description: "Channel where brass and eggs are available.".into(),
                default: DEFAULT_GAME_ROOM.into(),
                kind: SettingKind::String { max_len: 64 },
                scopes: vec![SettingScope::Global, SettingScope::Network],
                applies_immediately: true,
            },
            SettingSpec {
                key: "announcement_room".into(),
                description: "Channel for mythic-pull announcements.".into(),
                default: DEFAULT_ANNOUNCEMENT_ROOM.into(),
                kind: SettingKind::String { max_len: 64 },
                scopes: vec![SettingScope::Global, SettingScope::Network],
                applies_immediately: true,
            },
        ],
    })?)
}

#[plugin_fn]
pub fn data_export(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let values = request.entries.iter().filter(|entry| key_belongs_to_subject(&entry.key, &request.subject, &request.aliases) && !entry.value.is_empty()).map(|entry| Ok(serde_json::json!({ "key": entry.key, "value": serde_json::from_str::<serde_json::Value>(&entry.value)? }))).collect::<Result<Vec<_>, Error>>()?;
    let data = if values.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::json!({ "records": values })
    };
    Ok(serde_json::to_string(&ModuleDataResponse {
        version: DATA_LIFECYCLE_VERSION,
        data,
    })?)
}

#[plugin_fn]
pub fn data_delete(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let mutations = request
        .entries
        .iter()
        .filter(|entry| key_belongs_to_subject(&entry.key, &request.subject, &request.aliases))
        .map(|entry| ModuleKvMutation {
            key: entry.key.clone(),
            value: None,
        })
        .collect();
    Ok(serde_json::to_string(&ModuleDataDeletePlan {
        version: DATA_LIFECYCLE_VERSION,
        mutations,
    })?)
}

fn key_belongs_to_subject(key: &str, subject: &DataSubject, aliases: &[String]) -> bool {
    let collection = format!("collection:{}:{}", subject.server, subject.profile_id);
    let balance = format!("economy:balance:{}:{}", subject.server, subject.profile_id);
    let ledger_prefix = format!("economy:ledger:{}:{}:", subject.server, subject.profile_id);
    if key == collection || key == balance || key.starts_with(&ledger_prefix) {
        return true;
    }
    aliases.iter().any(|alias| {
        key == format!("collection:{}:{alias}", subject.server)
            || key == format!("economy:balance:{}:{alias}", subject.server)
            || key.starts_with(&format!("economy:ledger:{}:{alias}:", subject.server))
    })
}

fn kv_load(key: &str) -> Result<String, Error> {
    Ok(unsafe { kv_get(serde_json::to_string(&KvGet { key: key.into() })?)? })
}
fn kv_list_entries() -> Result<Vec<jeeves_abi::ModuleKvEntry>, Error> {
    Ok(serde_json::from_str(&unsafe {
        kv_list(serde_json::to_string(&KvList::default())?)?
    })?)
}
fn kv_save(key: &str, value: &str) -> Result<(), Error> {
    unsafe {
        kv_set(serde_json::to_string(&KvSet {
            key: key.into(),
            value: value.into(),
        })?)?;
    }
    Ok(())
}

fn room_key(channel: &str) -> String {
    channel.to_ascii_lowercase()
}
fn collection_key(server: &str, profile_id: &str) -> String {
    format!("collection:{server}:{profile_id}")
}
/// How to address the caller: the host's pronoun-aware honorific, or their name from an older
/// host that doesn't send one.
fn honorific(msg: &MessagePayload) -> &str {
    if msg.honorific.is_empty() {
        display(msg)
    } else {
        &msg.honorific
    }
}

fn display(msg: &MessagePayload) -> &str {
    if msg.display.is_empty() {
        &msg.nick
    } else {
        &msg.display
    }
}

fn setting_string(key: &str, server: &str, channel: &str, fallback: &str) -> String {
    (|| -> Option<String> {
        let value = unsafe {
            setting_get(
                serde_json::to_string(&SettingGet {
                    key: key.into(),
                    server: Some(server.into()),
                    channel: Some(channel.into()),
                })
                .ok()?,
            )
            .ok()?
        };
        let value = value.trim();
        (!value.is_empty()).then_some(value.to_string())
    })()
    .unwrap_or_else(|| fallback.into())
}
fn game_room(server: &str, channel: &str) -> String {
    setting_string("game_room", server, channel, DEFAULT_GAME_ROOM)
}
fn announcement_room(server: &str, channel: &str) -> String {
    setting_string(
        "announcement_room",
        server,
        channel,
        DEFAULT_ANNOUNCEMENT_ROOM,
    )
}
fn in_game_room(server: &str, channel: &str) -> bool {
    room_key(channel) == room_key(&game_room(server, channel))
}

fn reply(server: &str, target: &str, text: &str) -> Result<(), Error> {
    unsafe {
        send_message(serde_json::to_string(&SendMessage {
            server: server.into(),
            target: target.into(),
            text: text.into(),
        })?)?;
    }
    Ok(())
}
fn themed(key: &str, defaults: &[&str], vars: &[(&str, &str)]) -> Result<String, Error> {
    Ok(unsafe {
        theme(serde_json::to_string(&ThemeReq {
            key: key.into(),
            default: defaults.iter().map(|value| (*value).into()).collect(),
            vars: vars
                .iter()
                .map(|(name, value)| ((*name).into(), (*value).into()))
                .collect(),
        })?)?
    })
}

/// A themed line addressed to the caller: `{user}` and `{honorific}` are always available.
fn say(
    msg: &MessagePayload,
    key: &str,
    default: &str,
    vars: &[(&str, &str)],
) -> Result<String, Error> {
    let mut all = vec![("user", display(msg)), ("honorific", honorific(msg))];
    all.extend_from_slice(vars);
    themed(key, &[default], &all)
}

fn profile_for_nick(server: &str, nick: &str) -> Result<Option<Profile>, Error> {
    let raw = unsafe {
        profile_get(serde_json::to_string(&ProfileKey {
            server: server.into(),
            nick: nick.into(),
        })?)?
    };
    if raw.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::from_str(&raw)?))
    }
}

fn load_collection(server: &str, profile_id: &str) -> Result<Collection, Error> {
    let raw = kv_load(&collection_key(server, profile_id))?;
    if raw.trim().is_empty() {
        Ok(Collection::default())
    } else {
        Ok(serde_json::from_str(&raw)?)
    }
}
fn save_collection(server: &str, profile_id: &str, collection: &Collection) -> Result<(), Error> {
    kv_save(
        &collection_key(server, profile_id),
        &serde_json::to_string(collection)?,
    )
}

fn random_u64() -> Result<u64, Error> {
    let raw = unsafe { random_bytes(serde_json::to_string(&RandomBytesRequest { count: 8 })?)? };
    let response: RandomBytesResponse = serde_json::from_str(&raw)?;
    let bytes: [u8; 8] = response
        .bytes
        .get(..8)
        .ok_or_else(|| Error::msg("randomness host returned too few bytes"))?
        .try_into()
        .map_err(|_| Error::msg("randomness host returned invalid bytes"))?;
    Ok(u64::from_le_bytes(bytes))
}
fn random_index(upper: usize) -> Result<usize, Error> {
    if upper == 0 {
        return Err(Error::msg("cannot select from an empty pool"));
    }
    Ok((random_u64()? % upper as u64) as usize)
}
fn random_token() -> Result<String, Error> {
    Ok(format!("{:016x}", random_u64()?))
}

/// The tier a per-mille roll lands in.
fn tier(roll: u64, odds: &[(Rarity, u64)]) -> Rarity {
    let mut edge = 0;
    for (rarity, weight) in odds {
        edge += weight;
        if roll < edge {
            return *rarity;
        }
    }
    odds.last()
        .map(|(rarity, _)| *rarity)
        .unwrap_or(Rarity::Common)
}

/// What an egg holds, rolled once and remembered in the pending record.
enum Contents {
    Item(&'static ItemDef),
    Cosmetic(&'static CosmeticDef),
}

impl Contents {
    fn pending_id(&self) -> String {
        match self {
            Contents::Item(item) => item.id.into(),
            Contents::Cosmetic(cosmetic) => format!("{COSMETIC_PREFIX}{}", cosmetic.id),
        }
    }

    fn from_pending_id(id: &str) -> Option<Self> {
        match id.strip_prefix(COSMETIC_PREFIX) {
            Some(id) => cosmetic_def(id).map(Contents::Cosmetic),
            None => item_def(id).map(Contents::Item),
        }
    }
}

fn roll_contents() -> Result<Contents, Error> {
    if random_index(1000)? < COSMETIC_CHANCE as usize {
        let rarity = tier(random_index(1000)? as u64, &COSMETIC_ODDS);
        let pool = COSMETICS
            .iter()
            .filter(|cosmetic| cosmetic.rarity == rarity)
            .collect::<Vec<_>>();
        return Ok(Contents::Cosmetic(pool[random_index(pool.len())?]));
    }
    let pool = match tier(random_index(1000)? as u64, &RARITY_ODDS) {
        Rarity::Common => COMMON,
        Rarity::Rare => RARE,
        Rarity::Legendary => LEGENDARY,
        Rarity::Mythic => MYTHIC,
    };
    Ok(Contents::Item(&pool[random_index(pool.len())?]))
}
fn item_def(id: &str) -> Option<&'static ItemDef> {
    COMMON
        .iter()
        .chain(RARE)
        .chain(LEGENDARY)
        .chain(MYTHIC)
        .find(|item| item.id == id)
}
fn cosmetic_def(id: &str) -> Option<&'static CosmeticDef> {
    COSMETICS.iter().find(|cosmetic| cosmetic.id == id)
}
fn now_secs() -> Result<i64, Error> {
    Ok(unsafe { now(String::new())? }.parse().unwrap_or(0))
}

fn balance(server: &str, profile_id: &str) -> Result<u64, Error> {
    let raw = unsafe {
        economy_balance(serde_json::to_string(&EconomyBalanceRequest {
            server: server.into(),
            profile_id: profile_id.into(),
        })?)?
    };
    Ok(serde_json::from_str::<EconomyBalanceResponse>(&raw)?.balance)
}
fn spend(
    server: &str,
    profile_id: &str,
    amount: u64,
    event_id: &str,
    reason: &str,
) -> Result<EconomyTransactionResponse, Error> {
    let raw = unsafe {
        economy_spend(serde_json::to_string(&EconomyTransactionRequest {
            server: server.into(),
            profile_id: profile_id.into(),
            amount,
            event_id: event_id.into(),
            reason: reason.into(),
        })?)?
    };
    Ok(serde_json::from_str(&raw)?)
}
fn award_brass(
    server: &str,
    profile_id: &str,
    amount: u64,
    event_id: &str,
    reason: &str,
) -> Result<EconomyTransactionResponse, Error> {
    let raw = unsafe {
        economy_award(serde_json::to_string(&EconomyTransactionRequest {
            server: server.into(),
            profile_id: profile_id.into(),
            amount,
            event_id: event_id.into(),
            reason: reason.into(),
        })?)?
    };
    Ok(serde_json::from_str(&raw)?)
}
fn award(server: &str, msg: &MessagePayload, stat: &str, event_id: &str) -> Result<(), Error> {
    unsafe {
        award_stats(serde_json::to_string(&AwardStatsRequest {
            server: server.into(),
            profile_id: msg.user_id.clone(),
            display_name: display(msg).into(),
            target: msg.target.clone(),
            increments: vec![StatIncrement {
                stat: stat.into(),
                amount: 1,
            }],
            deduplication_id: Some(event_id.into()),
        })?)?;
    }
    Ok(())
}

fn grant_cosmetic(
    server: &str,
    profile_id: &str,
    cosmetic: &CosmeticDef,
    event_id: &str,
) -> Result<CosmeticGrantResponse, Error> {
    let raw = unsafe {
        cosmetic_grant(serde_json::to_string(&CosmeticGrantRequest {
            server: server.into(),
            profile_id: profile_id.into(),
            cosmetic: Cosmetic {
                kind: cosmetic.kind,
                id: cosmetic.id.into(),
                name: cosmetic.name.into(),
                value: cosmetic.value.into(),
                module: String::new(),
                acquired_at: 0,
            },
            event_id: event_id.into(),
        })?)?
    };
    Ok(serde_json::from_str(&raw)?)
}

// ── eggs ────────────────────────────────────────────────────────────────────

/// What a finished hatch produced, for the caller to phrase.
enum Hatched {
    Item(&'static ItemDef),
    Cosmetic(&'static CosmeticDef),
    DuplicateCosmetic(&'static CosmeticDef),
}

fn add_item(collection: &mut Collection, item: &ItemDef, first_found: i64) {
    let entry = collection
        .items
        .entry(item.id.into())
        .or_insert_with(|| OwnedItem {
            name: item.name.into(),
            rarity: item.rarity.label().into(),
            count: 0,
            first_found,
        });
    entry.count = entry.count.saturating_add(1);
}

/// Apply rolled contents. Every step is idempotent under `event_id`, so a pending hatch can be
/// replayed after an interruption without double-granting or double-refunding.
fn finish_hatch(
    server: &str,
    msg: &MessagePayload,
    collection: &mut Collection,
    contents: Contents,
    event_id: &str,
) -> Result<Hatched, Error> {
    let profile_id = msg.user_id.as_str();
    let hatched = match contents {
        Contents::Item(item) => {
            add_item(collection, item, now_secs()?);
            Hatched::Item(item)
        }
        Contents::Cosmetic(cosmetic) => {
            if grant_cosmetic(server, profile_id, cosmetic, event_id)?.granted {
                Hatched::Cosmetic(cosmetic)
            } else {
                award_brass(
                    server,
                    profile_id,
                    DUPLICATE_REFUND,
                    &format!("{event_id}:refund"),
                    "duplicate_cosmetic",
                )?;
                Hatched::DuplicateCosmetic(cosmetic)
            }
        }
    };
    collection.eggs = collection.eggs.saturating_sub(1);
    collection.pending = None;
    save_collection(server, profile_id, collection)?;
    award(server, msg, "hatches", &format!("{event_id}:hatch"))?;
    match &hatched {
        Hatched::Item(item) => {
            announce_if_mythic(server, msg, item)?;
            let stat = match item.rarity {
                Rarity::Common => None,
                Rarity::Rare => Some("rare_pulls"),
                Rarity::Legendary => Some("legendary_pulls"),
                Rarity::Mythic => Some("mythic_pulls"),
            };
            if let Some(stat) = stat {
                award(server, msg, stat, &format!("{event_id}:{stat}"))?;
            }
        }
        Hatched::Cosmetic(_) => award(server, msg, "cosmetics", &format!("{event_id}:cosmetic"))?,
        Hatched::DuplicateCosmetic(_) => {}
    }
    Ok(hatched)
}

/// Phrase a hatch. `paid` is true for `!pull`, which bought the egg in the same breath.
fn hatch_text(msg: &MessagePayload, hatched: &Hatched, paid: bool) -> Result<String, Error> {
    let cost = EGG_COST.to_string();
    let refund = DUPLICATE_REFUND.to_string();
    match (hatched, paid) {
        (Hatched::Item(item), false) => say(
            msg,
            "gacha.hatched",
            "{user} hatches an egg and finds {item} ({rarity}).",
            &[("item", item.name), ("rarity", item.rarity.label())],
        ),
        (Hatched::Item(item), true) => say(
            msg,
            "gacha.pulled",
            "{user} pays {cost} brass, hatches an egg, and finds {item} ({rarity}).",
            &[("item", item.name), ("rarity", item.rarity.label()), ("cost", &cost)],
        ),
        (Hatched::Cosmetic(cosmetic), paid) => say(
            msg,
            if paid {
                "gacha.pulled_cosmetic"
            } else {
                "gacha.hatched_cosmetic"
            },
            if paid {
                "{user} pays {cost} brass and the egg holds something to wear: the {item} {value} ({rarity} {kind})! !wear {id} puts it on."
            } else {
                "{user} hatches an egg holding something to wear: the {item} {value} ({rarity} {kind})! !wear {id} puts it on."
            },
            &[
                ("item", cosmetic.name),
                ("value", cosmetic.value),
                ("rarity", cosmetic.rarity.label()),
                ("kind", cosmetic.kind.as_str()),
                ("id", &cosmetic.id.replace('_', " ")),
                ("cost", &cost),
            ],
        ),
        (Hatched::DuplicateCosmetic(cosmetic), _) => say(
            msg,
            "gacha.duplicate_cosmetic",
            "{user} finds another {item} {value}; already owned, so it's exchanged for {refund} brass.",
            &[
                ("item", cosmetic.name),
                ("value", cosmetic.value),
                ("refund", &refund),
            ],
        ),
    }
}

fn buy_egg(
    server: &str,
    msg: &MessagePayload,
    collection: &mut Collection,
) -> Result<Option<u64>, Error> {
    let profile_id = msg.user_id.as_str();
    let event_id = format!("gacha:buy:{}:{}", profile_id, random_token()?);
    collection.pending = Some(Pending {
        kind: "buy".into(),
        event_id: event_id.clone(),
        item_id: String::new(),
    });
    save_collection(server, profile_id, collection)?;
    let result = spend(server, profile_id, EGG_COST, &event_id, "egg_purchase")?;
    collection.pending = None;
    if !result.applied {
        save_collection(server, profile_id, collection)?;
        return Ok(Some(result.balance));
    }
    collection.eggs = collection.eggs.saturating_add(1);
    save_collection(server, profile_id, collection)?;
    award(server, msg, "eggs_bought", &event_id)?;
    Ok(None)
}

fn hatch(
    server: &str,
    msg: &MessagePayload,
    collection: &mut Collection,
) -> Result<Hatched, Error> {
    let contents = roll_contents()?;
    let event_id = format!("gacha:hatch:{}:{}", msg.user_id, random_token()?);
    collection.pending = Some(Pending {
        kind: "hatch".into(),
        event_id: event_id.clone(),
        item_id: contents.pending_id(),
    });
    save_collection(server, &msg.user_id, collection)?;
    finish_hatch(server, msg, collection, contents, &event_id)
}

fn cannot_afford(msg: &MessagePayload, balance: u64) -> Result<String, Error> {
    say(
        msg,
        "gacha.cannot_afford",
        "An egg costs {cost} brass; you have {balance}, {honorific}.",
        &[
            ("cost", &EGG_COST.to_string()),
            ("balance", &balance.to_string()),
        ],
    )
}

fn no_eggs(msg: &MessagePayload) -> Result<String, Error> {
    say(
        msg,
        "gacha.no_eggs",
        "You have no eggs, {honorific}. !egg buys one for {cost} brass, or !pull buys and hatches at once.",
        &[("cost", &EGG_COST.to_string())],
    )
}

fn complete_pending(
    server: &str,
    msg: &MessagePayload,
    collection: &mut Collection,
) -> Result<Option<String>, Error> {
    let Some(pending) = collection.pending.clone() else {
        return Ok(None);
    };
    let profile_id = msg.user_id.as_str();
    match pending.kind.as_str() {
        "buy" => {
            let result = spend(
                server,
                profile_id,
                EGG_COST,
                &pending.event_id,
                "egg_purchase",
            )?;
            collection.pending = None;
            if result.applied {
                collection.eggs = collection.eggs.saturating_add(1);
            }
            save_collection(server, profile_id, collection)?;
            Ok(Some(if result.applied {
                say(
                    msg,
                    "gacha.buy_recovered",
                    "Your interrupted egg purchase went through, {honorific}. Eggs on hand: {eggs}.",
                    &[("eggs", &collection.eggs.to_string())],
                )?
            } else {
                say(
                    msg,
                    "gacha.buy_failed",
                    "Your interrupted egg purchase could not be completed, {honorific}; you have {balance} brass.",
                    &[("balance", &result.balance.to_string())],
                )?
            }))
        }
        "hatch" => {
            let contents = Contents::from_pending_id(&pending.item_id)
                .ok_or_else(|| Error::msg("pending egg contents are unknown"))?;
            let hatched = finish_hatch(server, msg, collection, contents, &pending.event_id)?;
            Ok(Some(hatch_text(msg, &hatched, false)?))
        }
        "trade" => {
            let result = award_brass(
                server,
                profile_id,
                TRASH_VALUE,
                &pending.event_id,
                "junk_trade",
            )?;
            if result.applied {
                remove_trash(collection, TRASH_BUNDLE);
                award(server, msg, "trades", &pending.event_id)?;
            }
            collection.pending = None;
            save_collection(server, profile_id, collection)?;
            Ok(Some(if result.applied {
                recycled_text(msg)?
            } else {
                say(
                    msg,
                    "gacha.recycle_failed",
                    "The junk could not be recycled just now, {honorific}.",
                    &[],
                )?
            }))
        }
        _ => Err(Error::msg("unknown pending gacha action")),
    }
}

fn remove_trash(collection: &mut Collection, amount: u64) {
    let mut remaining = amount;
    for item in collection
        .items
        .values_mut()
        .filter(|item| item.rarity == Rarity::Common.label())
    {
        let removed = remaining.min(item.count);
        item.count -= removed;
        remaining -= removed;
        if remaining == 0 {
            break;
        }
    }
    collection.items.retain(|_, item| item.count > 0);
}
fn trash_count(collection: &Collection) -> u64 {
    collection
        .items
        .values()
        .filter(|item| item.rarity == Rarity::Common.label())
        .map(|item| item.count)
        .sum()
}

fn recycled_text(msg: &MessagePayload) -> Result<String, Error> {
    say(
        msg,
        "gacha.recycled",
        "{bundle} common junk items have become {value} brass. Civilization advances.",
        &[
            ("bundle", &TRASH_BUNDLE.to_string()),
            ("value", &TRASH_VALUE.to_string()),
        ],
    )
}

fn recycle(
    server: &str,
    msg: &MessagePayload,
    collection: &mut Collection,
) -> Result<String, Error> {
    let count = trash_count(collection);
    if count < TRASH_BUNDLE {
        return say(
            msg,
            "gacha.recycle_short",
            "You have {count} common junk item(s); recycling takes {bundle}, {honorific}.",
            &[
                ("count", &count.to_string()),
                ("bundle", &TRASH_BUNDLE.to_string()),
            ],
        );
    }
    let profile_id = msg.user_id.as_str();
    // The pending kind stays "trade" so interrupted recycles from before the rename recover.
    let event_id = format!("gacha:trade:{}:{}", profile_id, random_token()?);
    collection.pending = Some(Pending {
        kind: "trade".into(),
        event_id: event_id.clone(),
        item_id: String::new(),
    });
    save_collection(server, profile_id, collection)?;
    let result = award_brass(server, profile_id, TRASH_VALUE, &event_id, "junk_trade")?;
    if result.applied {
        remove_trash(collection, TRASH_BUNDLE);
    }
    collection.pending = None;
    save_collection(server, profile_id, collection)?;
    if !result.applied {
        return say(
            msg,
            "gacha.recycle_failed",
            "The junk could not be recycled just now, {honorific}.",
            &[],
        );
    }
    award(server, msg, "trades", &event_id)?;
    recycled_text(msg)
}

// ── shelves ─────────────────────────────────────────────────────────────────

fn item_sort(left: &(&String, &OwnedItem), right: &(&String, &OwnedItem)) -> std::cmp::Ordering {
    rarity_rank(&right.1.rarity)
        .cmp(&rarity_rank(&left.1.rarity))
        .then_with(|| left.1.first_found.cmp(&right.1.first_found))
        .then_with(|| left.0.cmp(right.0))
}
fn rarity_rank(label: &str) -> u8 {
    match label {
        "mythic" => 3,
        "legendary" => 2,
        "rare" => 1,
        _ => 0,
    }
}
fn shelf_items(collection: &Collection) -> Vec<(&String, &OwnedItem)> {
    let mut items = collection
        .items
        .iter()
        .filter(|(_, item)| item.count > 0)
        .collect::<Vec<_>>();
    items.sort_by(item_sort);
    items.truncate(SHELF_SIZE);
    items
}
/// Break every word of a name with a zero-width space after its first character, so listing
/// someone on a leaderboard doesn't highlight (ping) them. Display names may carry a title
/// ("sir aureate"), so each word is broken rather than just the first.
fn no_highlight(name: &str) -> String {
    name.split(' ')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => format!("{first}\u{200B}{}", chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// "Alice: the royal biscuit tin [legendary] x1", naming items from the current catalogue.
fn shelf_entry(owner: &str, id: &str, item: &OwnedItem) -> String {
    let name = item_def(id).map_or(item.name.as_str(), |def| def.name);
    format!("{owner}: {name} [{}] x{}", item.rarity, item.count)
}

fn announce_if_mythic(server: &str, msg: &MessagePayload, item: &ItemDef) -> Result<(), Error> {
    if item.rarity != Rarity::Mythic {
        return Ok(());
    }
    let room = announcement_room(server, &msg.target);
    let text = themed("gacha.mythic_announcement", &["{user} just pulled the mythic {item} in {room}! Do join us there before the next miracle."], &[("user", display(msg)), ("item", item.name), ("room", &game_room(server, &msg.target))])?;
    let _ = reply(server, &room, &text);
    Ok(())
}

fn shelf(server: &str, msg: &MessagePayload, argument: &str) -> Result<String, Error> {
    if argument.eq_ignore_ascii_case("top") {
        let prefix = format!("collection:{server}:");
        let mut entries = Vec::new();
        let listed = kv_list_entries()?;
        let collections = listed
            .iter()
            .filter(|entry| entry.key.starts_with(&prefix))
            .filter_map(|entry| serde_json::from_str::<Collection>(&entry.value).ok())
            .collect::<Vec<_>>();
        for collection in &collections {
            for (id, item) in shelf_items(collection) {
                entries.push((collection.display.as_str(), id, item));
            }
        }
        entries.sort_by(|left, right| item_sort(&(left.1, left.2), &(right.1, right.2)));
        entries.truncate(GLOBAL_SHELF_SIZE);
        if entries.is_empty() {
            return say(
                msg,
                "gacha.shelf_top_empty",
                "The global shelf is empty.",
                &[],
            );
        }
        let items = entries
            .iter()
            .map(|(owner, id, item)| shelf_entry(&no_highlight(owner), id, item))
            .collect::<Vec<_>>()
            .join(" | ");
        return say(
            msg,
            "gacha.shelf_top",
            "The room's finest: {items}",
            &[("items", &items)],
        );
    }
    let (owner, collection) = if argument.is_empty() {
        (
            display(msg).to_string(),
            load_collection(server, &msg.user_id)?,
        )
    } else {
        let nick = argument.trim_start_matches('$');
        let Some(profile) = profile_for_nick(server, nick)? else {
            return say(
                msg,
                "gacha.shelf_unknown",
                "I have no shelf for {nick}, {honorific}.",
                &[("nick", nick)],
            );
        };
        (nick.to_string(), load_collection(server, &profile.id)?)
    };
    let items = shelf_items(&collection);
    if items.is_empty() {
        return if argument.is_empty() {
            say(
                msg,
                "gacha.shelf_empty",
                "Your shelf is empty, {honorific}. The egg awaits.",
                &[],
            )
        } else {
            say(
                msg,
                "gacha.shelf_other_empty",
                "{nick}'s shelf is empty.",
                &[("nick", &owner)],
            )
        };
    }
    let items = items
        .iter()
        .map(|(id, item)| shelf_entry(&owner, id, item))
        .collect::<Vec<_>>()
        .join(" | ");
    say(msg, "gacha.shelf", "{items}", &[("items", &items)])
}

// ── wardrobe ────────────────────────────────────────────────────────────────

fn inventory(server: &str, profile_id: &str) -> Result<CosmeticInventory, Error> {
    Ok(serde_json::from_str(&unsafe {
        cosmetic_list(serde_json::to_string(&CosmeticListRequest {
            server: server.into(),
            profile_id: profile_id.into(),
        })?)?
    })?)
}

fn wear(
    server: &str,
    profile_id: &str,
    kind: CosmeticKind,
    id: Option<String>,
) -> Result<CosmeticWearResponse, Error> {
    Ok(serde_json::from_str(&unsafe {
        cosmetic_wear(serde_json::to_string(&CosmeticWearRequest {
            server: server.into(),
            profile_id: profile_id.into(),
            kind,
            id,
        })?)?
    })?)
}

/// Match "owl", "top hat", "top_hat", or "owl badge" against owned cosmetics.
fn find_owned<'a>(owned: &'a [Cosmetic], query: &str) -> Option<&'a Cosmetic> {
    let query = query.trim().to_lowercase().replace(' ', "_");
    let spaced = query.replace('_', " ");
    owned
        .iter()
        .find(|item| item.id == query || item.name.to_lowercase() == spaced)
        .or_else(|| {
            owned
                .iter()
                .find(|item| item.id.contains(&query) || item.name.to_lowercase().contains(&spaced))
        })
}

fn wardrobe(server: &str, msg: &MessagePayload, argument: &str) -> Result<String, Error> {
    let (sub, rest) = argument
        .split_once(char::is_whitespace)
        .map(|(sub, rest)| (sub, rest.trim()))
        .unwrap_or((argument, ""));
    let owned = inventory(server, &msg.user_id)?;
    match sub.to_ascii_lowercase().as_str() {
        "" => {
            if owned.owned.is_empty() {
                return say(
                    msg,
                    "gacha.wardrobe_empty",
                    "Your wardrobe is empty, {honorific}. About one egg in twelve holds a badge or flourish.",
                    &[],
                );
            }
            let badges = owned
                .owned
                .iter()
                .filter(|item| item.kind == CosmeticKind::Badge)
                .map(|item| format!("{} {}", item.value, item.name.trim_end_matches(" badge")))
                .collect::<Vec<_>>()
                .join(", ");
            let flourishes = owned
                .owned
                .iter()
                .filter(|item| item.kind == CosmeticKind::Flourish)
                .map(|item| format!("{} ({})", item.name, item.value))
                .collect::<Vec<_>>()
                .join(", ");
            let items = [("Badges", badges), ("Flourishes", flourishes)]
                .into_iter()
                .filter(|(_, list)| !list.is_empty())
                .map(|(label, list)| format!("{label}: {list}"))
                .collect::<Vec<_>>()
                .join(" · ");
            let wearing = [&owned.badge, &owned.flourish]
                .into_iter()
                .flatten()
                .map(|item| item.value.clone())
                .collect::<Vec<_>>()
                .join(" and ");
            let wearing = if wearing.is_empty() {
                "nothing yet (!wear <name>)".to_string()
            } else {
                wearing
            };
            say(
                msg,
                "gacha.wardrobe",
                "{user}'s wardrobe. {items}. Wearing: {wearing}.",
                &[("items", &items), ("wearing", &wearing)],
            )
        }
        "wear" => {
            let Some(item) = find_owned(&owned.owned, rest).filter(|_| !rest.is_empty()) else {
                return say(
                    msg,
                    "gacha.wear_unknown",
                    "You don't own anything called '{query}', {honorific}. !wardrobe lists what you have.",
                    &[("query", rest)],
                );
            };
            let result = wear(server, &msg.user_id, item.kind, Some(item.id.clone()))?;
            if !result.ok {
                return say(
                    msg,
                    "gacha.wear_unknown",
                    "You don't own anything called '{query}', {honorific}. !wardrobe lists what you have.",
                    &[("query", rest)],
                );
            }
            say(
                msg,
                "gacha.wear_ok",
                "{user} now wears the {item} {value}.",
                &[("item", &item.name), ("value", &item.value)],
            )
        }
        "remove" | "off" => {
            let Some(kind) = CosmeticKind::parse(rest) else {
                return say(
                    msg,
                    "gacha.remove_usage",
                    "Remove which, {honorific}: !wardrobe remove badge or !wardrobe remove flourish?",
                    &[],
                );
            };
            wear(server, &msg.user_id, kind, None)?;
            say(
                msg,
                "gacha.remove_ok",
                "Very good, {honorific}; your {kind} is put away.",
                &[("kind", kind.as_str())],
            )
        }
        _ => say(
            msg,
            "gacha.wardrobe_usage",
            "Use !wardrobe, !wardrobe wear <name>, or !wardrobe remove <badge|flourish>, {honorific}.",
            &[],
        ),
    }
}

// ── dispatch ────────────────────────────────────────────────────────────────

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    let server = env.server.as_str();
    let text = msg.text.trim();
    let (command, rest) = text
        .split_once(char::is_whitespace)
        .map(|(command, rest)| (command.to_ascii_lowercase(), rest.trim()))
        .unwrap_or((text.to_ascii_lowercase(), ""));
    if !matches!(command.as_str(), "!brass" | "!egg" | "!wardrobe") {
        return Ok(());
    }
    let dest = if msg.is_private {
        msg.nick.as_str()
    } else {
        msg.target.as_str()
    };
    if msg.user_id.is_empty() {
        reply(
            server,
            dest,
            &say(
                &msg,
                "gacha.profile_missing",
                "I cannot establish your profile, {honorific}; the brass ledger must wait.",
                &[],
            )?,
        )?;
        return Ok(());
    }
    // The wardrobe is personal, so it works anywhere, including by private message.
    if command == "!wardrobe" {
        reply(server, dest, &wardrobe(server, &msg, rest)?)?;
        return Ok(());
    }
    if msg.is_private {
        reply(
            server,
            dest,
            &say(
                &msg,
                "gacha.channel_only",
                "The brass and eggs are kept in {room}, {honorific}.",
                &[("room", &game_room(server, &msg.nick))],
            )?,
        )?;
        return Ok(());
    }
    if !in_game_room(server, &msg.target) {
        reply(
            server,
            dest,
            &say(
                &msg,
                "gacha.room_redirect",
                "The economy has decamped to {room}, {user}. Do join us there.",
                &[("room", &game_room(server, &msg.target))],
            )?,
        )?;
        return Ok(());
    }
    let mut collection = load_collection(server, &msg.user_id)?;
    collection.display = display(&msg).into();
    if let Some(recovered) = complete_pending(server, &msg, &mut collection)? {
        reply(server, dest, &recovered)?;
        return Ok(());
    }
    let (sub, argument) = if command == "!brass" {
        ("brass".to_string(), "")
    } else {
        rest.split_once(char::is_whitespace)
            .map(|(sub, argument)| (sub.to_ascii_lowercase(), argument.trim()))
            .unwrap_or((rest.to_ascii_lowercase(), ""))
    };
    let text = match sub.as_str() {
        "brass" => say(
            &msg,
            "gacha.balance",
            "{user} has {balance} brass.",
            &[("balance", &balance(server, &msg.user_id)?.to_string())],
        )?,
        "" | "buy" => match buy_egg(server, &msg, &mut collection)? {
            Some(balance) => cannot_afford(&msg, balance)?,
            None => say(
                &msg,
                "gacha.bought",
                "{user} buys an egg. Eggs on hand: {eggs}.",
                &[("eggs", &collection.eggs.to_string())],
            )?,
        },
        "hatch" if collection.eggs == 0 => no_eggs(&msg)?,
        "hatch" => {
            let hatched = hatch(server, &msg, &mut collection)?;
            hatch_text(&msg, &hatched, false)?
        }
        "pull" => match buy_egg(server, &msg, &mut collection)? {
            Some(balance) => cannot_afford(&msg, balance)?,
            None => {
                let hatched = hatch(server, &msg, &mut collection)?;
                hatch_text(&msg, &hatched, true)?
            }
        },
        "recycle" | "trade" => recycle(server, &msg, &mut collection)?,
        "shelf" => shelf(server, &msg, argument)?,
        "odds" => say(
            &msg,
            "gacha.odds",
            "Egg odds: 85% common · 11% rare · 3.5% legendary · 0.5% mythic, and about one egg in twelve holds a badge or flourish instead. {cost} brass an egg.",
            &[("cost", &EGG_COST.to_string())],
        )?,
        _ => say(
            &msg,
            "gacha.usage",
            "Use !egg [buy | hatch | pull | recycle | odds | shelf], {honorific}.",
            &[],
        )?,
    };
    reply(server, dest, &text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_fifty_items_and_unique_cosmetics() {
        assert_eq!(
            COMMON.len() + RARE.len() + LEGENDARY.len() + MYTHIC.len(),
            50
        );
        let mut ids = COSMETICS
            .iter()
            .map(|cosmetic| cosmetic.id)
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), COSMETICS.len());
        for rarity in [Rarity::Common, Rarity::Rare, Rarity::Legendary] {
            assert!(COSMETICS.iter().any(|cosmetic| cosmetic.rarity == rarity));
        }
        for cosmetic in COSMETICS {
            let max = match cosmetic.kind {
                CosmeticKind::Badge => 16,
                CosmeticKind::Flourish => 40,
            };
            assert!(cosmetic.value.chars().count() <= max, "{}", cosmetic.id);
        }
    }

    #[test]
    fn odds_are_tiered_as_advertised() {
        assert_eq!(
            RARITY_ODDS.iter().map(|(_, weight)| weight).sum::<u64>(),
            1000
        );
        assert_eq!(
            COSMETIC_ODDS.iter().map(|(_, weight)| weight).sum::<u64>(),
            1000
        );
        assert_eq!(tier(0, &RARITY_ODDS), Rarity::Common);
        assert_eq!(tier(849, &RARITY_ODDS), Rarity::Common);
        assert_eq!(tier(850, &RARITY_ODDS), Rarity::Rare);
        assert_eq!(tier(960, &RARITY_ODDS), Rarity::Legendary);
        assert_eq!(tier(995, &RARITY_ODDS), Rarity::Mythic);
        assert_eq!(tier(999, &RARITY_ODDS), Rarity::Mythic);
    }

    #[test]
    fn pending_contents_round_trip() {
        let cosmetic = Contents::Cosmetic(cosmetic_def("owl").unwrap());
        assert_eq!(cosmetic.pending_id(), "cosmetic:owl");
        assert!(matches!(
            Contents::from_pending_id("cosmetic:owl"),
            Some(Contents::Cosmetic(def)) if def.id == "owl"
        ));
        assert!(matches!(
            Contents::from_pending_id("last_biscuit"),
            Some(Contents::Item(def)) if def.rarity == Rarity::Mythic
        ));
        assert!(Contents::from_pending_id("cosmetic:nope").is_none());
    }

    #[test]
    fn wardrobe_matching_is_forgiving() {
        let owned = [
            cosmetic_def("top_hat").unwrap(),
            cosmetic_def("ta_da").unwrap(),
        ]
        .iter()
        .map(|def| Cosmetic {
            kind: def.kind,
            id: def.id.into(),
            name: def.name.into(),
            value: def.value.into(),
            module: String::new(),
            acquired_at: 0,
        })
        .collect::<Vec<_>>();
        for query in ["top hat", "top_hat", "Top Hat Badge", "hat"] {
            assert_eq!(find_owned(&owned, query).unwrap().id, "top_hat", "{query}");
        }
        assert_eq!(find_owned(&owned, "ta-da").unwrap().id, "ta_da");
        assert!(find_owned(&owned, "crown").is_none());
    }

    #[test]
    fn recycle_count_only_includes_common_items() {
        let mut collection = Collection::default();
        for (id, rarity) in [("junk", "common"), ("mythic", "mythic")] {
            collection.items.insert(
                id.into(),
                OwnedItem {
                    name: id.into(),
                    rarity: rarity.into(),
                    count: 100,
                    first_found: 0,
                },
            );
        }
        assert_eq!(trash_count(&collection), 100);
        remove_trash(&mut collection, 100);
        assert!(!collection.items.contains_key("junk"));
        assert!(collection.items.contains_key("mythic"));
    }
}
