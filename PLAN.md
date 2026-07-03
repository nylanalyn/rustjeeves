# rustjeeves — Live Plan

Milestone checklist. Check items off as they land. See `SPEC.md` for the what/why and `AGENTS.md`
for conventions.

## Milestones

- [x] **M0 — Scaffold.** Cargo workspace; `jeeves` binary + `jeeves-abi` crates;
      `modules-src/admin` plugin crate; `modules/` runtime dir. Deps: `tokio`, `irc`, `rusqlite`,
      `ratatui`, `crossterm`, `extism`, `serde`, `serde_json`, `clap`, `anyhow`. `cargo build`
      clean.
- [x] **M1 — Config + DB.** `db.rs` rusqlite actor + schema migrations; `config.rs` load/save;
      sane defaults when the DB is empty.
- [x] **M2 — IRC connect (headless first).** `irc` actor: TLS, CAP, SASL PLAIN, NickServ-message
      fallback, join channels, stream events → log bus. `--headless` connects and sits.
      *Verified live: TLS + RPL_WELCOME against irc.libera.chat; **SASL PLAIN end-to-end** against
      a local ergo container (CAP ACK → AUTHENTICATE → 900 logged-in → join). Regression test
      `cap_acks_sasl` guards the CAP-field parsing bug found during that test.*
- [x] **M3 — Log bus.** Broadcast `LogEvent` (levels + categories ERROR/DEBUG/MESSAGE/COMMAND);
      stdout + DB sink.
- [x] **M4 — TUI.** ratatui app; Settings screen (edit → save to SQLite); Logs screen (scroll +
      filter by category). `--interactive` launches it. *Verified under a pty: renders, edits,
      Ctrl-S persists to SQLite, clean exit.*
- [x] **M5 — Module host.** extism loader over `modules/`; ABI dispatch of events to guest hooks;
      host functions wired to the Action channel + DB actor; `reload` re-reads the folder.
      *Verified by integration test (`modules::tests`).*
- [x] **M6 — Admin module.** Build `admin.wasm`; parses `!reload`/`!refresh`/`!shutdown` (+
      `!ping`/`!help`); calls privileged host fns; logs under `COMMAND`. *Verified by integration
      test and a live headless run (auto-load + COMMAND log).*

## Verification

- `cargo build --workspace` and `cargo clippy` clean.
- **Headless connect:** point config at a test network (local `ergo`/`ngircd`, or libera in a
  throwaway channel); `jeeves --headless` negotiates CAP, completes SASL, joins, and stays up.
- **Interactive:** `jeeves --interactive` → edit + save settings; confirm the row persists in
  `bot.db`; watch the Logs screen populate and filter.
- **Modules:** empty `modules/` → runs with no plugins; drop in `admin.wasm` → loads, `COMMAND`
  category appears, `!shutdown`/`!reload` work.
- **Per-module storage:** a module `kv_set` then `kv_get` round-trips through `module_kv`.

## Current status

**All milestones (M0–M6) complete and verified.** Headless connects live (TLS + SASL-capable),
the TUI edits/saves config, and the admin WASM module auto-loads and drives the bot. `cargo build
--workspace`, `cargo clippy`, and `cargo test -p jeeves` are clean.

SASL PLAIN is verified end-to-end against a local ergo IRCd over **both plaintext and TLS**. An
`accept-invalid-certs` toggle (off by default; settable in the TUI) allows TLS against self-signed
certs for local testing.

## v2 milestones — complete & verified

- [x] **Multi-server.** One IRC actor per enabled profile; connect to all networks simultaneously.
      Events carry the originating server label (`EventEnvelope`); host functions target a network
      by label via a shared registry. *Verified against two ergo containers: a `!ping` on each
      network is answered on that same network.*
- [x] **Graceful QUIT.** Shutdown sends QUIT to every connection and waits for close (2s grace),
      not an abrupt abort. *Verified: an observer client sees the QUIT on SIGINT.*
