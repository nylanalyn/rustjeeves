# PLAN-PIRATE.md — Pirate Isles Module for rustjeeves

## 1. Overview

A persistent, asynchronous pirate-king IRC game for 5–6 players. Each player captains an island with crew, gold, and buildings. Crew are sent on voyages (missions) that resolve after a fixed delay. Players can raid each other or run NPC missions. Combat is zero-warning ambush — you must allocate crew defensively before you log off. Seasons last ~14 days, then the fleet "sets sail" to a new sea with new rules. Profile badges (Legends) persist across seasons; resources reset.

**Design pillars:**
- Async-first: no real-time reaction required.
- High-risk PvP: raiding a player can backfire catastrophically.
- Small public command set, with complex actions handled by a guided PM menu.
- No death spirals: 2 loyal crew are indestructible.
- Social deduction: the channel sees voyages depart, but not their destination.

---

## 2. Architecture

This is a **rustjeeves WASM module**. It does not handle IRC connections, authentication,
SQLite connections, scheduling infrastructure, or profile management; the host does.

The module communicates through the existing `jeeves-abi` contract:

- `on_message(String)` receives an `EventEnvelope` containing a channel or private
  `MessagePayload`. The host stamps `server`, `user_id`, `nick`, `display`, `target`, and the
  sender role onto the message.
- `on_event(String)` receives an `EventEnvelope`; timer work is delivered as `Event::Timer` with
  an id, channel, due time, and payload. The module may also handle `Event::NickChanged` to refresh
  display caches; it must ignore unrelated event variants.
- `send_message` posts to a server-qualified IRC target. The module must use the message target
  for replies and the channel from the timer event for scheduled announcements.
- The module stores state in its namespaced host KV store. The KV store is backed by the host's
  SQLite database, but the WASM module does not create tables or call SQLite directly.

The canonical persisted KV key is `data`, containing one versioned JSON state blob. State is
partitioned by `server/channel`, and player records inside a game are keyed by stable profile UUID.
Private-menu sessions are keyed by `server/profile_uuid`. Nicknames are display-only caches.

---

## 3. Player Identity and Scope

The module **never keys persistent state on a nick**. For a state-changing message it uses the
host-stamped `MessagePayload.user_id`. If the host cannot resolve a stable profile, the module
must not create or mutate a pirate profile under a nick fallback; it should return a bounded,
themed error instead. Use `profile_get` (and `profile_ensure` only when actually needed and
granted) for target resolution.

Each game is isolated by both network and channel. A captain may have separate islands in separate
game channels, while the same profile UUID identifies them within one network. Cached `nick` or
`display` values are refreshed from incoming messages and are never identity keys.

Joining is **deliberate**: `!signon` creates the pirate player for the stable UUID with the current
settings, and PMs them the basics (see §5.3). Nobody is enrolled by merely using a command —
being silently entered into a PvP game, and immediately starting to accrue missed paydays, is a
poor welcome and lets idle onlookers consume the player cap. `!here` and `!captain` stay open to
non-players so a channel can be watched without joining it.

The game must enforce a documented player cap; the design target is 5–6 captains, while the
implementation may choose a small configurable cap.

### 5.3 Onboarding

`!signon` announces the new captain in channel and sends four PM lines — no more. They cover who
you are, the daily wage obligation, the `!menu` → `!collect` loop, and where raids come from,
ending with a pointer to `!help pirate`. Depth belongs in `!help`, not in a wall of PMs.

---

## 4. Core Resources

| Resource | Starting | Description |
|----------|----------|-------------|
| **Gold** | 200g | Currency. Used for buildings, upkeep, ransoms. |
| **Rum** | 20 | Alternative currency for crew wages. |
| **Regular Crew** | 3 | Can die, desert, or be captured. |
| **Loyal Crew** | 2 | Never desert. Cannot be permanently lost. Cannot be captured. If the island is raided, they "retreat to the cove" and are unavailable for 6 hours instead of being captured. |
| **Notoriety** | 0 | Earned from bold raids, public declarations, marooning prisoners. Determines Navy target priority and season awards. |
| **Buildings** | Cove L1 (free) | Passive defenses and bonuses. |

**Total starting crew:** 5 (3 Regular + 2 Loyal).

**Crew cap:** None, but Regular Crew beyond 12 cost double upkeep (see §12).

---

## 5. Commands

### 5.1 Channel Commands (Public)

All channel commands are scoped to the channel the game runs in. The module should ignore them elsewhere.

