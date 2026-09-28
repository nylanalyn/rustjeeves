//! Achievement collections, leaderboards, rarity, and an opt-in weekly digest over the
//! host-owned achievement store.

use extism_pdk::*;
use jeeves_abi::{
    AchievementBoardRequest, AchievementBoardResponse, AchievementModuleProgress,
    AchievementOptOutRequest, AchievementProfileSummary, AchievementPublicRequest,
    AchievementsGetRequest, CommandManifest, CommandSpec, CosmeticsWornRequest, Event,
    EventEnvelope, MessagePayload, Profile, ProfileKey, Role, ScheduleCancel, ScheduleList,
    ScheduleSet, ScheduledJob, SendMessage, SettingGet, SettingKind, SettingScope, SettingSpec,
    SettingsManifest, ThemeReq, WornCosmetics, COMMAND_MANIFEST_VERSION, SETTINGS_MANIFEST_VERSION,
};
use serde::{Deserialize, Serialize};

const TOP_SIZE: u32 = 5;
const RARE_SIZE: u32 = 5;
const DIGEST_PEOPLE: usize = 6;
const WEEK: i64 = 7 * 86_400;
const WEEKDAYS: [&str; 7] = [
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
];

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn theme(input: String) -> String;
    fn profile_get(input: String) -> String;
    fn achievements_get(input: String) -> String;
    fn achievement_optout(input: String) -> String;
    fn achievement_public(input: String) -> String;
    fn achievement_board(input: String) -> String;
    fn cosmetics_worn(input: String) -> String;
    fn schedule_set(input: String) -> String;
    fn schedule_cancel(input: String) -> String;
    fn schedule_list(input: String) -> String;
    fn setting_get(input: String) -> String;
    fn now(input: String) -> String;
}

#[plugin_fn]
pub fn settings(_: String) -> FnResult<String> {
    let scopes = vec![
        SettingScope::Global,
        SettingScope::Network,
        SettingScope::Channel,
    ];
    Ok(serde_json::to_string(&SettingsManifest {
        version: SETTINGS_MANIFEST_VERSION,
        settings: vec![
            SettingSpec {
                key: "digest_weekday".into(),
                description: "Day the weekly digest is posted, where an admin turned it on.".into(),
                default: "sunday".into(),
                kind: SettingKind::Choice {
                    options: WEEKDAYS.iter().map(|day| day.to_string()).collect(),
                },
                scopes: scopes.clone(),
                applies_immediately: true,
            },
            SettingSpec {
                key: "digest_hour_utc".into(),
                description: "Hour (UTC) the weekly digest is posted.".into(),
                default: "18".into(),
                kind: SettingKind::Integer { min: 0, max: 23 },
                scopes,
                applies_immediately: true,
            },
        ],
    })?)
}

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![CommandSpec {
            name: "achievements".into(),
            aliases: vec!["ach".into()],
            description: "Show achievement collections, progress, leaders, and rarities.".into(),
            usage: "!achievements [nick | list [module] [nick] | top [module] | rare [module] | \
                    digest [on|off] | optout confirm | optin | publish | hide]"
                .into(),
            ..Default::default()
        }],
    })?)
}

fn themed(key: &str, default: &str, vars: &[(&str, &str)]) -> Result<String, Error> {
    Ok(unsafe {
        theme(serde_json::to_string(&ThemeReq {
            key: key.into(),
            default: vec![default.into()],
            vars: vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        })?)?
    })
}

fn reply(server: &str, target: &str, text: String) -> Result<(), Error> {
    unsafe {
        send_message(serde_json::to_string(&SendMessage {
            server: server.into(),
            target: target.into(),
            text,
        })?)?;
    }
    Ok(())
}

fn prestige_name(name: &str, rank: u64) -> String {
    if rank <= 1 {
        return name.into();
    }
    let mut value = rank.min(3_999);
    let mut numeral = String::new();
    for (number, text) in [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ] {
        while value >= number {
            value -= number;
            numeral.push_str(text);
        }
    }
    format!("{name} {numeral}")
}

