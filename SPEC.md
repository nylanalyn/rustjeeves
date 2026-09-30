# rustjeeves — Specification

`rustjeeves` (binary: `jeeves`) is an IRCv3 bot framework written in Rust. It is an exploratory
rewrite of an existing Python bot — the goal is a small but real, extensible framework rather than
feature parity.

## Goals (this iteration)

The bot must:

1. **Connect to an IRC server**, with optional TLS.
2. **Authenticate to services (NickServ)** via **SASL** (PLAIN), with a fallback to messaging
   NickServ directly.
3. **Join configured channels** and **stay running**.
4. Run in one of two modes:
   - **Interactive** — opens a TUI (settings + logs).
   - **Non-interactive / headless** — no TUI; logs to stdout/file.
5. Be **modular**: WASM plugins dropped into a `modules/` folder are auto-loaded at startup.
6. Persist configuration and per-module state in **SQLite**.

"Done, for a start" = it connects, authenticates, joins rooms, and sits running, in both modes,
with a working settings UI, a filterable log view, the WASM module loader, and an `admin` module.

## Non-goals (deferred — see bottom)

Deep IRCv3 spec coverage beyond CAP + SASL + message tags and a full operator-facing module
marketplace/signature system.

## Runtime modes

| Mode | Flag | Behaviour |
|------|------|-----------|
| Interactive | `--interactive` (default) | Launches the ratatui TUI. |
| Headless | `--headless` | No TUI; connects and runs, logging to stdout + DB. |

## IRCv3 scope

Implemented now (via the `irc` crate): connection + optional TLS, `CAP LS/REQ/END` negotiation,
**SASL PLAIN**, `account-tag` negotiation, and surfacing of message tags on events.
NickServ-message authentication is available as a fallback when SASL is not configured.

Deferred IRCv3 work: `batch`, `labeled-response`, `away-notify`, `chghost`, `server-time`
semantics, multi-prefix handling, and `echo-message`.

## TUI (interactive mode)

Built with **ratatui** + **crossterm**.

- **Servers screen** — list of network profiles; add / edit / delete / enable-disable.
- **Edit server** — per-profile fields: label, enabled, host/port, TLS + "accept invalid TLS cert"
  (testing only; off by default), nick/user/realname, SASL account/password, NickServ password,
  channels, and user modes (e.g. `+B` bot flag, applied to ourselves on connect). Saved directly
  to SQLite; `Ctrl-R` applies (reconnects all enabled networks).
- **Admins screen** (per selected server) — list/add/edit/delete admin entries `(nick, role,
  optional account)`; shows the bound hostmask/account.
- **Logs screen** — scrollable log view, **filterable by category**: `ERROR`, `DEBUG`, `MESSAGE`,
  and `COMMAND`. Log lines are prefixed with the originating network label.
- **Integrations screen** — masked global API credential editing. Tavily and DeepL changes apply
  on the next request without reconnecting.
- **Commands screen (F4)** — loaded commands and editable aliases/prefixes.
- **Modules screen (F5)** — validated global/network/channel module setting overrides. Changes
  apply immediately; `Ctrl-D` removes an override and restores its fallback/default.

## Storage (SQLite via `rusqlite`)

A single `bot.db`, accessed through a DB actor task (rusqlite is synchronous; the actor keeps it
off the async tasks). Schema:

```sql
config(key TEXT PRIMARY KEY, value TEXT);
servers(id INTEGER PRIMARY KEY, label TEXT UNIQUE, enabled INTEGER,
        host TEXT, port INTEGER, tls INTEGER,
        nick TEXT, username TEXT, realname TEXT, accept_invalid_certs INTEGER, umodes TEXT);
sasl(server_id INTEGER, mechanism TEXT, account TEXT, password TEXT, nick_password TEXT);
channels(server_id INTEGER, name TEXT, key TEXT);
admins(server_id INTEGER, nick TEXT, role TEXT, account TEXT,
       bound_hostmask TEXT, bound_account TEXT, PRIMARY KEY(server_id, nick));
profiles(id TEXT UNIQUE, server TEXT, nick TEXT, created INTEGER, last_seen INTEGER, title TEXT,
         birthday TEXT, pronoun_subject/object/possessive TEXT,
         location_display TEXT, location_label TEXT, lat REAL, lon REAL, timezone TEXT,
         PRIMARY KEY(server, nick));
module_kv(module TEXT, key TEXT, value TEXT, PRIMARY KEY(module, key));
module_setting_overrides(module TEXT, key TEXT, scope TEXT, server TEXT, channel TEXT, value TEXT,
                        PRIMARY KEY(module, key, scope, server, channel));
scheduled_jobs(module TEXT, id TEXT, server TEXT, channel TEXT, owner_profile_id TEXT,
               due_at INTEGER, payload TEXT, created_at INTEGER, PRIMARY KEY(module, id));
logs(id INTEGER PRIMARY KEY, ts INTEGER, level TEXT, category TEXT,
     source TEXT, message TEXT);
profile_aliases(server TEXT, nick TEXT, profile_id TEXT, last_seen INTEGER);
profile_accounts(server TEXT, account TEXT, profile_id TEXT);
achievement_stats(server TEXT, profile_id TEXT, module TEXT, stat TEXT, value INTEGER);
achievement_unlocks(server TEXT, profile_id TEXT, module TEXT, achievement_id TEXT,
                    unlocked_at INTEGER);
achievement_prestige(server TEXT, profile_id TEXT, module TEXT, prestige_id TEXT,
                     max_rank INTEGER);
achievement_backfills(server TEXT, profile_id TEXT, module TEXT, catalog_version INTEGER);
achievement_catalog_versions(server TEXT, module TEXT, catalog_version INTEGER);
achievement_dedup(server TEXT, profile_id TEXT, module TEXT, event_id TEXT, created_at INTEGER);
```

The bot connects to **all `enabled` server profiles simultaneously** (one IRC actor per network).
Events are tagged with the originating server `label`; module host functions take a `server` label
to target a specific network. Each actor is supervised and reconnects with exponential backoff.

Profiles receive a stable per-network UUID. Nicknames and services accounts are aliases of that
UUID, so `NICK` changes preserve profile and module identity. Existing nick-keyed rows are migrated
in place on startup. Each IRC actor reads `CASEMAPPING` from numeric `005` and applies the
network-specific `ascii`, `rfc1459`, or `strict-rfc1459` folding rules to profile aliases, admin
nicks, bound hostmasks, and module nickname lookups. Until advertised, the protocol-defined
`rfc1459` default applies. Conflicting legacy aliases fail visibly rather than merging profiles
arbitrarily.

## Data lifecycle

`jeeves --db bot.db --export-profile SERVER:NICK [--export-dir PATH]` writes the host-owned portion
of a versioned JSON export with private file permissions and exits. While the bot is running,
PM-only `!mydata summary` and `!mydata export` also invoke each loaded module's versioned lifecycle
hook and include module-owned data. Super-admin equivalents are `!data <nick> summary|export`.
Exports fail rather than silently omit a known module that is absent or lacks working hooks.

`!mydata delete` and super-admin-only `!data <nick> delete` issue requester-bound confirmation
tokens valid for ten minutes. Confirmation (`!mydata confirm <token>` or `!data confirm <token>`)
creates a resumable journal workflow. Each module receives only its own opaque KV entries and
returns an idempotent mutation plan; the host rejects unknown/duplicate keys and applies each plan
transactionally. Missing modules and malformed state leave the workflow pending for retry on module
reload/restart. Host profile rows, identity aliases/accounts, and UUID-owned scheduled jobs are
removed only after every registered module completes. Completed, cancelled, and expired journal
rows retain operational status/timestamps but redact profile and requester identifiers.
Super-admins may inspect confirmed pending/failed workflow IDs and remaining module counts with
PM-only `!data pending`; this status output contains no profile identifiers.

Lifecycle retention semantics:

- Shared profile fields, nick aliases, services-account bindings, owned reminders, module
  progression/cooldowns, active-game membership, seen records, and memos/quotes involving the
  subject are exportable and deleted on a confirmed request.
- Channel/system timers and state not owned by the subject remain. Aggregate records are rewritten
  to remove only that subject; empty user-created aggregates are removed.
- Operational logs are not identity-indexed or rewritten. They continue to age out under the
  existing 30-day/100,000-row cap.
- Admin configuration is an operator security record, not self-service profile data. It remains
  until a super-admin changes the admin list.
- Erasure is immediate in the live database. Existing backups are not rewritten in place: local
  restore points age out after 3 daily, 4 weekly, and 3 monthly copies, while encrypted Backblaze
  restore points retain 4 weekly copies.

Data otherwise remains until a user or super-admin requests deletion, except for features with an
existing documented expiry such as retained logs or expiring memos.

## Achievements

Achievements are cosmetic, per-network collections keyed by stable profile UUID. The host owns all
state; modules declare versioned stat, finite milestone, optional/secret, and prestige metadata in
an `achievements` export, then submit successful domain events through `award_stats`. A batch is
validated against the calling module's manifest and applied atomically. Unknown/cross-module stats,
missing profiles, zero/overflowing increments, and duplicate event IDs are rejected without partial
writes.