| Command | Args | Description |
|---------|------|-------------|
| `!crew` | none | Shows your island status: gold, crew (regular/loyal/total), buildings, any returned voyages waiting to be collected, prisoner alerts, active debuffs, current season. |
| `!pay` | none | Pays the configured gold wages for every employed crew member for the current day, including crew assigned to active voyages; Regular Crew beyond the soft cap cost double. |
| `!signon` | none | Claims an isle and joins the game, then PMs the new captain the basics. Refused if they already hold an isle or the seas are full. |
| `!build` | none | The shipwright's prices: every building, the level your gold buys next, and what is out of reach. |
| `!rum` | none | Pays the configured rum wages for the current day. Deducts the rum wage for every crew member; Frozen North doubles this cost. |
| `!collect` | none | Collects returned voyage rewards and gives a channel-safe catch-up report; private scout reports are delivered by PM. |
| `!park` | none | Parks your ship: pauses payday penalties and active gameplay while voyages continue resolving. |
| `!unpark` | none | Resumes a parked captain in the channel. |
| `!here` | none | Shows the room state: current season name, days remaining, top 3 players by Notoriety, recent public voyage departures (last 6 hours), who is unpaid (vulnerable). |
| `!raid` | `<crew_count>` | **The ambush.** Sails against the isle in your current scout report (see §7.0). Silent — no channel line — and costs no Notoriety. Consumes the report. |
| `!raid` | `<player_nick> <crew_count>` | **Public declaration.** The bot announces in channel that you are raiding the target with N crew. Same mechanics as the ambush, but you gain +2 Notoriety immediately and the target knows a raid is en route (but not when). Needs no scout report — this is the only way to choose your own target. |
| `!captain` | `[nick]` | Shows a captain's career profile: Legends, career stats (total raids, defenses, gold plundered), current season rank. If no nick given, shows your own. |

### 5.2 PM Commands (Guided Menu)

Private messages arrive through the same `on_message` export with `is_private = true`. The module
maintains a bounded, persisted conversation state keyed by `server/profile_uuid`; it may show the
top-level menu for an empty PM, but must not create unbounded state for arbitrary senders.

`!menu` in the game channel opens the PM session and immediately sends the voyage options; the
player does not need a second `!voyage` command.

The current MVP opens the voyage menu directly:

```
Choose a voyage with !pirate <number>: 1: Merchant Convoy (4h, min 2 crew) | 2: Rum Runners (2h, min 2 crew) | 3: Pressgang (3h, min 2 crew)
```

Builds, prisoners, profile, and private payment remain available through their dedicated PM
commands; a broader top-level menu can be layered on later without changing the voyage flow.

The module maintains a **per-server, per-UUID conversation state** in the `data` KV blob. Sessions
expire after a bounded idle period and are capped. A PM-only command used in a channel, or a
channel-only command used in a PM, receives a themed usage response and does not mutate state.

**Menu 1: Send a Voyage**
1. Bot presents 3 random voyage options (see §6).
2. Player replies with `1`, `2`, or `3`.
3. Bot asks: `How many crew? You have X available.`
4. Player replies with a number.
5. Bot validates (sufficient crew, not already on 2+ voyages, etc.) and confirms.
6. Bot schedules the voyage return event.
7. Bot announces in channel: `⚓ <nick>'s N crew cast off into the mist... destination unknown.`

**Menu 2: Build or Upgrade**
1. Bot lists affordable buildings and current levels.
2. Player picks by number.
3. Bot deducts gold, upgrades building, confirms.

**Menu 3: Handle Prisoners**
1. Only shown if the player has prisoners.
2. Lists prisoners by count and origin captain.
3. Options:
   - `!ransom <amount>` — Offer back to original captain for gold.
   - `!pressgang` — Attempt to convert 50% of prisoners to your Regular Crew.
   - `!maroon` — Execute all prisoners. Gain +3 Notoriety per prisoner.

**Menu 4: View Full Profile**
Shows the same as `!captain` but with full detail.

**Menu 5: Pay Crew (Private)**
Same as `!pay` / `!rum` but done privately. Useful if a player wants to pay without announcing it in channel.

---

## 6. The Voyage System

### 6.1 Voyage Catalog

When a player opens Menu 1, the bot randomly selects **3 options** from this pool. Player raids are only offered if at least one valid target exists (not shielded, not already targeted by 2+ active raids).

| # | Mission | Time | Min Crew | Risk | Reward | Notes |
|---|---------|------|----------|------|--------|-------|
| A | Merchant Convoy | 4h | 2 | Low | 60–100g | Bread and butter. |
| B | Rum Runners | 2h | 2 | Low | 4–6 rum | Essential for rum economies. |
| C | Pressgang | 3h | 2 | Low | 1–2 Regular Crew | Can be done with Loyal Crew. |
| D | Smuggler's Cache | 4h | 3 | Med | 40g + 2–3 rum | Mixed payout. |
| E | Navy Payroll | 6h | 5 | High | 150–250g | 25% chance to lose 1–2 crew. |
| F | Explore Unknown | 6h | 4 | Med | Variable | 50%: 60g / 30%: 100g / 15%: 2 rum + 40g / 5%: Lose 1 crew, gain 50g |
| G | Raid [Player] | 4h | 1+ | Player-dependent | 15–25% of vulnerable gold + prisoners | See §7. |
| H | Scout [Player] | 2h | 1 | Low | Intel report on target's island | See §6.3. |