- [x] **Hot reload.** `notify` watches `modules/`; debounced auto-reload on add/change/remove;
      `!reload` still works. *Verified by dropping/modifying `.wasm` files live.*
- [x] **Permissions (per-network admin / super-admin).** Host-side resolver (`perms.rs` +
      `db::resolve_role`) stamps the sender's role onto each message; the admin module enforces
      (`!shutdown`=super-admin, `!reload`/`!refresh`=admin). Identity: services account
      (`account-tag`) preferred, hostmask trust-on-first-use fallback. *Verified against ergo:
      hostmask-TOFU admin granted; non-admin denied; SASL-account super-admin shutdown. 5 unit
      tests cover the policy branches.*
- [x] **TUI overhaul.** Servers list (add/edit/delete/enable), per-profile edit form, per-server
      Admins screen, multi-server logs. TUI reads/writes SQLite directly (blocking DB API);
      Ctrl-R applies/reconnects. *Verified under a pty: lists servers, drills into admins, adds a
      persisted server.*

At completion of v2, `cargo build --workspace`, `cargo clippy --workspace`, and the then-current
7-test host suite were clean.

## v3 — modules & integrations

- [x] **Themes.** `theme.toml` + `theme(key, default, vars)` host fn (lazy registration, list
      random-choice, `{var}` substitution, live reload, global scope).
- [x] **User profiles (host service).** `profiles` table + `profile_*` host fns; `users.wasm`
      (`!title`/`!birthday`/`!pronouns`/`!location`/`!whoami`/`!clear`). A set title makes the host
      stamp `display = "{title} {nick}"` so every module addresses the user that way.
- [x] **Weather.** `geocode`/`weather` host fns (keyless Open-Meteo); `weather.wasm` (`!weather`
      via a saved location or ad-hoc query).
- [x] **Per-server user modes.** `servers.umodes` (e.g. `+B`), applied to ourselves on connect.
- [x] **Discord admin bridge.** Localhost token-gated HTTP API (`adminapi.rs`) matching
      `ircbot_core/discord_admin.py`'s contract (`/v1/command`, `/v1/events`).
- [x] **`build-modules.sh`.** Builds every `modules-src/*` to wasm and installs into `modules/`;
      detects a missing wasm `std` and prints the distro-specific fix.
- [x] **Fishing mini-game** (`fishing.wasm`, full `fish_database.json`). Added a `now()` host fn
      (wasm has no clock); host-entropy-seeded game PRNG; one namespaced kv state blob.
  - [x] **Phase 1 — core loop.** `!cast`/`!reel` (10 locations Puddle→The Void, distance,
        rarity-by-wait, junk, line-breaks, weight, XP + bonuses, level-ups) and the read-only
        displays (`!fishing`/`top`/`location`/`fishinfo`/`aquarium`/`help`).
  - [x] **Phase 2 — events, artifacts, lures, chum.** 5%-on-cast timed/location events;
        artifacts via the junk path (+`!discard`); `!lure` (30 XP); `!chum` (250 XP, server-wide).
  - [x] **Phase 3 — champions, seasonal reset, risk toys, admin.** Per-server champions
        (Traveler/Caster/Collector, +20% bonuses + in-message titles); lazy quarterly
        reset/announce/wipe (civil-date math, no scheduler);
        `!dynamite` (chicken / glorious haul / lose-hands → 7-day ban); `!fish bless` gated on
        `role == SuperAdmin`. *Verified live against ergo: bless denied for non-admins and forces a
        legendary for a super-admin; champion title + bonus surface in catches; a forced past
        boundary crowns champions, announces, and wipes the season. 9 module unit tests
        (xp/rarity/weight/PRNG/db + civil-date round-trip, quarter boundaries, champion tie-break,
        reset) clean.*
  - [x] **Phase 4 — Q3 2026 Void expansion and XP sink.** Reset-gated levels 10–19 unlock ten
        coloured Void locations generated from one fish-template list, with tier-scaled weights and
        distances. Optional cast bait spends 100–1,700 XP to advance rarity timing by 1–17 hours
        for that cast only; it does not bypass the minimum reel time, increase weight, or reduce
        post-24-hour danger. The expansion activates at the July 1, 2026 UTC season boundary even
        when its WASM is built or deployed earlier.

