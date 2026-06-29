# Future module backlog

This is the design backlog for shared foundations and modules beyond the completed v5 utility work.
Checking a box means the behavior is implemented and verified, not merely discussed. Completed
design sections remain here as implementation records until they are consolidated into `PLAN.md`.

## Priority decisions

Build shared operational foundations before adding more feature modules:

- [x] Build module settings and per-network/per-channel enablement next.
- [x] Add the durable scheduler and prove it with self-reminders.
- [x] Add the host randomness capability before adding new games.
- [x] Address outbound rate limiting, IRC output sanitization, and CTCP hygiene as reliability
      foundations before the bot is run publicly.
- [x] Choose either darts or the six-letter word game as the next independent game. (Chose darts.)
- [x] Hunt and roadtrip implemented with `enabled = false` default for operator control.
- [ ] Keep achievements last, after several modules emit useful milestone events.

Suggested implementation order, based on dependencies and risk:

1. Module settings and channel activity policy
2. Durable scheduler foundation
3. Self-reminders, then consent-based reminders for other users
4. Host randomness capability
5. Outbound rate limiting, host-side IRC output sanitization, CTCP hygiene
6. Darts or the six-letter word game
7. Hunt
8. Roadtrip
9. Achievements

Aliases, memos, sed corrections, and clock are complete. Reminders, hunt, and roadtrip all need the
same durable scheduler and should not each invent their own timer system. Hunt and roadtrip also
depend on module settings because spontaneous channel output must be explicitly enabled.

## Shared foundations

### Module settings and enablement

**Assessment:** Foundation implemented; continue migrating hardcoded module values as useful.
Modules previously hardcoded settings such as cooldowns and retention,
while spontaneous modules need operator-controlled channel access. This should be one host service,
not a separate configuration format invented by each module.

- [x] Add versioned setting metadata to module manifests: key, type, description, default, scope,
      bounds, and whether changing it takes effect immediately.
- [x] Support global, per-network, and per-channel values with a documented precedence order:
      channel → network → global → module default.
- [x] Add host functions for modules to read their own effective settings without granting access
      to another module's settings.
- [x] Persist operator overrides in SQLite separately from module defaults so values survive a
      module being temporarily absent.
- [x] Add a TUI settings screen grouped by module and scope, with validation before saving.
- [x] Add standard per-module/per-channel enablement; future spontaneous modules must advertise
      `enabled = false` as their default.
- [x] Apply safe setting changes immediately without reconnecting or reloading modules.
- [ ] Retain unknown/temporarily unavailable settings but clearly mark them inactive in the TUI.
- [x] Log setting changes without exposing secrets or unrelated configuration.
- [ ] Test precedence, validation, persistence, module unload/reinstall, and concurrent reads.

Initial types should remain deliberately small: boolean, bounded integer, duration, and bounded
string/choice. Secrets belong in the existing integrations system, not ordinary module settings.

### Data lifecycle and privacy

- [ ] Define which profile and module data users may inspect, clear, or opt out of.
- [ ] Add operator reset/export tools that do not require direct SQLite editing.
- [ ] Require explicit retention behavior for user-generated text and scheduled jobs.
- [ ] Ensure deleting a stable profile either removes or deliberately anonymizes dependent module
      state without leaving dangling identities.

### Shared game services

- [x] Add a narrow host randomness capability before implementing more games; do not make every
      module seed a predictable PRNG from the current timestamp.
- [x] Generate central `!help` output from command manifests so installed modules are discoverable.
- [ ] Defer a generic cross-module event API until at least two concrete consumers need it;
      achievements alone should not force a broad event bus design.

### Command registry and customizable aliases

**Assessment:** This is a useful host feature and should not be hardcoded separately into every
module. A central registry makes aliases easy to edit, gives the TUI an authoritative list of
installed commands, and can later drive richer `!help` output.

Example configuration:

```text
weather    w,weath
search     g,google
translate tr
```

Proposed design:

- [x] Add an optional module export that returns command metadata as ABI-versioned JSON.
- [x] Include the canonical command, owning module, description, usage, and built-in aliases.
- [x] Build and refresh a host command registry whenever modules load, reload, or unload.
- [x] Store operator-defined aliases in SQLite separately from module defaults.
- [x] Add a TUI Commands/Aliases screen listing canonical commands and an editable comma-separated
      alias field.
- [x] Apply TUI edits immediately without restarting the bot or rebuilding a module.
- [x] Match aliases case-insensitively and rewrite only the first exact command token for the
      command's owning module, preserving all arguments unchanged (`!w London` becomes
      `!weather London`).
- [x] Continue sending the original message to passive modules such as history, so quotes and logs
      preserve what the user actually typed.
- [x] Keep command prefixes explicit: an alias entered as `w` represents `!w`, not ordinary chat.
- [x] Reject aliases containing whitespace, commas, control characters, or the command prefix.
- [x] Reject collisions with canonical commands, aliases owned by another command, and reserved
      host/admin commands; show a useful conflict message in the TUI.
- [x] Remove stale registry entries when a module unloads, while retaining their configured aliases
      so they return if the module is reinstalled.
- [x] Log alias changes without logging secrets or unrelated configuration.
- [x] Test argument preservation, case handling, collisions, persistence, and owner-only rewriting.
- [x] Test unload/reinstall lifecycle behavior for retained overrides.

Aliases should resolve directly to a canonical command once; aliases must never point to other
aliases. The registry identifies the owning module, which receives a canonicalized copy of the
event, while every other module receives the untouched event. Initial scope should be global to the
bot. Per-network or per-channel aliases can be added later if a real need appears, but introducing
that scope now would make conflicts and the TUI much harder to understand.

Existing hardcoded alternatives such as `!g`, `!google`, `!search`, `!tr`, and `!translate` can be
registered as module defaults first. Once the registry is proven, their duplicate parsing can be
removed from the modules without breaking existing installations.

### Durable scheduler

Required by reminders, hunt, and roadtrip.

- [x] Add host-owned durable scheduled jobs, persisted in SQLite.
- [x] Address jobs by module, server, channel, stable job ID, and due timestamp.
- [x] Deliver a timer event to the owning module without granting general host access.
- [x] Restore overdue/future jobs after restart or module reload.
- [x] Support cancellation and replacement without duplicate delivery.
- [x] Enforce per-module job quotas, bounded payloads, and a maximum scheduling horizon.
- [x] Let operators inspect pending/failed jobs without exposing private reminder text.
- [x] Log creation, cancellation, overdue delivery, and permanent failure with stable job IDs.
- [x] Define sensible behavior for overdue jobs: fire once shortly after startup, never repeatedly.
- [ ] Test restart recovery, cancellation races, duplicate IDs, clock changes, malformed persisted
      jobs, and module unload/reinstall.

The scheduler belongs in the host because WASM modules only run when an IRC event invokes them.
Polling on ordinary channel messages would make reminders late and spontaneous games unreliable.

### Channel activity policy (implemented through module settings)

Hunt and roadtrip speak without being directly commanded, so operators need control over noise.

- [x] Add per-module/per-channel enablement for spontaneous activity.
- [ ] Add configurable minimum and maximum intervals.
- [x] Default spontaneous modules to disabled until explicitly enabled.
- [ ] Enforce one active spontaneous event of each type per channel.
- [ ] Provide admin commands to enable, disable, cancel, and inspect state.

### Outbound rate limiting

IRC servers disconnect clients that send messages too quickly. As more modules accumulate and
scheduler deliveries can produce bursts, uncontrolled send rates are a reliability risk.

- [x] Add a per-network leaky-bucket rate limiter inside the IRC actor.
- [x] Queue outbound messages behind the bucket rather than dropping them when it is temporarily
      empty.
- [x] Cap the outbound queue size per network and log clearly when messages are dropped at that
      limit.
- [x] Choose conservative defaults (e.g. one line per 500 ms, burst of four) and expose them as
      network-level settings once the settings system is mature enough.