**Risk definitions:**
- **Low:** 0% crew death. Guaranteed minimum reward.
- **Med:** 10% chance to lose 1 crew. Variable reward.
- **High:** 25% chance to lose 1–2 crew. High reward.

### 6.2 Voyage Rules

- A player may have **up to 2 active voyages** at once.
- Crew on a voyage are **unavailable** until return. They cannot defend. They cannot be recalled.
- Voyages resolve at their scheduled time whether or not the player is online. The channel receives
  the public-safe result, while the resolved voyage remains in the captain's harbor queue.
- `!crew` lists the pending voyage identities, and `!collect` gives a detailed catch-up summary. Scout
  intelligence is persisted and delivered privately when collected; it is never included in the
  public catch-up line.
- The player must `!collect` (or use the PM menu) to claim rewards. This prevents "log in, grab loot, log out" automation — they must actively check in.

### 6.3 Scouting

Scouting is a voyage option (H). It sends 1 crew for 2 hours.

**Scout report (delivered privately when collected):**
```
DARKWATER DAVE'S ISLE (as of 2 hours ago):
Visible crew: 3
Gold: ~640g
Buildings: Vault L1, Walls L1
Note: Cove may hide additional crew.
```

**Intel goes stale.** The report is a snapshot. The target may have launched or returned voyages since the scout departed.

---

## 7. Combat System

### 7.0 How a raid is reached

There are exactly two routes to a raid, and the PM voyage menu deliberately offers neither — a
free raid in the menu would make scouting pointless.

1. **Scout, then strike.** The menu offers a scout against a *rolled* target; you never pick who.
   Collecting that report arms a raid on that isle for `SCOUT_INTEL_HOURS`, spent with
   `!raid <crew>`. Silent, and free of Notoriety.
2. **Declare war.** `!raid <nick> <crew>` lets you choose anyone, but it is announced in channel
   and costs Notoriety, which is what draws the Royal Navy.

The design intent is that being raided should read as bad luck rather than as a grudge. The
deliberate route still exists, but it is loud and it has a price.

**Mercy window.** Any raid that lands — won or repelled — puts the defending isle out of the
target pool for `RAID_MERCY_HOURS`. This applies to *both* routes: a public declaration that
ignored it would simply move a pile-on from the random roll into the channel. It also keeps the
scout pool honest, since intel is only ever handed out on isles that will still be raidable.

### 7.1 The Ambush

There is **zero warning** before a player raid lands. The first anyone sees is the resolution posted in channel.

```
💥 ALICE'S FLEET DESCENDS ON DAVE'S ISLE!
   Alice: 5 crew | Dave: 2 crew defending
   ⚔️ COMBAT...
   🛡️ DAVE WINS! Alice loses 3 crew.
   Dave captures 2 prisoners. Salvages 150 gold.
```

### 7.2 Combat Math

```
Attack Power  = (Crew_Sent × 10) × Random(0.8, 1.2)
Defense Power = (Home_Crew × 10) × Random(0.8, 1.2) + Building_Bonus
```

**Building bonuses:**
- Walls L1: +15 | Walls L2: +30
- Tavern: +5
- Cove crew get +2 power each (surprise defense)

**Loyal Crew in defense:** They fight at full power. If the attacker wins, Loyal Crew retreat to the cove for 6 hours instead of being captured. They are not counted as "lost" in the combat report.

The implementation must represent regular and loyal crew separately on every voyage. A launch
uses regular crew first and loyal crew only for the remainder. NPC risk, PvP deaths, and prisoner
capture apply only to regular crew. Loyal crew return safely or become temporarily unavailable in
the cove; they are never captured, permanently lost, or included in a prisoner count.

### 7.3 Outcomes

| Result | Condition | Attacker Gets | Attacker Loses | Defender Gets |
|--------|-----------|---------------|----------------|---------------|
| **Crushing Victory** | Attack > Defense × 1.5 | 25% of vulnerable gold, lose 0–1 crew | 0–1 Regular Crew | Humiliated. No loot. |
| **Victory** | Attack > Defense | 15% of vulnerable gold, lose 1–2 crew | 1–2 Regular Crew | Nothing. |
| **Defeat** | Attack < Defense | Nothing | 50% of sent Regular Crew (rounded up) | Captures prisoners + 50g salvage per prisoner. |
| **Crushing Defeat** | Attack < Defense × 0.5 | Nothing | **ALL** sent Regular Crew | Captures ALL sent Regular Crew + 200g + 10 Notoriety. |

**Vulnerable gold** = Total gold minus Vault protection.
- Vault L1: protects 50% of gold (50% vulnerable).
- Vault L2: protects 75% of gold (25% vulnerable).
- No Vault: 100% vulnerable.

**If defender has 0 crew home:** Auto-Victory for attacker (not Crushing). They still only get the % of vulnerable gold. Crew hidden in the cove is not home crew; if any loyal crew are hidden there, they remain safe and unavailable for the configured cove duration.

### 7.4 Public Raid Bonus

