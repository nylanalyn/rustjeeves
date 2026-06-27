# Future module backlog

This is the design backlog for modules we want to consider after the current v5 utility work.
Checking a box means the behavior is implemented and verified, not merely discussed.

## Priority decisions

The ideas have not yet been fully divided into “definitely build” and “maybe later.” Memos is the
first confirmed module:

- [x] Build memos first.
- [ ] Choose the remaining **build soon** modules.
- [ ] Move the remaining ideas into **someday**.
- [ ] Agree on whether spontaneous channel activity is enabled globally or per channel.

Suggested implementation order, based on dependencies and risk:

1. Memos (confirmed next)
2. Command registry and customizable aliases
3. Sed corrections
4. Clock/timezones
5. Durable scheduler foundation
6. Reminders
7. Darts
8. Six-letter Wordle
9. Hunt
10. Roadtrip

Memos and sed are self-contained. Clock needs a timezone service. Reminders, hunt, and roadtrip all
need the same durable scheduler and should not each invent their own timer system.

## Shared foundations

### Command registry and customizable aliases

**Assessment:** This is a useful host feature and should not be hardcoded separately into every
module. A central registry makes aliases easy to edit while also giving the TUI and `!help` an
authoritative list of installed commands.

Example configuration:

```text
weather    w,weath
search     g,google
translate tr
```

Proposed design:

- [ ] Add an optional module export that returns command metadata as ABI-versioned JSON.
- [ ] Include the canonical command, owning module, description, usage, and built-in aliases.
- [ ] Build and refresh a host command registry whenever modules load, reload, or unload.
- [ ] Store operator-defined aliases in SQLite separately from module defaults.
- [ ] Add a TUI Commands/Aliases screen listing canonical commands and an editable comma-separated
      alias field.
- [ ] Apply TUI edits immediately without restarting the bot or rebuilding a module.
- [ ] Match aliases case-insensitively and rewrite only the first exact command token for the
      command's owning module, preserving all arguments unchanged (`!w London` becomes
      `!weather London`).
- [ ] Continue sending the original message to passive modules such as history, so quotes and logs
      preserve what the user actually typed.
- [ ] Keep command prefixes explicit: an alias entered as `w` represents `!w`, not ordinary chat.
- [ ] Reject aliases containing whitespace, commas, control characters, or the command prefix.
- [ ] Reject collisions with canonical commands, aliases owned by another command, and reserved
      host/admin commands; show a useful conflict message in the TUI.
- [ ] Remove stale registry entries when a module unloads, while retaining their configured aliases
      so they return if the module is reinstalled.
- [ ] Log alias changes without logging secrets or unrelated configuration.
- [ ] Add tests for argument preservation, case handling, collisions, reloads, disabled modules,
      and alias chains.

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

- [ ] Add host-owned durable scheduled jobs, persisted in SQLite.
- [ ] Address jobs by module, server, channel, stable job ID, and due timestamp.
- [ ] Deliver a timer event to the owning module without granting general host access.
- [ ] Restore overdue/future jobs after restart or module reload.
- [ ] Support cancellation and replacement without duplicate delivery.
- [ ] Define sensible behavior for overdue jobs: fire once shortly after startup, never repeatedly.
- [ ] Test restart recovery, cancellation races, duplicate IDs, and clock changes.

The scheduler belongs in the host because WASM modules only run when an IRC event invokes them.
Polling on ordinary channel messages would make reminders late and spontaneous games unreliable.

### Channel activity policy

Hunt and roadtrip speak without being directly commanded, so operators need control over noise.

- [ ] Add per-module/per-channel enablement for spontaneous activity.
- [ ] Add configurable minimum and maximum intervals.
- [ ] Default spontaneous modules to disabled until explicitly enabled.
- [ ] Enforce one active spontaneous event of each type per channel.
- [ ] Provide admin commands to enable, disable, cancel, and inspect state.

### Common rules

- Use stable profile IDs, never nicknames alone, for ownership and scores.
- Keep all state scoped by server and channel unless a command explicitly says otherwise.
- Never consume or reveal private-message history through a channel command.
- Route every posted wrapper, error, announcement, and help line through `theme.toml`.
- Cap stored text, queue sizes, command frequency, and output length.
- Treat module reloads and bot restarts as normal operation, not exceptional cases.

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
- [ ] Expire old memos after a configurable period, suggested default 30 days.
- [x] Expire old memos after the initial fixed 30-day period.
- [x] Limit memo length and pending memos per sender/recipient/channel.
- [x] Reject or specially handle self-memos so accidental typos are clear.

