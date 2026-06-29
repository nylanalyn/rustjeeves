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
- [x] Keep achievements deferred until several modules emit useful milestone events.

Completed implementation order (kept as a dependency record):

1. Module settings and channel activity policy
2. Durable scheduler foundation
3. Self-reminders, then consent-based reminders for other users
4. Host randomness capability
5. Outbound rate limiting, host-side IRC output sanitization, CTCP hygiene
6. Darts or the six-letter word game
7. Hunt
8. Roadtrip
9. Data lifecycle stages 1 and 2

Achievements remain deferred. Backup automation is the next shared operational milestone.
After backups, the AI responder is the next feature module. The profile/module repair UI is a
later administrative project and should not delay either item.

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
- [x] Test setting precedence and validation.
- [ ] Test setting persistence, module unload/reinstall, and concurrent reads.

Initial types should remain deliberately small: boolean, bounded integer, duration, and bounded
string/choice. Secrets belong in the existing integrations system, not ordinary module settings.

### Data lifecycle and privacy

Treat this as the next substantive host milestone. Module KV is intentionally opaque to the host,
so deletion cannot be implemented safely as ad-hoc SQL against guessed JSON structures.

Agreed rollout:

1. Versioned lifecycle ABI, operator JSON profile export, and scheduler ownership metadata.
2. PM-only `!mydata` summary/export/confirmed deletion plus equivalent super-admin controls and a
   resumable deletion journal.
3. Backup automation after lifecycle controls are proven.

- [x] Define a versioned host export envelope for shared profile data, identity bindings, and
      explicitly owned jobs.
- [x] Inventory stored data by category: profile fields; user-authored text; scheduled payloads;
      game/progression state; moderation/admin records; and operational logs.
- [x] Document retention and deletion semantics for each category, including what is deleted,
      anonymized, retained for moderation, or retained only in backups.
- [x] Add ABI-versioned module lifecycle exports for subject summary/export and idempotent deletion,
      keyed by `(server, profile_id)`; modules remain responsible for their opaque KV structures.
- [x] Add optional `owner_profile_id` metadata to user-owned scheduled jobs so the host can find and
      cancel them without parsing private payloads.
- [x] Add a PM-only authenticated user flow to summarize/export personal data and request erasure,
      with explicit confirmation before destructive action.
- [x] Add an operator JSON export for the host-owned profile and explicitly owned scheduler jobs.
- [x] Add PM-only super-admin profile summary, full runtime export, and confirmed erasure controls.
- [ ] Add operator module/channel reset and retention-pruning tools without direct SQLite editing;
      destructive bulk operations need dry-run output.
- [x] Make erasure resumable and auditable so a module failure cannot leave a half-deleted profile;
      lifecycle handlers and retries must be idempotent and audit logs must exclude private data.
- [x] Define backup behavior clearly: erasure applies to live data immediately, while encrypted
      backups age out under a documented retention window rather than being rewritten in place.
- [x] Test PM/export authorization, requester-bound confirmation, cross-network isolation,
      scheduled-job cleanup, malformed module state, transactional mutation scope, and journal
      redaction.
- [x] Test module absence/reinstall retry, repeated finalization, and legacy/nick-alias cleanup.

### Backup policy (after data lifecycle)

- [x] Add a host backup settings screen: enabled, local directory, schedule, daily/weekly/monthly
      retention counts, and a safe **Run now** action with last-success/error status.
- [x] Add SQLite-consistent local snapshots (backup API or `VACUUM INTO`, never a raw live-WAL
      copy), retaining 3 daily, 4 weekly, and 3 monthly restore points.
- [x] Add Backblaze settings: enabled, endpoint/region, bucket, object prefix, and weekly schedule;
      keep application key ID/secret and client-side encryption key in masked Integrations fields.
- [x] Add a weekly client-side-encrypted upload to the operator's Backblaze bucket, retain four
      remote weekly restore points, and request bucket-side encryption as a second layer.
- [x] Store checksums and a small manifest with schema version and creation time; never upload API
      credentials inside the archive.
- [x] Document restore steps and verify every backup by opening it and running migrations
      plus integrity checks.

### AI responder

Provide a deliberately narrow chat responder rather than giving a WASM module unrestricted HTTP,
filesystem, credentials, or bot-control capabilities.

- [x] Add a host-owned `ai_chat` capability with bounded request/response sizes, timeout, limited
      concurrency, and operator-configured endpoints only.
- [x] Support Ollama over its OpenAI-compatible `/v1/chat/completions` endpoint as the default use
      case, plus OpenAI and generic OpenAI-compatible providers without changing module code.
- [x] Add masked optional API-key storage under Integrations; Ollama/LAN use must work without a
      secret, while remote providers send credentials only from the host.
- [x] Keep endpoint/provider/model in host-owned Integrations and add module settings for
      per-network aliases, channel enablement, cooldown, temperature, and output limit. The active
      IRC bot nick is always recognized automatically; aliases such as `jeeves` are additional names.
- [x] Recognize bounded channel addressing such as `jeeves, question` and `jeeves: question`, with
      configurable PM behavior. Never react to an alias embedded in ordinary prose.
- [x] Add a host-read `SOUL.md` path setting. The TUI selects the path rather than editing a large
      prompt; the host size-bounds and reloads the file without granting WASM filesystem
      access.
- [x] Keep v1 stateless and tool-free. If bounded conversation history is added later, partition it
      by network/channel and integrate it with profile export/deletion hooks before enabling it.