Finite non-optional achievements form the dynamic completion catalog. Optional, secret, social,
configuration-dependent, and meta achievements never block “The Whole Shooting Match”; prestige
ranks are endless and also do not block completion. Catalog reload reconciles stored stats,
silently revokes stale completion, and announces a later regain caused by a real award. Roman rank
I omits its numeral.

Modules with reliable historical state expose a pure `achievement_backfill` hook. The host supplies
only that module's KV entries on first deployment or catalog-version increase and transactionally
applies absolute `set_max` values. Backfill unlocks, prestige, meta milestones, and completion are
silent and idempotent. Achievement stats, unlocks, ranks, per-profile backfill markers, and
deduplication records are included in profile exports and deleted with the subject.

`!achievements [nick]`, `!achievements list [module] [nick]` (module names in any case), and the
summary provide bounded collection, recent-unlock, closest-milestone, module, catalog, and
prestige views; empty clauses are omitted, and the module overview lists started modules first and
folds untouched ones into a count. `!achievements top [module]` ranks the five profiles holding
the most current achievements (ties to whoever got there first) and `!achievements rare [module]`
lists the achievements with the fewest holders; both come from the host `achievement_board`
query (under `achievements_get`), exclude opted-out profiles, derived meta milestones, and retired
modules, mask secret names, and break nicks so nobody is highlighted. An admin can start or stop a
weekly digest of the week's unlocks in a channel with `!achievements digest on|off` (a durable
channel job; `digest_weekday` and `digest_hour_utc` settings, default Sunday 18:00 UTC); quiet
weeks post nothing. Unearned secrets
expose only an “Undiscovered secret” placeholder with no name, condition, stat, progress, or
threshold. Unlocks from one user/channel within approximately three seconds are combined into one
themed announcement showing at most three names plus an additional count.

## AI responder

AI chat is an optional WASM module backed by the narrow host-owned `ai_chat` capability.
The host alone reads provider credentials, the configured OpenAI-compatible endpoint/model, and a
size-bounded `SOUL.md`; the module has no general HTTP or filesystem access. Channel responses are
off by default and require explicit `<bot nick or alias>,` or `<name>:` addressing. Private-message
behavior, aliases, stable-profile cooldown, temperature, token output limit, and IRC response byte
length/count are operator settings. AI responses split at sentence boundaries where possible and
send at most three lines by default.
Obvious command/how-to questions include a bounded, host-generated snapshot of the live command
registry as trusted reference context, including effective aliases. The model is told not to invent
syntax and to direct users to `!help` when the registry metadata is insufficient.
Requests and responses are bounded and sanitized, and only one provider call runs at a time. An
optional, default-off `web_search_enabled` setting lets time-sensitive questions (for example,
scores, current weather, news, and prices) make one Tavily-backed `web_search` request before the
AI call. Search snippets are bounded, labelled as untrusted reference material, and the reply
includes the first source URL; unavailable or empty searches do not fall back to an ungrounded
current-events answer. Context is a configurable, age-limited 0–30-line transcript that is never
written to disk: channel lines come from the host's recent-lines buffer (the line being answered is
sent once, as the question), while the bot's own answers and private conversations are held only in
the ai worker's memory (bounded, lost on reload, included in data export and purged by erasure).
Transcripts older builds kept in module KV are emptied the first time each conversation asks
again. Network/channel and per-user PM contexts are isolated and sent to the provider as
explicitly untrusted context separate from the current question.

