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
- **Commands screen (F4)** — loaded commands and editable aliases.
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
```

The bot connects to **all `enabled` server profiles simultaneously** (one IRC actor per network).
Events are tagged with the originating server `label`; module host functions take a `server` label
to target a specific network. Each actor is supervised and reconnects with exponential backoff.

Profiles receive a stable per-network UUID. Nicknames and services accounts are aliases of that
UUID, so `NICK` changes preserve profile and module identity. Existing nick-keyed rows are migrated
in place on startup.

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

## AI responder

AI chat is an optional, stateless WASM module backed by the narrow host-owned `ai_chat` capability.
The host alone reads provider credentials, the configured OpenAI-compatible endpoint/model, and a
size-bounded `SOUL.md`; the module has no general HTTP or filesystem access. Channel responses are
off by default and require explicit `<bot nick or alias>,` or `<name>:` addressing. Private-message
behavior, aliases, stable-profile cooldown, temperature, and output limit are operator settings.
Requests and responses are bounded and sanitized, only one provider call runs at a time, and no
conversation history or tools are available.

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
metadata announcements. The standard scoped `enabled` setting suppresses ambient events but does
not suppress a command explicitly routed to that module, allowing passive announcements to remain
off by default while manual search stays available. Provider responses, module output, cooldowns,
and per-channel seen-video state are bounded; personal cooldown state participates in lifecycle
export and deletion.

## Channel banter

`banter.wasm` provides two opt-in channel rituals without commands or personal state. A whole-word
`sail` triggers only for the scoped `sailor_nick`; whole-word `caw` and `kaw` trigger for any user.
Matching is case-insensitive and punctuation-tolerant but never substring-based. Sailing takes
precedence if one message contains both trigger classes, so output remains bounded to one reply.
The module ignores PMs and the bot's own nick, stores only per-channel cooldown timestamps, and
offers independent scoped cooldown settings plus theme-editable response pools.

## Permissions (per network)

Each network has an `admins` list of `(nick, role)` where `role` is `admin` or `super-admin`
(super-admin implies admin). The **host** resolves the sender's role for every message and stamps it
onto the event; modules enforce by checking `msg.role` (the bundled admin module gates `!shutdown`
to super-admin and `!reload`/`!refresh` to admin).

Identity is verified by, in order: an operator-pinned services account (matched against the IRCv3
`account-tag`); else a previously-bound account; else a previously-bound `nick!user@host` hostmask;
else — on first contact — the strongest identity available is bound ("introduction" /
trust-on-first-use), preferring the services account over the hostmask. The bot negotiates the
`account-tag` capability so verified accounts are available.

`module_kv` is the namespaced store modules persist into via the `kv_get`/`kv_set` host functions
— this is how modules "add their own info to the database".

## Module system (WASM via extism)

Any `*.wasm` file in the `modules/` directory (relative to the bot's working directory) is loaded
automatically at startup. Modules are sandboxed WASM plugins run via the **extism** host SDK; they
may be written in any language with an extism PDK (Rust is used for the bundled `admin` module).
Each module has a bounded worker thread and a 20-second guest execution deadline. Host functions
enforce the operator-owned policy in `module-capabilities.toml`; unknown modules receive only
`log`, `theme`, `now`, and namespaced setting reads.

### Guest exports (a module implements any subset)

- `init` — called once at load; the module may register metadata/commands.
- `commands` — optional versioned command metadata used by the host alias registry and TUI.
- `settings` — optional versioned typed setting metadata used by the host and TUI.
- `on_message` — channel/PM `PRIVMSG` events (JSON payload).
- `on_event` — connection/join/part/numeric events (JSON payload).

### Host functions — the "base" capability API (available to all modules)

There is no separate `base.wasm`; the common operations are the host-function surface:

- `send_message(server, target, text)`, `send_notice(server, target, text)`
- `join(server, channel)`, `part(server, channel)`
- `kv_get(key) -> value`, `kv_set(key, value)` (namespaced by the calling module's name)
- `setting_get(key, server?, channel?) -> value` — the calling module's validated effective value;
  precedence is channel → network → global → advertised default
- `schedule_set(job)`, `schedule_cancel(id)`, `schedule_list(server?, channel?)` — namespaced,
  quota-limited durable jobs delivered back to the owning module as targeted timer events
- `log(level, category, message)`
- `now() -> unix_seconds` — current time (WASM modules have no system clock)
- `theme(key, default, vars) -> string` — fetch a user-configurable string (see Themes)
- `profile_ensure(server, nick)`, `profile_get(server, nick) -> Profile`,
  `profile_set(ProfileUpdate)`, `profile_clear(server, nick, field)` — shared, host-level user
  profiles any module can read/write
- `geocode(query) -> GeoResult` — keyless Open-Meteo geocoding (lat/lon + canonical label)
- `local_time(timezone, unix_seconds?) -> LocalTimeResult` — IANA timezone conversion using the
  host's timezone database, including daylight-saving transitions
- `weather(lat, lon) -> WeatherResult` — keyless Open-Meteo current conditions
- `web_search(query) -> SearchResponse` — Tavily ranked web results; the API key remains in the
  host process and is read from the global SQLite setting, then
  `RUSTJEEVES_TAVILY_API_KEY`/`TAVILY_API_KEY` as fallback
- `translate(text, target_lang, source_lang?) -> TranslateResponse` — DeepL text translation;
  Free (`:fx`) and standard keys select the correct endpoint automatically, and the key remains in
  the host process
- **privileged:** `bot_reload()`, `bot_refresh()`, `bot_shutdown()`

Events are delivered as an `EventEnvelope { server, event }`; message events carry the sender's
resolved `role` (see Permissions) plus `nick`, `user`, `host`, `target`, `text`, and IRCv3 tags.

Payloads cross the host/guest boundary as JSON (serde types defined in the `jeeves-abi` crate).

Every loaded module receives a standard boolean `enabled` setting at global, network, and channel
scope unless it advertises its own boolean definition. The host checks this before dispatch, so an
override disables the module without reload. Spontaneous modules should advertise a default of
`false`. Overrides are retained in SQLite while a module is absent. Ordinary settings cannot hold
secrets; credentials remain in the masked integrations system.

### Commands and aliases

Modules advertise canonical commands, descriptions, usage, and default aliases through the
optional `commands` export. Operator overrides are stored globally in SQLite and edited under TUI
**Commands (F4)**. Names omit the leading `!`, match case-insensitively, and may contain only ASCII
letters, digits, `-`, or `_`. The registry rejects collisions with canonical commands or aliases.

When an alias is used, only the owning module receives a copy with its first token rewritten to the
canonical command. Other modules receive the untouched IRC message so history and quotes preserve
what the user actually typed. Overrides remain stored while a module is absent and become active
again if it is reinstalled.

### Utility modules

`search.wasm` provides `!g`, `!google`, and `!search`. It returns the first ranked Tavily result,
enforces a per-user cooldown, and falls back to a normal search URL when Tavily is unconfigured or
unavailable. The plugin receives neither unrestricted HTTP access nor the API key.

The interactive TUI exposes global API credentials under **Integrations (F3)**. Secret fields are
masked while editing and stored in SQLite's `config` table; the database itself is not encrypted.
Saving or clearing a Tavily or DeepL key takes effect on the next request without a reconnect or
module reload.

`history.wasm` provides channel-local `!seen <nick>` and quotes. `!quote <nick>` saves that user's
latest non-command line, `!quote "text"` saves a self-attributed quote, `!quote` selects a random
quote, and `!quote #N` retrieves one. Private messages are never recorded or exposed. Quote
deletion is limited to the quoted person, submitter, or an admin. It also supports sed-style
corrections of the speaker's own latest line: `s/pattern/replacement/`, with optional `g` and `i`
flags, escaped slashes, regex capture replacements, bounded output, and chained corrections.

