//! A small, unhurried garden. All growth follows care; nothing withers.
mod lifecycle;
mod model;

use extism_pdk::*;
use jeeves_abi::*;
use jeeves_guest::{
    encode, honorific, kv_load, kv_save, no_highlight, reply, setting_bool, setting_i64, themed,
    timestamp, Entropy,
};
use model::*;
use std::collections::BTreeSet;

#[host_fn]
extern "ExtismHost" {
    fn profile_get(input: String) -> String;
    fn channel_members(input: String) -> String;
    fn award_stats(input: String) -> String;
}

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![CommandSpec {
            name: "botany".into(),
            description: "Tend a gentle plant, keep photos, and wander the garden.".into(),
            usage: "!botany plant | water | look [user] | harvest | save | photo [1-5] | discard <1-5> | garden".into(),
            shortcuts: vec![
                CommandShortcut::new("plant", "plant").described("Plant your free seed.", "!plant"),
                CommandShortcut::new("water", "water").described("Give your plant a little care.", "!water"),
                CommandShortcut::new("harvest", "harvest").described("Gather ripe fruit and receive a new seed.", "!harvest"),
                CommandShortcut::new("photo", "photo").described("Browse your five plant photos.", "!photo [1-5]"),
            ],
            ..Default::default()
        }],
    })?)
}

#[plugin_fn]
pub fn settings(_: String) -> FnResult<String> {
    let specs = [
        (
            "enabled",
            "Enable the gentle garden.",
            "true",
            SettingKind::Boolean,
        ),
        (
            "growth_waterings",
            "Waterings to maturity, captured when a seed is planted.",
            "28",
            SettingKind::Integer { min: 4, max: 365 },
        ),
        (
            "watering_interval_seconds",
            "Fixed UTC watering buckets; 86400 means once per UTC day. Captured at planting.",
            "86400",
            SettingKind::DurationSeconds {
                min: 60,
                max: 604800,
            },
        ),
        (
            "cooldown_seconds",
            "Per-person delay between botany commands.",
            "3",
            SettingKind::DurationSeconds { min: 0, max: 60 },
        ),
        (
            "garden_names",
            "Maximum names shown while browsing a channel garden.",
            "12",
            SettingKind::Integer { min: 1, max: 20 },
        ),
    ];
    Ok(serde_json::to_string(&SettingsManifest {
        version: SETTINGS_MANIFEST_VERSION,
        settings: specs
            .into_iter()
            .map(|(key, description, default, kind)| SettingSpec {
                key: key.into(),
                description: description.into(),
                default: default.into(),
                kind,
                scopes: vec![
                    SettingScope::Global,
                    SettingScope::Network,
                    SettingScope::Channel,
                ],
                applies_immediately: true,
            })
            .collect(),
    })?)
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    let milestones = [
        ("first_seed", "A Small Beginning", "planted", 1, false),
        ("care_7", "A Little Care", "watered", 7, false),
        ("care_28", "Time Well Spent", "watered", 28, false),
        ("care_100", "Quiet Company", "watered", 100, false),
        ("first_bloom", "Something Lovely", "matured", 1, false),
        ("first_harvest", "A New Beginning", "harvested", 1, false),
        ("harvest_10", "Ten Gentle Seasons", "harvested", 10, false),
        (
            "harvest_20",
            "A Familiar Patch of Sun",
            "harvested",
            20,
            false,
        ),
        (
            "harvest_50",
            "A Lifetime of Little Gardens",
            "harvested",
            50,
            false,
        ),
        ("first_photo", "Keep This Moment", "photos", 1, false),
        ("rare_bloom", "A Quiet Wonder", "rare", 1, true),
    ];
    Ok(serde_json::to_string(&AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        catalog_version: 1,
        stats: STATS
            .iter()
            .map(|id| AchievementStat {
                id: (*id).into(),
                description: format!("Botany {id} total"),
            })
            .collect(),
        achievements: milestones
            .into_iter()
            .map(|(id, name, stat, threshold, optional)| AchievementSpec {
                id: id.into(),
                name: name.into(),
                description: format!("Reach {threshold} {stat} in your garden, at your own pace."),
                stat: stat.into(),
                threshold,
                optional,
                secret: optional,
            })
            .collect(),
        prestige: vec![],
    })?)
}