- [x] Sanitize and bound IRC output, apply stable-profile cooldowns, suppress bot/self loops,
      and test aliases, PM isolation, provider failures, timeouts, malformed responses, and mocked
      Ollama/OpenAI-compatible replies.

### Profile and module data repair TUI — later idea

Add an operator-facing way to inspect and repair known profile data without direct SQLite editing.
This should use validated host/module contracts, not expose arbitrary raw KV editing.

- [ ] Add a Profiles TUI page grouped by network, showing stable UUID, current nick, known aliases,
      services-account bindings, and last-seen time with search/filter support.
- [ ] Opening a profile shows host-owned fields and installed modules that report data for the
      subject through lifecycle inspection hooks; absent modules remain visible but inactive.
- [ ] Host profile fields may be edited with the same validation used by normal commands.
- [ ] Define an optional module-owned admin schema/repair hook so a module controls which fields are
      displayable/editable and validates every mutation. Do not make opaque module JSON generally
      editable.
- [ ] Support narrowly scoped repair actions such as correcting a display value, removing an
      inappropriate quote/memo/stat entry, or resetting one module's state for one profile.
- [ ] Show a dry-run diff, require confirmation for destructive changes, write a privacy-safe audit
      record, and create/verify a local database snapshot before applying a repair.
- [ ] Test nick changes, cross-network isolation, malformed/legacy module state, module absence and
      reinstall, concurrent chat updates, validation failures, and rollback after failed repairs.

### Shared game services

- [x] Add a narrow host randomness capability before implementing more games; do not make every
      module seed a predictable PRNG from the current timestamp.
- [x] Generate central `!help` output from command manifests so installed modules are discoverable.
- [x] Defer a generic cross-module event API until at least two concrete consumers need it;
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
- [x] Test restart recovery, cancellation/idempotence, duplicate replacement IDs, in-flight
      scheduling, and module absence/reinstall.
- [ ] Test wall-clock changes and malformed persisted jobs.

The scheduler belongs in the host because WASM modules only run when an IRC event invokes them.
Polling on ordinary channel messages would make reminders late and spontaneous games unreliable.

### Channel activity policy (implemented through module settings)

Hunt and roadtrip speak without being directly commanded, so operators need control over noise.

- [x] Add per-module/per-channel enablement for spontaneous activity.
- [x] Add configurable minimum and maximum intervals to Hunt and Roadtrip.
- [x] Default spontaneous modules to disabled until explicitly enabled.
- [x] Enforce one active spontaneous event of each type per channel through stable job IDs and
      persisted phase/event state.
- [x] Provide TUI enable/disable settings plus admin cancel and status/inspection commands.

### Outbound rate limiting

IRC servers disconnect clients that send messages too quickly. As more modules accumulate and
scheduler deliveries can produce bursts, uncontrolled send rates are a reliability risk.

- [x] Add a per-network leaky-bucket rate limiter inside the IRC actor.
- [x] Queue outbound messages behind the bucket rather than dropping them when it is temporarily
      empty.
- [x] Cap the outbound queue size per network and log clearly when messages are dropped at that
      limit.
- [x] Choose conservative defaults (one line per 500 ms, burst of four).
- [ ] Expose rate-limit values as network settings if operational experience shows a need.
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
- [x] Document that modules should still apply their own limits for semantic correctness (e.g.
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

- [x] Add `!memos admin list <nick>` (super-admin only): privately show the invoking admin pending
      memos queued for that nick in the current channel, including sender, age, and a preview,
      without delivering them or exposing them to the room.
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
- [x] Track totals strictly by stable profile UUID and channel; never fall back to nickname when
      mutating ownership. Legacy nick-only rows remain display-only. Counts are theme-stable.
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
- [x] Use stable profile IDs exclusively for membership while retaining bounded current display
      names for announcements; never fall back to nickname ownership.
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
- [x] Keep one shared unresolved word per network, visible collaboratively across its channels.
- [x] Give each user three guesses per UTC day; persist attempts across restarts.
- [x] Carry the word over between days until somebody solves it.
- [x] Keep the solved word visible for the rest of its UTC day; start a fresh word the next day or
      when an admin uses `!word new`.
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
- [x] Evaluate requested darts sequentially; a miss or bust ends that throw sequence while prior
      successful darts remain scored, matching the original Jeeves game.
- [x] No double-out requirement; reaching exactly zero wins.
- [x] Announce the winner, increment lifetime wins, then clear the completed match.
- [x] After three darts, apply a configurable rest that another player's throw releases.
- [x] Persist active scores and lifetime wins across restarts.
- [x] Theme throws, misses, busts, score displays, and wins.
- [x] Test exact finishes, busts, multiple players, reset behavior, and cooldowns.

### Open decisions

- **Resolved:** One/two/three dart syntax retained (`!darts`, `!darts 2`, `!darts 3`).
- **Resolved:** Matches persist until checkout or an administrator uses `!darts reset`; player
  count is bounded to keep state finite.

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

This is a reusable review template, not a list of unfinished project-wide tasks:

- Commands and edge cases have unit tests.
- State is partitioned correctly across servers and channels.
- Stable identity survives nick changes.
- Every posted line is themeable.
- Capability policy grants only required host functions.
- Reload/restart behavior is tested.
- Rate limits and output bounds are tested.
- Database migrations, ABI compatibility, and malformed persisted state are tested.
- IRC control characters are sanitized and output respects IRC line-length limits.
- `cargo test`, strict Clippy, release WASM build, and installation into `modules/` succeed.
- README, SPEC, PLAN, and this backlog are updated when behavior lands.
