//! Spontaneous Victorian gentleman's excursion game for rustjeeves.
//!
//! Jeeves proposes a trip to a destination; players have a signup window to
//! join with !roadtrip join; the party departs and returns after a while.
//! `!roadtrip` also starts a trip manually regardless of the `enabled` setting.
//!
//! IMPORTANT: spontaneous trips require `enabled = true` per channel.
//!            Manual !roadtrip always works.
//!
//! Commands: !roadtrip [join | status | cancel]
//!
//! Theme keys (all under "roadtrip.*"):
//!   destinations (list — the pool of destinations; operators edit this in theme.toml),
//!   announce (spontaneous trip proposed; vars: destination, mins),
//!   propose (manual trip started; vars: nick, destination, mins),
//!   join_prompt (trip forming, told to use !roadtrip join; vars: destination),
//!   joined (confirmed join; vars: nick, destination),
//!   already_joined (tried to join twice; vars: nick),
//!   join_closed (no open signup; vars: nick),
//!   already_travelling (trip active, can't start another; vars: destination),
//!   nobody (nobody joined, trip cancelled; vars: destination),
//!   depart (party departs; vars: passengers, destination, count),
//!   return (party returns; vars: passengers, destination, count),
//!   status_signup (signup open; vars: destination, passengers, count, mins),
//!   status_travelling (on a trip; vars: destination, passengers, count),
//!   status_none (nothing planned),
//!   cancelled (admin cancelled; vars: destination),
//!   cancel_denied (non-admin tried to cancel; vars: nick)

use extism_pdk::*;
use jeeves_abi::{
    CommandManifest, CommandSpec, Event, EventEnvelope, KvGet, KvSet, RandomBytesRequest,
    RandomBytesResponse, Role, ScheduleCancel, ScheduleList, ScheduleSet, ScheduledJob,
    SendMessage, SettingGet, SettingKind, SettingScope, SettingSpec, SettingsManifest, ThemeReq,
    COMMAND_MANIFEST_VERSION, SETTINGS_MANIFEST_VERSION,
};
use serde::{Deserialize, Serialize};

// Default destination pool — operators override "roadtrip.destinations" in theme.toml.
const DEFAULT_DESTINATIONS: &[&str] = &[
    "Cairo",
    "Monte Carlo",
    "The French Riviera",
    "The Swiss Alps",
    "Rome",
    "Vienna",
    "Constantinople",
    "The Orient Express",
    "Ascot Races",
    "The Scottish Highlands",
    "The Palace of Versailles",
    "Bath",
    "The Savoy",
    "Lord's Cricket Ground",
    "Niagara Falls",
    "The Nile Delta",
    "Deauville",
    "Venice",
    "Florence",
    "Biarritz",
    "Edinburgh Castle",
    "San Remo",
    "Lisbon",
    "Madrid",
    "The Amalfi Coast",
];

// ── host function imports ─────────────────────────────────────────────────────

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn theme(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn now(input: String) -> String;
    fn setting_get(input: String) -> String;
    fn random_bytes(input: String) -> String;
    fn schedule_set(input: String) -> String;
    fn schedule_cancel(input: String) -> String;
    fn schedule_list(input: String) -> String;
}

// ── command manifest ──────────────────────────────────────────────────────────

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![CommandSpec {
            name: "roadtrip".into(),
            description: "Propose or join a Victorian gentleman's excursion. Jeeves arranges the details.".into(),
            usage: "!roadtrip [join | status | cancel]".into(),
            aliases: vec!["rt".into()],
        }],
    })?)
}

// ── settings manifest ─────────────────────────────────────────────────────────