pub(crate) fn book_key(server: &str, id: &str) -> String {
    format!("plant:{}:{}", encode(server), encode(id))
}

fn load(server: &str, id: &str) -> Result<GardenBook, Error> {
    let raw = kv_load(&book_key(server, id))?;
    if raw.is_empty() {
        Ok(GardenBook::default())
    } else {
        GardenBook::parse(&raw)
    }
}

fn profile(server: &str, nick: &str) -> Result<Option<Profile>, Error> {
    let raw = unsafe {
        profile_get(serde_json::to_string(&ProfileKey {
            server: server.into(),
            nick: nick.into(),
        })?)?
    };
    if raw.is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::from_str(&raw)?))
    }
}

struct Ctx<'a> {
    server: &'a str,
    target: &'a str,
    msg: &'a MessagePayload,
}
impl Ctx<'_> {
    fn say(&self, key: &str, default: &str, vars: &[(&str, &str)]) -> Result<(), Error> {
        let mut vars = vars.to_vec();
        vars.push(("honorific", honorific(self.msg)));
        reply(self.server, self.target, &themed(key, &[default], &vars)?)
    }
}

/// Each stage has its own full sentence: operators can rewrite any stage and every trait.
fn description(p: &Plant) -> Result<String, Error> {
    let (key, default) = match p.stage() {
        0 => ("botany.seed", "A seed rests in soft, damp earth. There is no hurry."),
        1 => ("botany.sprout", "The first {leaves} leaves are finding their way into the light."),
        2 => ("botany.bud", "The {leaves} leaves cradle a budding {flowers} flower."),
        3 => ("botany.fruit", "The {leaves} leaves shelter {flowers} flowers. Small, {fruit} fruit is beginning to grow."),
        4 => ("botany.full", "A {character} plant stretches into the light. The {leaves} leaves glint beneath bobbing {flowers} flowers, and {fruit} fruit hangs low on the branches."),
        _ => ("botany.ripe", "A {character} plant stretches into the light. The {leaves} leaves glint beneath bobbing {flowers} flowers. The {fruit} fruit appears ready to harvest! It can wait here as long as you like."),
    };
    themed(
        key,
        &[default],
        &[
            ("leaves", LEAVES[p.leaves]),
            ("flowers", FLOWERS[p.flowers]),
            ("fruit", FRUIT[p.fruit]),
            ("character", CHARACTER[p.character]),
        ],
    )
}

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    Ok(handle_message(&input)?)
}

