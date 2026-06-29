# rustjeeves

`rustjeeves` (`jeeves`) is an IRCv3 bot framework written in Rust. It connects to multiple IRC
networks, runs in a ratatui TUI or headless mode, and loads Extism WASM modules from `modules/`.

## Current feature checklist

- [x] TLS, CAP negotiation, SASL PLAIN, NickServ fallback, channel auto-join
- [x] Multiple simultaneous networks with automatic reconnect and exponential backoff
- [x] Interactive server/admin/log management, API credentials, command aliases, and scoped module
      settings
- [x] SQLite configuration, stable UUID user profiles, nick/account aliases, and retained logs
- [x] Hot-reloaded WASM modules with per-module capabilities, worker isolation, and time limits
- [x] Live `theme.toml` customization for every bundled module, including fishing
- [x] Admin, users, weather, local time, fishing, Tavily search, DeepL translation, channel
      history/quotes/sed corrections, channel-local memos, and durable reminders modules
- [x] Host-owned durable scheduler with restart recovery and targeted module timer events
- [x] Token-protected localhost HTTP admin bridge

## Build and run

```bash
cargo build --workspace
./build-modules.sh
cargo run -p jeeves -- --headless
# Interactive mode is the default:
cargo run -p jeeves -- --interactive
```

In interactive mode, enter Tavily and DeepL keys under **Integrations (F3)** and save with
`Ctrl-S`. Keys are masked in the TUI and stored in `bot.db` (like the IRC passwords; SQLite is not
encrypted). Settings apply immediately and take precedence over environment variables. For
headless deployments, the modules use these fallbacks:

```bash
RUSTJEEVES_TAVILY_API_KEY="..." \
RUSTJEEVES_DEEPL_API_KEY="..." \
  cargo run -p jeeves -- --headless
```

`TAVILY_API_KEY` and `DEEPL_AUTH_KEY` are also accepted as provider-standard aliases.

Open **Commands (F4)** to view commands advertised by loaded modules. Select a command and press
Enter to edit its comma-separated aliases without the leading `!`; save with `Ctrl-S`. An empty
saved list disables all aliases for that command, while `r` restores the module defaults. Alias
changes are persisted in SQLite and apply immediately.

Open **Modules (F5)** to configure settings advertised by loaded modules. Overrides can be global,
per network, or per channel; precedence is channel → network → global → module default. Every
module has a standard `enabled` switch. Save with `Ctrl-S`, or remove the selected override with
`Ctrl-D`. Changes apply immediately.

Runtime files default to `bot.db`, `modules/`, `theme.toml`, and
`module-capabilities.toml`. See `AGENTS.md` for the full development guide, `SPEC.md` for behavior,
`PLAN.md` for milestone history, and `MODULES_TODO.md` for the future module design backlog.

To write the host-owned portion of a user's profile to a private JSON file and exit:

```bash
cargo run -p jeeves -- --db bot.db --export-profile libera:Alice --export-dir data-exports
```

The offline export contains the shared profile, nick aliases, services-account bindings, and
scheduler jobs explicitly owned by its stable UUID. Runtime PM exports additionally include
module-private data through explicit lifecycle hooks; the host never guesses from opaque values.

Users can privately run `!mydata summary`, `!mydata export`, or `!mydata delete`. Deletion requires
the PM confirmation token and is journaled so unavailable modules or malformed state cannot produce
a false success. Super-admins have equivalent PM-only controls with `!data <nick> <action>` and
`!data confirm <token>`. Runtime exports use `--export-dir` (default `data-exports`).
Super-admins can inspect resumable deletion work with `!data pending`; the listing exposes workflow
IDs and status, not profile identifiers.

## Module security

Host access is controlled by the operator-owned `module-capabilities.toml`. Unknown modules receive
only `log`, `theme`, `now`, and their own setting reads; privileged bot controls should remain
restricted to trusted modules.
Each plugin runs on its own bounded worker with a 20-second execution deadline, so a slow plugin
does not stop unrelated modules.

## Possible next additions

- See [`MODULES_TODO.md`](MODULES_TODO.md) for the current lifecycle, backup, and module backlog.
- [ ] Moderation actions and richer channel membership events
- [ ] Safe outbound HTTP capability for RSS, release notifications, and URL titles
- [ ] Factoids, polls, trivia, and channel statistics modules
- [ ] IRC casemapping negotiated from `005 CASEMAPPING`