#[plugin_fn]
pub fn settings(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&SettingsManifest {
        version: SETTINGS_MANIFEST_VERSION,
        settings: vec![
            SettingSpec {
                key: "enabled".into(),
                description: "Whether Jeeves proposes spontaneous trips in this channel.".into(),
                default: "false".into(),
                kind: SettingKind::Boolean,
                scopes: vec![SettingScope::Channel, SettingScope::Network, SettingScope::Global],
                applies_immediately: true,
            },
            SettingSpec {
                key: "signup_secs".into(),
                description: "Seconds the signup window stays open after a trip is proposed.".into(),
                default: "90".into(),
                kind: SettingKind::Integer { min: 30, max: 300 },
                scopes: vec![SettingScope::Global, SettingScope::Channel],
                applies_immediately: true,
            },
            SettingSpec {
                key: "min_trip_mins".into(),
                description: "Minimum minutes a trip lasts before the party returns.".into(),
                default: "30".into(),
                kind: SettingKind::Integer { min: 5, max: 120 },
                scopes: vec![SettingScope::Global, SettingScope::Channel],
                applies_immediately: true,
            },
            SettingSpec {
                key: "max_trip_mins".into(),
                description: "Maximum minutes a trip lasts before the party returns.".into(),
                default: "60".into(),
                kind: SettingKind::Integer { min: 5, max: 240 },
                scopes: vec![SettingScope::Global, SettingScope::Channel],
                applies_immediately: true,
            },
            SettingSpec {
                key: "min_interval_mins".into(),
                description: "Minimum minutes between spontaneous trip proposals.".into(),
                default: "120".into(),
                kind: SettingKind::Integer { min: 30, max: 1440 },
                scopes: vec![SettingScope::Global, SettingScope::Channel],
                applies_immediately: true,
            },
            SettingSpec {
                key: "max_interval_mins".into(),
                description: "Maximum minutes between spontaneous trip proposals.".into(),
                default: "360".into(),
                kind: SettingKind::Integer { min: 30, max: 2880 },
                scopes: vec![SettingScope::Global, SettingScope::Channel],
                applies_immediately: true,
            },
        ],
    })?)
}

// ── state structs ─────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone)]
struct Passenger {
    user_id: String,
    nick: String,
    display: String,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
enum TripPhase {
    #[default]
    None,
    Signup,
    Travelling,
}

#[derive(Serialize, Deserialize, Default)]
struct TripState {
    phase: TripPhase,
    destination: String,
    passengers: Vec<Passenger>,
}

// ── job ID helpers (encoded per server+channel to avoid cross-channel cancel) ─

fn next_job_id(server: &str, channel: &str) -> String {
    format!("next:{server}:{channel}")
}

fn depart_job_id(server: &str, channel: &str) -> String {
    format!("depart:{server}:{channel}")
}

fn return_job_id(server: &str, channel: &str) -> String {
    format!("return:{server}:{channel}")
}

// ── KV helpers ────────────────────────────────────────────────────────────────

fn kv_load(key: &str) -> Result<String, Error> {
    Ok(unsafe { kv_get(serde_json::to_string(&KvGet { key: key.into() })?)? })
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

fn state_key(server: &str, channel: &str) -> String {
    format!("trip:{server}:{channel}")
}

fn load_state(server: &str, channel: &str) -> Result<TripState, Error> {
    let raw = kv_load(&state_key(server, channel))?;
    if raw.is_empty() {
        return Ok(TripState::default());
    }
    Ok(serde_json::from_str(&raw).unwrap_or_default())
}

fn save_state(server: &str, channel: &str, state: &TripState) -> Result<(), Error> {
    kv_save(&state_key(server, channel), &serde_json::to_string(state)?)
}

fn clear_state(server: &str, channel: &str) -> Result<(), Error> {
    kv_save(&state_key(server, channel), "")
}

// ── host helpers ──────────────────────────────────────────────────────────────

fn now_secs() -> i64 {
    unsafe {
        now(String::new())
            .unwrap_or_default()
            .trim()
            .parse()
            .unwrap_or(0)
    }
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
            default: defaults.iter().map(|s| s.to_string()).collect(),
            vars: vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        })?)?
    })
}

fn read_setting_raw(key: &str, server: &str, channel: &str) -> Option<String> {
    let raw = unsafe {
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
    Some(raw)
}

fn read_setting_bool(key: &str, server: &str, channel: &str, default: bool) -> bool {
    read_setting_raw(key, server, channel)
        .and_then(|s| match s.trim() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        })
        .unwrap_or(default)
}

fn read_setting_i64(key: &str, server: &str, channel: &str, default: i64) -> i64 {
    read_setting_raw(key, server, channel)
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(default)
}

fn get_random_bytes(count: usize) -> Result<Vec<u8>, Error> {
    let raw =
        unsafe { random_bytes(serde_json::to_string(&RandomBytesRequest { count })?)? };
    let resp: RandomBytesResponse = serde_json::from_str(&raw)?;
    Ok(resp.bytes)
}