fn handle_message(input: &str) -> Result<(), Error> {
    let envelope: EventEnvelope = serde_json::from_str(input)?;
    let Event::Message(msg) = envelope.event else {
        return Ok(());
    };
    if msg.is_action {
        return Ok(());
    }
    let mut words = msg.text.split_whitespace();
    if !words
        .next()
        .is_some_and(|word| word.eq_ignore_ascii_case("!botany"))
    {
        return Ok(());
    }
    let target = if msg.is_private {
        &msg.nick
    } else {
        &msg.target
    };
    let ctx = Ctx {
        server: &envelope.server,
        target,
        msg: &msg,
    };
    let channel = (!msg.is_private).then_some(msg.target.as_str());
    if !setting_bool("enabled", ctx.server, channel)? {
        return Ok(());
    }
    if msg.user_id.is_empty() || msg.user_id.starts_with("nick:") {
        return ctx.say("botany.identity", "I couldn't find your garden profile just now, {honorific}. Please try again in a moment.", &[]);
    }
    let args: Vec<_> = words.take(4).collect();
    let action = args.first().copied().unwrap_or("look").to_ascii_lowercase();
    let valid = match action.as_str() {
        "look" | "photo" => args.len() <= 2,
        "discard" => args.len() == 2,
        "plant" | "water" | "harvest" | "save" | "garden" => args.len() == 1,
        _ => false,
    };
    if !valid || msg.text.len() > 160 {
        return ctx.say("botany.usage", "A little garden, {honorific}: !botany plant | water | look [user] | harvest | save | photo [1-5] | discard <1-5> | garden.", &[]);
    }
    let now = timestamp()?;
    let cooldown_key = format!("cooldown:{}:{}", encode(ctx.server), encode(&msg.user_id));
    let seconds = setting_i64("cooldown_seconds", ctx.server, channel, 3).clamp(0, 60);
    if matches!(
        jeeves_guest::cooldown_check(&cooldown_key, now, seconds)?,
        jeeves_guest::Cooldown::Warn(_) | jeeves_guest::Cooldown::Quiet
    ) {
        return Ok(());
    }
    jeeves_guest::cooldown_start(&cooldown_key, now)?;
    if action == "garden" {
        return garden(&ctx);
    }
    if action == "look" {
        let (id, name) = if let Some(nick) = args.get(1) {
            if nick.len() > 64 {
                return ctx.say(
                    "botany.no_neighbor",
                    "I couldn't find a plant for that gardener, {honorific}.",
                    &[],
                );
            }
            let Some(p) = profile(ctx.server, nick)? else {
                return ctx.say(
                    "botany.no_neighbor",
                    "I couldn't find a plant for that gardener, {honorific}.",
                    &[],
                );
            };
            (p.id, no_highlight(&p.nick))
        } else {
            (msg.user_id.clone(), no_highlight(&msg.nick))
        };
        if id.is_empty() {
            return Ok(());
        }
        let book = load(ctx.server, &id)?;
        return if let Some(p) = book.plant {
            ctx.say(
                "botany.look",
                "{gardener}'s little patch: {plant}",
                &[("gardener", &name), ("plant", &description(&p)?)],
            )
        } else {
            ctx.say("botany.empty_patch", "A soft patch of earth waits for {gardener}. A free seed is ready whenever they are: !botany plant.", &[("gardener", &name)])
        };
    }
    let mut book = load(ctx.server, &msg.user_id)?;
    let before = book.totals;
    match action.as_str() {
        "plant" => {
            if book.plant.is_some() {
                return ctx.say("botany.already_planted", "Your plant is already here, {honorific}, taking its time. !botany look will show you how it's doing.", &[]);
            }
            book.plant = Some(Plant::new(
                setting_i64("growth_waterings", ctx.server, channel, 28).clamp(4, 365) as u32,
                setting_i64("watering_interval_seconds", ctx.server, channel, 86400)
                    .clamp(60, 604800),
                &mut Entropy::default(),
            )?);
            book.record(0);
            commit(&ctx, &book, before)?;
            ctx.say("botany.planted", "You tuck your free seed into the soft earth, {honorific}. A little beginning. !botany water whenever you're ready; missed days change nothing.", &[])
        }
        "water" => {
            let Some(p) = &mut book.plant else {
                return no_plant(&ctx);
            };
            if !p.water(now) {
                return ctx.say(
                    "botany.resting",
                    "Your plant has everything it needs for now, {honorific}. {plant}",
                    &[("plant", &description(p)?)],
                );
            }
            let mature = p.stage() == 5;
            let rare = p.rare();
            let text = description(p)?;
            book.record(1);
            if mature {
                book.record(2);
                if rare {
                    book.record(5);
                }
            }
            commit(&ctx, &book, before)?;
            ctx.say(
                "botany.watered",
                "You give your plant a little water, {honorific}. {plant}",
                &[("plant", &text)],
            )
        }
        "harvest" => {
            let Some(p) = &book.plant else {
                return no_plant(&ctx);
            };
            if p.stage() != 5 {
                return ctx.say(
                    "botany.not_ripe",
                    "The fruit is still taking its time, {honorific}. There is no hurry.",
                    &[],
                );
            }
            let fruit = FRUIT[p.fruit];
            book.plant = None;
            book.record(3);
            commit(&ctx, &book, before)?;
            ctx.say("botany.harvested", "You gather the {fruit} fruit, {honorific}. Your plant becomes a swirl of petals and soft light, drifting into the garden. A new seed rests in your palm. Your photos keep its memory.", &[("fruit", fruit)])
        }
        "save" => {
            let Some(p) = &book.plant else {
                return no_plant(&ctx);
            };
            let Some(slot) = book.photos.iter().position(Option::is_none) else {
                return ctx.say("botany.album_full", "All five photos have a home, {honorific}. !botany discard <1-5> can make room for another memory.", &[]);
            };
            book.photos[slot] = Some(Photo {
                plant: p.clone(),
                taken_at: now,
            });
            book.record(4);
            commit(&ctx, &book, before)?;
            ctx.say("botany.saved", "A soft click, and this moment is yours to keep, {honorific}. Your plant's photo is in slot {slot}.", &[("slot", &(slot+1).to_string())])
        }
        "photo" | "discard" => {
            if args.len() == 1 {
                let slots = book
                    .photos
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| p.is_some())
                    .map(|(i, _)| (i + 1).to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                return if slots.is_empty() {
                    ctx.say("botany.album_empty", "Your album is waiting for its first memory, {honorific}. !botany save takes a photo of your plant.", &[])
                } else {
                    ctx.say("botany.album", "Your plant photos are in slots {slots}, {honorific}. !botany photo <1-5> opens one.", &[("slots", &slots)])
                };
            }
            let Some(slot) = args[1]
                .parse::<usize>()
                .ok()
                .filter(|n| (1..=5).contains(n))
                .map(|n| n - 1)
            else {
                return ctx.say(
                    "botany.slot",
                    "Choose a photo slot from 1 to 5, {honorific}.",
                    &[],
                );
            };
            let Some(photo) = &book.photos[slot] else {
                return ctx.say(
                    "botany.photo_empty",
                    "That photo slot is a little space waiting for a memory, {honorific}.",
                    &[],
                );
            };
            if action == "photo" {
                ctx.say(
                    "botany.photo",
                    "Photo {slot}: {plant}",
                    &[
                        ("slot", &(slot + 1).to_string()),
                        ("plant", &description(&photo.plant)?),
                    ],
                )
            } else {
                book.photos[slot] = None;
                commit(&ctx, &book, before)?;
                ctx.say("botany.discarded", "You make a little room in your album, {honorific}. Slot {slot} is ready for a new memory.", &[("slot", &(slot+1).to_string())])
            }
        }
        _ => Ok(()),
    }
}