If the raid was declared publicly via `!raid`, the attacker gains **+2 Notoriety** immediately upon launch, win or lose.

---

## 8. Buildings

Buildings are passive, always-on, and have daily upkeep. If upkeep is unpaid at rollover, the building degrades one level (L2 → L1, L1 → destroyed).

| Building | Build Cost | Effect | Upkeep/Day |
|----------|------------|--------|------------|
| **Vault L1** | 200g | Protects 50% of gold from raids | 10g |
| **Vault L2** | 400g | Protects 75% of gold from raids | 20g |
| **Cove L1** | 300g | Hides 2 crew from scouts and `!here` | 15g |
| **Cove L2** | 600g | Hides 4 crew from scouts and `!here` | 30g |
| **Walls L1** | 250g | +15 defense power | 10g |
| **Walls L2** | 500g | +30 defense power | 20g |
| **Shipyard L1** | 200g | Voyages return 20% faster | 10g |
| **Shipyard L2** | 400g | Voyages return 35% faster | 20g |
| **Tavern L1** | 200g | No desertion on missed payday; +5 defense | 10g |

**Starting building:** Every new player gets **Cove L1 for free** (new player protection).

**New player shield:** 48 hours of immunity to player raids. NPC events still hit. The shield drops with a channel announcement.

---

## 9. Prisoner Economy

When a defender wins a raid, they capture the attacker's lost Regular Crew. These sit in a "brig" queue.

**Commands (via PM Menu 3):**

| Command | Effect |
|---------|--------|
| `!ransom <amount>` | Offer prisoners back to their original captain for gold. The original captain is PM'd with the offer and can `!payransom` or `!abandon`. If abandoned, the channel sees: *"Dave abandoned 3 crew to the sharks."* (-1 Notoriety for abandoner). |
| `!pressgang` | 50% chance per prisoner to convert to your Regular Crew. 50% chance they escape and return to their original captain. |
| `!maroon` | Execute all prisoners. Gain +3 Notoriety per prisoner. Brutal. Permanent. |

**Fair ransom price:** ~25g per prisoner. Players can charge whatever.

---

## 10. Daily Rollover / Payday

**Frequency:** Once per day at a configurable time (e.g., 00:00 UTC).

**Player action:** Players may `!pay` or `!rum` at any time before rollover. There is no time window.

**At rollover, the bot:**
1. Checks every active captain.
2. If paid: nothing happens. Crew remain loyal.
3. If unpaid: All Regular Crew lose 1 Loyalty tier. Loyalty: 3 → 2 → 1 → 0 (Desert).
4. If Loyalty hits 0: 1 Regular Crew deserts per day until paid.
5. Loyal Crew are unaffected by unpaid wages.

Parked captains are explicitly absent: daily rollover skips their loyalty decay, desertion, and
building upkeep/degradation. Voyages already at sea continue to resolve, but parked captains cannot
launch, raid, build, pay, collect, or use the PM menu until they reply `!unpark` in the channel.

Crew wages are charged when `!pay` or `!rum` succeeds, not again at rollover. Building upkeep is
settled separately at rollover: a paid captain pays each building's upkeep; an unaffordable
building degrades one level. An unpaid captain's buildings also degrade one level, but no building
upkeep is charged. The operation must be atomic from the module's point of view: invalid payment
does not set `paid_today`.

**Loyalty Tiers:** 3 (max) → 2 → 1 → 0 (Desert).

**Channel announcement at rollover:**
```
🍺 PAYDAY comes to Tortuga!
   Paid: Alice, Bob, Dave
   Unpaid: Carol (2 days — crew are deserting!)
```

**Disloyal crew intel:** For each day unpaid, Regular Crew have a 10% cumulative chance to "tip off" scouts. A scout report on an unpaid island includes: *"Tavern talk suggests morale is low. Some crew might not fight to the death."* This reduces defense power by 5% per unpaid day (capped at 25%).

---

## 11. The Navy / Shared Threat

**Frequency:** Every 3–4 days (so 3–4 times per 14-day season).

**Target:** The captain with the **highest Notoriety** at the time of announcement.

**Effect (announced 48 hours in advance):**
```
🚢 THE ROYAL FLEET HAS BEEN SIGHTED!
   They will blockade <nick>'s isle in 48 hours.
   Effect: No voyages may launch from that island for 24h.
   Gold income from all sources halved for 24h.
```

**Strategic implication:** High-Notoriety players must either:
- Dump Notoriety by running humble NPC missions.
- Ask for "protection" (other players keep crew home to help defend, but this costs them mission time).
- Take the hit.

This prevents one player from snowballing indefinitely.

---

## 12. Anti-Snowball Measures

