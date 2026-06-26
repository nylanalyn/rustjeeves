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

Deep IRCv3 spec coverage beyond CAP + SASL + message tags; multi-server runtime; per-`.wasm`
hot-reload; a full module permission model.

## Runtime modes

| Mode | Flag | Behaviour |
|------|------|-----------|
| Interactive | `--interactive` (default) | Launches the ratatui TUI. |
| Headless | `--headless` | No TUI; connects and runs, logging to stdout + DB. |

## IRCv3 scope

Implemented now (via the `irc` crate): connection + optional TLS, `CAP LS/REQ/END` negotiation,
**SASL PLAIN**, and surfacing of message tags on events. NickServ-message authentication is
available as a fallback when SASL is not configured.

Deferred IRCv3 work: `batch`, `labeled-response`, `account-tag`, `away-notify`, `chghost`,
`server-time` semantics, multi-prefix handling, and `echo-message`.

## TUI (interactive mode)

Built with **ratatui** + **crossterm**.

- **Settings screen** — edit the IRC server host/port, TLS on/off, nick/user/realname, SASL
  account/password, and the channel list. A **Save** action writes to SQLite.
- **Logs screen** — scrollable log view, **filterable by category**: `ERROR`, `DEBUG`, `MESSAGE`,
  and `COMMAND`. The `COMMAND` category becomes meaningful once the admin module is loaded.

## Storage (SQLite via `rusqlite`)

A single `bot.db`, accessed through a DB actor task (rusqlite is synchronous; the actor keeps it
off the async tasks). Schema:

```sql
config(key TEXT PRIMARY KEY, value TEXT);
servers(id INTEGER PRIMARY KEY, host TEXT, port INTEGER, tls INTEGER,
        nick TEXT, username TEXT, realname TEXT);
sasl(server_id INTEGER, mechanism TEXT, account TEXT, password TEXT);
channels(server_id INTEGER, name TEXT, key TEXT);
module_kv(module TEXT, key TEXT, value TEXT, PRIMARY KEY(module, key));
logs(id INTEGER PRIMARY KEY, ts INTEGER, level TEXT, category TEXT,
     source TEXT, message TEXT);
```

`module_kv` is the namespaced store modules persist into via the `kv_get`/`kv_set` host functions
— this is how modules "add their own info to the database".

## Module system (WASM via extism)

Any `*.wasm` file in the `modules/` directory (relative to the bot's working directory) is loaded
automatically at startup. Modules are sandboxed WASM plugins run via the **extism** host SDK; they
may be written in any language with an extism PDK (Rust is used for the bundled `admin` module).

### Guest exports (a module implements any subset)

- `init` — called once at load; the module may register metadata/commands.
- `on_message` — channel/PM `PRIVMSG` events (JSON payload).
- `on_event` — connection/join/part/numeric events (JSON payload).

### Host functions — the "base" capability API (available to all modules)

There is no separate `base.wasm`; the common operations are the host-function surface:

- `send_message(target, text)`, `send_notice(target, text)`
- `join(channel)`, `part(channel)`
- `kv_get(key) -> value`, `kv_set(key, value)` (namespaced by the calling module's name)
- `log(level, category, message)`
- **privileged:** `bot_reload()`, `bot_refresh()`, `bot_shutdown()`

Payloads cross the host/guest boundary as JSON (serde types defined in the `jeeves-abi` crate).

### Admin module

`admin.wasm` (built from `modules-src/admin`) registers bot commands and, on authorized
`PRIVMSG`s, parses commands such as `!reload`, `!refresh`, `!shutdown` and invokes the privileged
host functions. It emits `COMMAND`-category log lines so actions appear in the TUI logs screen.

## Architecture

tokio runtime with long-lived tasks wired by channels:

- **IRC actor** owns the `irc::Client`: streams server messages into `Event`s (→ log bus + module
  dispatch) and executes `Action`s (send/join/part/quit) received over an mpsc channel.
- **DB actor** owns the single rusqlite connection and serves requests over a channel.
- **Module host** loads `modules/*.wasm`, dispatches events to guest hooks, and wires host
  functions back to the Action channel and DB actor.
- **Log bus** is a broadcast of `LogEvent { ts, level, category, source, message }`; the TUI and a
  stdout/DB sink subscribe.

## Deferred / future work

- Deeper IRCv3 specs (see IRCv3 scope above).
- Multi-server support (schema allows it; runtime starts single-server).
- Hot-reload of an individual changed `.wasm` without a full folder rescan.
- A module permission model (which modules may call privileged host functions) — initially
  admin-only by config.