fn no_plant(ctx: &Ctx) -> Result<(), Error> {
    ctx.say(
        "botany.no_plant",
        "A free seed is waiting for you, {honorific}. !botany plant will give it a home.",
        &[],
    )
}

fn commit(ctx: &Ctx, book: &GardenBook, before: [u64; 6]) -> Result<(), Error> {
    kv_save(
        &book_key(ctx.server, &ctx.msg.user_id),
        &serde_json::to_string(book)?,
    )?;
    let increments: Vec<_> = STATS
        .iter()
        .enumerate()
        .filter_map(|(i, stat)| {
            let amount = book.totals[i].saturating_sub(before[i]);
            (amount > 0).then(|| StatIncrement {
                stat: (*stat).into(),
                amount,
            })
        })
        .collect();
    if !increments.is_empty() {
        unsafe {
            award_stats(serde_json::to_string(&AwardStatsRequest {
                server: ctx.server.into(),
                profile_id: ctx.msg.user_id.clone(),
                display_name: ctx.msg.nick.clone(),
                target: ctx.target.into(),
                increments,
                deduplication_id: Some(format!(
                    "botany:{}:{}",
                    ctx.msg.user_id,
                    serde_json::to_string(&book.totals)?
                )),
            })?)?;
        }
    }
    Ok(())
}