| Mechanic | Description |
|----------|-------------|
| **Desertion** | Unpaid players bleed Regular Crew daily. |
| **Upkeep** | Buildings drain gold constantly. Must play to maintain power. |
| **Vaults** | Even a broke player with Vault L2 only loses 25% of gold max. |
| **Crew capture** | Successful defenders steal attacker workforce. The rich get raided for crew, not gold. |
| **Navy** | Winningest player gets punished by bot. |
| **New player shield** | 48h immunity to player raids. Starting Cove L1 gives hidden defense. |
| **Crew soft cap** | Regular Crew beyond 12 cost **double upkeep** (10g/day each instead of 5g). This makes massive crews expensive to maintain. |
| **Season reset** | Everyone starts fresh in a new sea. Only Legends carry over. |

---

## 13. Season System

**Season length:** 14 days (configurable).

**Season end announcement:**
```
🌅 THE TORTUGA ISLES ARE PLUNDERED OUT.
   The rum is drunk. The gold is spent. The captains are bored.

🏆 TORTUGA SEASON AWARDS:
   Gold King: Alice (3,200g held)
   Raid Lord: Bob (14 successful raids)
   The Fortress: Dave (survived 11 raids, 0 breaches)
   Notorious: Carol (102 Notoriety)

All participants earn: Legend: Tortuga Holds

⚓ The fleet sets sail for THE BLACK SEA at dawn...
   New waters. New rules. Same grudges.
```

**What carries over:**
- Profile Legends (badges).
- Career stats (total raids, defenses, gold plundered — for bragging rights).
- +1 starting Regular Crew per previous season played (max +3). So a 3-season veteran starts with 5 Regular + 2 Loyal = 7 crew instead of 5.

**What resets:**
- Gold, Rum, Crew (except the +1 bonus).
- Buildings.
- Notoriety.
- All active voyages are cancelled/returned.
- Prisoners are released.

**Per-Sea Mechanics:** Each sea changes one rule:

| Sea | Mechanic |
|-----|----------|
| **Tortuga Isles** | Standard rules. |
| **Black Sea** | Storms add 1–2 hours to all voyage ETAs. Lookouts become essential. |
| **Crimson Archipelago** | Navy is hyper-aggressive. Any PvP raid alerts the Navy to **both** islands within 24h. |
| **Sargasso Depths** | Unpaid crew form bot "mutiny fleets" that attack random islands. |
| **Frozen North** | Crew consume double rum (2 rum per crew per day). But all voyages yield +50% gold. |
| **Shattered Reef** | Cove buildings are 50% less effective (scouts see hidden crew 50% of the time). |

The module stores the current sea in the versioned game state and applies the appropriate rule set
to voyage generation, combat, payment/rollover, and announcements.

---

## 14. False Flags

Late-game social poison toy.

| | |
|---|---|
| **Cost** | 150g |
| **Command** | PM Menu option during voyage setup, or `!flag <player_nick>` then send voyage |
| **Effect** | Your next *quiet* voyage appears to depart from the flagged player's island — in the channel departure line and in `!here` alike |
| **Limit** | Once per 24 hours per player |
| **Reveal** | On arrival: *"Wait... those are ALICE'S colors! FALSE FLAG!"* |

A public `!raid <nick>` declaration names the attacker by definition, so it never spends a held
flag — the flag keeps until there is a departure actually worth disguising. The flag stores the
target's canonical `nick_cache`, so a forged departure is indistinguishable from a real one.

This is purely social. It does not change combat math. It just makes Bob look guilty. A false flag
must never alter the stable target UUID or conceal the actual attacker from the resolving module.

---

## 15. Persistent State and Lifecycle

The module does not create SQLite tables. It uses the host's namespaced KV functions with one
bounded JSON blob under the key `data`. The host persists that KV data in its database and applies
`data_delete` mutation plans transactionally.

The logical state is versioned and must remain forward-compatible:

```text
State {
    schema_version: integer,
    games: map<server/channel, Game>,
    pm_sessions: map<server/profile_uuid, PmState>,
    next_id: integer
}

Game {
    sea, season_started, season_index,
    players: map<profile_uuid, Player>,
    voyages, prisoners, ransoms,
    recent_public_departures,
    scheduler_state
}
```

Every collection and text field is bounded. At minimum, cap players, active/resolved voyages,
prisoners, ransom offers, recent departures, Legends, PM sessions, nick caches, and serialized
voyage results. Reject malformed relevant state rather than replacing it with defaults.

`Player` stores gameplay resources, buildings, timers, career/season counters, and a cached display
name. It does not store identity under the cached name. Voyage records store both regular and loyal
crew sent, target profile UUID where applicable, public/private status, timestamps, resolution
state, and an idempotent result. Cross-player references use UUIDs and must be handled explicitly
when a profile is deleted.

The module must export the standard lifecycle hooks:

- `data_export(String)` returns a versioned `ModuleDataResponse` for the requested server/profile,
  including relevant player state, active voyages, prisoners, ransom references, and PM state.
- `data_delete(String)` returns a versioned, idempotent `ModuleDataDeletePlan`. It must validate
  the host-supplied entries, isolate by server, include profile UUID plus legacy aliases, remove
  the subject's player and sessions, cancel their voyages, release or safely resolve prisoners and
  ransom offers, and rewrite aggregate state without deleting unrelated captains.
