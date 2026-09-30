//! Heists: `!brass heist <brass>` (shortcut `!heist`) plans a job on a randomly chosen target, and
//! anyone in the game room can `!heist join <brass>` within `heist_join_seconds`. Then the job
//! plays out over a couple of scheduled story beats, and each member escapes or is caught on their
//! own roll. A bigger crew is both safer and better value, though the house keeps its edge: one
//! thief escapes 40% of the time and averages 85% of the stake back, a crew of six or more 80% and
//! about 97%. Stakes count toward the daily loss cap and `gambling_enabled` closes heists with the
//! other games. After a job the channel lies low for `heist_cooldown_minutes`.

use super::brass::{
    load_wagers, place_stake, save_wagers, setting_i64, Stake, DAY, DEFAULT_MAX_BET,
};
use super::*;
use jeeves_abi::ScheduleSet;

pub(super) const DEFAULT_JOIN_SECONDS: i64 = 120;
pub(super) const DEFAULT_COOLDOWN_MINUTES: i64 = 10;
const MIN_STAKE: u64 = 10;
const MAX_CREW: usize = 8;
const BEAT_SECONDS: i64 = 10;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(super) struct Member {
    pub(super) profile_id: String,
    name: String,
    stake: u64,
    /// The stake's economy event; winnings are paid under it too.
    event_id: String,
}

/// A channel's heist: being planned, under way, or (between jobs) just the cooldown.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub(super) struct Heist {
    #[serde(default)]
    server: String,
    #[serde(default)]
    channel: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    planner: String,
    #[serde(default)]
    target: String,
    #[serde(default)]
    pub(super) crew: Vec<Member>,
    /// 0 while the crew gathers, then each story beat told.
    #[serde(default)]
    beat: u8,
    #[serde(default)]
    active: bool,
    #[serde(default)]
    join_until: i64,
    #[serde(default)]
    cooldown_until: i64,
    /// Bumped whenever a timer is booked, so an older timer is recognised and ignored.
    #[serde(default)]
    seq: u64,
}

pub(super) const HEIST_PREFIX: &str = "heist:";

fn heist_key(server: &str, channel: &str) -> String {
    format!("{HEIST_PREFIX}{server}:{}", room_key(channel))
}

fn load_heist(server: &str, channel: &str) -> Result<Heist, Error> {
    let raw = kv_load(&heist_key(server, channel))?;
    if raw.trim().is_empty() {
        Ok(Heist::default())
    } else {
        Ok(serde_json::from_str(&raw)?)
    }
}

fn save_heist(heist: &Heist) -> Result<(), Error> {
    kv_save(
        &heist_key(&heist.server, &heist.channel),
        &serde_json::to_string(heist)?,
    )
}

fn book(heist: &mut Heist, due_at: i64) -> Result<(), Error> {
    heist.seq += 1;
    save_heist(heist)?;
    unsafe {
        schedule_set(serde_json::to_string(&ScheduleSet {
            id: heist_key(&heist.server, &heist.channel),
            server: heist.server.clone(),
            channel: heist.channel.clone(),
            owner_profile_id: None,
            due_at,
            payload: heist.seq.to_string(),
        })?)?
    };
    Ok(())
}

/// Chance in 1000 that one member escapes, and the average return in 1000ths of the stake, for a
/// crew of `size`.
pub(super) fn odds(size: usize) -> (u64, u64) {
    let extra = size.saturating_sub(1) as u64;
    ((400 + 80 * extra).min(850), (850 + 25 * extra).min(970))
}

/// What an escaping member takes home for their stake.
pub(super) fn haul(stake: u64, size: usize) -> u64 {
    let (escape, value) = odds(size);
    stake * value / escape
}