fn has_pending_job(server: &str, channel: &str, id: &str) -> bool {
    let raw = unsafe {
        schedule_list(
            serde_json::to_string(&ScheduleList {
                server: Some(server.into()),
                channel: Some(channel.into()),
            })
            .unwrap_or_default(),
        )
        .unwrap_or_default()
    };
    let jobs: Vec<ScheduledJob> = serde_json::from_str(&raw).unwrap_or_default();
    jobs.iter().any(|j| j.id == id)
}

fn cancel_job(server: &str, channel: &str, id: &str) {
    let full_id = format!("{id}:{server}:{channel}");
    let _ = unsafe {
        schedule_cancel(
            serde_json::to_string(&ScheduleCancel { id: full_id }).unwrap_or_default(),
        )
    };
}

// ── scheduling helpers ────────────────────────────────────────────────────────

fn schedule_next_announce(server: &str, channel: &str) -> Result<(), Error> {
    let min = read_setting_i64("min_interval_mins", server, channel, 120);
    let max = read_setting_i64("max_interval_mins", server, channel, 360).max(min + 1);
    let bytes = get_random_bytes(4)?;
    let range = ((max - min) * 60).max(1) as u64;
    let r = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as u64;
    let delay = min * 60 + (r % range) as i64;
    unsafe {
        schedule_set(serde_json::to_string(&ScheduleSet {
            id: next_job_id(server, channel),
            server: server.into(),
            channel: channel.into(),
            due_at: now_secs() + delay,
            payload: String::new(),
        })?)?;
    }
    Ok(())
}

fn schedule_depart(server: &str, channel: &str, signup_secs: i64) -> Result<(), Error> {
    unsafe {
        schedule_set(serde_json::to_string(&ScheduleSet {
            id: depart_job_id(server, channel),
            server: server.into(),
            channel: channel.into(),
            due_at: now_secs() + signup_secs,
            payload: String::new(),
        })?)?;
    }
    Ok(())
}

fn schedule_return(server: &str, channel: &str) -> Result<(), Error> {
    let min = read_setting_i64("min_trip_mins", server, channel, 30);
    let max = read_setting_i64("max_trip_mins", server, channel, 60).max(min + 1);
    let bytes = get_random_bytes(4)?;
    let range = ((max - min) * 60).max(1) as u64;
    let r = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as u64;
    let delay = min * 60 + (r % range) as i64;
    unsafe {
        schedule_set(serde_json::to_string(&ScheduleSet {
            id: return_job_id(server, channel),
            server: server.into(),
            channel: channel.into(),
            due_at: now_secs() + delay,
            payload: String::new(),
        })?)?;
    }
    Ok(())
}

fn ensure_next_scheduled(server: &str, channel: &str) -> Result<(), Error> {
    if !has_pending_job(server, channel, &next_job_id(server, channel))
        && !has_pending_job(server, channel, &depart_job_id(server, channel))
        && !has_pending_job(server, channel, &return_job_id(server, channel))
    {
        let state = load_state(server, channel)?;
        if state.phase == TripPhase::None {
            schedule_next_announce(server, channel)?;
        }
    }
    Ok(())
}

// ── formatting ────────────────────────────────────────────────────────────────

fn format_passengers(passengers: &[Passenger]) -> String {
    match passengers {
        [] => String::new(),
        [p] => p.display.clone(),
        [a, b] => format!("{} and {}", a.display, b.display),
        _ => {
            let init: Vec<&str> = passengers[..passengers.len() - 1]
                .iter()
                .map(|p| p.display.as_str())
                .collect();
            format!(
                "{}, and {}",
                init.join(", "),
                passengers.last().unwrap().display
            )
        }
    }
}

// ── core trip logic ───────────────────────────────────────────────────────────