- The hook must never mutate KV directly; the host applies its returned mutations transactionally.

Channel-owned recurring jobs remain channel state. Player-owned voyage jobs should set
`owner_profile_id` so host profile deletion can remove them safely.

---

## 16. Scheduler Events

The module uses the durable host functions `schedule_set`, `schedule_cancel`, and `schedule_list`.
Jobs are one-shot unless the handler explicitly schedules the next occurrence. Job IDs are stable,
server/channel-qualified, and idempotent; payloads are bounded JSON. The scheduler survives bot
restarts and delivers each job through `on_event` as `Event::Timer`.

| Job | Payload | Fires When |
|-----|---------|------------|
| `pirate:v1:{server}:{channel}:voyage:{id}` | `{ "voyage_id": integer }` | When a voyage's `returns_at` is reached. Owner is the voyage captain's profile UUID. |
| `pirate:v1:{server}:{channel}:daily` | `{}` | At the next configured daily rollover; the handler schedules the following day after a successful state commit. |
| `pirate:v1:{server}:{channel}:navy_announce` | `{}` | 48 hours before a Navy blockade. |
| `pirate:v1:{server}:{channel}:navy_hit` | `{ "target_uuid": string }` | When the Navy arrives. |
| `pirate:v1:{server}:{channel}:season_end` | `{}` | At the configured season end. |
| `pirate:v1:{server}:{channel}:loyal_return:{uuid}` | `{ "profile_id": string }` | When loyal crew return from the cove. |

**Voyage return resolution:**
1. Load the state blob and validate the event's server/channel and payload.
2. Look up the voyage by ID; an unknown or already-resolved job is a successful no-op.
3. If raid/scout, resolve combat or intel using host randomness; otherwise resolve the NPC mission.
4. Return surviving crew and persist the resolved result before sending any announcement.
5. Post a themed public-safe result to the channel and retain private details for the captain's
   catch-up report. `Event::NickChanged` and later messages must refresh that cache.
6. The player must `!collect` (or use the PM menu) to claim stored rewards.

Every timer handler must be safe to retry. It must not award stats, loot, prisoners, or public
announcements twice if the host redelivers a job.

---

## 17. Module Contract and Capabilities

The module must use the actual rustjeeves WASM exports and ABI. It must not invent separate
`on_command`, `on_pm`, or `on_timer` callbacks.

Required exports:

```text
commands() -> FnResult<String>       # CommandManifest JSON
settings() -> FnResult<String>       # SettingsManifest JSON
on_message(String) -> FnResult<()>   # EventEnvelope containing Event::Message
on_event(String) -> FnResult<()>      # EventEnvelope containing Event::Timer
achievements() -> FnResult<String>   # AchievementManifest JSON
achievement_backfill(String) -> FnResult<String>
data_export(String) -> FnResult<String>
data_delete(String) -> FnResult<String>
```

`init()` is optional and may log startup, but it has no server/channel context and must not be
used to schedule game jobs or resolve overdue voyages. Every command handled by the module must
be declared in `commands()` with a name, description, usage, and any built-in aliases. The
command manifest includes both public commands and PM-only commands so `!help` and the TUI alias
editor remain accurate.

The planned implementation uses these host functions, with only actually-used capabilities granted in
`module-capabilities.toml`:

```text
send_message, theme, kv_get, kv_set, now, setting_get,
random_bytes, schedule, irc_casefold, profile_get, award_stats, log
```

If implementation chooses to call `profile_ensure`, it must add that capability explicitly. There
is no module SQLite API, `get_nick`, `get_uuid`, `reply_channel`, `reply_pm`, `random_range`, or
general network access. The host supplies `MessagePayload.user_id`, `nick`, `display`, `target`,
`is_private`, and `role`; target nick resolution uses the profile service and a server-local
cached display name only as a display aid, never as identity.

Every IRC-visible string, including errors, menu text, scheduled announcements, and private
results, goes through `theme` using a namespaced key such as `pirate.voyage_return`. Dynamic values
are passed as theme variables. Internal diagnostics may use logging directly.

Randomness comes from `random_bytes`; the module may seed a local bounded PRNG from those bytes for
one event. Time comes from `now`. State-changing operations validate input lengths, numeric ranges,
channel/PM context, target existence, active-voyage limits, player caps, and available crew before
mutating the blob. Achievement awards happen only after the corresponding state mutation is
persisted and use the stable profile UUID.

The settings manifest must include a channel-scoped, default-off `enabled` boolean and every
operator-tunable gameplay value. Runtime values are read with `setting_get` and clamped against
the manifest's declared ranges.

### Achievements