fn chunks(parts: Vec<String>, max_chars: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for part in parts {
        let separator = if current.is_empty() { "" } else { "; " };
        if !current.is_empty()
            && current.chars().count() + separator.len() + part.chars().count() > max_chars
        {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push_str("; ");
        }
        current.extend(part.chars().take(max_chars));
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Handle `!achievements optout` / `!achievements optin`. Acts on the caller's own profile only.
fn handle_opt_out(
    server: &str,
    msg: &jeeves_abi::MessagePayload,
    subcommand: &str,
) -> Result<(), Error> {
    let dest = if msg.is_private {
        msg.nick.as_str()
    } else {
        msg.target.as_str()
    };
    let caller: &str = if msg.display.is_empty() {
        msg.nick.as_str()
    } else {
        msg.display.as_str()
    };
    let raw = unsafe {
        profile_get(serde_json::to_string(&ProfileKey {
            server: server.into(),
            nick: msg.nick.clone(),
        })?)?
    };
    if raw.is_empty() {
        return reply(
            server,
            dest,
            themed(
                "achievements.unknown",
                "No profile is known for {user}.",
                &[("user", caller)],
            )?,
        );
    }
    let profile: Profile = serde_json::from_str(&raw)?;
    let opt_out = subcommand == "optout";
    unsafe {
        achievement_optout(serde_json::to_string(&AchievementOptOutRequest {
            server: server.into(),
            profile_id: profile.id,
            opt_out,
        })?)?
    };
    let (key, default) = if opt_out {
        (
            "achievements.optout_confirm",
            "You've opted out of achievements, {user}. Your existing progress has been cleared. Use !achievements optin to resume earning from zero.",
        )
    } else {
        (
            "achievements.optin_confirm",
            "Welcome back to achievements, {user}. You'll start earning from zero.",
        )
    };
    reply(server, dest, themed(key, default, &[("user", caller)])?)
}

fn handle_public(
    server: &str,
    msg: &jeeves_abi::MessagePayload,
    publish: bool,
) -> Result<(), Error> {
    let dest = if msg.is_private {
        &msg.nick
    } else {
        &msg.target
    };
    let caller = if msg.display.is_empty() {
        msg.nick.as_str()
    } else {
        msg.display.as_str()
    };
    let raw = unsafe {
        profile_get(serde_json::to_string(&ProfileKey {
            server: server.into(),
            nick: msg.nick.clone(),
        })?)?
    };
    if raw.is_empty() {
        return reply(
            server,
            dest,
            themed(
                "achievements.unknown",
                "No profile is known for {user}.",
                &[("user", caller)],
            )?,
        );
    }
    let profile: Profile = serde_json::from_str(&raw)?;
    if publish && profile.achievements_opt_out == Some(true) {
        return reply(
            server,
            dest,
            themed(
                "achievements.publish_opted_out",
                "Opt back into achievements before publishing a collection, {user}.",
                &[("user", caller)],
            )?,
        );
    }
    unsafe {
        achievement_public(serde_json::to_string(&AchievementPublicRequest {
            server: server.into(),
            profile_id: profile.id,
            public: publish,
        })?)?
    };
    let (key, default) = if publish {
        (
            "achievements.publish_confirm",
            "Your achievement collection may now appear in the public gallery, {user}. Use !achievements hide to remove it.",
        )
    } else {
        (
            "achievements.hide_confirm",
            "Your achievement collection is now hidden from the public gallery, {user}.",
        )
    };
    reply(server, dest, themed(key, default, &[("user", caller)])?)
}

// ── shared helpers ──────────────────────────────────────────────────────────

fn destination(msg: &MessagePayload) -> &str {
    if msg.is_private {
        &msg.nick
    } else {
        &msg.target
    }
}

fn caller(msg: &MessagePayload) -> &str {
    if msg.display.is_empty() {
        &msg.nick
    } else {
        &msg.display
    }
}

fn get_profile(server: &str, nick: &str) -> Result<Option<Profile>, Error> {
    let raw = unsafe {
        profile_get(serde_json::to_string(&ProfileKey {
            server: server.into(),
            nick: nick.into(),
        })?)?
    };
    Ok(if raw.is_empty() {
        None
    } else {
        Some(serde_json::from_str(&raw)?)
    })
}

fn board(request: AchievementBoardRequest) -> Result<AchievementBoardResponse, Error> {
    Ok(serde_json::from_str(&unsafe {
        achievement_board(serde_json::to_string(&request)?)?
    })?)
}

fn worn(server: &str, profile_ids: Vec<String>) -> Result<Vec<WornCosmetics>, Error> {
    if profile_ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(serde_json::from_str(&unsafe {
        cosmetics_worn(serde_json::to_string(&CosmeticsWornRequest {
            server: server.into(),
            profile_ids,
        })?)?
    })?)
}

/// "🦉 alice" when they wear a badge, otherwise the name as given.
fn badged(name: &str, badge: Option<&str>) -> String {
    match badge {
        Some(badge) => format!("{badge} {name}"),
        None => name.into(),
    }
}

/// Break every word of a name with a zero-width space so listing it doesn't highlight anyone.
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

fn timestamp() -> Result<i64, Error> {
    Ok(unsafe { now(String::new())? }.parse().unwrap_or(0))
}

fn setting(server: &str, channel: &str, key: &str) -> Result<String, Error> {
    Ok(unsafe {
        setting_get(serde_json::to_string(&SettingGet {
            key: key.into(),
            server: Some(server.into()),
            channel: Some(channel.into()),
        })?)?
    })
}

fn find_module<'a>(
    summary: &'a AchievementProfileSummary,
    name: &str,
) -> Option<&'a AchievementModuleProgress> {
    summary
        .modules
        .iter()
        .find(|entry| entry.module.eq_ignore_ascii_case(name))
}