Channel answers are prefixed with the asker's name (`ai.channel_response`, "{user}: {response}"),
and any answer line that would start with a command prefix (`!`, `.`, `/`, …) gets a zero-width
space in front so other bots never run it. `<name>, tl;dr` (also "tldr", "catch me up", "what did
I miss") summarises the recent channel conversation since the asker last spoke, or all of it when
fewer than three lines followed, in at most three sentences ("{user}: TL;DR — …"). Private
questions have a per-person daily allowance (`pm_daily_limit`, default 20, UTC days, 0 for none),
counted in module KV and covered by data export and deletion. `!ai` explains how to ask, and
`!ai privacy` names the provider (`provider_name`, default Neuralwatt) and links its privacy policy
(`privacy_url`); `!help ai` notes that questions and recent channel lines go to that provider.

The AI can look things up with read-only bot commands (`tool_commands`, default weather, forecast,
time, until, wiki, define, etym, calc, convert, crypto; empty disables). The request names them
and the host describes them from its own command registry in a trusted instruction: if one would
answer better than general knowledge, the model replies with exactly `RUN: !command args`. The
module runs it through `run_command` as the asker (so that module's cooldowns apply), then asks
again with the output as untrusted context and no tools, so each question makes at most one lookup
and two provider calls. Tools aren't offered when a web search already supplied material. AI
(and tarot) guest calls may run for 60 seconds instead of the default 20.

## Operator profile repair

The F8 Profiles page exposes stable identity metadata read-only and permits validated edits only to
host-owned profile fields. Lifecycle-aware modules may expose their existing export for inspection;
operators may reset that subject's contribution through the module's pure deletion plan, but may
not edit opaque JSON or KV directly. Every repair requires a preview and explicit confirmation,
creates and verifies a local pre-repair SQLite snapshot, logs affected field/module names without
values, and fails if the underlying host or module data changed after preview.

## YouTube integration

YouTube credentials and HTTP access are host-owned behind the narrow `youtube_lookup` and
`youtube_search` capabilities. The WASM module provides `!yt` search and optional canonical-link
metadata announcements. Summaries emit canonical `youtube.com/watch?v=` links reconstructed from
validated video IDs, omitting share-link tracking parameters. The standard scoped `enabled` setting suppresses ambient events but does
not suppress a command explicitly routed to that module, allowing passive announcements to remain
off by default while manual search stays available. Provider responses, module output, cooldowns,
and per-channel seen-video state are bounded; personal cooldown state participates in lifecycle
export and deletion.

## GIF search integration

KLIPY credentials and HTTP access are host-owned behind the provider-neutral `gif_search`
capability. The `gif.wasm` module provides channel-only `!gif <search terms>` and randomly selects
from a bounded top-result pool. Queries, provider responses, HTTPS media URLs, output, and per-user
request frequency are bounded. Every successful reply includes provider attribution, uses themed
output, awards only after send, and stores cooldown state under the sender's stable profile UUID
with lifecycle export/deletion.

## Channel triggers

`triggers.wasm` replaces the retired banter module with operator-defined call-and-response. Each
channel keeps up to 50 triggers; a trigger has up to five alternative phrases (whole words, up to
four words each, case- and punctuation-insensitive, never substring matches), a pool of up to 25
responses of at most 300 characters (one chosen with host randomness), an optional nick
restriction, and its own cooldown (default 10s), plus a channel-wide spacing setting. Responses may
use `{user}`, `{nick}`, `{honorific}`, and `{channel}`. Admins manage triggers with
`!trigger add|del|nick|cooldown|preset`; anyone can `!trigger list`, and `!trigger show <word>`
sends a trigger's numbered responses privately. The crow and sailing presets carry banter's
original lines. Output is off until the channel's `enabled` setting is on; commands, PMs, and the
bot's own lines never trigger. Triggers are operator configuration and store no personal data.

Retired modules (`pop`, `banter`) are purged on startup: their scheduled jobs, KV, and setting
overrides are removed so the scheduler never retries timers for code that no longer exists.

## Permissions (per network)

Each network has an `admins` list of `(nick, role)` where `role` is `admin` or `super-admin`
(super-admin implies admin). The **host** resolves the sender's role for every message and stamps it
onto the event; modules enforce by checking `msg.role` (the bundled admin module gates `!shutdown`
to super-admin and `!reload`/`!refresh` to admin).

`operator.wasm` provides channel-only, admin-gated moderation commands when the bot itself holds
the required channel privileges: `!ban <nick|mask> <duration>`, `!unban`, `!kick`, `!op`/`!deop`,
`!hop`/`!dehop`, `!voice`/`!devoice`, and `!topic <text>`. Timed bans are durable and automatically
removed after the requested duration. The host exposes a narrow validated operator-action API, not
arbitrary raw IRC commands.

Identity is verified by, in order: an operator-pinned services account (matched against the IRCv3
`account-tag`); else a previously-bound account; else a previously-bound `nick!user@host` hostmask;
else — on first contact — the strongest identity available is bound ("introduction" /
trust-on-first-use), preferring the services account over the hostmask. The bot negotiates the
`account-tag` capability so verified accounts are available. Admin entries without a pinned or
bound account are flagged in red in the TUI and logged as errors at every connect.

Profiles (the stable UUIDs modules key state on) follow the same rule when a message is attributed
to its sender: an account-backed profile is only given to a sender presenting that account. Someone
using a registered user's nick without being logged in gets a separate profile, and the real user
reclaims the nick (and plain by-name lookups such as `!raid <nick>`) the next time they speak while
logged in. Nicks that have never been account-backed still resolve by nick alone.

`module_kv` is the namespaced store modules persist into via the `kv_get`/`kv_set` host functions
— this is how modules "add their own info to the database".

## Module system (WASM via extism)

Any `*.wasm` file in the `modules/` directory (relative to the bot's working directory) is loaded
automatically at startup. Modules are sandboxed WASM plugins run via the **extism** host SDK; they
may be written in any language with an extism PDK (Rust is used for the bundled `admin` module).
Each module has a bounded worker thread and a 20-second guest execution deadline. Host functions
enforce the operator-owned policy in `module-capabilities.toml`; unknown modules receive only
`log`, `theme`, `now`, and namespaced setting reads.

Compiled modules are cached on disk in `wasm-cache/` (`--wasm-cache DIR`; an empty value disables
it), so startup and reloads only compile `.wasm` files whose bytes changed. wasmtime keys each
entry on the module bytes, engine settings, and its own version; the directory holds a generated
`wasmtime-cache.toml` and the entries under `compiled/`, which wasmtime prunes itself. The host
never reads the user-wide `~/.config/wasmtime` config or needs `$HOME`, and a cache that cannot be
prepared is logged and skipped rather than blocking modules.

### Guest exports (a module implements any subset)

- `init` — called once at load; the module may register metadata/commands.
- `commands` — optional versioned command metadata used by the host alias registry and TUI.
- `settings` — optional versioned typed setting metadata used by the host and TUI.
- `achievements` — versioned stat, finite milestone, secret/optional, and prestige metadata.
- `achievement_backfill` — pure, versioned historical `set_max` values from host-supplied KV.
- `data_export`, `data_delete` — pure personal-data lifecycle views and mutation plans.
- `on_message` — channel/PM `PRIVMSG` events (JSON payload).
- `on_event` — connection/join/part/numeric events (JSON payload). Other users' joins
  (`UserJoined { channel, nick, account }`) reach only modules holding the `join_events`
  capability.

### Host functions — the "base" capability API (available to all modules)

There is no separate `base.wasm`; the common operations are the host-function surface:

- `send_message(server, target, text)`, `send_notice(server, target, text)`
- `join(server, channel)`, `part(server, channel)`
- `kv_get(key) -> value`, `kv_set(key, value)` (namespaced by the calling module's name)
- `setting_get(key, server?, channel?) -> value` — the calling module's validated effective value;
  precedence is channel → network → global → advertised default
- `schedule_set(job)`, `schedule_cancel(id)`, `schedule_list(server?, channel?)` — namespaced,
  quota-limited durable jobs delivered back to the owning module as targeted timer events.
  Persisted timers are explicit work and are delivered whenever their module is loaded, regardless
  of its ambient `enabled` setting; handlers gate spontaneous output themselves so manual
  multi-stage workflows can still finish
- `log(level, category, message)`
- `now() -> unix_seconds` — current time (WASM modules have no system clock)
- `theme(key, default, vars) -> string` — fetch a user-configurable string (see Themes)
- `award_stats(request) -> AwardStatsResponse`, `achievements_get(request)` — validated,
  module-namespaced achievement writes and bounded profile/catalog reads
- `profile_ensure(server, nick)`, `profile_get(server, nick) -> Profile`,
  `profile_set(ProfileUpdate)`, `profile_clear(server, nick, field)` — shared, host-level user
  profiles any module can read/write
- `geocode(query) -> GeoResult` — keyless Open-Meteo geocoding (lat/lon + canonical label);
  leading `ft`/`ft.` place abbreviations are expanded to `Fort`
- `local_time(timezone, unix_seconds? | local?) -> LocalTimeResult` — timezone conversion using
  the host's timezone database, including daylight-saving transitions. Zones may be IANA ids in
  any case, common abbreviations (mapped to a representative DST-aware zone), or UTC offsets; a
  `local` wall time resolves to its instant (ambiguous → earlier, skipped → one hour later)
- `channel_members(server, channel) -> [nick]` — the host's current view of a channel's members,
  tracked from NAMES/JOIN/PART/KICK/QUIT/NICK and rebuilt on every connection
- `weather(lat, lon) -> WeatherResult` — keyless Open-Meteo current conditions (including wind
  direction, gusts, and UV), the local calendar day's forecast liquid-rain total, a three-day
  local daily forecast with sunrise/sunset, plus optional CAMS US AQI and particulate readings;
  AQI failure does not suppress the weather response. Responses are cached 10 minutes per ~1 km
  cell
- `weather_alerts(lat, lon) -> WeatherAlertsResult` — active official warnings covering a point:
  US National Weather Service alerts inside US coverage, and MeteoAlarm warnings for Germany,
  Sweden, and the United Kingdom (country feeds cached five minutes; Swedish and UK areas matched
  by polygon, German ones by the DWD warn cell containing the point, looked up once from DWD's
  map server). Each alert carries a stable key (NWS VTEC, or source/event/area/level), the area,
  a 0–3 colour level (US watches yellow, warnings orange, emergencies and extreme warnings red),
  its end time, and its source; superseded and cancelled European messages are dropped, and
  `incomplete` marks a provider that couldn't be reached. Cached like `weather`; alert lookup failure does not suppress the weather response
- `weatherlink_current() -> WeatherLinkResult` — normalized current outdoor observations from one
  configured WeatherLink v2 station; the host owns the API key, API secret, station ID, a
  30-second response cache, and safe provider-error mapping
- `web_search(query) -> SearchResponse` — Tavily ranked web results; the API key remains in the
  host process and is read from the global SQLite setting, then
  `RUSTJEEVES_TAVILY_API_KEY`/`TAVILY_API_KEY` as fallback
- `wikipedia_lookup(query) -> WikipediaResponse` — a bounded introductory extract and stable
  attribution link from English Wikipedia; public MediaWiki HTTP and caching remain host-owned
- `run_command(server, channel?, text, user, allowed) -> RunCommandResponse` — capability
  `run_commands`: runs another module's command on someone's behalf with no admin role and
  returns what it would have said instead of posting it (the target worker runs the message with a
  thread-local capture buffer that `send_message`/`send_notice` write into). The host resolves
  aliases and shortcuts first, refuses anything outside the caller's `allowed` canonical names,
  the caller's own commands, and the admin/operator/data/ai modules, and waits at most 15 seconds
- `link_title(url) -> LinkTitleResponse` — capability `link_title`: a page's `og:title` (or
  `<title>`) and `og:site_name`. Every connection goes through a resolver that keeps only public
  unicast addresses (no loopback, private, link-local, CGNAT, documentation, multicast, IPv6
  unique/link-local, or mapped private addresses) and dials exactly those, so redirects (at most
  three) and DNS rebinding can't reach internal hosts. Only HTML is read, at most 512 KB, within
  six seconds; results are cached for an hour and fetches are limited to 30 a minute
- `wikiquote(topic, pick) -> WikiquoteResponse` — one quote from the Wikiquote page best matching
  `topic` (the module supplies `pick` from `random_bytes`), attributed to its work heading, with
  sections about the subject, disputed/misattributed quotes, and cast lists skipped; an empty
  topic returns today's quote of the day. Parsed pages are cached for a day
- `etymology_lookup(word) -> EtymologyResponse` — the English etymology sections of a Wiktionary
  entry as plain text (up to two), trying the word as typed, lower case, and capitalised; gated by
  the `dictionary_lookup` capability
- `translate(text, target_lang, source_lang?) -> TranslateResponse` — DeepL text translation;
  Free (`:fx`) and standard keys select the correct endpoint automatically, and the key remains in
  the host process
- **privileged:** `bot_reload()`, `bot_refresh()`, `bot_shutdown()`

Events are delivered as an `EventEnvelope { server, event }`; message events carry the sender's
resolved `role` (see Permissions) plus `nick`, `user`, `host`, `target`, `text`, and IRCv3 tags.

Payloads cross the host/guest boundary as JSON (serde types defined in the `jeeves-abi` crate).
Modules share guest-side behaviour through the `jeeves-guest` crate: the common host calls
(themed replies, the clock, settings, KV including prefix listing), hex key encoding,
non-highlighting names, pronoun-aware address, unbiased randomness, and a warn-once cooldown. Its
`encode` matches every module's former copy, so stored keys are unchanged. Modules whose native
tests stub host functions (gacha, wordle) keep their own host-calling helpers and share only the
pure ones.

Every loaded module receives a standard boolean `enabled` setting at global, network, and channel
scope unless it advertises its own boolean definition. The host checks this before dispatch, so an
override disables the module without reload. Spontaneous modules should advertise a default of
`false`. Overrides are retained in SQLite while a module is absent. Ordinary settings cannot hold
secrets; credentials remain in the masked integrations system.

### Commands and aliases

Modules advertise canonical commands, descriptions, usage, and default aliases through the
optional `commands` export. Operator overrides are stored globally in SQLite and edited under TUI
**Commands (F4)**. Names omit the command prefix, match case-insensitively, and may contain only ASCII
letters, digits, `-`, or `_`. The registry rejects collisions with canonical commands or aliases.

When an alias is used, only the owning module receives a copy with its first token rewritten to the
canonical command. Other modules receive the untouched IRC message so history and quotes preserve
what the user actually typed. Overrides remain stored while a module is absent and become active
again if it is reinstalled. Modules match only their canonical names, so removing an alias really
frees it.

A command may also declare **shortcuts**: top-level names that expand into one of its subcommands
(`!yes` → `!fish yes`, `!pay` → `!isles pay`, `!hatch` → `!egg hatch`). Shortcuts are default
aliases carrying an expansion: the owning module receives `!{command} {expansion} …`, the F4
editor shows them as `!yes→yes`, `!help <shortcut>` shows the shortcut's own usage and description,
and removing one in the editor frees the name for another module. Modules keep one or two
top-level nouns; generic verbs (`yes`, `no`, `pay`, `menu`, `heal`, `clear`, …) exist only as
removable shortcuts. `!help <command|alias|shortcut>` also works when the word isn't a module name.

Channel targets are normalized before dispatch: the resolver learns each channel's spelling from
the bot's own JOIN and rewrites case variants (`#Games` vs `#games`) to it, so module state and
timers never split by case.

The host keeps a volatile in-memory buffer of recent channel lines (100 per channel, at most three
hours old, never persisted), readable through the `recent_lines` capability. Translate's bare `!tr`,
history's `s///`, and the AI's conversation context read it instead of copying chat into module KV. Profile erasure purges a
subject's buffered lines immediately.

Messages also carry a host-stamped `honorific` for the `{honorific}` placeholder: "sir" for he,
"madam" for she, and otherwise the person's display name, so nobody is misgendered.

An optional operator-local dispatch policy may be supplied outside the repository with
`--local-rules PATH`. Rules match a network label, channel, stable profile UUID, and selected module
names, then probabilistically drop targeted command delivery before the module runs. This policy
is disabled when no file is supplied, has no user-facing response, and never mutates module state
for a dropped command. It is intended for reversible operator-local behavior and is not part of
the normal persisted bot configuration.

Accepted command-prefix characters are a global SQLite setting, editable from **Commands (F4)**
with `p`; the default is `!`. Set it to `!.,` to accept all three styles, or `.` to replace `!`.
The host rewrites a matched command to `!canonical` only for its owning module, preserving existing
module compatibility while passive modules retain the original text.

### Utility modules

`search.wasm` provides `!g`, `!google`, and `!search`. It returns the first ranked Tavily result,
enforces a per-user cooldown, and falls back to a normal search URL when Tavily is unconfigured or
unavailable. The plugin receives neither unrestricted HTTP access nor the API key.

`wiki.wasm` provides `!wiki <topic>` and the `!wikipedia` alias. It returns the first matching
English Wikipedia article's introductory extract, cut at a sentence end where possible, and a
stable attribution link. When the best match is a disambiguation page it lists the first few
meanings in page order ("Mercury could mean several things: Mercury (planet) · …") instead.
`!wq <topic>` (`!wikiquote`) gives a random quote from the matching Wikiquote page, attributed to
the page and, where the page groups quotes by work, the work ("Discworld, Small Gods (1992)");
bare `!wq` gives Wikiquote's quote of the day. Both commands share the lookup cooldown.

`calc.wasm` provides `!calc`, `!convert`, and `!crypto`. `!calc` is a recursive-descent
evaluator (no `eval`) with `+ - * / % ^ !`, right-associative powers, implicit multiplication
(`2pi`, `3(4+1)`), `1e6`/`1,000`/`1_000` numbers, constants (`pi`, `e`, `tau`, `phi`), argument-
checked functions (roots, logs, trig in radians and degrees, rounding, min/max/avg/sum, gcd/lcm,
factorial), and `ans` for the caller's previous result (memory only). Results use significant
figures when tiny and scientific notation beyond 1e15. `!convert` accepts `to`/`into`/`as`/
`->`/`=`/`in` separators, compound amounts (`5 ft 10 in`, `1 h 30 min`), degree symbols, and
rejects temperatures below absolute zero; it covers length, mass, volume (US and UK pints,
quarts, gallons, fl oz, with a `pint_system` setting for the plain names), speed, data (bytes vs
`Mb` bits), data rate, area, time, pressure, energy, and power. Anything that isn't a physical
unit goes to the host `money` capability, so `50 pounds to kg` is mass and `50 pounds to euros`
is currency. The host converts fiat through ECB reference rates (Frankfurter, cached six hours)
and crypto through CoinGecko (cached five minutes per coin, at most 15 calls a minute, less
common tickers resolved by search and cached a day); replies show the rate and source.
`!crypto btc eth doge` shows up to three prices in USD, GBP, and EUR with 24-hour change.

`animal.wasm` provides `!animal [kind | list]` with shortcuts `!pug`, `!cat`, `!dog`, `!fox`,
`!capy`/`!capybara`, `!duck`, and `!bunny`; bare `!animal` picks at random and `list` is sent by PM.
The host owns the catalogue (about 150 animals) and every image source through `animal_image`:
dedicated keyless APIs for capybaras (capy.lol), foxes, dogs and ~25 breeds (random.dog,
dog.ceo), cats, ducks, and bunnies, and Wikimedia Commons species categories for the rest, with
each category's file list cached for a day. Modules can only name a catalogue animal, never a URL
or search term; returned URLs are HTTPS-only and bounded, and outbound requests are capped at 30
a minute. Replaces the retired pug module, whose achievement progress migrates to animal.

`define.wasm` provides `!define <word or phrase>` (up to three words). The host asks
dictionaryapi.dev first (phonetics, up to three senses, synonyms) and falls back to English
Wiktionary's definitions when that service is down or lacks the term; cut definitions end in `…`. The
plugin receives no unrestricted HTTP access; the native host validates, caches, and performs the
public MediaWiki request. `!etym` (`!etymology`) gives the word's English etymology from
Wiktionary, numbering a second etymology when a word has two and it fits.

The interactive TUI exposes global API credentials under **Integrations (F3)**. Secret fields are
masked while editing and stored in SQLite's `config` table; the database itself is not encrypted.
Saving or clearing a Tavily or DeepL key takes effect on the next request without a reconnect or
module reload.

`history.wasm` provides channel-local `!seen <nick>` and quotes. Reading is the default: `!quote`
shows a random quote, `!quote #N` one by number, `!quote <nick>` a random quote by that person,
and `!quote <words>` a random quote containing every word. `!quote add <nick>` saves that person's
latest line here, and `!quote add <nick> <words>` saves the whole line in which they said those
words, but only if they said it in this channel within the last hour (checked against the host's
recent-line buffer), so nobody can be quoted saying what they didn't. `!quote add "text"` (or the
old `!quote "text"`) quotes yourself. A channel holds at most 1,000 quotes. `!seen` answers for
this channel and, without naming it, mentions when the person has been active in another room
more recently (or only elsewhere); a network-wide "last active" record per profile makes that
possible. Private messages are never recorded or exposed. Quote deletion is limited to the quoted
person, submitter, or an admin. It also supports sed-style
corrections of the speaker's own most recent matching line among their last ten lines within the
past hour (read from the host's recent-line buffer; corrected text is remembered in memory so
corrections chain):
`s/pattern/replacement` (the final `/` is optional), with optional `g` and `i` flags, escaped
slashes, regex capture replacements, bounded output, and chained corrections. The `g` flag applies
to every match in that one selected line only. Corrections apply only to your own lines.

`karma.wasm` keeps per-channel scores keyed on stable profiles: `nick++`/`nick--` as the last word
of a line, or as the first word followed by a reason (`bob++ for fixing the build`). Only nicks
with a profile count, you can't vote for yourself, and each voter/target pair has a cooldown
(`cooldown_seconds`); a vote inside it gets one "that one didn't count" notice, then silence.
With `announce` on (the default) votes are confirmed in the channel ("bob → 12 (for fixing the build)"), at most six per channel per minute, with names broken so nobody is highlighted.
`!karma [nick]` shows a score, `!karma top|bottom` the leaderboard, `!karma reasons <nick>` the
last five reasons (shown without who gave them; stored with the voter so erasure removes them),
and `!karma given` whom you've upvoted most here.

`memos.wasm` provides `!tell <nick> <message>`. In a channel the memo waits there and is delivered
when that user next speaks in it, or by NOTICE when they join it (the host's `UserJoined` event,
via the `join_events` capability). Sent by private message, `!tell` leaves a private memo delivered
by PM when the recipient next speaks anywhere on the network, messages the bot, or joins a shared
channel. Stable profile identity is used where available so nick changes don't lose messages. A
recipient the bot has never seen is accepted with a warning to check the spelling. `!memos` reports
a waiting count without exposing text (private memos when asked by PM), `!memos clear` discards
them, `!memos sent` lists the caller's still-waiting memos here, and `!memos unsend <id>` withdraws
one. A small per-book "anything pending?" flag spares ordinary chat from loading the memo book.
Memos expire after 30 days by default; retention is configurable globally, per network, or per
channel. Super-admin memo inspection and clearing are initiated in the relevant channel, return
their results privately to the invoking admin, and emit content-free audit logs.

`translate.wasm` provides `!tr` and `!translate`. Text without a language goes to the
`target_language` setting (default `EN-US`; any scope),
`!tr fr Hello` auto-detects the source language, and `!tr de:en Guten Morgen` supplies it
explicitly. Two-letter codes that are also everyday words (`it`, `no`, `de`, `es`, `en`, `el`,
`da`, `et`, `id`, `ja`, `vi`, `uk`) are treated as text unless written `>it`, `to it`, a language
name, or `src:it`. Bare `!tr` translates a likely non-English message from the host's recent-line
buffer and includes its speaker. It limits input and per-user request rate,
maps common language names to DeepL codes, themes every wrapper/error, and never receives the API
key. The per-user delay is the `cooldown_seconds` setting (default 10).

Auto-translation is off unless the module's `enabled` setting is on for a channel (commands work
either way). It considers only lines of at least `auto_min_words` words (default 4; unspaced
scripts count two characters per word) with commands, CTCP, URLs, and a leading "nick:" removed,
and only when whatlang is confident (reliable, ≥ 0.85) that the line is in a DeepL-supported
language other than the target and not in `auto_skip_languages`. Speakers who ran `!tr auto off`
are never translated. Each channel has an hourly post cap (`auto_hourly_limit`, default 60) and a
per-UTC-day character budget (`auto_daily_chars`, default 20,000) charged before each request;
nothing is posted when DeepL decides the line was already in the target language. Posts read
"↪ speaker (fr): …" with the speaker's nick broken so it doesn't highlight them. `!tr auto`
shows the channel's state and budget and the caller's own choice.

`clock.wasm` provides `!time`, with `!clock` as a default alias. With no argument it uses the
caller's saved profile location; a nickname uses that user's saved location; a zone name,
abbreviation, or UTC offset is used directly; any other argument is geocoded as a place. Saved
IANA timezones are converted host-side with current daylight-saving rules, and responses do not
disclose a user's exact saved location. `!time a, b, c` answers several at once; `!time #channel`
(or `here`) groups the channel's current members by local time using their saved timezones
without highlighting them; `!time 3pm PST in London` converts a wall time; `!time format 12|24`
is a per-profile preference. `!until` (`!countdown`) counts down to a date, weekday, or named
event (christmas, new year, halloween…) in the caller's timezone, or UTC with a note.

`weather.wasm` provides `!weather` (`!w`) and `!forecast` (`!fc`) for the caller, a nickname's
saved location, or a geocoded place (the reply names the place the geocoder found). Reports carry
wind direction, notable gusts, UV when high in daylight, optional AQI, and significant US alerts.
Per-profile preferences: `!weather units metric|imperial|both` (default both), `!weather aqi
on|off`, and `!weather daily HH:MM|off`, a morning forecast by PM delivered through a
profile-owned scheduler job in the person's own timezone.

Severe-weather broadcasts: an admin runs `!weather alerts on|off` in a channel. Every ten minutes
a durable channel job looks up the saved locations of the channel's current members (unless they
ran `!weather alerts me off`), checks each distinct ~1 km cell once, and posts each new warning at
or above `alert_level` (yellow, orange — the default — or red) once, most severe first and at most
four per check, as "⚠ {title}: {area}, until {local time}." Posts name the warned area, never the
person. When a watched area stops reporting a warning the channel hears "✓ … has ended"; warnings
whose only watchers left, opted out, or fell below a raised threshold are forgotten silently, and
an unreachable provider never ends a warning. `alert_quiet_hours` (UTC, e.g. `23-7`) defers
posts; warnings still in force post afterwards. The channel's record of posted warnings stores
one-way hashes of the cells that reported them, not coordinates. On-demand `!weather` reports
include European warnings (yellow and up) alongside US ones.