- [ ] Test burst behavior, queue backpressure, and drain after reconnect.

### Host-side IRC output sanitization

The common rules require modules to strip IRC control/newline characters and respect line-length
limits, but enforcement is per-module convention rather than a host guarantee. A misbehaving or
new module can send malformed output or trigger a server disconnect.

- [x] Strip `\r` and `\n` from all outbound `PRIVMSG`/`NOTICE` text in the IRC actor or in
      `dispatch_action`, regardless of module source.
- [x] Truncate lines that would exceed 510 bytes after encoding (leaving room for the `:prefix `
      header that the server prepends).
- [x] Log a warning when truncation occurs so the offending module can be identified and fixed.
- [ ] Document that modules should still apply their own limits for semantic correctness (e.g.
      avoiding mid-sentence truncation at the host boundary), but the host is the safety net.

### Protocol hygiene

Small IRC protocol obligations that improve interoperability and operator experience.

- [x] Respond to `CTCP VERSION` with a brief bot name and version string.
- [x] Consider responding to `CTCP PING` for latency measurement by other clients.

### Common rules

- Use stable profile IDs, never nicknames alone, for ownership and scores.
- Keep all state scoped by server and channel unless a command explicitly says otherwise.
- Never consume or reveal private-message history through a channel command.
- Route every posted wrapper, error, announcement, and help line through `theme.toml`.
- Cap stored text, queue sizes, command frequency, and output length.
- Treat module reloads and bot restarts as normal operation, not exceptional cases.
- Sanitize IRC control/newline characters and respect IRC line-length limits.

---

## Memos (`memos.wasm`)

**Assessment:** Strong fit. This is useful IRC-native behavior, needs no external API, and exercises
stable identity and channel-local persistence without requiring new host infrastructure.

### Commands

```text
!tell <nick> <message>
!memos
!memos clear
```

Example delivery:

```text
Ah, a message for you, Alice — Bob said 2 hours ago: remember the logs.
```

### Proposed behavior

- [x] `!tell Alice message` queues a memo for Alice on the current server and channel.
- [x] Resolve the recipient to a stable profile ID when possible.
- [x] Deliver queued memos the next time the recipient sends a public message in that channel.
- [x] Attribute each memo to its sender and include a human-readable age.
- [x] Deliver pending memos in order, in bounded batches to prevent flooding.
- [x] Never deliver channel memos in private or in another channel.
- [x] Permit recipients to inspect their waiting count and clear their pending memos.
- [x] Expire old memos after a configurable global/network/channel period, default 30 days.
- [x] Limit memo length and pending memos per sender/recipient/channel.
- [x] Reject or specially handle self-memos so accidental typos are clear.

### Open decisions

- Implemented: any public message triggers delivery except `!memos` management commands.
- Implemented: memos are delivered individually, up to three per message; overflow remains queued.
- Should admins be able to remove abusive queued messages before delivery?

### Admin visibility (deferred)

Memos are stored as opaque KV blobs inside `memos.wasm`; the host has no structured view of them,
so they cannot appear in the TUI scheduler screen without the module's cooperation. The right
approach is to add super-admin commands inside `memos.wasm` rather than exposing raw KV data to
the host:

- [x] Add `!memos admin list <nick>` (super-admin only): show pending memos queued for that nick
      in the current channel, including sender, age, and a preview, without delivering them.
- [x] Add `!memos admin clear <nick>` (super-admin only): discard all pending memos for that nick
      in the current channel and log the action.
- [x] Theme the admin output separately so it is clearly marked as an admin view.
- [ ] Consider whether the TUI should surface a summary count per channel via a future module-data
      export capability, once a second concrete consumer justifies the design.

---

## Hunt (`hunt.wasm`)

**Assessment:** Fun and well matched to the bot’s themed personality. The main risk is unsolicited
channel noise, so per-channel opt-in and conservative timing are requirements, not polish.

### Commands

```text
!hunt
!hug
!hunt score [nick]
!hunt top
```

### Proposed behavior