`memos.wasm` provides channel-local `!tell <nick> <message>`. The memo is delivered when that user
next speaks in the same channel, using stable profile identity where available so nick changes do
not lose messages. `!memos` reports a user's waiting count without exposing the text, and
`!memos clear` discards their waiting messages. Memos expire after 30 days by default; retention is
configurable globally, per network, or per channel. Private-message commands cannot create or
reveal channel memos. Super-admin memo inspection and clearing are initiated in the relevant
channel, return their results privately to the invoking admin, and emit content-free audit logs.

`translate.wasm` provides `!tr` and `!translate`. `!tr fr Hello` auto-detects the source language;
`!tr de:en Guten Morgen` supplies it explicitly. It limits input and per-user request rate, maps
common language names to DeepL codes, themes every wrapper/error, and never receives the API key.

`clock.wasm` provides `!time`, with `!clock` as a default alias. With no argument it uses the
caller's saved profile location; a nickname uses that user's saved location; any other argument is
geocoded as a place. Saved IANA timezones are converted host-side with current daylight-saving
rules, and responses do not disclose a user's exact saved location.

`darts.wasm` provides the original asynchronous 301 race: `!darts [1|2|3]` spends up to three
darts in a player's turn, the third starts a configurable rest, and another player's throw releases
resting players. Darts are resolved sequentially against a weighted board, exact zero clears the
match, and active players plus lifetime results use stable profile IDs.

