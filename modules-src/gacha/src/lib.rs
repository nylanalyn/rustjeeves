//! The #games brass economy and deliberately silly egg pulls.

use extism_pdk::*;
use jeeves_abi::{
    AchievementManifest, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandSpec, DataSubject, EconomyBalanceRequest, EconomyBalanceResponse,
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

const COMMON: &[ItemDef] = &[
    ItemDef {
        id: "melted_spoon",
        name: "melted spoon",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "french_fry",
        name: "French fry",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "spiderman_photo",
        name: "photo of Spider-Man",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "damp_receipt",
        name: "damp receipt",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "single_shoelace",
        name: "single shoelace",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "button_unknown",
        name: "button of unknown origin",
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
        id: "loose_button",
        name: "a loose button",
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
        id: "receipt_fragment",
        name: "half a receipt",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "cold_teaspoon",
        name: "a cold teaspoon",
        rarity: Rarity::Common,
    },
    ItemDef {
        id: "unlucky_button",
        name: "an unlucky button",
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
        name: "key to a room that does not exist",
        rarity: Rarity::Rare,
    },
    ItemDef {
        id: "pigeon_apology",
        name: "signed apology from a pigeon",
        rarity: Rarity::Rare,
    },
    ItemDef {
        id: "haunted_receipt",
        name: "receipt that remembers you",
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
        name: "monocle that judges you",
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
        "eggs_bought",
        "hatches",
        "rare_pulls",
        "legendary_pulls",
        "mythic_pulls",
        "trades",
    ]
    .into_iter()
    .map(|id| AchievementStat {
        id: id.into(),
        description: id.replace('_', " "),
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
            true,
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
            "Trade in 10 bundles of common junk.",
            "trades",
            10,
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
        optional: false,
        secret,
    }
}

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![
            CommandSpec {
                name: "brass".into(),
                aliases: vec!["wallet".into()],
                description: "Show your brass balance.".into(),
                usage: "!brass".into(),
            },
            CommandSpec {
                name: "egg".into(),
                aliases: Vec::new(),
                description: "Buy one gacha egg for 50 brass.".into(),
                usage: "!egg".into(),
            },
            CommandSpec {
                name: "hatch".into(),
                aliases: Vec::new(),
                description: "Hatch one egg and discover its contents.".into(),
                usage: "!hatch".into(),
            },
            CommandSpec {
                name: "shelf".into(),
                aliases: Vec::new(),
                description: "Show your best pulls or the room's finest discoveries.".into(),
                usage: "!shelf [<user> | top]".into(),
            },
            CommandSpec {
                name: "trade".into(),
                aliases: Vec::new(),
                description: "Trade 100 common junk items for 10 brass.".into(),
                usage: "!trade".into(),
            },
            CommandSpec {
                name: "odds".into(),
                aliases: Vec::new(),
                description: "Show the egg pull odds.".into(),
                usage: "!odds".into(),
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
fn identity(msg: &MessagePayload) -> String {
    if msg.user_id.is_empty() {
        format!("nick:{}", msg.nick.to_ascii_lowercase())
    } else {
        msg.user_id.clone()
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
fn response(text: &str) -> Result<String, Error> {
    themed("gacha.response", &["{text}"], &[("text", text)])
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

fn random_index(upper: usize) -> Result<usize, Error> {
    if upper == 0 {
        return Err(Error::msg("cannot select from an empty pool"));
    }
    let raw = unsafe { random_bytes(serde_json::to_string(&RandomBytesRequest { count: 8 })?)? };
    let response: RandomBytesResponse = serde_json::from_str(&raw)?;
    let bytes: [u8; 8] = response
        .bytes
        .get(..8)
        .ok_or_else(|| Error::msg("randomness host returned too few bytes"))?
        .try_into()
        .map_err(|_| Error::msg("randomness host returned invalid bytes"))?;
    Ok((u64::from_le_bytes(bytes) % upper as u64) as usize)
}
fn random_token() -> Result<String, Error> {
    let raw = unsafe { random_bytes(serde_json::to_string(&RandomBytesRequest { count: 8 })?)? };
    let response: RandomBytesResponse = serde_json::from_str(&raw)?;
    if response.bytes.len() < 8 {
        return Err(Error::msg("randomness host returned too few bytes"));
    }
    Ok(response.bytes[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
fn roll_item() -> Result<&'static ItemDef, Error> {
    let bucket = random_index(100)?;
    let (pool, _) = if bucket < 90 {
        (COMMON, Rarity::Common)
    } else if bucket < 95 {
        (RARE, Rarity::Rare)
    } else if bucket < 99 {
        (LEGENDARY, Rarity::Legendary)
    } else {
        (MYTHIC, Rarity::Mythic)
    };
    Ok(&pool[random_index(pool.len())?])
}
fn item_def(id: &str) -> Option<&'static ItemDef> {
    COMMON
        .iter()
        .chain(RARE)
        .chain(LEGENDARY)
        .chain(MYTHIC)
        .find(|item| item.id == id)
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
    if msg.user_id.is_empty() {
        return Ok(());
    }
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

fn complete_pending(
    server: &str,
    msg: &MessagePayload,
    collection: &mut Collection,
) -> Result<Option<String>, Error> {
    let Some(pending) = collection.pending.clone() else {
        return Ok(None);
    };
    let profile_id = identity(msg);
    match pending.kind.as_str() {
        "buy" => {
            let result = spend(
                server,
                &profile_id,
                EGG_COST,
                &pending.event_id,
                "egg_purchase",
            )?;
            collection.pending = None;
            if result.applied {
                collection.eggs = collection.eggs.saturating_add(1);
                save_collection(server, &profile_id, collection)?;
                return Ok(Some(format!(
                    "Egg purchase completed. You now have {} egg(s).",
                    collection.eggs
                )));
            }
            save_collection(server, &profile_id, collection)?;
            Ok(Some(format!(
                "The egg purchase could not be completed; you have {} brass.",
                result.balance
            )))
        }
        "hatch" => {
            let item = item_def(&pending.item_id)
                .ok_or_else(|| Error::msg("pending egg item is unknown"))?;
            add_item(collection, item, now_secs()?);
            collection.eggs = collection.eggs.saturating_sub(1);
            collection.pending = None;
            save_collection(server, &profile_id, collection)?;
            announce_if_mythic(server, msg, item)?;
            award_pull(server, msg, item, &pending.event_id)?;
            Ok(Some(format!(
                "{user} hatches an egg and finds {item} ({rarity}).",
                user = display(msg),
                item = item.name,
                rarity = item.rarity.label()
            )))
        }
        "trade" => {
            let result = award_brass(
                server,
                &profile_id,
                TRASH_VALUE,
                &pending.event_id,
                "junk_trade",
            )?;
            if result.applied {
                remove_trash(collection, TRASH_BUNDLE);
                collection.pending = None;
                save_collection(server, &profile_id, collection)?;
                award(server, msg, "trades", &pending.event_id)?;
                return Ok(Some(
                    "One hundred common junk items have become 10 brass. Civilization advances."
                        .into(),
                ));
            }
            collection.pending = None;
            save_collection(server, &profile_id, collection)?;
            Ok(Some("The junk trade could not be completed.".into()))
        }
        _ => Err(Error::msg("unknown pending gacha action")),
    }
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

fn item_sort(left: &OwnedItem, right: &OwnedItem) -> std::cmp::Ordering {
    let left_rank = rarity_rank(&left.rarity);
    let right_rank = rarity_rank(&right.rarity);
    right_rank
        .cmp(&left_rank)
        .then_with(|| left.first_found.cmp(&right.first_found))
        .then_with(|| left.name.cmp(&right.name))
}
fn rarity_rank(label: &str) -> u8 {
    match label {
        "mythic" => 3,
        "legendary" => 2,
        "rare" => 1,
        _ => 0,
    }
}
fn shelf_items(collection: &Collection) -> Vec<&OwnedItem> {
    let mut items = collection
        .items
        .values()
        .filter(|item| item.count > 0)
        .collect::<Vec<_>>();
    items.sort_by(|left, right| item_sort(left, right));
    items.truncate(SHELF_SIZE);
    items
}
fn shelf_line(user: &str, item: &OwnedItem) -> String {
    format!("{user}: {} [{}] x{}", item.name, item.rarity, item.count)
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
fn award_pull(
    server: &str,
    msg: &MessagePayload,
    item: &ItemDef,
    event_id: &str,
) -> Result<(), Error> {
    award(server, msg, "hatches", &format!("{event_id}:hatch"))?;
    let stat = match item.rarity {
        Rarity::Common => None,
        Rarity::Rare => Some("rare_pulls"),
        Rarity::Legendary => Some("legendary_pulls"),
        Rarity::Mythic => Some("mythic_pulls"),
    };
    if let Some(stat) = stat {
        award(server, msg, stat, &format!("{event_id}:{stat}"))?;
    }
    Ok(())
}

fn buy_egg(
    server: &str,
    msg: &MessagePayload,
    collection: &mut Collection,
) -> Result<String, Error> {
    let profile_id = identity(msg);
    let event_id = format!("gacha:buy:{}:{}", profile_id, random_token()?);
    collection.pending = Some(Pending {
        kind: "buy".into(),
        event_id: event_id.clone(),
        item_id: String::new(),
    });
    save_collection(server, &profile_id, collection)?;
    let result = spend(server, &profile_id, EGG_COST, &event_id, "egg_purchase")?;
    if !result.applied {
        collection.pending = None;
        save_collection(server, &profile_id, collection)?;
        return Ok(format!(
            "An egg costs {EGG_COST} brass; you have {}.",
            result.balance
        ));
    }
    collection.eggs = collection.eggs.saturating_add(1);
    collection.pending = None;
    save_collection(server, &profile_id, collection)?;
    award(server, msg, "eggs_bought", &event_id)?;
    Ok(format!(
        "{user} buys an egg. Eggs on hand: {eggs}.",
        user = display(msg),
        eggs = collection.eggs
    ))
}

fn hatch(server: &str, msg: &MessagePayload, collection: &mut Collection) -> Result<String, Error> {
    if collection.eggs == 0 {
        return Ok("You have no eggs, sir. !egg purchases one for 50 brass.".into());
    }
    let item = roll_item()?;
    let profile_id = identity(msg);
    let event_id = format!("gacha:hatch:{}:{}", profile_id, random_token()?);
    collection.pending = Some(Pending {
        kind: "hatch".into(),
        event_id: event_id.clone(),
        item_id: item.id.into(),
    });
    save_collection(server, &profile_id, collection)?;
    add_item(collection, item, now_secs()?);
    collection.eggs -= 1;
    collection.pending = None;
    save_collection(server, &profile_id, collection)?;
    announce_if_mythic(server, msg, item)?;
    award_pull(server, msg, item, &event_id)?;
    Ok(format!(
        "{user} hatches an egg and finds {item} ({rarity}).",
        user = display(msg),
        item = item.name,
        rarity = item.rarity.label()
    ))
}

fn trade(server: &str, msg: &MessagePayload, collection: &mut Collection) -> Result<String, Error> {
    let count = trash_count(collection);
    if count < TRASH_BUNDLE {
        return Ok(format!(
            "You have {count} common junk item(s); a trade requires {TRASH_BUNDLE}."
        ));
    }
    let profile_id = identity(msg);
    let event_id = format!("gacha:trade:{}:{}", profile_id, random_token()?);
    collection.pending = Some(Pending {
        kind: "trade".into(),
        event_id: event_id.clone(),
        item_id: String::new(),
    });
    save_collection(server, &profile_id, collection)?;
    let result = award_brass(server, &profile_id, TRASH_VALUE, &event_id, "junk_trade")?;
    if result.applied {
        remove_trash(collection, TRASH_BUNDLE);
        collection.pending = None;
        save_collection(server, &profile_id, collection)?;
        award(server, msg, "trades", &event_id)?;
        return Ok(
            "One hundred common junk items have become 10 brass. Civilization advances.".into(),
        );
    }
    collection.pending = None;
    save_collection(server, &profile_id, collection)?;
    Ok("The junk trade could not be completed.".into())
}

fn shelf(server: &str, msg: &MessagePayload, argument: &str) -> Result<String, Error> {
    if argument.eq_ignore_ascii_case("top") {
        let prefix = format!("collection:{server}:");
        let mut entries = Vec::new();
        for entry in kv_list_entries()? {
            if !entry.key.starts_with(&prefix) {
                continue;
            }
            let Ok(collection) = serde_json::from_str::<Collection>(&entry.value) else {
                continue;
            };
            for item in shelf_items(&collection) {
                entries.push((collection.display.clone(), item.clone()));
            }
        }
        entries.sort_by(|left, right| item_sort(&left.1, &right.1));
        entries.truncate(GLOBAL_SHELF_SIZE);
        let text = if entries.is_empty() {
            "The global shelf is empty.".into()
        } else {
            entries
                .iter()
                .map(|(user, item)| shelf_line(user, item))
                .collect::<Vec<_>>()
                .join(" | ")
        };
        return Ok(text);
    }
    if argument.is_empty() {
        let collection = load_collection(server, &identity(msg))?;
        let items = shelf_items(&collection);
        return Ok(if items.is_empty() {
            "Your shelf is empty. The egg awaits.".into()
        } else {
            items
                .iter()
                .map(|item| shelf_line(display(msg), item))
                .collect::<Vec<_>>()
                .join(" | ")
        });
    }
    let nick = argument.trim_start_matches('$');
    let Some(profile) = profile_for_nick(server, nick)? else {
        return Ok(format!("I have no shelf for {nick}, sir."));
    };
    let collection = load_collection(server, &profile.id)?;
    let items = shelf_items(&collection);
    Ok(if items.is_empty() {
        format!("{nick}'s shelf is empty.")
    } else {
        items
            .iter()
            .map(|item| shelf_line(nick, item))
            .collect::<Vec<_>>()
            .join(" | ")
    })
}

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    let mut parts = msg.text.split_whitespace();
    let command = parts.next().unwrap_or("").to_ascii_lowercase();
    if !matches!(
        command.as_str(),
        "!brass" | "!wallet" | "!egg" | "!hatch" | "!shelf" | "!trade" | "!odds"
    ) {
        return Ok(());
    }
    if msg.is_private {
        reply(
            &env.server,
            &msg.nick,
            &themed(
                "gacha.channel_only",
                &["The brass and eggs are kept in {room}, sir."],
                &[("room", &game_room(&env.server, &msg.nick))],
            )?,
        )?;
        return Ok(());
    }
    if !in_game_room(&env.server, &msg.target) {
        reply(
            &env.server,
            &msg.target,
            &themed(
                "gacha.room_redirect",
                &["The economy has decamped to {room}, {user}. Do join us there."],
                &[
                    ("room", &game_room(&env.server, &msg.target)),
                    ("user", display(&msg)),
                ],
            )?,
        )?;
        return Ok(());
    }
    if msg.user_id.is_empty() {
        reply(
            &env.server,
            &msg.target,
            &themed(
                "gacha.profile_missing",
                &["I cannot establish your profile, sir; the brass ledger must wait."],
                &[],
            )?,
        )?;
        return Ok(());
    }
    let profile_id = identity(&msg);
    let mut collection = load_collection(&env.server, &profile_id)?;
    collection.display = display(&msg).into();
    if let Some(recovered) = complete_pending(&env.server, &msg, &mut collection)? {
        reply(&env.server, &msg.target, &response(&recovered)?)?;
        return Ok(());
    }
    let argument = parts.next().unwrap_or("");
    let text = match command.as_str() {
        "!brass" | "!wallet" => format!(
            "{user} has {balance} brass.",
            user = display(&msg),
            balance = balance(&env.server, &profile_id)?
        ),
        "!egg" => buy_egg(&env.server, &msg, &mut collection)?,
        "!hatch" => hatch(&env.server, &msg, &mut collection)?,
        "!trade" => trade(&env.server, &msg, &mut collection)?,
        "!shelf" => shelf(&env.server, &msg, argument)?,
        "!odds" => {
            "Egg odds: 90% common | 5% rare | 4% legendary | 1% mythic. Egg cost: 50 brass.".into()
        }
        _ => unreachable!(),
    };
    reply(&env.server, &msg.target, &response(&text)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_fifty_items() {
        assert_eq!(
            COMMON.len() + RARE.len() + LEGENDARY.len() + MYTHIC.len(),
            50
        );
    }

    #[test]
    fn rarity_order_keeps_mythic_above_common() {
        assert!(rarity_rank(Rarity::Mythic.label()) > rarity_rank(Rarity::Common.label()));
    }

    #[test]
    fn trade_count_only_includes_common_items() {
        let mut collection = Collection::default();
        collection.items.insert(
            "junk".into(),
            OwnedItem {
                name: "junk".into(),
                rarity: "common".into(),
                count: 100,
                first_found: 0,
            },
        );
        collection.items.insert(
            "mythic".into(),
            OwnedItem {
                name: "mythic".into(),
                rarity: "mythic".into(),
                count: 100,
                first_found: 0,
            },
        );
        assert_eq!(trash_count(&collection), 100);
        remove_trash(&mut collection, 100);
        assert!(!collection.items.contains_key("junk"));
        assert!(collection.items.contains_key("mythic"));
    }
}