## v4 — reliability, security, and identity

- [x] **Reconnect supervision.** Every enabled network reconnects with capped exponential backoff;
      refresh and shutdown remain graceful.
- [x] **Stable user identity.** Per-network profile UUIDs with nick and services-account aliases;
      IRC `NICK` events retain identity and fishing state migrates from legacy nick keys.
- [x] **Module capabilities.** `module-capabilities.toml` is enforced by every host function;
      privileged lifecycle controls are granted only to the trusted admin module by default.
- [x] **Module isolation/backpressure.** One bounded worker per plugin, bounded dispatch queues,
      explicit drop logging, and a 20-second Extism execution deadline.
- [x] **Theme hardening.** Invalid or structurally incompatible TOML is never overwritten and cannot
      panic module execution. Fishing routes all posted output through named theme keys.
- [x] **Database durability.** Server updates/deletes are transactional; logs retain 30 days with a
      100,000-row cap and supporting indexes.
- [x] **CLI/docs.** `--headless` and `--interactive` conflict correctly; README/SPEC/PLAN reflect
      current behavior.

Current verification: 29 host tests plus 13 standalone module tests pass; strict Clippy passes for
the workspace and every standalone module; all four release WASM artifacts build and install.

## v5 — utility modules

- [x] **Web search.** Tavily-backed `search.wasm` (`!g`/`!google`/`!search`) through a dedicated
      capability that keeps HTTP access and the API key in the host. Includes query limits,
      per-user cooldowns, bounded requests/responses, themed output, and a search-URL fallback.
- [x] **Integration credentials UI.** Global masked Tavily and DeepL key editing under TUI F3,
      persisted in SQLite with immediate application and environment-variable fallback for
      headless use.
- [x] **Translation.** DeepL-backed `translate.wasm` (`!tr`/`!translate`) with automatic source
      detection, optional explicit source language, common language names, request limits,
      cooldowns, themed output/errors, and Free/standard endpoint selection. Its masked key is
      managed alongside Tavily under TUI F3.
- [x] **Seen and quotes.** Channel-local `history.wasm` with stable-profile identity,
      `!seen <nick>`, capture-last-line and manual self-quotes, random/ID retrieval, controlled
      deletion, themed output, and strict exclusion of private messages.
- [x] **Memos.** Channel-local `memos.wasm` with `!tell`, stable-profile delivery across nick
      changes, bounded queues and delivery batches, configurable 30-day-default expiry,
      private-message isolation,
      waiting-count and clear commands, and fully themed output.
- [x] **Custom command aliases.** Versioned command metadata exported by every bundled module;
      collision-safe host registry; global SQLite overrides; immediate TUI editing under F4;
      owner-only canonicalization that preserves original text for passive modules; defaults such
      as `!w`, `!g`, and `!tr`; and retention of overrides for temporarily absent modules.
- [x] **Sed corrections.** `history.wasm` reuses its channel-local last-line cache for
      `s/pattern/replacement/` with escaped slashes, `g`/`i` flags, bounded Rust regexes, capture
      replacements, chained corrections, cooldowns, private-message isolation, and themed output.

Current verification: all 40 workspace tests and every standalone module test pass; strict Clippy
passes across the workspace and modules; and all eight release WASM modules build and install.

## v6 — clock

- [x] **Local time.** Geocoding now records IANA timezones in shared profiles; the host exposes a
      narrow daylight-saving-aware `local_time` capability; and `clock.wasm` provides `!time`
      for the caller, another saved user, or an ad-hoc location. All responses are themed and the
      command manifest makes `time`/`clock` available in the TUI alias editor.