`fishing.wasm` provides the persistent `!cast`/`!reel` fishing game, including locations, species
careers, records, seasons, artifacts, and operator-themed narration. Each angler is stored in their
own `player:{server}/{id}` KV entry and shared state (casts, chum, events, champions) in `data`, so a
save rewrites only what changed; saves from older builds that kept every angler inside `data` load
unchanged and move out on the next save. `!aquarium` keeps the 50 most recent rare catches plus a
lifetime count. Its only top-level commands are
`!cast`, `!reel`, and `!fish`; everything else is a `!fish` subcommand (`!fish mastery`,
`!fish yes`…) with a default shortcut of the old name (`!mastery`, `!yes`…). Its personal opt-in
`!danger` mode requires a short explicit `!yes`/`!no` confirmation and reuses the same catch,
progression, and persistence transaction while changing the fiction to armed conflict. Successful
DANGER catches can replace a cosmetic weapon or remove one of four otherwise cosmetic limbs;
each limb returns three days after its injury. Losing all four blocks fishing until the first limb
returns. `!safety`
leaves the mode, and `!limbs` reports its current equipment and recovery state. While DANGER MODE
is active, `!hands` provides that same injury report instead of its usual dynamite status. `!heal` can
restore missing limbs from either DANGER MODE or `!dynamite` for 10,000 XP per limb by default;
it clears the associated ban but does not disable DANGER MODE.