- [x] At a random scheduled time, release one animal into an enabled channel.
- [x] Start with cats, puppies, and ducks (plus more defaults; full list in `hunt.animals` theme key).
- [x] Put animal names and announcement variations in `theme.toml` lists.
- [x] Record the selected animal in durable event state so reloads do not change it.
- [x] The first valid `!hunt` or `!hug` resolves the event; later attempts get a themed miss line.
- [x] `!hunt` adds one hunted count for that animal and user.
- [x] `!hug` adds one hugged count for that animal and user.
- [x] Track totals by stable profile and channel (counts are theme-stable: animal names can change
      without resetting scores).
- [x] `!hunt score` shows both hunted and hugged totals; `!hunt top` shows the leaderboard.
- [x] Schedule the next release only after the current event is resolved or expires.
- [x] Expire unattended animals after a configurable interval.
- [x] Add admin cancel and status commands (`!hunt cancel`, `!hunt status`). Enable/disable is via TUI settings.

### Open decisions

- **Resolved:** Default interval is 2–4 hours; all thresholds are theme/setting configurable.
- **Resolved:** All animals have equal odds from the theme pool.
- Disapproving theme text for hunting puppies/cats is left to theme.toml customization.

---

## Reminders (`reminders.wasm`)

**Assessment:** Self-reminder MVP implemented as the first consumer of the durable scheduler.
Parsing human
durations and preventing reminders aimed at other people from becoming harassment need care.

### Commands

```text
!remind me in 1 hour to talk
!remind Alice in 1 hour to talk
!reminders
!remind cancel <id>
```

### Proposed behavior

- [x] Ship self-reminders first and prove scheduler recovery before enabling reminders aimed at
      another user.
- [x] Parse combinations such as `10 minutes`, `1 hour`, `2 days`, and `1h30m`.
- [x] Resolve self-reminder ownership to stable profile IDs.
- [x] Persist requester, target, server, channel, due time, text, and reminder ID.
- [x] Deliver in the channel where the reminder was created.
- [x] Survive restarts and fire overdue reminders once.
- [x] Allow requesters to list and cancel reminders they created.
- [ ] Allow recipients to disable reminders from other users while retaining self-reminders.
- [x] Set maximum text length, maximum future horizon, and queue limits.
- [x] Reject zero, negative, nonsensical, or excessively distant durations.
- [x] Theme confirmations, deliveries, parsing errors, and cancellation output.

### Open decisions

- Reminders for another user remain deferred until recipient opt-out/consent is implemented. If
  enabled later, announce at the due time; use `!tell` for next-seen delivery.
- Should admins be able to inspect all reminders in a channel? Recommendation: IDs and due times,
  but not necessarily private reminder text.

---

## Roadtrip (`roadtrip.wasm`)

**Assessment:** Charming, but it is the most stateful and potentially noisy proposal. Build it after
the scheduler has proven reliable in reminders and hunt.

### Commands

```text
!roadtrip
!roadtrip join
!roadtrip status
!roadtrip cancel        # admin
```

### Proposed flow

1. Jeeves announces a proposed random destination in an enabled channel.
2. A short signup window opens, suggested 60–90 seconds.
3. While signup is open, `!roadtrip join` adds that user once.
4. Jeeves announces the final passenger list and departure.
5. The trip lasts a random 30–60 minutes.
6. Jeeves announces the return and a themed activity based on party size.

### Implementation checklist

- [x] Persist the destination, signup deadline, passengers, departure, and return job.
- [x] Scope `!roadtrip join` to an open signup.
- [x] Use stable profile IDs while retaining current display names for announcements.
- [x] Put destinations and response variations in `theme.toml` lists (`roadtrip.destinations`).
- [x] Use separate completion themes for one, two, and three-or-more travelers.
- [x] Cancel cleanly if nobody joins.
- [x] Prevent duplicate joins and simultaneous trips in one channel.
- [x] Recover an in-progress trip after restart without announcing departure twice.
- [x] Add manual start/status/cancel controls in addition to optional spontaneous trips.
- [x] Cap passenger-list output and format long lists safely.