## v7 — module settings foundation

- [x] **Typed scoped settings.** Modules may advertise versioned boolean, bounded integer,
      duration, bounded string, and choice settings. SQLite overrides resolve channel → network →
      global → default, remain stored while modules are absent, and update a shared runtime cache
      immediately.
- [x] **Operator UI and enablement.** TUI F5 lists module settings and edits validated scoped
      overrides. Every module receives a standard host-enforced `enabled` setting, and memos proves
      module-owned settings with configurable global/network/channel retention.

## v8 — durable self-reminders

- [x] **Durable scheduler.** Host-owned, SQLite-backed jobs are namespaced by module, bounded by
      quota/payload/horizon, restored after restart, replaceable/cancellable, and delivered only to
      the owning loaded module. An absent module leaves its due jobs pending for retry.
- [x] **Reminders.** `reminders.wasm` implements themed channel-local `!remind me in … to …`,
      `!reminders`, and `!remind cancel <id>` using stable profile identity, bounded queues and
      text, configurable limits, natural/compact durations, and durable timer delivery.

## v9 — randomness capability

- [x] **Host randomness.** A `random_bytes` host function fills up to 64 bytes directly from the
      operating system CSPRNG, gated on the `random_bytes` capability in
      `module-capabilities.toml`. Modules request a count and receive a `Vec<u8>` JSON payload;
      they can combine bytes into a `u64`, use multiple calls for sequences, or treat them as direct
      indices. New game modules must use this instead of seeding their own PRNG from `now()`.

## v10 — games

- [x] **Darts.** Asynchronous channel-local 301 race based on the original Jeeves module. Players
      may throw up to three sequentially evaluated darts before a configurable rest; another
      player's throw releases resting players. Exact zero ends and clears the match. Active state
      and lifetime results use stable profile IDs; board-weighted randomness comes from
      `random_bytes`.
- [x] **Wordle.** Daily collaborative six-letter puzzle based on the original Jeeves module. One
      shared word per network carries across UTC days until solved, users receive a configurable
      daily attempt allowance, and guesses build shared correct/present/absent discoveries.
      Stable-ID stats, leaderboard, admin reset, compatibility commands, bounded used-word history,
      and `random_bytes` answer selection are included.
- [x] **Hunt.** Spontaneous per-channel animal appearances on a durable scheduler. At a random
      scheduled time a themed animal appears; the first `!hunt` or `!hug` resolves it and records a
      count on the user's board. Animal pool and announcement text are theme-configurable
      (`hunt.animals`); counts are stable across theme changes and strictly owned by profile UUID,
      never by nickname fallback. Per-channel `enabled = false` default ensures spontaneous output
      is opt-in.
- [x] **Roadtrip.** Victorian excursion game with optional spontaneous initiation. Jeeves proposes
      a themed destination; a signup window (60 s) collects `!me` passengers; then he
      announces departure and schedules a return job (30–60 min). Passengers are stored as stable
      profile IDs with current display names. Destination pool is theme-configurable
      (`roadtrip.destinations`). Manual `!roadtrip` always works regardless of `enabled`; admin
      cancel gated on `Role::Admin`. Passenger ownership is UUID-only, and both persisted party
      size and rendered name lists are bounded. Repeated bare `!roadtrip` commands are silent until
      the active trip completes. Per-channel `enabled = false` default.

Current verification: all core host tests pass; strict Clippy clean; darts, hunt, and roadtrip
build to WASM via `build-modules.sh`.

Production-candidate smoke test: the uploaded bot connects and the reviewed command/module flows
work in private IRC rooms. Broader public-room and long-running operational testing remains an
operator rollout step rather than an unfinished implementation milestone.

Future module designs and implementation order are tracked in `MODULES_TODO.md`.

## v11 — data lifecycle foundation

- [x] **Versioned operator export.** `--export-profile SERVER:NICK` writes a private JSON file
      containing the stable shared profile, nick/account identity bindings, and explicitly owned
      scheduler jobs. Unknown profiles fail without creating an export, and module-private KV is
      excluded until lifecycle hooks define its ownership.