fn profile_summary(server: &str, profile_id: &str) -> Result<AchievementProfileSummary, Error> {
    let request = AchievementsGetRequest::Profile {
        server: server.into(),
        profile_id: profile_id.into(),
    };
    Ok(serde_json::from_str(&unsafe {
        achievements_get(serde_json::to_string(&request)?)?
    })?)
}

// ── summary and lists ───────────────────────────────────────────────────────

fn summary_reply(server: &str, msg: &MessagePayload, nick: &str) -> Result<(), Error> {
    let dest = destination(msg);
    let Some(profile) = get_profile(server, nick)? else {
        return reply(
            server,
            dest,
            themed(
                "achievements.unknown",
                "No profile is known for {user}.",
                &[("user", nick)],
            )?,
        );
    };
    let summary = profile_summary(server, &profile.id)?;
    let badge = worn(server, vec![profile.id.clone()])?
        .into_iter()
        .next()
        .and_then(|worn| worn.badge);
    let mut text = themed(
        "achievements.summary",
        "{user}: {earned}/{available} collected.",
        &[
            ("user", &badged(nick, badge.as_deref())),
            ("earned", &summary.earned.to_string()),
            ("available", &summary.available.to_string()),
        ],
    )?;
    let recent = summary
        .recent
        .iter()
        .take(3)
        .map(|unlock| unlock.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let closest = summary
        .closest
        .iter()
        .take(3)
        .map(|item| format!("{} {}/{}", item.name, item.current, item.threshold))
        .collect::<Vec<_>>()
        .join("; ");
    let prestige = summary
        .modules
        .iter()
        .flat_map(|module| module.prestige.iter())
        .take(3)
        .map(|rank| prestige_name(&rank.name, rank.rank))
        .collect::<Vec<_>>()
        .join(", ");
    if recent.is_empty() && closest.is_empty() && prestige.is_empty() {
        text.push_str(&themed(
            "achievements.summary_empty",
            " Nothing yet; almost anything you do here can start a collection.",
            &[],
        )?);
    }
    for (key, default, value) in [
        ("achievements.summary_recent", " Recent: {items}.", &recent),
        (
            "achievements.summary_closest",
            " Closest: {items}.",
            &closest,
        ),
        (
            "achievements.summary_prestige",
            " Prestige: {items}.",
            &prestige,
        ),
    ] {
        if !value.is_empty() {
            text.push_str(&themed(key, default, &[("items", value)])?);
        }
    }
    reply(server, dest, text)
}

fn list_reply(server: &str, msg: &MessagePayload, args: &[&str]) -> Result<(), Error> {
    let dest = destination(msg);
    // `list`, `list fishing`, `list alice`, `list fishing alice`, `list alice fishing`.
    let own = get_profile(server, &msg.nick)?;
    let own_summary = match &own {
        Some(profile) => Some(profile_summary(server, &profile.id)?),
        None => None,
    };
    let is_module = |word: &str| {
        own_summary
            .as_ref()
            .is_some_and(|summary| find_module(summary, word).is_some())
    };
    let (module, nick) = match args {
        [] => (None, msg.nick.as_str()),
        [one] if is_module(one) => (Some(*one), msg.nick.as_str()),
        [one] => (None, *one),
        [first, second, ..] if is_module(first) => (Some(*first), *second),
        [first, second, ..] => (Some(*second), *first),
    };
    let Some(profile) = get_profile(server, nick)? else {
        return reply(
            server,
            dest,
            themed(
                "achievements.unknown",
                "No profile is known for {user}.",
                &[("user", nick)],
            )?,
        );
    };
    let summary = profile_summary(server, &profile.id)?;
    let parts = if let Some(selected) = module {
        let Some(entry) = find_module(&summary, selected) else {
            return reply(
                server,
                dest,
                themed(
                    "achievements.unknown_module",
                    "I know of no achievements for '{module}', {user}. !achievements list shows them all.",
                    &[("module", selected), ("user", caller(msg))],
                )?,
            );
        };
        std::iter::once(format!(
            "{} {}/{}",
            entry.module, entry.earned, entry.available
        ))
        .chain(entry.achievements.iter().map(|item| {
            if item.earned {
                format!("✓ {}", item.name)
            } else if item.secret {
                "? Undiscovered secret".into()
            } else {
                format!("· {} {}/{}", item.name, item.current, item.threshold)
            }
        }))
        .chain(
            entry
                .prestige
                .iter()
                .map(|rank| format!("★ {}", prestige_name(&rank.name, rank.rank))),
        )
        .collect::<Vec<_>>()
    } else {
        overview_parts(&summary)
    };
    for modules in chunks(parts, 330) {
        reply(
            server,
            dest,
            themed(
                "achievements.list",
                "{user}: {modules}",
                &[("user", nick), ("modules", &modules)],
            )?,
        )?;
    }
    Ok(())
}

/// Modules with progress first (most earned first), untouched ones folded into a count.
fn overview_parts(summary: &AchievementProfileSummary) -> Vec<String> {
    let mut started = summary
        .modules
        .iter()
        .filter(|module| module.earned > 0 || !module.prestige.is_empty())
        .collect::<Vec<_>>();
    started.sort_by(|left, right| {
        right
            .earned
            .cmp(&left.earned)
            .then_with(|| left.module.cmp(&right.module))
    });
    let untouched = summary.modules.len() - started.len();
    let mut parts = started
        .iter()
        .map(|module| format!("{} {}/{}", module.module, module.earned, module.available))
        .collect::<Vec<_>>();
    if untouched > 0 {
        parts.push(format!("{untouched} more not yet started"));
    }
    parts
}

// ── leaderboards ────────────────────────────────────────────────────────────

fn top_reply(server: &str, msg: &MessagePayload, module: Option<&str>) -> Result<(), Error> {
    let dest = destination(msg);
    let response = board(AchievementBoardRequest::Top {
        server: server.into(),
        module: module.map(str::to_string),
        limit: TOP_SIZE,
    })?;
    if let Some(unknown) = module.filter(|_| response.available == 0) {
        return unknown_module(server, msg, unknown);
    }
    if response.top.is_empty() {
        return reply(
            server,
            dest,
            themed(
                "achievements.top_empty",
                "Nobody has earned anything here yet, {user}. The field is wide open.",
                &[("user", caller(msg))],
            )?,
        );
    }
    let badges = worn(
        server,
        response
            .top
            .iter()
            .map(|leader| leader.profile_id.clone())
            .collect(),
    )?;
    let leaders = response
        .top
        .iter()
        .enumerate()
        .map(|(index, leader)| {
            let badge = badges
                .iter()
                .find(|worn| worn.profile_id == leader.profile_id)
                .and_then(|worn| worn.badge.as_deref());
            format!(
                "{}. {} {}",
                index + 1,
                badged(&no_highlight(&leader.nick), badge),
                leader.earned
            )
        })
        .collect::<Vec<_>>()
        .join(" · ");
    let (key, default) = if module.is_some() {
        (
            "achievements.top_module",
            "Most accomplished in {module} (of {available}): {leaders}",
        )
    } else {
        (
            "achievements.top",
            "Most accomplished (of {available}): {leaders}",
        )
    };
    reply(
        server,
        dest,
        themed(
            key,
            default,
            &[
                ("module", &module.unwrap_or("").to_ascii_lowercase()),
                ("available", &response.available.to_string()),
                ("leaders", &leaders),
                ("user", caller(msg)),
            ],
        )?,
    )
}

fn rare_reply(server: &str, msg: &MessagePayload, module: Option<&str>) -> Result<(), Error> {
    let dest = destination(msg);
    let response = board(AchievementBoardRequest::Rare {
        server: server.into(),
        module: module.map(str::to_string),
        limit: RARE_SIZE,
    })?;
    if let Some(unknown) = module.filter(|_| response.available == 0) {
        return unknown_module(server, msg, unknown);
    }
    if response.rare.is_empty() {
        return reply(
            server,
            dest,
            themed(
                "achievements.rare_empty",
                "Nothing has been earned yet, {user}, so everything is equally rare.",
                &[("user", caller(msg))],
            )?,
        );
    }
    let items = response
        .rare
        .iter()
        .map(|item| {
            let name = if item.secret {
                format!("a {} secret", item.module)
            } else {
                format!("{} ({})", item.name, item.module)
            };
            let holders = if item.holders == 1 {
                "1 holder".to_string()
            } else {
                format!("{} holders", item.holders)
            };
            format!("{name}: {holders}")
        })
        .collect::<Vec<_>>()
        .join(" · ");
    reply(
        server,
        dest,
        themed(
            "achievements.rare",
            "Rarest achievements among {collectors}: {items}",
            &[
                (
                    "collectors",
                    &if response.collectors == 1 {
                        "1 collector".to_string()
                    } else {
                        format!("{} collectors", response.collectors)
                    },
                ),
                ("items", &items),
                ("user", caller(msg)),
            ],
        )?,
    )
}

fn unknown_module(server: &str, msg: &MessagePayload, module: &str) -> Result<(), Error> {
    reply(
        server,
        destination(msg),
        themed(
            "achievements.unknown_module",
            "I know of no achievements for '{module}', {user}. !achievements list shows them all.",
            &[("module", module), ("user", caller(msg))],
        )?,
    )
}

// ── weekly digest ───────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
struct DigestPlan {
    /// Unlocks at or after this instant belong to the next digest.
    since: i64,
}

fn encode(value: &str) -> String {
    value.bytes().map(|byte| format!("{byte:02x}")).collect()
}

fn digest_id(server: &str, channel: &str) -> String {
    format!(
        "digest:{}:{}",
        encode(server),
        encode(&channel.to_ascii_lowercase())
    )
}

/// The first `weekday` `hour`:00 UTC strictly after `now`. Monday is 0; 1970-01-01 was a
/// Thursday (3).
fn next_digest_at(now: i64, weekday: usize, hour: i64) -> i64 {
    let day = now.div_euclid(86_400);
    let today = (day + 3).rem_euclid(7) as usize;
    let mut offset = (weekday + 7 - today) % 7;
    loop {
        let at = (day + offset as i64) * 86_400 + hour * 3_600;
        if at > now {
            return at;
        }
        offset += 7;
    }
}

fn digest_schedule(server: &str, channel: &str) -> Result<(usize, i64), Error> {
    let weekday = setting(server, channel, "digest_weekday")?;
    let weekday = WEEKDAYS.iter().position(|day| *day == weekday).unwrap_or(6);
    let hour = setting(server, channel, "digest_hour_utc")?
        .parse::<i64>()
        .ok()
        .filter(|hour| (0..24).contains(hour))
        .unwrap_or(18);
    Ok((weekday, hour))
}

fn schedule_digest(server: &str, channel: &str, since: i64) -> Result<i64, Error> {
    let (weekday, hour) = digest_schedule(server, channel)?;
    let due_at = next_digest_at(timestamp()?, weekday, hour);
    unsafe {
        schedule_set(serde_json::to_string(&ScheduleSet {
            id: digest_id(server, channel),
            server: server.into(),
            channel: channel.into(),
            owner_profile_id: None,
            due_at,
            payload: serde_json::to_string(&DigestPlan { since })?,
        })?)?;
    }
    Ok(due_at)
}

fn digest_command(server: &str, msg: &MessagePayload, choice: Option<&str>) -> Result<(), Error> {
    let dest = destination(msg);
    let user = caller(msg);
    if msg.is_private {
        return reply(
            server,
            dest,
            themed(
                "achievements.digest_channel_only",
                "The weekly digest is set up in the channel that should receive it, {user}.",
                &[("user", user)],
            )?,
        );
    }
    let id = digest_id(server, &msg.target);
    match choice.map(str::to_ascii_lowercase).as_deref() {
        None => {
            let raw = unsafe {
                schedule_list(serde_json::to_string(&ScheduleList {
                    server: Some(server.into()),
                    channel: None,
                })?)?
            };
            let jobs: Vec<ScheduledJob> = serde_json::from_str(&raw).unwrap_or_default();
            let text = match jobs.iter().find(|job| job.id == id) {
                Some(job) => themed(
                    "achievements.digest_status_on",
                    "A weekly achievement digest is posted here; the next is due in {days} day(s). An admin can stop it with !achievements digest off.",
                    &[
                        ("days", &((job.due_at - timestamp()?).max(0) / 86_400 + 1).to_string()),
                        ("user", user),
                    ],
                )?,
                None => themed(
                    "achievements.digest_status_off",
                    "No weekly achievement digest is posted here, {user}. An admin can start one with !achievements digest on.",
                    &[("user", user)],
                )?,
            };
            reply(server, dest, text)
        }
        Some(state @ ("on" | "off")) => {
            if !msg.role.is_some_and(|role| role.satisfies(Role::Admin)) {
                return reply(
                    server,
                    dest,
                    themed(
                        "achievements.digest_admin_only",
                        "Only an admin may change the weekly digest, {user}.",
                        &[("user", user)],
                    )?,
                );
            }
            if state == "off" {
                unsafe { schedule_cancel(serde_json::to_string(&ScheduleCancel { id })?)? };
                return reply(
                    server,
                    dest,
                    themed(
                        "achievements.digest_off",
                        "Very good, {user}; no more weekly digests here.",
                        &[("user", user)],
                    )?,
                );
            }
            let (weekday, hour) = digest_schedule(server, &msg.target)?;
            schedule_digest(server, &msg.target, timestamp()?)?;
            let mut day = WEEKDAYS[weekday].to_string();
            day[..1].make_ascii_uppercase();
            reply(
                server,
                dest,
                themed(
                    "achievements.digest_on",
                    "Very good, {user}: a digest of the week's achievements every {day} at {hour}:00 UTC.",
                    &[("day", &day), ("hour", &format!("{hour:02}")), ("user", user)],
                )?,
            )
        }
        Some(_) => reply(
            server,
            dest,
            themed(
                "achievements.digest_usage",
                "Use !achievements digest on, off, or with nothing to see whether one is set, {user}.",
                &[("user", user)],
            )?,
        ),
    }
}

/// "a​lice 3 (Quotable, Walking Dictionary, …)" per person, busiest first.
fn digest_highlights(response: &AchievementBoardResponse) -> (usize, String) {
    let mut people = Vec::<(&str, Vec<&str>)>::new();
    for unlock in &response.unlocks {
        let name = if unlock.secret {
            "a secret"
        } else {
            &unlock.name
        };
        match people.iter_mut().find(|(nick, _)| *nick == unlock.nick) {
            Some((_, names)) => names.push(name),
            None => people.push((&unlock.nick, vec![name])),
        }
    }
    let total_people = people.len();
    people.sort_by_key(|(_, names)| std::cmp::Reverse(names.len()));
    let mut parts = people
        .iter()
        .take(DIGEST_PEOPLE)
        .map(|(nick, names)| {
            let shown = names.iter().take(2).copied().collect::<Vec<_>>().join(", ");
            let more = if names.len() > 2 {
                format!(", +{}", names.len() - 2)
            } else {
                String::new()
            };
            format!("{} {} ({shown}{more})", no_highlight(nick), names.len())
        })
        .collect::<Vec<_>>();
    if total_people > DIGEST_PEOPLE {
        parts.push(format!("and {} more", total_people - DIGEST_PEOPLE));
    }
    (total_people, parts.join(" · "))
}

#[plugin_fn]
pub fn on_event(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let Event::Timer {
        id,
        channel,
        payload,
        ..
    } = env.event
    else {
        return Ok(());
    };
    if !id.starts_with("digest:") {
        return Ok(());
    }
    let server = env.server;
    let plan: DigestPlan = serde_json::from_str(&payload)?;
    let now = timestamp()?;
    let since = plan.since.max(now - 2 * WEEK);
    let response = board(AchievementBoardRequest::Unlocks {
        server: server.clone(),
        since,
        limit: 200,
    })?;
    // A quiet week posts nothing rather than an empty report.
    if !response.unlocks.is_empty() {
        let (people, highlights) = digest_highlights(&response);
        reply(
            &server,
            &channel,
            themed(
                "achievements.digest",
                "This week's achievements: {count} earned by {people}. {highlights}",
                &[
                    ("count", &response.unlocks.len().to_string()),
                    (
                        "people",
                        &if people == 1 {
                            "1 person".to_string()
                        } else {
                            format!("{people} people")
                        },
                    ),
                    ("highlights", &highlights),
                ],
            )?,
        )?;
    }
    schedule_digest(&server, &channel, now)?;
    Ok(())
}

// ── dispatch ────────────────────────────────────────────────────────────────

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    let mut words = msg.text.split_whitespace();
    if words.next() != Some("!achievements") {
        return Ok(());
    }
    let args = words.collect::<Vec<_>>();
    let first = args.first().map(|word| word.to_ascii_lowercase());
    let server = env.server.as_str();
    match first.as_deref() {
        None => summary_reply(server, &msg, &msg.nick)?,
        Some("optout")
            if args.get(1).map(|word| word.to_ascii_lowercase()).as_deref() != Some("confirm") =>
        {
            // Opting out erases all progress, so it needs an explicit second word.
            reply(
                server,
                destination(&msg),
                themed(
                    "achievements.optout_warning",
                    "Opting out permanently erases all of your achievement progress, {user}. If you are certain, use !achievements optout confirm.",
                    &[("user", caller(&msg))],
                )?,
            )?;
        }
        Some(word @ ("optout" | "optin")) => handle_opt_out(server, &msg, word)?,
        Some(word @ ("publish" | "hide")) => handle_public(server, &msg, word == "publish")?,
        Some("list") => list_reply(server, &msg, &args[1..])?,
        Some("top") => top_reply(server, &msg, args.get(1).copied())?,
        Some("rare") => rare_reply(server, &msg, args.get(1).copied())?,
        Some("digest") => digest_command(server, &msg, args.get(1).copied())?,
        Some(_) => summary_reply(server, &msg, args[0])?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_chunks_are_bounded_without_dropping_entries() {
        let lines = chunks(vec!["one".into(), "two".into(), "three".into()], 8);
        assert_eq!(lines, ["one; two", "three"]);
        assert!(lines.iter().all(|line| line.chars().count() <= 8));
    }

    #[test]
    fn digests_land_on_the_next_configured_slot() {
        // 2026-09-28 (a Monday) 12:00 UTC.
        let monday_noon = 1_790_596_800;
        assert_eq!((monday_noon / 86_400 + 3) % 7, 0, "fixture is a Monday");
        // Sunday 18:00 is six days and six hours later.
        assert_eq!(
            next_digest_at(monday_noon, 6, 18),
            monday_noon + 6 * 86_400 + 6 * 3_600
        );
        // Monday 09:00 has passed today, so it is next week's.
        assert_eq!(
            next_digest_at(monday_noon, 0, 9),
            monday_noon + 7 * 86_400 - 3 * 3_600
        );
        // Monday 13:00 is still today.
        assert_eq!(next_digest_at(monday_noon, 0, 13), monday_noon + 3_600);
    }

    #[test]
    fn overview_puts_started_modules_first_and_folds_the_rest() {
        let module = |name: &str, earned| AchievementModuleProgress {
            module: name.into(),
            earned,
            available: 5,
            ..AchievementModuleProgress::default()
        };
        let summary = AchievementProfileSummary {
            modules: vec![
                module("cards", 0),
                module("fishing", 2),
                module("wiki", 4),
                module("tarot", 0),
            ],
            ..AchievementProfileSummary::default()
        };
        assert_eq!(
            overview_parts(&summary),
            ["wiki 4/5", "fishing 2/5", "2 more not yet started"]
        );
    }

    #[test]
    fn digest_groups_by_person_and_hides_secrets() {
        let unlock = |nick: &str, name: &str, secret| jeeves_abi::AchievementRecentUnlock {
            profile_id: nick.into(),
            nick: nick.into(),
            module: "m".into(),
            id: name.into(),
            name: name.into(),
            unlocked_at: 0,
            secret,
        };
        let response = AchievementBoardResponse {
            unlocks: vec![
                unlock("bob", "One", false),
                unlock("alice", "Two", false),
                unlock("alice", "Secret", true),
                unlock("alice", "Four", false),
            ],
            ..AchievementBoardResponse::default()
        };
        let (people, text) = digest_highlights(&response);
        assert_eq!(people, 2);
        assert_eq!(
            text,
            "a\u{200B}lice 3 (Two, a secret, +1) · b\u{200B}ob 1 (One)"
        );
    }

    #[test]
    fn prestige_one_omits_the_numeral() {
        assert_eq!(prestige_name("Master", 1), "Master");
        assert_eq!(prestige_name("Master", 4), "Master IV");
    }
}