### Open decisions

- Implemented: any public message triggers delivery except `!memos` management commands.
- Implemented: memos are delivered individually, up to three per message; overflow remains queued.
- Should admins be able to remove abusive queued messages before delivery?

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

- [ ] At a random scheduled time, release one animal into an enabled channel.
- [ ] Start with cats, puppies, and ducks.
- [ ] Put animal names and announcement variations in `theme.toml` lists.
- [ ] Record the selected animal in durable event state so reloads do not change it.
- [ ] The first valid `!hunt` or `!hug` resolves the event; later attempts get a themed miss line.
- [ ] `!hunt` adds one hunted count for that animal and user.
- [ ] `!hug` adds one hugged count for that animal and user.
- [ ] Track totals and per-animal breakdowns by stable profile and channel.
- [ ] `!hunt score` shows both hunted and hugged animals without producing an oversized line.
- [ ] Schedule the next release only after the current event is resolved or expires.
- [ ] Expire unattended animals after a configurable interval.
- [ ] Add admin enable/disable/cancel commands.

### Open decisions

- Pick a default appearance interval; something measured in hours is safer than minutes.
- Decide whether hunting puppies/cats deserves deliberately disapproving theme text.
- Decide whether animals have equal odds or configurable rarity.

---

## Reminders (`reminders.wasm`)

**Assessment:** High-value module and the best first consumer of a durable scheduler. Parsing human
durations and preventing reminders aimed at other people from becoming harassment need care.

### Commands

```text
!remind me in 1 hour to talk
!remind Alice in 1 hour to talk
!reminders
!remind cancel <id>
```

### Proposed behavior

- [ ] Parse combinations such as `10 minutes`, `1 hour`, `2 days`, and `1h30m`.
- [ ] Resolve reminder targets to stable profile IDs.
- [ ] Persist requester, target, server, channel, due time, text, and reminder ID.
- [ ] Deliver in the channel where the reminder was created.
- [ ] Survive restarts and fire overdue reminders once.
- [ ] Allow requesters to list and cancel reminders they created.
- [ ] Allow recipients to disable reminders from other users while retaining self-reminders.
- [ ] Set maximum text length, maximum future horizon, and queue limits.
- [ ] Reject zero, negative, nonsensical, or excessively distant durations.
- [ ] Theme confirmations, deliveries, parsing errors, and cancellation output.

### Open decisions

- Should reminders for another user announce at the due time even if that person is absent, or wait
  until they next speak like a memo? Recommendation: announce at the due time; use `!tell` for
  next-seen delivery.
- Should admins be able to inspect all reminders in a channel? Recommendation: IDs and due times,
  but not necessarily private reminder text.

---

## Roadtrip (`roadtrip.wasm`)

**Assessment:** Charming, but it is the most stateful and potentially noisy proposal. Build it after
the scheduler has proven reliable in reminders and hunt.

### Commands

```text
!roadtrip
!me
!roadtrip status
!roadtrip cancel        # admin
```

### Proposed flow

1. Jeeves announces a proposed random destination in an enabled channel.
2. A short signup window opens, suggested 60–90 seconds.
3. While signup is open, `!me` adds that user once.
4. Jeeves announces the final passenger list and departure.
5. The trip lasts a random 30–60 minutes.
6. Jeeves announces the return and a themed activity based on party size.

### Implementation checklist

- [ ] Persist the destination, signup deadline, passengers, departure, and return job.
- [ ] Scope `!me` to an open signup so it does not steal ordinary channel usage.
- [ ] Use stable profile IDs while retaining current display names for announcements.
- [ ] Put destinations and response variations in `theme.toml` lists.
- [ ] Use separate completion themes for one, two, and three-or-more travelers.
- [ ] Cancel cleanly if nobody joins.
- [ ] Prevent duplicate joins and simultaneous trips in one channel.
- [ ] Recover an in-progress trip after restart without announcing departure twice.
- [ ] Add manual start/status/cancel controls in addition to optional spontaneous trips.
- [ ] Cap passenger-list output and format long lists safely.

### Open decisions

- Whether spontaneous proposals should exist at all, or whether `!roadtrip` should always initiate
  them. Recommendation: support both, with spontaneous mode disabled by default.
- Whether locations are purely fictional, real-world, or a mixture.

---

## Sed corrections (`sed.wasm`)

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