- [x] **Scheduler ownership.** Durable jobs accept an optional stable `owner_profile_id`;
      reminders populate it while channel/system timers remain unowned. The field is migrated,
      persisted, restored, and backward-compatible in serialized requests.
- [x] **User and administrator controls.** PM-only self-service summary/export/confirmed erasure,
      super-admin equivalents, pure module lifecycle hooks, transactional mutation validation, and
      a resumable/redacted deletion journal form Stage 2. Missing modules and malformed state block
      completion safely; legacy aliases and cross-network isolation are handled explicitly.
- [x] **Backups.** Stage 3 provides verified SQLite snapshots, 3 daily/4 weekly/3 monthly local
      retention, encrypted and credential-scrubbed weekly Backblaze replication, remote retention,
      manifests/checksums, F7 controls/status, and offline verification/decryption commands.

## v12 — context-aware AI responder

- [x] **Narrow host provider.** `ai_chat` owns OpenAI-compatible HTTP, credentials, endpoint/model
      selection, bounded `SOUL.md` loading, request/response limits, timeout, and concurrency guard.
- [x] **Addressed WASM module.** Private messages and opt-in channel aliases invoke bounded chat;
      explicit punctuation prevents ambient mentions from triggering it. Stable UUID cooldowns,
      lifecycle hooks, theming, self-loop suppression, and scoped settings are included. Enabled
      conversations retain an isolated, age-limited 0–30-line transcript (25 by default), with
      lifecycle export/deletion and host-enforced untrusted-context labelling.

## v13 — safe profile repair

- [x] **F8 profile inspection.** Filter stable profiles and inspect UUID, network, aliases, account
      bindings, timestamps, validated host fields, and lifecycle-aware module exports.
- [x] **Guarded repair.** Host fields support atomic validated replacement; module data supports
      whole-subject reset only through the owning module's lifecycle hook. Dry runs, confirmation,
      verified pre-repair snapshots, privacy-safe audit logs, and optimistic concurrency checks
      prevent silent overwrites. Generic opaque JSON/KV editing remains prohibited.

## v14 — YouTube search and link metadata

- [x] **Narrow host provider.** Host-owned API credentials, bounded HTTP, safe error categories,
      parsed video metadata, and a short-lived bounded cache back `youtube_lookup` and
      `youtube_search`; search resolves its result through `videos.list` for full metadata.
- [x] **Opt-in WASM module.** `!yt` searches with stable-profile cooldowns while disabled modules
      still accept explicitly targeted commands. Passive canonical-link announcements remain off
      by default and use bounded per-channel repeat suppression, lifecycle hooks, scoped settings,
      capability policy, and themed output.

## v15 — channel banter rituals

- [x] **Sailing response.** In enabled channels, a whole-word `sail` from the configurable
      `witeshark2` nick selects one of twenty theme-editable sailing lines grounded in real sail
      trim, wind, tactics, and seamanship terminology.
- [x] **Crow response.** A whole-word `caw` or `kaw` from any non-bot user selects one of twenty
      theme-editable pieces of crow lore. Both triggers are punctuation/case tolerant, substring
      safe, channel-only, independently cooldown-limited, and bounded to one reply per message.

## v16 — negotiated IRC casemapping

- [x] **Network-aware identity.** Parse `CASEMAPPING` from `RPL_ISUPPORT` (`005`), default safely to
      `rfc1459`, and partition the negotiated mapping by network. Profile aliases, administrator
      matching, bound hostmasks, and self JOIN/PART recognition use the selected folding rules.
- [x] **Module nickname lookup.** A narrow capability exposes host case-folding without leaking
      other network state. Fishing statistics/blessings and legacy identity migration, hunt score
      lookup, and memo fallback delivery now respect the network's mapping.

## v17 — persistent fishing careers and seasonal play