/// Open a signup window: pick destination, store state, announce, schedule depart.
/// `initiator` is None for spontaneous trips, Some for manual !roadtrip.
fn open_signup(
    server: &str,
    channel: &str,
    initiator: Option<&Passenger>,
) -> Result<(), Error> {
    let destination = themed("roadtrip.destinations", DEFAULT_DESTINATIONS, &[])?;
    let signup_secs = read_setting_i64("signup_secs", server, channel, 90);
    let mins = (signup_secs / 60).max(1).to_string();

    let passengers = initiator.map(|p| vec![p.clone()]).unwrap_or_default();
    let state = TripState {
        phase: TripPhase::Signup,
        destination: destination.clone(),
        passengers,
    };
    save_state(server, channel, &state)?;
    schedule_depart(server, channel, signup_secs)?;

    match initiator {
        None => reply(
            server,
            channel,
            &themed(
                "roadtrip.announce",
                &[
                    "Jeeves has arranged an excursion to {destination}! Join with !roadtrip join. Departing in {mins} minutes.",
                    "One has taken the liberty of booking passage to {destination}. Those wishing to accompany may say !roadtrip join. Departure in {mins} minutes.",
                ],
                &[("destination", &destination), ("mins", &mins)],
            )?,
        )?,
        Some(p) => reply(
            server,
            channel,
            &themed(
                "roadtrip.propose",
                &[
                    "{nick} proposes an excursion to {destination}! Join with !roadtrip join. Departing in {mins} minutes.",
                    "{nick} has suggested a trip to {destination}. Interested parties may say !roadtrip join before departure in {mins} minutes.",
                ],
                &[
                    ("nick", &p.display),
                    ("destination", &destination),
                    ("mins", &mins),
                ],
            )?,
        )?,
    }

    Ok(())
}

// ── timer handlers ────────────────────────────────────────────────────────────

fn handle_next(server: &str, channel: &str) -> Result<(), Error> {
    if !read_setting_bool("enabled", server, channel, false) {
        return Ok(());
    }
    let state = load_state(server, channel)?;
    if state.phase != TripPhase::None {
        // A manual trip was started between the schedule and the fire — reschedule for later.
        if read_setting_bool("enabled", server, channel, false) {
            schedule_next_announce(server, channel)?;
        }
        return Ok(());
    }
    open_signup(server, channel, None)
}

fn handle_depart(server: &str, channel: &str) -> Result<(), Error> {
    let mut state = load_state(server, channel)?;
    if state.phase != TripPhase::Signup {
        return Ok(());
    }

    if state.passengers.is_empty() {
        clear_state(server, channel)?;
        reply(
            server,
            channel,
            &themed(
                "roadtrip.nobody",
                &[
                    "Nobody joined the trip to {destination}. Jeeves quietly unpacks the luggage.",
                    "The carriages for {destination} departed empty. Most unfortunate.",
                ],
                &[("destination", &state.destination)],
            )?,
        )?;
        if read_setting_bool("enabled", server, channel, false) {
            schedule_next_announce(server, channel)?;
        }
        return Ok(());
    }

    state.phase = TripPhase::Travelling;
    save_state(server, channel, &state)?;
    schedule_return(server, channel)?;

    let names = format_passengers(&state.passengers);
    let count = state.passengers.len().to_string();
    reply(
        server,
        channel,
        &themed(
            "roadtrip.depart",
            &[
                "The party sets off for {destination}! Bon voyage, {passengers}.",
                "Right, then! {passengers} are bound for {destination}. One trusts all will be orderly.",
                "Jeeves has arranged the carriages. {passengers} depart for {destination}.",
            ],
            &[
                ("passengers", &names),
                ("destination", &state.destination),
                ("count", &count),
            ],
        )?,
    )?;
    Ok(())
}

fn handle_return(server: &str, channel: &str) -> Result<(), Error> {
    let state = load_state(server, channel)?;
    if state.phase != TripPhase::Travelling {
        return Ok(());
    }

    clear_state(server, channel)?;

    let names = format_passengers(&state.passengers);
    let count = state.passengers.len().to_string();
    reply(
        server,
        channel,
        &themed(
            "roadtrip.return",
            &[
                "The party returns from {destination}, refreshed and unencumbered by scandal.",
                "Against all odds, the expedition to {destination} has concluded without incident.",
                "What larks! {passengers} have returned from {destination}, all present and accounted for.",
                "{passengers} return from {destination}, somewhat windswept but otherwise intact.",
            ],
            &[
                ("passengers", &names),
                ("destination", &state.destination),
                ("count", &count),
            ],
        )?,
    )?;

    if read_setting_bool("enabled", server, channel, false) {
        schedule_next_announce(server, channel)?;
    }
    Ok(())
}

// ── command handlers ──────────────────────────────────────────────────────────