- [ ] Cache each user’s latest non-command public line per server/channel.
- [ ] Parse escaped delimiters and optional `g` and `i` flags.
- [ ] Apply the correction to the sender’s own latest line only.
- [ ] Use a bounded Rust regex implementation, or explicitly document literal-only matching.
- [ ] Refuse empty patterns, invalid expressions, no-op replacements, and oversized output.
- [ ] Do not treat correction commands or bot output as new source lines.
- [ ] Strip unsafe IRC control/newline characters.
- [ ] Add a short per-user cooldown and per-channel disable switch.
- [ ] Theme success, no-match, no-history, invalid-expression, and cooldown responses.

### Open decisions

- Regex or literal matching. Recommendation: support regex with strict length limits because users
  expect sed syntax, but omit dangerous/unbounded features.
- Whether a corrected line replaces the cached original for chained corrections. Recommendation:
  yes, so a second correction works on the corrected sentence.

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

- [ ] Use exactly six-letter answers and guesses, as proposed here rather than standard Wordle.
- [ ] Bundle separate curated answer and accepted-guess lists from a documented permissive source.
- [ ] Exclude slurs and unsuitable surprise answers from the answer list.
- [ ] Keep one shared unresolved word per server/channel.
- [ ] Give each user three guesses per UTC day; persist attempts across restarts.
- [ ] Carry the word over between days until somebody solves it.
- [ ] Start a new word immediately after a solve and announce the solver and old answer.
- [ ] Implement duplicate-letter scoring with the standard two-pass algorithm.
- [ ] `!word` shows known correct positions, present letters, absent letters, and solve status.
- [ ] A guess not in the accepted dictionary does not consume an attempt.
- [ ] Track solves, guesses, and optional streaks by stable profile ID.
- [ ] Add cooldowns and IRC-safe ASCII/Unicode feedback options.

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

- [ ] One shared game per server/channel; users join implicitly on their first throw.
- [ ] Every player starts at 301.
- [ ] Model each dart as a board segment, multiplier, bull, or miss rather than uniform `1..60`.
- [ ] Subtract the turn total when it does not take the player below zero.
- [ ] A bust restores the score from the beginning of that turn.
- [ ] No double-out requirement; reaching exactly zero wins.
- [ ] Announce the winner, increment lifetime wins, then reset everyone to 301.
- [ ] Apply a configurable per-user turn cooldown.
- [ ] Persist active scores and lifetime wins across restarts.
- [ ] Theme throws, misses, busts, score displays, and wins.
- [ ] Test exact finishes, busts, multiple players, reset behavior, and cooldowns.

### Open decisions

- Whether `!darts` should default to one dart as proposed, or always throw a conventional
  three-dart turn. Recommendation: retain the proposed one/two/three syntax.
- Whether inactive players remain on the board forever. Recommendation: remove them after a
  configurable number of days without affecting lifetime wins.

---

## Clock (`clock.wasm`)

**Assessment:** Small, useful, and naturally complements stored profile locations. Coordinates are
not enough on their own: correct local time requires a timezone and daylight-saving rules.

### Commands

```text
!time
!time <nick>
```

### Proposed behavior

- [ ] Extend geocoding/profile storage with an IANA timezone such as `America/New_York`.
- [ ] Add a narrow host local-time service backed by a timezone database.
- [ ] Update timezone data when a user changes their saved location.
- [ ] `!time` reports the caller’s local time from their stored location.
- [ ] `!time Alice` reports Alice’s local time when Alice has a saved location/timezone.
- [ ] Handle daylight-saving transitions correctly; never derive timezone from longitude alone.
- [ ] Theme success, missing-location, unknown-user, and service errors.
- [ ] Avoid unnecessarily exposing the exact saved location in the response.
- [ ] Test half-hour and quarter-hour zones as well as daylight-saving boundaries.

### Open decisions

- Whether to store the IANA timezone at geocoding time or query an external timezone service on
  every command. Recommendation: store the timezone and use a host timezone database; it is faster,
  keyless, and deterministic.

---

## Definition of done for every module

- [ ] Commands and edge cases have unit tests.
- [ ] State is partitioned correctly across servers and channels.
- [ ] Stable identity survives nick changes.
- [ ] Every posted line is themeable.
- [ ] Capability policy grants only required host functions.
- [ ] Reload/restart behavior is tested.
- [ ] Rate limits and output bounds are tested.
- [ ] `cargo test`, strict Clippy, release WASM build, and installation into `modules/` succeed.
- [ ] README, SPEC, PLAN, and this backlog are updated when behavior lands.