- [x] **Non-destructive seasons.** Separate permanent career progress from quarterly competition.
      Levels, XP, catches, aquarium entries, artifacts, records, active casts, and lifetime totals
      survive the boundary; only dedicated seasonal counters reset. Traveler is awarded for XP
      earned during the quarter, Caster for the furthest seasonal cast, and Collector for seasonal
      rare/legendary catches. Legacy pre-change saves migrate from their lifetime totals so an
      operator can safely restore a backup from the final destructive season.
- [x] **Species mastery and personal records.** Bronze/Silver/Gold/Iridescent mastery derives from
      permanent catch counts at 5/25/100/250. Location-qualified species careers preserve legacy
      counts, store landed-weight records separately from unboosted specimen quality, recognize
      natural catches above 95% of the species maximum, and announce records/mastery through named
      theme keys. `!mastery [nick]` and `!records [nick]` expose permanent career progress.
- [x] **Reinforced rod skill (level 15+).** A permanent time-sink for endgame anglers that lowers
      line-break chance and opens up the Void megafauna that were previously unlandable (any fish
      above ~6,500 lb guaranteed a snap under the uncapped `0.02 + weight/1000*0.15` formula).
      `!rod` inspects strength and any in-progress fix; `!fix [1-24h]` commits time to gain +1
      strength per hour. Strength 0–50, each point a 1% flat break reduction, floored at 50% of the
      fish's natural risk. The raw break chance is clamped to 95% before strength is applied
      (`MAX_NATURAL_BREAK_CHANCE`), so **every fish in the game is landable** — design intent is
      "harder, not impossible." A Prismatic Kraken (raw ~422%) caps at 95%, flooring to ~47.5% at
      max rod strength, roughly a coin-flip that still demands a fully-maintained rod. This also
      future-proofs against heavier fish or chum+lure size combos silently recreating an impossible
      catch. Protects both the weight-snap and the 24h danger-zone break. Decays only on big fish
      (over 2,000 lb): every 10th such catch costs 1 strength, so the rod is a maintenance loop,
      not a one-time unlock; small fish and offline time never wear it. While fixing, `!cast` is
      refused. State rides `#[serde(default)]` on four new `Player` fields (no migration, host,
      ABI, DB, or capability changes); seven unit tests pass and the WASM rebuilds clean.
- [ ] **Weekly contracts.** Offer three rotating objectives per player from a bounded catalog,
      derive rollover from UTC weeks, track progress without scheduler polling, and reward useful
      consumables, cosmetics, or bait credit rather than creating a pure XP loop.
- [ ] **Collectible variants and dock shop.** Add rare cosmetic fish variants and a small set of XP
      purchases that create new situations (record bait, location charts, strange chum). The
      reinforced rod shipped separately as a time-sink skill rather than an XP purchase.
- [ ] **Recovery events and voluntary voyages.** Add temporary setbacks with explicit recovery
      paths, then offer an opt-in level/location restart that preserves collections, records,
      mastery, titles, lifetime statistics, and permanent voyage rank.

## v18 — premium fish couture and ambient-room safety

- [x] **Premium fish couture.** Super-admins can grant, revoke, and inspect a cosmetic DLC flag by
      stable profile with `!fish dlc grant|revoke|status <nick>`. Successful catches receive a
      random theme-editable outfit without changing species, rarity, weight, XP, records, or any
      other mechanic; the entitlement follows existing fishing lifecycle export and deletion.
- [x] **Channel-only ambient activation.** Hunt and roadtrip no longer accept network/global
      `enabled` overrides. Both default off and require an explicit channel override, while manual
      roadtrip commands remain available. Hunt release, escape, catch, and hug lines are randomized
      theme pools.

## v19 — everyday utility modules

Ordered by implementation. Each is a small, standalone WASM module that delivers value on its own
and follows the module contract (command manifest, theming, stable-profile state where relevant,
scoped settings, capability policy, per-user cooldowns on any expensive path).