Catch replies are two lines: the catch itself (fish, bonuses, lure reveal, DANGER outcomes, and a
worn flourish), then a ★ line for personal records, trophies, mastery, level-ups, and brass, so the
catch stays readable however much happened. Staged tips introduce deeper features as players reach
them — choosing a location (level 2), bait (3), lures (5), chum (7), mastery and records (9) — one
tip after a reel at most. Using a feature (a named-location cast, bait, a rigged lure, thrown chum)
marks its tip used; an unused one gets a gentle reminder after a week, at most weekly and twice per
tip. Players already past a tip's level when tips arrived never see it. `!fish tips off|on`.

Fishing levels never cap. Past level 19 each level demands more XP while catch payouts stay
fixed, so progress slows but never stops, and from level 20 catches sometimes wear one of ten
colour epithets, unlocked one per ten levels (Verdant at 20, Ashen at 30, and so on). The retired
parallel-universe expedition system (`!fish jump`/`universe`/`expedition`) migrates away on load:
every stashed world's lifetime XP folds into the single Prime save, re-deriving the level on the
endless curve; surviving Deep Stars remain as permanent cosmetic badges.

Rarely (about one landing in fifty), a successful reel instead snags a wormhole: the angler is
pulled inside and assigned one task — catch a specific oddly named fish, or find one odd piece of
junk, drawn from wormhole-only pools. Wormhole casts ignore location, bait, and wait-time rules,
and the target turns up at a flat 2-in-6 rate per cast so nobody is trapped forever; near misses
still surface strange detritus. Finishing the task pays three levels' worth of XP and returns the
angler to ordinary fishing; a non-secret achievement marks the first completion.

`darts.wasm` provides the asynchronous 301 race: `!darts [1|2|3]` spends up to three darts in a
player’s turn, the third starts a configurable rest, and another player’s throw releases resting
players. Darts are resolved sequentially against a weighted board; a miss scores nothing and the
volley continues. Double-out checkout and beginning-of-turn bust rollback (to the score when the
turn's first dart was thrown, across however many commands) are enabled by default. A turn ends on
its third dart, a bust, or the day's last allowed dart, and never carries across a UTC day; asking
for more darts than remain throws those that remain. Leaving one point under double-out is a
bust; legacy players stranded on one resume from two on their next throw. Permanent skill remains
distinct from temporary throwing form: each dart causes configurable fatigue, rare configurable
pub mishaps cause an additional form loss, and a completed rest restores form. Exact zero clears
the match, and active players plus lifetime results use stable profile IDs. `!darts wins` reports the top five
lifetime winners; `!dartsstats` reports the invoking player's skill and current form.
Operators may enable `free_play_enabled` at channel scope. Such a channel has an independent
match, per-user skill/form, daily counters, and leaderboard; it bypasses the daily dart cap and
between-turn cooldown without changing any main-room records or limits. Free-play wins do not
contribute to the normal achievement counters. Normal darts are available only in the configured
network-level `game_room` (default `#games`); commands elsewhere reply with a themed redirect and
do not touch state. Aimed darts land true 50% of the time at skill 10, rising to 90% at skill 100 (scaled by form);
a slip wobbles to a neighbouring bed (a double falls to its single or goes wide, a triple to its
single, the bullseye to the outer bull). Any player on a finish aims at it at least
`novice_checkout_aim_percent` (default 15) of the time. Form recovers continuously:
`form_recovery_per_rest` points per cooldown-length of rest, carried across partial periods, so a
day away restores it fully. Mishaps carry themed flavour text (`darts.mishap`). Busts and
near-misses are counted per player so all three achievement stats backfill. Match and free-play keys are case-insensitive by channel. The legacy `#transience` carry-over is retired (it re-ran on every empty room and resurrected the stale match); the old key is kept but never read. Free-play channels work in any room where they are enabled. Records that fail to parse are refused rather than overwritten, and commands without a stable profile id are refused. The
`free_play_enabled` setting and its separate namespace remain available for a future `#freeplay`
room, but are disabled for the current game room.

`wordle.wasm` provides a daily personal six-letter puzzle through `!word` (`!wordle` alias).
Each stable user receives an independent answer, discovery board, and configurable number of
attempts per UTC day; answers may repeat between players so users can still help each other. An
unsolved puzzle carries forward for one additional fully failed daily round; after the second
fully failed round, the bot quietly returns the answer to that player's recent circulation and
assigns a fresh word on the next UTC day without revealing the old answer. A solved player receives
a new puzzle on the next UTC day. `!word` also lists people who solved their own puzzle today;
`stats`/`score` reports
the invoking player's solved count, games played, win rate, and average valid guesses for completed
words; `previous` (or `!word previous`) lists guesses already made on the current puzzle, including
guesses carried over from earlier UTC days; `top` and admin `new` retain the longer-running
household game controls. The authenticated Discord admin
bridge adds `wordle [network] <nick> new` to replace only that profile's puzzle and
`wordle [network] <nick> chances <1-10>` to set exactly how many valid guesses remain on its
existing puzzle; the network is optional when only one is connected.

Guesses are answered either as coloured letter tiles (mIRC green for placed, yellow for present,
grey for absent, letters kept in order so a colour-stripping client still reads the word) with
"Out: B K S", or as the plain-text sentence (better for screen readers), which also lists the
ruled-out letters. The channel default is `feedback_style` (default tiles); `!word style tiles|text`
sets a personal choice, stored per profile and covered by data export and deletion. Tower guesses
follow the same choice. Storage: each player's word histories (up to 4,096 daily and 512 Tower
words) live in their own `history:<kind>:<server>:<profile>` records, loaded only for the player
being served and written only when they change, so the shared save that holds everyone's live
boards stays small; a malformed shared save now stops the game instead of loading as empty. The
fields from the pre-personal shared game are gone.