fn cmd_start(
    server: &str,
    channel: &str,
    nick: &str,
    display: &str,
    user_id: &str,
) -> Result<(), Error> {
    let state = load_state(server, channel)?;
    match state.phase {
        TripPhase::Signup => {
            reply(
                server,
                channel,
                &themed(
                    "roadtrip.join_prompt",
                    &["A trip to {destination} is forming! Say !roadtrip join to hop aboard."],
                    &[("destination", &state.destination)],
                )?,
            )?;
        }
        TripPhase::Travelling => {
            reply(
                server,
                channel,
                &themed(
                    "roadtrip.already_travelling",
                    &["The party is currently travelling to {destination}. Another trip can be arranged upon their return."],
                    &[("destination", &state.destination)],
                )?,
            )?;
        }
        TripPhase::None => {
            // Cancel any pending next-announce and start immediately.
            cancel_job(server, channel, "next");
            let p = Passenger {
                user_id: user_id.to_string(),
                nick: nick.to_string(),
                display: display.to_string(),
            };
            open_signup(server, channel, Some(&p))?;
        }
    }
    Ok(())
}

fn cmd_join(
    server: &str,
    channel: &str,
    nick: &str,
    display: &str,
    user_id: &str,
) -> Result<(), Error> {
    let mut state = load_state(server, channel)?;
    match state.phase {
        TripPhase::None | TripPhase::Travelling => {
            reply(
                server,
                channel,
                &themed(
                    "roadtrip.join_closed",
                    &["There's no open trip signup right now, {nick}. Watch for the next announcement!"],
                    &[("nick", display)],
                )?,
            )?;
        }
        TripPhase::Signup => {
            // Check for duplicate
            let already = if !user_id.is_empty() {
                state.passengers.iter().any(|p| p.user_id == user_id)
            } else {
                state.passengers.iter().any(|p| p.nick.eq_ignore_ascii_case(nick))
            };
            if already {
                reply(
                    server,
                    channel,
                    &themed(
                        "roadtrip.already_joined",
                        &["{nick} is already in the party."],
                        &[("nick", display)],
                    )?,
                )?;
                return Ok(());
            }
            state.passengers.push(Passenger {
                user_id: user_id.to_string(),
                nick: nick.to_string(),
                display: display.to_string(),
            });
            save_state(server, channel, &state)?;
            reply(
                server,
                channel,
                &themed(
                    "roadtrip.joined",
                    &[
                        "{nick} joins the party for {destination}!",
                        "Splendid! {nick} is coming along to {destination}.",
                    ],
                    &[("nick", display), ("destination", &state.destination)],
                )?,
            )?;
        }
    }
    Ok(())
}

fn cmd_status(server: &str, channel: &str) -> Result<(), Error> {
    let state = load_state(server, channel)?;
    match state.phase {
        TripPhase::None => {
            reply(
                server,
                channel,
                &themed(
                    "roadtrip.status_none",
                    &["No trip is currently planned. Say !roadtrip to propose one."],
                    &[],
                )?,
            )?;
        }
        TripPhase::Signup => {
            let names = format_passengers(&state.passengers);
            let count = state.passengers.len().to_string();
            reply(
                server,
                channel,
                &themed(
                    "roadtrip.status_signup",
                    &["Trip to {destination} is forming ({count} aboard so far: {passengers}). Say !roadtrip join!"],
                    &[
                        ("destination", &state.destination),
                        ("passengers", &names),
                        ("count", &count),
                    ],
                )?,
            )?;
        }
        TripPhase::Travelling => {
            let names = format_passengers(&state.passengers);
            let count = state.passengers.len().to_string();
            reply(
                server,
                channel,
                &themed(
                    "roadtrip.status_travelling",
                    &["The party of {count} ({passengers}) is currently travelling to {destination}."],
                    &[
                        ("destination", &state.destination),
                        ("passengers", &names),
                        ("count", &count),
                    ],
                )?,
            )?;
        }
    }
    Ok(())
}

fn cmd_cancel(server: &str, channel: &str) -> Result<(), Error> {
    let state = load_state(server, channel)?;
    if state.phase == TripPhase::None {
        return Ok(());
    }
    let destination = state.destination.clone();
    clear_state(server, channel)?;
    cancel_job(server, channel, "depart");
    cancel_job(server, channel, "return");
    reply(
        server,
        channel,
        &themed(
            "roadtrip.cancelled",
            &["The trip to {destination} has been cancelled. Jeeves repacks the trunks."],
            &[("destination", &destination)],
        )?,
    )?;
    if read_setting_bool("enabled", server, channel, false) {
        schedule_next_announce(server, channel)?;
    }
    Ok(())
}