### Open decisions

- **Resolved:** Both spontaneous and manual modes implemented; spontaneous is `enabled = false` by
  default; `!roadtrip` manual start always works regardless of the enabled setting.
- **Resolved:** Destinations are Victorian/Wodehousian real-world and fictional British locations.

---

## Sed corrections (`history.wasm`)

**Assessment:** Small and useful, but easy to make annoying. Restricting corrections to the
speaker’s own previous line keeps attribution honest and avoids one user rewriting another.

### Syntax

```text
s/thing/thing2/
s/thing/thing2/g
s/thing/thing2/i
```

Example:

```text
What Alice meant to say is: full sentence with thing2 replacing thing.
```

### Proposed behavior

- [x] Reuse each user’s latest non-command public line per server/channel from `history.wasm`.
- [x] Parse escaped delimiters and optional `g` and `i` flags.
- [x] Apply the correction to the sender’s own latest line only.
- [x] Use Rust's bounded, linear-time `regex` implementation.
- [x] Refuse empty patterns, invalid expressions, no-op replacements, and oversized output.
- [x] Do not treat correction expressions as new source lines.
- [x] Strip unsafe IRC control/newline characters.
- [x] Add a short per-user cooldown.
- [x] Add a per-channel disable switch (`sed_corrections` Boolean setting, default true).
- [x] Theme success, no-match, no-history, invalid-expression, and cooldown responses.

### Open decisions

- Implemented: regex matching with strict pattern, replacement, compiled-regex, and output limits.
- Implemented: a corrected line replaces the cached original so chained corrections work.

---

## Six-letter Wordle (`wordle.wasm`)

**Assessment:** Good channel game with no scheduler dependency. The difficult parts are correct
duplicate-letter scoring, a legally reusable word list, and concise IRC feedback.

### Commands

```text
!word <six-letter-guess>
!word
!word stats [nick]
```

### Proposed behavior

- [x] Use exactly six-letter answers and guesses, as proposed here rather than standard Wordle.
- [x] Bundle separate curated answer and accepted-guess lists from a documented permissive source.
- [x] Exclude slurs and unsuitable surprise answers from the answer list.
- [x] Keep one shared unresolved word per server/channel.
- [x] Give each user three guesses per UTC day; persist attempts across restarts.
- [x] Carry the word over between days until somebody solves it.
- [x] Start a new word immediately after a solve and announce the solver and old answer.
- [x] Implement duplicate-letter scoring with the standard two-pass algorithm.
- [x] `!wordle` shows known correct positions, present letters, absent letters, and solve status.
- [x] A guess not in the accepted dictionary does not consume an attempt.
- [x] Track solves and guesses by stable profile ID.
- [x] IRC-safe ASCII feedback (colour-coded output).

Suggested compact status:

```text
Pattern: A _ _ L _ _ | present: E, R | absent: S, T, N
```

### Open decisions

- Whether all users share discovered hints. Recommendation: yes; it makes this a channel game
  rather than many private games using one answer.
- Whether the three-attempt allowance resets at UTC midnight or a configurable channel timezone.

---

## Darts (`darts.wasm`)

**Assessment:** Straightforward and likely robust. It needs a precise scoring model so “random
number” does not make exact finishes either trivial or nearly impossible.

### Commands

```text
!darts              # throw one dart
!darts 2            # throw two darts
!darts 3            # throw three darts
!darts score [nick]
!darts board
```

### Proposed behavior

- [x] One shared game per server/channel; users join implicitly on their first throw.
- [x] Every player starts at 301.
- [x] Model each dart as a board segment, multiplier, bull, or miss rather than uniform `1..60`.
- [x] Subtract the turn total when it does not take the player below zero.
- [x] A bust restores the score from the beginning of that turn.
- [x] No double-out requirement; reaching exactly zero wins.
- [x] Announce the winner, increment lifetime wins, then reset everyone to 301.
- [x] Apply a configurable per-user turn cooldown.
- [x] Persist active scores and lifetime wins across restarts.
- [x] Theme throws, misses, busts, score displays, and wins.
- [x] Test exact finishes, busts, multiple players, reset behavior, and cooldowns.