The same module also provides a persistent personal Wordle Tower through `!wordle tower` (with
`!tower`/`!wt` aliases). Tower starts every user on Floor 5, uses six guesses per puzzle, and
maps Floors 5–10 to five- through ten-letter lexicons. Four consecutive solves promote the user
to the next floor and clear strikes; three strikes on one floor demote the user one floor, never
below Floor 5. Tower puzzles persist across UTC days while active, but exhausting all six guesses
ends the run and locks Tower until the next UTC day. The current floor, strikes, promotion streak,
highest floor, total solves, longest run, and fastest promotion are retained per stable profile.
Floor 10 is the summit: solving four puzzles there clears the cap and continues with more
ten-letter puzzles rather than inventing Floor 11.

Wordle and Tower also support the channel-scoped `free_play_enabled` setting. A free-play channel
has independent Wordle players, Tower players, stats, and leaderboards. Wordle assigns the next
puzzle immediately after a solve or exhausted word; `free_answer_pool = "full"` uses the complete
six-letter guess list as its answer pool. Tower retains six guesses, three strikes, promotions,
demotions, and the Floor-10 continuation, but an exhausted puzzle starts again immediately instead
of locking the player until the next UTC day. The default daily-room state and scores are unchanged.

Normal Wordle and Tower commands are available only in the configured network-level `game_room`
(default `#games`). Commands in other rooms receive a themed redirect without consuming attempts
or changing state. The existing normal daily state is server-wide, so moving the room from
`#transience` to `#games` preserves active personal puzzles and scores. The channel-scoped
free-play state remains separate and available for a future dedicated room. A normal Wordle win
awards 10 brass to the solver's host-owned economy balance; free-play wins do not award brass.
Daily Tower solves award 5 brass each and floor promotions award 10 more, paid only in the normal
game-room Tower; free-play Tower runs do not award brass.

`gacha.wasm` provides the `#games` brass economy and egg collection. Normal Wordle wins award 10
brass, normal Darts wins award 20 brass, fishing pays for good catches (uncommon 2, rare 5,
legendary 15, a new species record 10, and 5 per level gained, scaled by fishing's
`brass_percent`, default 100), and Tower solves award 5 brass plus 10 per floor
promotion. `!brass`/`!wallet` shows the balance. `!egg` is the module's noun: `!egg` (or `!egg buy`)
spends 50 brass for one egg, and `hatch`, `pull` (buy and hatch at once), `recycle`, `odds`, and
`shelf` are subcommands with top-level shortcuts (`!hatch`, `!pull`, …). About one egg in twelve
(8%) holds a cosmetic (70% common, 25% rare, 5% legendary); otherwise the hatch rolls 85% common,
11% rare, 3.5% legendary, or 0.5% mythic from a fixed 50-item catalogue of intentionally absurd
junk. A cosmetic already owned is exchanged for 20 brass. `!shelf` shows the user's best three
items, `!shelf <user>` inspects another shelf, and `!shelf top` shows the best discoveries across
the room. One hundred common items become 10 brass with `!recycle` (formerly `!trade`, a name now
free for real trading); `!odds` documents the pull table. Every reply has its own theme key.
`!wardrobe` lists owned badges and flourishes (anywhere, including by PM), `!wear <name>` (for
`!wardrobe wear`) puts one on, and `!wardrobe remove badge|flourish` takes it off. Cosmetics live
in the host store: `cosmetic_grant`/`cosmetic_list`/`cosmetic_wear` under the `cosmetics`
capability (grants are idempotent per event, so an interrupted hatch neither double-grants nor
double-refunds) and `cosmetics_worn` under the read-only `cosmetics_read` capability. The worn
badge appears beside the name in `!whoami` and `!achievements` summaries and leaderboards; the
worn flourish follows Wordle solves, Darts wins, and fishing catches. Owned and worn cosmetics are
part of profile exports and are deleted with the profile. Mythic pulls announce in the configured
`announcement_room` (default `#transience`) with a prompt to join `#games`. Economy, collection,
and shelf state are keyed by stable profile IDs; fishing remains server-wide and is not part of
the room migration.

`!brass history` lists the caller's five most recent brass transactions from the host ledger
(each entry is timestamped; the host keeps each person's newest 200, entries from before
timestamps pruned first). `!brass give <nick> <amount>` moves brass to another known profile
(never to yourself), up to `daily_gift_limit` (default 200) a UTC day. `!brass flip <amount>` bets up to `max_bet` (default 100) on a coin that wins 48% of the
time and pays double, and `!brass slots` (shortcut `!slots`) spins three reels of ⚙🗝🔔👑 for
`slots_cost` (default 5): three crowns pay 50×, bells 12×, keys 6×, cogs 4×, two crowns 2×, and a
pair of bells or keys returns the stake, about 94% back overall. Both games stop for the day once a
person's net losses would pass `daily_loss_limit` (default 200) and can be switched off with
`gambling_enabled`. Today's losses and gifts are one `wager:` record per person, covered by export
and deletion. The slots jackpot unlocks the secret "Three Crowns"; ten gifts unlock the optional
"Generous to a Fault".

`hunt.wasm` schedules opt-in animal appearances with channel-only activation; network/global
activation is deliberately unsupported for this spontaneous output. An animal remains active until
caught, hugged, or dismissed by an admin, with a configurable five-hour reminder by default.
Release, reminder, catch, and hug responses are theme-configurable. Claims and leaderboard
ownership are keyed strictly by stable profile UUID; a reused nickname cannot inherit or overwrite
another profile's score, and legacy nick-only rows remain display-only. Hunt scores retain aggregate
totals plus per-animal hunted/hugged counts; scores created before per-animal tracking show their
historical remainder as untracked animals.

Grabs can miss (`miss_percent`, default 20): the animal stays loose for anyone else and the one who
missed waits `miss_lockout_seconds` (default 10) before trying that animal again. Claims report the
time since release ("caught the hedgehog in 4s!") with "A new channel record!" or "(A personal
best.)"; each score keeps its best time and `!hunt fastest` lists the channel's quickest five
without pinging them. `rare_percent` (default 5) of releases come from a separate themeable
`hunt.rare_animals` list (golden hedgehog, axolotl, capybara in a tiny hat…), are announced with a
✨, count three times toward `!hunt top`, and unlock the optional secret "Once in a Blue Moon".
Claim awards carry a per-release dedup id. Miss lockouts are covered by data deletion.

Bare `!hug` remains the animal claim, while `!hug <nick>` starts a separate, scoreless social
incident. Self-hugs and random misses resolve immediately; otherwise the known-profile target has a
configurable short window to use `!reject` and produce a themed counter-move before a themed hug
completion. Pending attempts, scheduler ownership, and cooldowns use stable profile UUIDs; only the
target may reject, each initiator and target may participate in at most one unresolved attempt, and
module output/state are bounded. Social hugs are channel-only, operator-disableable independently
of spontaneous animal releases, included in profile lifecycle export/deletion, and never award
animal-hug achievements.

`links.wasm` shows page titles. Where its `enabled` setting is on (off by default, per channel),
links in channel lines get "↳ Title — Site" (the site is dropped when the title already names it),
at most `max_per_message` (default two) per line, and the same link isn't titled again in that
channel for `repeat_seconds` (default 30 minutes). `ignore_domains` (default youtube.com,
youtu.be, which the youtube module covers with richer details) skips a domain and its subdomains.
`!link <url>` looks one up on request anywhere. Failed passive lookups stay silent. Fetching is
the host's `link_title`.