`wordle.wasm` provides a daily collaborative six-letter puzzle through `!word` (`!wordle` alias).
Each network shares discoveries across its channels, each stable user receives a configurable
number of attempts per UTC day, and an unsolved word carries forward. `stats`, `top`, and admin
`new` subcommands reproduce the original module's longer-running household game.

`hunt.wasm` schedules opt-in animal appearances per channel. Claims and leaderboard ownership are
keyed strictly by stable profile UUID; a reused nickname cannot inherit or overwrite another
profile's score, and legacy nick-only rows remain display-only.

`roadtrip.wasm` stores passenger membership strictly by stable profile UUID. `!roadtrip` starts a
trip only when none is active and otherwise remains silent; `!me` joins an open signup. Missing
identities cannot join or initiate trips, legacy nick-only passengers remain display-only, and party
state plus rendered passenger lists are bounded.

`reminders.wasm` provides durable channel-local self-reminders. `!remind me in 10 minutes to check
the oven` persists a timer, `!reminders` lists the caller's pending reminders in that channel, and
`!remind cancel <id>` cancels one. Jobs survive restart and module reload, overdue jobs fire once,
and all confirmations, errors, listings, and deliveries are themed. Reminders targeting another
user are deliberately deferred until recipient consent/opt-out behavior is designed.

### Admin module

`admin.wasm` (built from `modules-src/admin`) registers bot commands and, on authorized
`PRIVMSG`s, parses commands such as `!reload`, `!refresh`, `!shutdown` and invokes the privileged
host functions. It emits `COMMAND`-category log lines so actions appear in the TUI logs screen.

## Themes (configurable personality)

All **user-facing** text the bot posts is configurable via a human-editable `theme.toml`
(CLI `--theme`, default `theme.toml`), so Jeeves' phrasing can be changed without code. One
`[section]` per module (the section is the module's name). A module never hardcodes a posted
string — it calls the `theme(key, default, vars)` host function, which:

- writes `default` to the file on first use (lazy registration; `toml_edit` preserves existing
  edits/comments),
- reads the current value — a string, or a **list** of which one is chosen at random,
- substitutes `{var}` placeholders (e.g. `{user}`),
- returns the rendered line.

Edits to `theme.toml` apply live (the file is reloaded when its mtime changes). The personality is
**global** across networks. Internal/debug text is intentionally not themable.

```toml
[admin]
denied = "I'm afraid I can't allow that, {user}."
pong   = ["Pong.", "At your service, {user}.", "Indeed."]
```

## Discord / admin HTTP API

An optional localhost HTTP admin API (enabled with `--admin-token`, or `RUSTJEEVES_ADMIN_TOKEN`;
bind via `--admin-bind`, default `127.0.0.1:9110`) lets an external Discord router
(`ircbot_core/discord_admin.py`) drive the bot. It implements that router's contract:

- `GET /health` → `{"ok":true}` (unauthenticated)
- `POST /v1/command` (Bearer auth) — body `{"command","args"}` → `{"messages":[...]}`
- `GET /v1/events?since=N` (Bearer auth) → `{"events":[{"id","message"}]}` — surfaces ERROR-level
  and COMMAND-category log events (disconnects, admin actions) for the router to post to Discord

Commands: `help`, `status`, `modules`, `reload`/`refresh`/`shutdown`, and
`say`/`join`/`part <server> <target/#chan> …` (the `<server>` may be omitted when only one network
is connected). Add the bot to the router's `bots:` list with its `url` + `token_env`.

## Architecture

tokio runtime with long-lived tasks wired by channels:

- **IRC actor** owns the `irc::Client`: streams server messages into `Event`s (→ log bus + module
  dispatch) and executes `Action`s (send/join/part/quit) received over an mpsc channel.
- **DB actor** owns the single rusqlite connection and serves requests over a channel.
- **Scheduler actor** restores persisted jobs, waits for due times, and targets timer events to the
  owning loaded module without polling ordinary chat activity.
- **Module host** loads `modules/*.wasm`, dispatches events to guest hooks, and wires host
  functions back to the Action channel and DB actor.
- **Log bus** is a broadcast of `LogEvent { ts, level, category, source, message }`; the TUI and a
  stdout/DB sink subscribe.

## Deferred / future work

- Deeper IRCv3 specs (see IRCv3 scope above).
- Hot-reload of an individual changed `.wasm` without a full folder rescan.
- Negotiated IRC casemapping and deeper IRCv3 coverage.
- Signed/trusted module distribution beyond the local capability policy.
- A constrained general-purpose outbound HTTP host capability.