fn garden(ctx: &Ctx) -> Result<(), Error> {
    if ctx.msg.is_private {
        return ctx.say("botany.garden_channel", "Visit a channel to wander its shared garden, {honorific}. Your own plant is always here with !botany look.", &[]);
    }
    let raw = unsafe {
        channel_members(serde_json::to_string(&Channel {
            server: ctx.server.into(),
            channel: ctx.target.into(),
        })?)?
    };
    let members: Vec<String> = serde_json::from_str(&raw)?;
    let mut seen = BTreeSet::new();
    let mut names = Vec::new();
    let mut features = BTreeSet::new();
    for nick in members.iter().take(500) {
        let Some(p) = profile(ctx.server, nick)? else {
            continue;
        };
        if p.id.is_empty() || !seen.insert(p.id.clone()) {
            continue;
        }
        if let Some(plant) = load(ctx.server, &p.id)?.plant {
            names.push(no_highlight(nick));
            for feature in visible_features(&plant)? {
                features.insert(feature);
            }
        }
    }
    if names.is_empty() {
        return ctx.say("botany.garden_empty", "A quiet patch of earth waits in the sunlight, {honorific}. The garden can begin with a single seed: !botany plant.", &[]);
    }
    let mut features: Vec<_> = features.into_iter().collect();
    let mut entropy = Entropy::default();
    for i in 0..features.len().min(2) {
        let j = i + entropy.below((features.len() - i) as u32)? as usize;
        features.swap(i, j);
    }
    let scenery = if features.is_empty() {
        themed(
            "botany.garden_seeds",
            &["Small seeds rest beneath the earth. Something lovely is taking its time."],
            &[],
        )?
    } else {
        let glimpses = features
            .iter()
            .take(2)
            .cloned()
            .collect::<Vec<_>>()
            .join("; ");
        themed(
            "botany.garden_glimpses",
            &["Among the little patches of green, you notice {features}."],
            &[("features", &glimpses)],
        )?
    };
    names.sort();
    let limit = setting_i64("garden_names", ctx.server, Some(ctx.target), 12).clamp(1, 20) as usize;
    let remaining = names.len().saturating_sub(limit);
    let names = names.into_iter().take(limit).collect::<Vec<_>>().join(", ");
    if remaining > 0 {
        ctx.say("botany.garden_more", "A quiet garden welcomes you. {scenery} Tending plants: {gardeners}, and {remaining} more.", &[("scenery", &scenery), ("gardeners", &names), ("remaining", &remaining.to_string())])
    } else {
        ctx.say(
            "botany.garden",
            "A quiet garden welcomes you. {scenery} Tending plants: {gardeners}.",
            &[("scenery", &scenery), ("gardeners", &names)],
        )
    }
}

fn visible_features(p: &Plant) -> Result<Vec<String>, Error> {
    let mut features = Vec::new();
    if p.stage() >= 1 {
        features.push(themed(
            "botany.feature_leaves",
            &["{leaves} leaves catching the light"],
            &[("leaves", LEAVES[p.leaves])],
        )?);
    }
    if p.stage() >= 2 {
        features.push(themed(
            "botany.feature_flowers",
            &["{flowers} flowers nodding in the breeze"],
            &[("flowers", FLOWERS[p.flowers])],
        )?);
    }
    if p.stage() >= 3 {
        features.push(themed(
            "botany.feature_fruit",
            &["{fruit} fruit tucked among the branches"],
            &[("fruit", FRUIT[p.fruit])],
        )?);
    }
    Ok(features)
}