`dice.wasm` rolls dice and makes choices, anywhere including by PM, and stores nothing. `!roll`
(alias `!dice`) takes dice notation: a d6 by default, `d20`, `2d6+3`, `4d6k3` (keep the highest
three; `kl` keeps the lowest, dropped dice shown in parentheses), `d%`, and sums of up to ten terms
and 100 dice of up to 1,000 sides; words after the expression are a label (`!roll d20+5 stealth`).
Rolls of more than 20 dice show only the total. `!coin` (for `!roll coin`) flips a coin.
`!choose a | b | c` (or commas, or "a or b") picks one of up to 20 options, and `!8ball <question>`
(for `!choose 8ball`) answers from `dice.8ball`, a themeable list. Randomness is host
`random_bytes`, drawn without modulo bias. Achievements count rolls, natural twenties (kept dice
only), choices, and eight-ball questions.

`stats.wasm` keeps channel stats where its `enabled` setting is on (off by default, per channel),
in the channel's `timezone` (default America/New_York). Every line and `/me` action (the host
delivers actions, flagged `is_action`, only to modules with `action_events`, and never treats one
as a command) is counted per person and channel: lines, words, questions, exclamations, shouted
lines, links, actions, an hour-of-day profile, 35 days of daily counts, streaks, and this and last
week's counts for later weekly awards; each channel keeps an hour-by-weekday heatmap and a year of
daily totals. Nothing anyone said is stored. Counts gather in memory and are written about once a
minute or every 50 lines (a scheduled flush catches quiet channels; a restart loses at most that
minute), and commands flush first. `!stats` gives the channel's day (lines, people, busiest hour,
top three, counting since), `!stats top [today|week|month|all]` (shortcut `!top`; `!stats week`
works too) the top ten with names that don't ping, `!stats me` / `!stats <nick>` lines, rank,
share, words a line, liveliest hour, and streaks, and `!stats hours` a 24-hour sparkline.
`!stats private` stops counting the caller on that network and wipes their figures; `!stats
public` resumes. Data export and deletion cover every figure and the opt-out. Achievements:
Chatterbox (1,000 lines), Pillar of the Community (10,000), A Regular (a seven-day streak), and
optional Night Owl and Early Bird (100 lines between midnight and five, or five and nine), with
an idempotent backfill of lines and streaks.

`!stats awards [last]` gives the week's superlatives (this week so far, or last week): Chatterbox
(most lines), The Inquisitor (questions), Most Excitable (exclamations), Caps Lock Champion
(shouted lines), Link Librarian (links), Night Owl (lines between midnight and five), Most
Theatrical (`/me` actions), and Wordsmith (most words a line, from ten lines up); ties go
alphabetically and an award nobody earned is left out. The `!stats` overview adds "a record day!"
when today beats every earlier day (given a fortnight of history). Where `digest` is also on (off
by default, per channel), stats books a durable job for Monday 09:00 in the channel's timezone and
posts last week: lines against the week before, the busiest day, the top three, any record day,
and new faces; then the week's awards; then "Remember this?" with a random entry from history's
quote book, fetched through `run_commands` (left out when the book is empty). The week posted is
recorded, so a redelivered timer never posts twice, and switching the digest off stops the
booking. Admins can preview the week so far with `!stats digest`.

Commands run through `run_commands` carry a `jeeves/run-by` message tag naming the calling module,
so the target can answer differently (history stays silent on an empty quote book rather than
reply "no quotes yet" into a digest). Reading quotes (`!quote`, `!quote #id`) no longer needs the
caller's profile; adding and deleting still do.

`trivia.wasm` runs trivia rounds in any channel where its `enabled` setting is on (on by
default; operators switch channels off). `!trivia [n]` (alias `!quiz`) starts a round of
`round_length` questions (default ten, 3–25). Players just type answers: matching ignores case,
accents, punctuation, and a leading "the/a/an", keeps "26.2" and "3,600" whole, and forgives one
typo in answers of five or more letters (two from nine), never in numbers. Multiple-choice
questions take the letter or the option's text, and true/false takes true/false/yes/no; a wrong
pick on either locks that player out of the question. A hint comes at `hint_seconds` (default 15:
first letters for typed answers, two wrong options ruled out for choices) and the answer is
revealed at `question_seconds` (default 30). The first right answer scores 10 points before the
hint and 5 after, plus 2 per answer already in a streak (up to 6), and pays `brass_per_answer`
(default 3). The round's top scorer (all of them, on a tie) gets `round_bonus` (default 15). Three
unanswered questions in a row end a round; `!trivia stop` ends it early for whoever started it or
an admin. `!trivia top [week|all]` and `!trivia me` show per-channel scores (UTC weeks). The round
lives in KV and the scheduler drives every step, so a reload mid-round carries on; stale timers are
recognised and ignored. Questions come from a bundled pack of 330 original questions in eleven
categories and, where `opentdb` is on, Open Trivia DB questions fetched through the host's
`trivia_fetch` (only opentdb.com, with a session token so questions don't repeat and requests at
least five seconds apart); fetched questions are credited "(opentdb.com)" per its CC BY-SA 4.0
licence, and rounds fall back to the pack whenever it doesn't answer. The last 250 questions per
channel aren't repeated. Achievements: Quick Study, Well Read, Walking Encyclopaedia (1, 100,
1,000 answers), Quizmaster and Grand Quizmaster (1 and 25 rounds won), optional On a Roll (five in
a row), and the optional secret Clean Sweep; data hooks cover careers and any round in progress.

`birthdays.wasm` greets people who have saved a birthday with `!birthday`. Where its `enabled`
setting is on (off by default, per channel), a person is wished a happy birthday the first time
they speak on the day, in their saved timezone (UTC otherwise), so nobody is congratulated to an
empty room; 29 February birthdays fall on the 28th in other years. Each person is greeted once per
network per year (a `greeted:` record holding the year, covered by export and deletion), with
`birthday_brass` (default 25, 0 for none) from the house, paid idempotently before the record is
written. Profiles are read once a day per person. The optional "Another Year Wiser" achievement
marks the first greeting. Clearing the birthday stops the greetings.

`reminders.wasm` provides durable reminders in plain words, read in the owner's saved timezone
(UTC, with a note, otherwise): `!remind me to check the oven in 10 minutes`, `in an hour`,
`at 5:30pm next tuesday`, `tomorrow at 9`, `on dec 25 at 8pm`, `at 530` (a bare hour means the
next such time, or the evening for small hours on a named day), and recurring `every day|weekday|
monday at 18:00` (at most `max_recurring`, default three, per person; rescheduled in the owner's
zone after each delivery, so daylight saving is followed). The time and message may come in either
order and `me` is optional. The date/time grammar is shared with the clock module's `when.rs`.
Set in a channel, a reminder is delivered there, or by PM (naming the channel) if the owner is no
longer in it; set by PM, it's delivered by PM. `!reminders` lists them compactly on one line,
`!remind cancel <id>` cancels one, and `!snooze [time]` (for `!remind snooze`) re-arms the last
delivered reminder within an hour of delivery, ten minutes by default. `!remind sally at 10 to eat
cheese` asks sally in the channel, showing the time in her own zone; nothing is scheduled unless
she answers `!remind accept` within the hour (`!remind decline` drops it). Requests are capped at
one per pair and three per recipient, reminders for others are one-off only, and pending requests
are covered by data export and erasure. Jobs survive restart and module reload, overdue jobs fire
once, and all confirmations, errors, listings, and deliveries are themed.

### Admin module

`admin.wasm` (built from `modules-src/admin`) registers bot commands and, on authorized
`PRIVMSG`s, parses commands such as `!reload`, `!refresh`, `!shutdown` and invokes the privileged
host functions. It emits `COMMAND`-category log lines so actions appear in the TUI logs screen.

### Operator module

`operator.wasm` is a separately capability-gated channel moderation module. It never receives raw
IRC output access: it can request only the host-validated channel modes needed for bans, op,
half-op, and voice, plus kick and topic actions. Every command requires the sender's `admin` role
and is rejected in private messages.

### Pirate Isles progression

Pirate Isles commands live under `!isles <command>` (`!isles pay auto`, bare `!isles` shows the
seas); every former top-level command (`!crew`, `!pay`, `!raid`, `!menu`, …) remains a default
shortcut. `!pirate <option>` still answers the private menu, and private messages that aren't
menu answers or commands are ignored rather than answered with a menu hint.

`pirate.wasm` retains captain balances, career history, and specialist choices in its existing
versioned game state. Inactive captains are automatically retired after 90 days by default (the
network setting `retire_after_days` accepts 0 to disable); retirement uses the reversible park path,
preserves their state, and `!unpark` restores them. Legacy records without activity timestamps get
a full grace interval. Retired captains free an active sign-on slot. The active roster is capped at
32 and persisted captain history at 128; returning captains may temporarily exceed the active cap.