The versioned achievement manifest should expose reliable career stats such as `voyages`,
`raids_won`, `defenses_won`, `gold_plundered`, `prisoners_taken`, `prisoners_marooned`,
`seasons_played`, and `rum_collected`. Finite milestones may be public; brutal or socially
dependent milestones should be optional or secret. The module must export a pure,
idempotent `achievement_backfill` because these totals live in the KV state. Backfill returns
absolute `set_max` values scoped to the requested server and stable profile UUIDs; it never awards
new gameplay effects or mutates KV.

---

## 18. Message Templates

These are the exact strings the bot should use. Keep them pirate-themed.

**Voyage departure (channel):**
```
⚓ <nick>'s <N> crew cast off into the mist... destination unknown.
```

**Public raid declaration (channel):**
```
🏴‍☠️ <nick> DECLARES WAR ON <target>! <N> crew have cast off!
```

**Voyage return (PM):**
```
Your <N> crew have returned from the <mission>!
   Loot: <reward>
   Crew lost: <N>
   Use !collect to claim your spoils.
```

**Raid resolution (channel):**
```
💥 <attacker>'s fleet descends on <defender>'s isle!
   <attacker>: <N> crew | <defender>: <M> crew defending
   ⚔️ COMBAT...
   🛡️ <winner> WINS! <loser> loses <N> crew.
   <defender> captures <N> prisoners. Salvages <G> gold.
```

**Crushing defeat (channel):**
```
💥 <attacker>'s fleet descends on <defender>'s isle!
   ⚔️ CRUSHING DEFENSE! <defender>'s fortress obliterated the raid!
   <attacker> lost ALL <N> crew (captured!)
   <defender> salvaged: <G> gold, <N> prisoners
   <attacker> gains: "Humiliated" debuff (-2 Notoriety, -10% attack for 24h)
```

**Payday (channel):**
```
🍺 PAYDAY comes to <sea>!
   Paid: <nicks>
   Unpaid: <nicks> (<days> days — crew are deserting!)
```

**Navy announcement (channel):**
```
🚢 THE ROYAL FLEET HAS BEEN SIGHTED.
   They will blockade <nick>'s isle in 48 hours.
   No voyages may launch. Gold income halved for 24h.
```

**Season end (channel):**
```
🌅 THE <SEA> ARE PLUNDERED OUT.
   The fleet sets sail for <NEW_SEA> at dawn.

🏆 <OLD_SEA> AWARDS:
   Gold King: <nick> (<gold>g)
   Raid Lord: <nick> (<raids> raids)
   The Fortress: <nick> (<defenses> defenses, <breaches> breaches)
   Notorious: <nick> (<notoriety> Notoriety)

All earn: Legend: <old_sea> Holds
```

---

## 19. Tuning Levers

These are the numbers most likely to need adjustment after the first playtest. Document them clearly so the coding agent can make them configurable.

| Variable | Default | Description |
|----------|---------|-------------|
| `STARTING_GOLD` | 200 | New player gold. |
| `STARTING_RUM` | 20 | New player rum. |
| `STARTING_REGULAR_CREW` | 3 | New player regular crew. |
| `LOYAL_CREW_COUNT` | 2 | Indestructible crew. |
| `CREW_WAGE_GOLD` | 5 | Gold cost per crew per day. |
| `CREW_WAGE_RUM` | 1 | Rum cost per crew per day. |
| `CREW_SOFT_CAP` | 12 | Regular crew above this cost double wages/upkeep. |
| `PLAYER_CAP` | 6 | Maximum captains in one game channel. |
| `MAX_ACTIVE_VOYAGES` | 2 | How many voyages a player can have running. |
| `SEASON_LENGTH_DAYS` | 14 | Days per season. |
| `NEW_PLAYER_SHIELD_HOURS` | 48 | Raid immunity for new players. |
| `NAVY_INTERVAL_DAYS_MIN` | 3 | Minimum days between Navy events. |
| `NAVY_INTERVAL_DAYS_MAX` | 4 | Maximum days between Navy events. |
| `DAILY_ROLLOVER_HOUR_UTC` | 0 | When payday fires. |
| `VOYAGE_OPTIONS_COUNT` | 3 | How many options the PM menu presents. |
| `RAID_GOLD_PCT_VICTORY` | 15 | % of vulnerable gold stolen on Victory. |
| `RAID_GOLD_PCT_CRUSHING` | 25 | % of vulnerable gold stolen on Crushing Victory. |
| `CREW_LOSS_PCT_DEFEAT` | 50 | % of sent crew lost on Defeat. |
| `NOTORIETY_PUBLIC_RAID` | 2 | Notoriety gained for public raid declaration. |
| `NOTORIETY_MAROON` | 3 | Notoriety gained per marooned prisoner. |
| `NOTORIETY_ABANDON` | -1 | Notoriety lost for abandoning prisoners. |
| `FALSE_FLAG_COST` | 150 | Gold cost to false-flag a voyage. |
| `FALSE_FLAG_COOLDOWN_HOURS` | 24 | Minimum time between false flags. |
| `SCOUT_STALE_HOURS` | 2 | How old scout intel is. |
| `SCOUT_INTEL_HOURS` | 12 | How long a collected scout report stays raidable. |
| `RAID_MERCY_HOURS` | 12 | How long a raided isle leaves the target pool. 0 disables. |
| `LOYAL_COVE_COOLDOWN_HOURS` | 6 | How long Loyal Crew hide after a lost raid. |
| `HUMILIATED_DEBUFF_HOURS` | 24 | Duration of -10% attack debuff after Crushing Defeat. |
| `DISLOYAL_SCOUT_PENALTY_PCT` | 5 | Defense penalty per unpaid day (capped at 25%). |