1. [x] **`!calc` / `!convert` (calc.wasm).** Safe arithmetic and unit conversion: `!calc 2+2*5`,
      `!convert 72F to C`, `!convert 5 km to mi`. Arithmetic uses a bounded, dependency-light
      hand-rolled shunting-yard evaluator (no `eval`, no untrusted crates) covering `+ - * / %`,
      parentheses, and `sqrt pow abs round min max`, with overflow and division-by-zero guards.
      Unit conversion covers temperature (affine), length, mass, volume, speed, data (base-1024),
      area, and time via a fixed, hand-reviewed unit table with case-insensitive aliases.
      Strict input length limits, themed output, PM-allowed, no external network access.
      Capabilities: `send_message`, `theme` only — the most locked-down module in the bot.
      No KV, no profiles — fully stateless. 26 unit tests pass; WASM builds and installs clean.
2. [x] **Karma (karma.wasm).** `nick++` / `nick--` in channel adjusts a per-channel score keyed on
      stable profile UUID (not the voter's nick). `!karma [nick]` shows a score; `!karma top` /
      `!karma bottom` shows the channel leaderboard — the social surface is the point, not the raw
      counter. Cooldown per voter-target pair prevents rapid-fire inflation; self-karma is rejected.
      Scores are channel-local and exportable/deletable via lifecycle hooks; ledger and cooldown
      state are explicitly bounded per channel. 13 unit tests pass; WASM builds and installs clean.
      Capabilities: `send_message`, `theme`, `kv_get`, `kv_set`, `profile_get`, `irc_casefold`,
      `now`, `setting_get`.
3. [x] **`!define` (define.wasm).** Dictionary lookups via a keyless, SFW API (Free Dictionary API,
      which fits the existing host-owned-HTTP pattern of search/translate/youtube). `!define word`
      returns a short definition; multiple senses are bounded to the first 2-3. Per-user cooldown,
      input length limit, themed output, graceful "no match" handling. Host-owned HTTP behind a
      `dictionary_lookup` capability so the module never sees raw network access; the endpoint is
      not configurable to a non-dictionary service. This deliberately replaces the old bot's
      Urban Dictionary feature, which was retired as a spam/NSFW vector. Cooldowns are configurable
      by scope and keyed on stable profile UUIDs with lifecycle export/deletion. Three module tests
      pass; host parser tests, workspace tests, clippy, and the release WASM build all pass.

## v20 — cross-game achievements

Completed 2026-07-03. The host owns atomic per-network stats, finite unlocks, prestige, dynamic
completion, deduplication, catalog-versioned `set_max` backfills, lifecycle export/deletion, and
three-second themed announcement bundles. Every applicable bundled module advertises a validated
manifest and awards only at committed success points; Darts, Fishing, Hunt, Karma, and Wordle
silently import reliable historical totals. `!achievements` provides bounded profile, module, and
catalog views with secret redaction and Roman prestige ranks. Full native tests, strict Clippy,
release WASM builds, and a fresh-database load of all 21 module workers pass.

- [x] **Host-owned achievement store.** A cross-module stat and achievement store (host-owned, like
      profiles and the scheduler) that game and utility modules report events into via a new
      `award_stats` host function. Stats accumulate silently; achievements fire themed announcements
      on threshold crossings. This is the connective tissue that lets the non-winner majority get a
      payoff — solving the winner-take-all problem identified across the games without diluting the
      drama of actually winning.
- [x] **Game-specific achievement tracks.** Seed the system with the tracks we already designed:
      Wordle letter-finding assists (one point per confirmed letter, more for an exact-position
      letter) and Darts "Almost Was" (a point when you finish a game within one good throw of the
      winner). Each track has a small ladder of achievements (e.g. 10/50/200 assists). Other games
      (fishing mastery milestones already exist; hunt/roadtrip) opt in as natural.
- [x] **Achievement surface.** `!achievements [nick]` and `!achievements list` show a player's
      earned achievements and progress toward the next tier. Announcements are throttled and
      theme-editable so a busy session doesn't flood the channel.
- [x] **Per-user opt-out.** `!achievements optout` wipes your achievement progress and suppresses
      all future awards — both self-caused (fishing, wordle) and other-caused (karma received) —
      via a one-query enforcement check in `award_stats` driven by a new `achievements_opt_out`
      profile column. `!achievements optin` resumes earning from zero. The wipe is atomic with the
      flag set (one DB-actor transaction reusing the existing achievement-table deletion logic).
      Default is opted-in: using the bot implies consent, and the opt-out is the off-ramp for users
      who find the feature noisy.

## v21 — public achievement gallery (deferred until v20 is complete)

- [ ] **Read-only public web surface.** Add a small host-owned Rust HTTP service with its own
      localhost bind/enable configuration, separate from the bearer-token admin API. It reads
      achievement snapshots through the DB actor and the live manifest registry; it never opens
      SQLite directly and exposes no commands, mutations, profile details, accounts, hostmasks,
      activity history, or module KV. Serve the gallery and a narrow versioned JSON API from the
      same origin. Keep it suitable for a Cloudflare Tunnel pointed at the localhost listener;
      document the tunnel setup but do not make cloudflared a bot dependency.
- [ ] **Dynamic catalog landing page.** The default page shows every current non-secret finite
      achievement grouped by module, with name and description, plus prestige tracks and catalog
      totals. Build the display from loaded achievement manifests so module additions and catalog
      version changes appear without editing website data. Omit undiscovered secrets entirely and
      never send their names, descriptions, stat IDs, or thresholds in the public catalog payload.
- [ ] **Achievement-holder selector.** Provide a network-aware dropdown containing only profiles
      that own at least one finite achievement, labelled with their current/main nick. Selecting a
      user makes earned cards prominent, visually subdues locked visible cards, shows module and
      overall collection totals, and includes earned prestige ranks. Stable profile UUIDs remain
      internal opaque route/query identifiers; duplicate nicks on different networks must be
      distinguishable without exposing aliases or account data.
- [ ] **Earned-secret presentation.** A selected user's earned secrets may appear by achievement
      name, marked as secret, but the public response and page must omit the unlock condition,
      threshold, underlying stat, and explanatory description. Secrets remain absent for users who
      have not earned them. Optional/social achievements may be displayed but must remain visually
      distinct from completion-required achievements.
- [ ] **Safe public API and operations.** Add bounded endpoints for catalog, eligible users, and one
      user's sanitized collection; enforce response-size limits, request timeouts, security headers,
      HTML escaping, method allowlists, and conservative caching/ETags. Add a configurable public
      display opt-out (default policy decided before implementation), rate-limit abusive clients,
      and avoid CORS unless a separate frontend origin is deliberately chosen. Bind to
      `127.0.0.1` by default and provide health/readiness endpoints for tunnel supervision.
- [ ] **Responsive gallery and acceptance tests.** Create an accessible mobile/desktop card grid
      with module navigation/filtering and progressive enhancement (the catalog remains useful
      without JavaScript). Test secret non-disclosure at the JSON and HTML boundaries, network and
      profile isolation, opt-out behavior, removed/added catalog entries, escaping, empty state,
      large catalogs, caching, and read-only routing. Verify locally, then document a supervised
      cloudflared deployment and capture browser screenshots before release.

## Maintenance hardening

- [x] **Review follow-up.** Fishing randomness is seeded from the host OS CSPRNG; self-service
      exports have stable-profile cooldowns plus seven-day/100-file retention; disconnect events
      have one owner; migrations fail on real errors; IRC channel detection honors negotiated
      `CHANTYPES`; and backup-key recovery requirements are explicit in both TUI and docs.
- [x] **Automated quality gates.** `test-all.sh` covers the root workspace and every standalone
      module. GitHub Actions enforces formatting, strict Clippy, all native tests, and release WASM
      builds.