// ── exports ───────────────────────────────────────────────────────────────────

#[plugin_fn]
pub fn on_event(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let server = env.server;
    let Event::Timer { id, channel, .. } = env.event else {
        return Ok(());
    };

    if id.starts_with("next:") {
        handle_next(&server, &channel)?;
    } else if id.starts_with("depart:") {
        handle_depart(&server, &channel)?;
    } else if id.starts_with("return:") {
        handle_return(&server, &channel)?;
    }

    Ok(())
}

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let server = env.server;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };

    if msg.is_private {
        return Ok(());
    }

    let channel = &msg.target;

    if read_setting_bool("enabled", &server, channel, false) {
        ensure_next_scheduled(&server, channel)?;
    }

    let text = msg.text.trim();
    let lower = text.to_ascii_lowercase();
    if !lower.starts_with("!roadtrip") && !lower.starts_with("!rt") {
        return Ok(());
    }

    let nick = &msg.nick;
    let display = if msg.display.is_empty() {
        nick.as_str()
    } else {
        msg.display.as_str()
    };
    let user_id = &msg.user_id;

    // Determine canonical rest — handle both !roadtrip and !rt
    let rest = if lower.starts_with("!roadtrip") {
        text[9..].trim()
    } else {
        text[3..].trim() // !rt
    };
    let sub = rest.split_whitespace().next().unwrap_or("");

    match sub {
        "" => cmd_start(&server, channel, nick, display, user_id)?,
        "join" => cmd_join(&server, channel, nick, display, user_id)?,
        "status" => cmd_status(&server, channel)?,
        "cancel" => {
            if msg.role.is_some_and(|r| r.satisfies(Role::Admin)) {
                cmd_cancel(&server, channel)?;
            } else {
                reply(
                    &server,
                    channel,
                    &themed(
                        "roadtrip.cancel_denied",
                        &["Only administrators may cancel an excursion, {nick}."],
                        &[("nick", display)],
                    )?,
                )?;
            }
        }
        _ => {}
    }

    Ok(())
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_ids_are_channel_scoped() {
        assert_ne!(next_job_id("net", "#a"), next_job_id("net", "#b"));
        assert_ne!(depart_job_id("net1", "#x"), depart_job_id("net2", "#x"));
        assert_ne!(return_job_id("net", "#a"), return_job_id("net", "#b"));
    }

    #[test]
    fn default_destinations_nonempty() {
        assert!(!DEFAULT_DESTINATIONS.is_empty());
        assert!(DEFAULT_DESTINATIONS.iter().all(|d| !d.is_empty()));
    }

    #[test]
    fn passenger_list_solo() {
        let p = vec![Passenger {
            user_id: String::new(),
            nick: "alice".into(),
            display: "alice".into(),
        }];
        assert_eq!(format_passengers(&p), "alice");
    }

    #[test]
    fn passenger_list_duo() {
        let passengers = vec![
            Passenger { user_id: String::new(), nick: "alice".into(), display: "alice".into() },
            Passenger { user_id: String::new(), nick: "bob".into(), display: "bob".into() },
        ];
        assert_eq!(format_passengers(&passengers), "alice and bob");
    }

    #[test]
    fn passenger_list_trio() {
        let passengers = vec![
            Passenger { user_id: String::new(), nick: "alice".into(), display: "alice".into() },
            Passenger { user_id: String::new(), nick: "bob".into(), display: "bob".into() },
            Passenger { user_id: String::new(), nick: "carol".into(), display: "carol".into() },
        ];
        assert_eq!(format_passengers(&passengers), "alice, bob, and carol");
    }

    #[test]
    fn random_delay_in_range() {
        let min_mins: i64 = 120;
        let max_mins: i64 = 360;
        let range = ((max_mins - min_mins) * 60).max(1) as u64;
        for bytes in [[0u8, 0, 0, 0], [255, 255, 255, 255], [42, 13, 7, 99]] {
            let r = u32::from_le_bytes(bytes) as u64;
            let delay = min_mins * 60 + (r % range) as i64;
            assert!(delay >= min_mins * 60);
            assert!(delay < max_mins * 60);
        }
    }
}