fn names(members: &[&Member]) -> String {
    let names = members.iter().map(|m| m.name.as_str()).collect::<Vec<_>>();
    match names.as_slice() {
        [] => String::new(),
        [one] => (*one).to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// `!brass heist …` in the game room.
pub(super) fn command(server: &str, msg: &MessagePayload, argument: &str) -> Result<String, Error> {
    let channel = msg.target.as_str();
    let now = now_secs()?;
    let mut heist = load_heist(server, channel)?;
    let (sub, rest) = argument
        .split_once(char::is_whitespace)
        .map(|(sub, rest)| (sub.to_ascii_lowercase(), rest.trim()))
        .unwrap_or((argument.to_ascii_lowercase(), ""));
    let amount = match sub.as_str() {
        "join" | "in" => rest,
        _ => argument.trim(),
    };
    if amount.is_empty() {
        return status(msg, &heist, now);
    }
    let max = setting_i64("max_bet", server, channel, DEFAULT_MAX_BET).max(1) as u64;
    let Some(stake) = amount
        .parse::<u64>()
        .ok()
        .filter(|stake| (MIN_STAKE..=max.max(MIN_STAKE)).contains(stake))
    else {
        return say(
            msg,
            "gacha.heist_usage",
            "Plan a job with !heist <brass>, or join one with !heist join <brass> ({min} to {max} brass), {honorific}.",
            &[("min", &MIN_STAKE.to_string()), ("max", &max.to_string())],
        );
    };
    if heist.active && heist.beat > 0 {
        return say(
            msg,
            "gacha.heist_underway",
            "The job's already under way, {honorific}; too late to climb in.",
            &[],
        );
    }
    if heist.active && heist.crew.iter().any(|m| m.profile_id == msg.user_id) {
        return say(
            msg,
            "gacha.heist_already_in",
            "You're already in on this one, {honorific}.",
            &[],
        );
    }
    if heist.active && heist.crew.len() >= MAX_CREW {
        return say(
            msg,
            "gacha.heist_full",
            "The crew is full, {honorific}; any more and someone would have to sit on the getaway driver.",
            &[],
        );
    }
    if !heist.active && now < heist.cooldown_until {
        let minutes = ((heist.cooldown_until - now) + 59) / 60;
        return say(
            msg,
            "gacha.heist_cooldown",
            "The heat's still on after the last job, {honorific}; lie low for {minutes} more minute(s).",
            &[("minutes", &minutes.to_string())],
        );
    }
    let placed = match place_stake(server, msg, "heist", stake)? {
        Stake::Placed(placed) => placed,
        Stake::Refused(text) => return Ok(text),
    };
    let member = Member {
        profile_id: msg.user_id.clone(),
        name: display(msg).chars().take(64).collect(),
        stake,
        event_id: placed.event_id,
    };
    if heist.active {
        heist.crew.push(member);
        save_heist(&heist)?;
        let total = heist.crew.iter().map(|m| m.stake).sum::<u64>();
        return say(
            msg,
            "gacha.heist_joined",
            "🕶 {user} is in with {stake} brass: {crew} crew, {total} brass on the line.",
            &[
                ("stake", &stake.to_string()),
                ("crew", &heist.crew.len().to_string()),
                ("total", &total.to_string()),
            ],
        );
    }
    let join_seconds =
        setting_i64("heist_join_seconds", server, channel, DEFAULT_JOIN_SECONDS).clamp(30, 600);
    heist = Heist {
        server: server.into(),
        channel: channel.into(),
        id: random_token()?,
        planner: msg.user_id.clone(),
        target: themed(
            "gacha.heist_targets",
            &[
                "the Bank of Kensington",
                "the Duchess's jewel vault",
                "the Museum of Unreasonable Antiquities",
                "the Royal Mint's back door",
                "Lord Emsworth's prize-pig trophy cabinet",
                "the Drones Club silver cupboard",
                "Aunt Agatha's wall safe",
            ],
            &[],
        )?,
        crew: vec![member],
        beat: 0,
        active: true,
        join_until: now + join_seconds,
        cooldown_until: 0,
        seq: heist.seq,
    };
    book(&mut heist, now + join_seconds)?;
    say(
        msg,
        "gacha.heist_planned",
        "💰 {user} is planning a job on {target} with {stake} brass: !heist join <brass> within {seconds}s.",
        &[
            ("target", &heist.target),
            ("stake", &stake.to_string()),
            ("seconds", &join_seconds.to_string()),
        ],
    )
}

fn status(msg: &MessagePayload, heist: &Heist, now: i64) -> Result<String, Error> {
    if !heist.active {
        return say(
            msg,
            "gacha.heist_idle",
            "No job on, {honorific}. Plan one with !heist <brass>; the more crew, the better the odds.",
            &[],
        );
    }
    let crew = heist.crew.iter().collect::<Vec<_>>();
    say(
        msg,
        "gacha.heist_status",
        "💰 A job on {target}: {names} ({total} brass), {seconds}s left to join.",
        &[
            ("target", &heist.target),
            ("names", &names(&crew)),
            (
                "total",
                &crew.iter().map(|m| m.stake).sum::<u64>().to_string(),
            ),
            ("seconds", &(heist.join_until - now).max(0).to_string()),
        ],
    )
}

/// The scheduler's knock: the crew sets off, the complication, then the outcome.
pub(super) fn on_timer(server: &str, channel: &str, seq: u64) -> Result<(), Error> {
    let mut heist = load_heist(server, channel)?;
    if !heist.active || heist.seq != seq {
        return Ok(());
    }
    let now = now_secs()?;
    match heist.beat {
        0 => {
            heist.beat = 1;
            let crew = heist.crew.iter().collect::<Vec<_>>();
            let total = crew.iter().map(|m| m.stake).sum::<u64>();
            let entry = themed(
                "gacha.heist_entries",
                &[
                    "slip in through the laundry chute",
                    "arrive disguised as a string quartet",
                    "tunnel up through the wine cellar",
                    "bluff past the doorman with forged invitations",
                    "come down the chimney, soot and all",
                ],
                &[],
            )?;
            reply(
                server,
                channel,
                &themed(
                    "gacha.heist_off",
                    &["🚪 {names} ({total} brass on the line) {entry}…"],
                    &[
                        ("names", &names(&crew)),
                        ("total", &total.to_string()),
                        ("entry", &entry),
                    ],
                )?,
            )?;
            book(&mut heist, now + BEAT_SECONDS)
        }
        1 => {
            heist.beat = 2;
            reply(
                server,
                channel,
                &themed(
                    "gacha.heist_complications",
                    &[
                        "🔦 A guard's torch sweeps the corridor…",
                        "🐕 Somewhere, a very large dog wakes up…",
                        "🔔 A floorboard creaks loudly enough to be heard in Surrey…",
                        "🗝 The vault is open, but so is the butler's eye…",
                    ],
                    &[],
                )?,
            )?;
            book(&mut heist, now + BEAT_SECONDS)
        }
        _ => outcome(&mut heist, now),
    }
}

fn outcome(heist: &mut Heist, now: i64) -> Result<(), Error> {
    let (server, channel) = (heist.server.clone(), heist.channel.clone());
    let size = heist.crew.len();
    let (escape, _) = odds(size);
    let mut escaped = Vec::new();
    let mut caught = Vec::new();
    for member in &heist.crew {
        if random_index(1000)? < escape as usize {
            escaped.push(member);
        } else {
            caught.push(member);
        }
    }
    // Pay out, and settle everyone's day against the loss cap.
    let today = now.div_euclid(DAY);
    let mut total = 0;
    for member in &heist.crew {
        let won = escaped.iter().any(|m| m.profile_id == member.profile_id);
        let payout = if won { haul(member.stake, size) } else { 0 };
        if payout > 0 {
            award_brass(
                &server,
                &member.profile_id,
                payout,
                &format!("{}:win", member.event_id),
                "heist_win",
            )?;
            total += payout;
        }
        let mut wagers = load_wagers(&server, &member.profile_id, today)?;
        wagers.net_lost = wagers
            .net_lost
            .saturating_add(member.stake as i64)
            .saturating_sub(payout as i64);
        save_wagers(&server, &member.profile_id, &wagers)?;
    }
    let text = match (escaped.is_empty(), caught.len()) {
        (false, 0) => themed(
            "gacha.heist_clean",
            &["💰 A clean job at {target}! {escaped} get away with {total} brass."],
            &[
                ("target", &heist.target),
                ("escaped", &names(&escaped)),
                ("total", &total.to_string()),
            ],
        )?,
        (true, _) => themed(
            "gacha.heist_busted",
            &["🚔 The whole crew is nabbed at {target}. Not a penny leaves the building."],
            &[("target", &heist.target)],
        )?,
        (false, 1) => themed(
            "gacha.heist_one_caught",
            &["🚨 Alarms at {target}! {caught} is nabbed; {escaped} escape with {total} brass."],
            &[
                ("target", &heist.target),
                ("caught", &names(&caught)),
                ("escaped", &names(&escaped)),
                ("total", &total.to_string()),
            ],
        )?,
        (false, _) => themed(
            "gacha.heist_some_caught",
            &["🚨 Alarms at {target}! {caught} are nabbed; {escaped} escape with {total} brass."],
            &[
                ("target", &heist.target),
                ("caught", &names(&caught)),
                ("escaped", &names(&escaped)),
                ("total", &total.to_string()),
            ],
        )?,
    };
    reply(&server, &channel, &text)?;
    let award_to = |member: &Member, stat: &str| -> Result<(), Error> {
        unsafe {
            award_stats(serde_json::to_string(&AwardStatsRequest {
                server: server.clone(),
                profile_id: member.profile_id.clone(),
                display_name: member.name.clone(),
                target: channel.clone(),
                increments: vec![StatIncrement {
                    stat: stat.into(),
                    amount: 1,
                }],
                deduplication_id: Some(format!("heist:{}:{stat}", heist.id)),
            })?)?
        };
        Ok(())
    };
    for member in &escaped {
        award_to(member, "heists_escaped")?;
    }
    if let Some(planner) = escaped.iter().find(|m| m.profile_id == heist.planner) {
        award_to(planner, "heists_masterminded")?;
    }
    if size >= 3 && caught.len() == 1 {
        award_to(caught[0], "heists_left_holding")?;
    }
    let cooldown = setting_i64(
        "heist_cooldown_minutes",
        &server,
        &channel,
        DEFAULT_COOLDOWN_MINUTES,
    )
    .clamp(0, 240);
    *heist = Heist {
        server,
        channel,
        cooldown_until: now + cooldown * 60,
        seq: heist.seq + 1,
        ..Heist::default()
    };
    save_heist(heist)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bigger_crews_are_safer_and_better_value_but_the_house_still_wins() {
        let mut previous = (0, 0);
        for size in 1..=MAX_CREW {
            let (escape, value) = odds(size);
            assert!(escape >= previous.0 && value >= previous.1, "{size}");
            assert!(value < 1000, "the house keeps an edge at {size}");
            // Expected return per 1000 staked, from the actual integer payout.
            let expected = haul(1000, size) * escape / 1000;
            assert!((800..=1000).contains(&expected), "{size}: {expected}");
            previous = (escape, value);
        }
        assert_eq!(odds(1), (400, 850));
        assert_eq!(haul(100, 1), 212);
        assert_eq!(odds(6), (800, 970));
    }

    #[test]
    fn crew_names_read_naturally() {
        let member = |name: &str| Member {
            profile_id: name.into(),
            name: name.into(),
            stake: 10,
            event_id: String::new(),
        };
        let (a, b, c) = (member("ann"), member("bob"), member("cy"));
        assert_eq!(names(&[&a]), "ann");
        assert_eq!(names(&[&a, &b]), "ann and bob");
        assert_eq!(names(&[&a, &b, &c]), "ann, bob and cy");
    }
}