Captains can recruit one career-earned specialist with channel-only `!specialist recruit <raid|defense|rum>`:
5 career player-raid wins unlock Raid Leader (+10% attack power in player raids), 5 successful
career defenses unlock Defense Specialist (+10% defense power in player raids), and 30 career rum
collected unlock Strategic Alcoholic (a 50% chance to add Rum Runners when a fresh distinct PM
offer set would otherwise omit it). Losses do not advance the unlocks. The first recruitment is
free; after that, one role switch is allowed per season. The active role persists through season
resets while the switch allowance resets. Existing career totals unlock roles without migration.

Aggression and upkeep carry costs. Launching a raid or committing a player blockade ends the
captain's new-player shield. A player blockade earns `notoriety_player_blockade`; if the target
breaks it, each regular crew member sent is lost for good at `blockade_broken_loss_pct` and the rest
straggle home after `blockade_straggler_hours` (loyal crew return at once). Sorties against a Royal
Navy blockade roll power like a raid (`ships × 10 × 0.8–1.2` per side), report a vague fleet
sighting on failure, and impose `navy_assault_cooldown_hours` before the next attempt. The Navy
sights the most notorious captain, drawing at random among ties. `!pay auto` / `!rum auto` hire a
purser who pays wages at rollover for `autopay_fee_pct` extra and, when gold exceeds
`autopay_skim_threshold`, may skim 1..=`autopay_skim_max_pct`% of the excess
(`autopay_skim_chance_pct`); `!pay off` dismisses him. Season Legends, awards, and seasons played
go only to captains active during that season.

## Themes (configurable personality)

All **user-facing** text the bot posts is configurable via a human-editable `theme.toml`
(CLI `--theme`, default `theme.toml`), so Jeeves' phrasing can be changed without code. One
`[section]` per module (the section is the module's name). A module never hardcodes a posted
string — it calls the `theme(key, default, vars)` host function, which:

- writes `default` to the file on first use (lazy registration; `toml_edit` preserves existing
  edits/comments),
- reads the current value — a string, or a **list** of which one is chosen at random,
- substitutes `{var}` placeholders (e.g. `{user}`) in a single pass over the template, so
  substituted values (nicks, user text) are never rescanned for further placeholders,
- returns the rendered line.

Edits to `theme.toml` apply live (the file is reloaded when its mtime changes). The personality is
**global** across networks. Internal/debug text is intentionally not themable.

Improved default copy reaches existing deployments: a bot-owned sidecar (`theme.seeded.toml`
beside the theme file) records the value the bot last wrote for each key. When a module's default
changes, a key whose value still equals that record — never edited by the operator — is upgraded
in place; edited keys are never touched. Keys seeded before the sidecar existed are adopted once
their value matches the current default, and a pre-sidecar key whose whole value is one bare
placeholder (a legacy pass-through like `"{text}"`) is upgraded when its module now supplies a real
sentence. Modules that retired such pass-throughs still supply the old variable (`{text}`,
`{summary}`, `{detail}`), filled with the default sentence, so an operator's wrapper around it
keeps working. A key is upgraded at most once per run, so a module that (wrongly) passes
conflicting defaults for one key cannot rewrite the file repeatedly.

Pass-through keys remain only where the module did not compose the text: AI answers, admin-defined
trigger responses, tarot readings, darts boards, and fishing's already-themed tips.

Every module reply also receives a readable `[Module]` label. Color-capable IRC clients render the
label in that module's configurable mIRC color; clients without color support see the same plain
label. The TUI Settings screen exposes the `irc_color` setting for every loaded module, with
global, network, and channel overrides; selecting `none` suppresses the label.

```toml
[admin]
denied = "I'm afraid I can't allow that, {user}."
pong   = ["Pong.", "At your service, {user}.", "Indeed."]
```

## Discord / admin HTTP API

An optional localhost HTTP admin API (enabled with a non-empty `--admin-token`, or
`RUSTJEEVES_ADMIN_TOKEN` — a blank token never enables it, a short one or a non-loopback bind logs
an error; bind via `--admin-bind`, default `127.0.0.1:9110`; each request runs on its own bounded
thread so slow commands never stall `/health`) lets an external Discord router
(`ircbot_core/discord_admin.py`) drive the bot. It implements that router's contract:

- `GET /health` (unauthenticated) → component JSON with `ok`, connected/configured network
  counts and names, and loaded module count and names. Returns `200` when every configured network
  is connected and at least one module is loaded, otherwise `503`, so a standard Uptime Kuma HTTP
  monitor alerts on degraded startup/runtime state.
- `POST /v1/command` (Bearer auth) — body `{"command","args"}` → `{"messages":[...]}`
- `GET /v1/events?since=N` (Bearer auth) → `{"events":[{"id","message"}]}` — surfaces ERROR-level
  and COMMAND-category log events (disconnects, admin actions) for the router to post to Discord

Commands: `help`, `status`, `modules`, `reload`/`refresh`/`shutdown`, and
`say`/`join`/`part <server> <target/#chan> …` (the `<server>` may be omitted when only one network
is connected). Add the bot to the router's `bots:` list with its `url` + `token_env`.

The authenticated admin bridge also supports `ignore <server> <nick>` and
`unignore <server> <nick>`. These target the user's stable profile on that network. While ignored,
the permission resolver drops the user's inbound messages before module dispatch, and scheduled
items owned by that profile remain stored but are suspended until the profile is unignored.

## Public achievement gallery

An optional, separate read-only HTTP listener is enabled with `--public-bind ADDR` or
`RUSTJEEVES_PUBLIC_BIND`. It is disabled by default and should bind to localhost (for example,
`127.0.0.1:9120`) behind a reverse proxy or Cloudflare Tunnel. It never shares the authenticated
admin listener.

The same origin serves a progressively enhanced responsive HTML gallery and versioned JSON
endpoints for the live non-secret catalog, explicitly published achievement holders, and one
sanitized collection. Profiles are default-private and use `!achievements publish` or
`!achievements hide`; achievement opt-out always removes public visibility. Only profiles with at
least one current finite unlock are listed. Duplicate nicks remain distinguishable by network.

Undiscovered secret achievements are omitted from every public payload and HTML response. Earned
secrets expose only their name and secret/optional markers—not their description, stat, threshold,
or unlock condition. The service reads SQLite only through the DB actor and reads catalogs from the
live manifest registry. It exposes no mutation route, account, hostmask, alias, profile detail,
activity history, or module KV beyond the channel stats snapshots the stats module chooses to
publish (below). Responses are size-bounded and rate-limited with method allowlists,
ETags, conservative caching, escaping, and restrictive security headers. `/health` reports the
listener and `/ready` reports whether manifests have loaded.

### Channel stats pages

`/stats` lists the channels whose stats are public, and `/stats?server=…&channel=…` shows one:
lines this week and all time, the record day, a weekday-by-hour heatmap in the channel's
timezone, a 90-day bar chart (inline SVG, no scripts), top-ten boards for this week, the last 30
days, and all time, and this week's awards. `/v1/stats` and `/v1/stats/channel` serve the same as
versioned JSON. The gallery's collection page (and `/v1/collection`) adds a **Talk** panel for a
holder with figures in public channels: lines, rank, and streaks per channel.

Everything comes from snapshots the stats module writes (`PublicChannelStats` in `jeeves-abi`,
under `public:` in its KV) at most every ten minutes, and only where its `public_page` setting is
on (off by default, per channel); switching it off removes the snapshot, and the site also checks
`stats.enabled` and `stats.public_page` live before serving a page, so it disappears at once.
People are named only if they have published their achievements (the gallery's own opt-in);
everyone else still counts and appears as "someone". `!stats private` and data erasure remove a
person from published snapshots as well. Snapshots keep profile IDs so erasure and Talk panels
can find people; no HTML or JSON response contains them.

## Architecture

tokio runtime with long-lived tasks wired by channels:

- **IRC actor** owns the `irc::Client`: streams server messages into `Event`s (→ log bus + module
  dispatch) and executes `Action`s (send/join/part/quit) received over an mpsc channel.
- **DB actor** owns the single rusqlite connection and serves requests over a channel.
- **Scheduler actor** restores persisted jobs, waits for due times, and targets timer events to the
  owning loaded module without polling ordinary chat activity.
- **Module host** loads `modules/*.wasm`, dispatches events to guest hooks, and wires host
  functions back to the Action channel and DB actor.
- **Public gallery** serves sanitized snapshots from the DB actor and live achievement registry on
  its own explicitly enabled localhost listener.
- **Log bus** is a broadcast of `LogEvent { ts, level, category, source, message }`; the TUI and a
  stdout/DB sink subscribe.

## Deferred / future work

- Deeper IRCv3 specs (see IRCv3 scope above).
- Hot-reload of an individual changed `.wasm` without a full folder rescan.
- Negotiated IRC casemapping and deeper IRCv3 coverage.
- Signed/trusted module distribution beyond the local capability policy.
- A constrained general-purpose outbound HTTP host capability.