### Open decisions

- **Resolved:** One/two/three dart syntax retained (`!darts`, `!darts 2`, `!darts 3`).
- **Resolved:** Stale players are pruned on every throw after a configurable period (default 7d).

---

## Clock (`clock.wasm`)

**Assessment:** Implemented. Small, useful, and naturally complements stored profile locations. Coordinates are
not enough on their own: correct local time requires a timezone and daylight-saving rules.

### Commands

```text
!time
!time <nick>
```

### Proposed behavior

- [x] Extend geocoding/profile storage with an IANA timezone such as `America/New_York`.
- [x] Add a narrow host local-time service backed by a timezone database.
- [x] Update timezone data when a user changes their saved location.
- [x] `!time` reports the caller’s local time from their stored location.
- [x] `!time Alice` reports Alice’s local time when Alice has a saved location/timezone.
- [x] Handle daylight-saving transitions correctly; never derive timezone from longitude alone.
- [x] Theme success, missing-location, location-not-found, and service errors.
- [x] Avoid unnecessarily exposing the exact saved location in the response.
- [x] Test fractional-offset zones as well as daylight-saving boundaries.

### Open decisions

- Whether to store the IANA timezone at geocoding time or query an external timezone service on
  every command. Recommendation: store the timezone and use a host timezone database; it is faster,
  keyless, and deterministic.

---

## Achievements (`achievements.wasm`) — low priority / someday

**Assessment:** Fun cross-module progression, but deliberately deferred until the existing modules
and shared foundations are cleaned up. This will need a small, stable achievement-event API so
modules can report activity without directly editing another module's state.

### Commands

```text
!achievements [nick]
```

### Ideas and proposed behavior

- [ ] Store unlocked achievements against stable profile IDs so nick changes do not lose them.
- [ ] Award milestone achievements for using individual modules or commands a certain number of
      times.
- [ ] Support permanent notable-event achievements such as setting a new biggest-fish or
      longest-cast record; keep current leaders in separate leaderboard output.
- [ ] Add hunt achievements such as most animals hunted and most animals hugged when that module
      exists.
- [ ] Add roadtrip achievements such as joining a certain number of trips when that module exists.
- [ ] Let users list their own achievements and optionally view another user's public achievements.
- [ ] Announce newly unlocked achievements in the channel where they were earned.
- [ ] Make achievement names, descriptions, list output, and unlock announcements themeable.
- [ ] Persist the achievement definition/version that caused each unlock so later balance changes
      do not silently remove earned achievements.
- [ ] Prevent retries, reloads, alias usage, or duplicated events from incrementing progress more
      than once for one action.
- [ ] Keep the integration generic enough that future modules can define achievements without
      adding module-specific logic to the host.

### Open decisions

- Competitive record achievements are permanent for anyone who sets a record; current record
  holders belong in leaderboard output rather than revocable achievements.
- Decide whether announcements are always enabled or follow a future per-channel activity policy.
- Decide whether achievement definitions belong to the originating modules, the achievements
  module, or host-owned metadata. Prefer originating modules plus a narrow shared event API.

---

## Definition of done for every module

- [ ] Commands and edge cases have unit tests.
- [ ] State is partitioned correctly across servers and channels.
- [ ] Stable identity survives nick changes.
- [ ] Every posted line is themeable.
- [ ] Capability policy grants only required host functions.
- [ ] Reload/restart behavior is tested.
- [ ] Rate limits and output bounds are tested.
- [ ] Database migrations, ABI compatibility, and malformed persisted state are tested.
- [ ] IRC control characters are sanitized and output respects IRC line-length limits.
- [ ] `cargo test`, strict Clippy, release WASM build, and installation into `modules/` succeed.
- [ ] README, SPEC, PLAN, and this backlog are updated when behavior lands.