---

## 20. Edge Cases

| Situation | Resolution |
|-----------|------------|
| **Two players raid the same empty target** | First arrival (by `returns_at` timestamp) gets the loot. Second arrival finds an already-looted island, gets nothing, but still risks crew loss if the target has hidden Cove crew. |
| **Player raids someone who has since gone shielded** | If the target gained a new-player shield after the voyage launched, the raid still resolves normally. Shields only block *new* voyage launches. |
| **Player has 0 crew and tries to voyage** | They can still send their 2 Loyal Crew on Pressgang. All other voyages require at least 1 Regular or 2 Loyal. |
| **Bot restarts mid-voyage** | Scheduler jobs remain in the host database. On the first message or timer event with a server/channel context, the module may scan the KV state for overdue unresolved voyages and resolve them idempotently; `init()` cannot do this because it has no channel context. |
| **Season ends with active voyages** | All active voyages are force-resolved immediately with their current results. Rewards are auto-collected. Then the season reset happens. |
| **Player deletes their profile** | `data_delete` returns a pure mutation plan over the supplied `data` entry. Their sessions and player record are removed, their active voyages are cancelled, their held prisoners are released or returned according to an explicit deterministic rule, and ransom references are rewritten without affecting unrelated players. |

---

## 21. MVP Scope

For the first working version, implement in this order:

1. **Contract scaffold** — actual exports, settings manifest, capability policy, themed reply helper,
   versioned KV state, stable UUID handling, lifecycle hooks, and achievement manifest.
2. **Game admission and displays** — enabled channel gate, player cap, player initialization,
   `!crew`, `!here`, `!captain`, and bounded error/usage responses.
3. **Payday** — `!pay`, `!rum`, one durable daily job, upkeep/desertion rules, and a replay-safe
   rollover announcement.
4. **NPC voyages** — PM menu, Merchant/Rum/Pressgang/Explore only, exact regular/loyal crew split,
   durable return jobs, stored results, `!collect`, and overdue-job recovery.
5. **Lifecycle and achievements** — export/delete plans, profile deletion edge cases, successful
   operation awards, and idempotent historical backfill.
6. **Buildings** — Vault, Cove, Walls, Tavern, Shipyard, with purchase validation and tests.
7. **Player raids and scouting** — combat math, zero-warning resolution, public raid declaration,
   prisoners, and concurrent-arrival tests.
8. **Prisoner economy** — ransom, pressgang, maroon, and deletion/abandonment interactions.
9. **Navy event** — announcement, blockade effect, replay-safe follow-up job, and Crimson behavior.
10. **Season system** — 14-day timer, awards, reset, Legends, and sea rules.
11. **False flags** — bounded social misdirection toy with cooldown and no combat effect.

PvP raiding (step 7) is the first "fun" feature. Everything before it is infrastructure. Get the
contract, lifecycle, NPC voyages, and payday rock-solid before touching player combat.

## 22. Definition of Done

The module is not ready for installation until all of the following are true:

```text
[ ] No invented callbacks or direct SQLite access; the actual jeeves-abi is used.
[ ] commands(), settings(), on_message(), on_event(), achievements(),
    achievement_backfill(), data_export(), and data_delete() are present as required.
[ ] Every handled command is declared with useful description, usage, and aliases.
[ ] Every IRC-visible string uses a namespaced theme key and bounded variables.
[ ] State is a versioned, bounded KV blob partitioned by server/channel and stable UUID.
[ ] Empty user_id never falls back to a persistent nick identity.
[ ] Timer IDs/payloads are server/channel-qualified and handlers are retry-safe.
[ ] Profile export/delete is pure, server-isolated, alias-aware, and idempotent.
[ ] Achievement awards occur only after the underlying state mutation is persisted.
[ ] module-capabilities.toml grants only capabilities the implementation calls.
[ ] cargo fmt --all --check passes.
[ ] cargo fmt --manifest-path modules-src/pirate/Cargo.toml --check passes.
[ ] cargo test --manifest-path modules-src/pirate/Cargo.toml passes.
[ ] cargo clippy --manifest-path modules-src/pirate/Cargo.toml -- -D warnings passes.
[ ] ./build-modules.sh pirate builds and installs modules/pirate.wasm.
[ ] Host integration tests load the WASM and exercise channel, PM, timer, lifecycle,
    capability, replay/idempotency, and multi-server/channel isolation paths.
```

---

*End of PLAN-PIRATE.md*
