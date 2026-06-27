# rustjeeves

`rustjeeves` (`jeeves`) is an IRCv3 bot framework written in Rust. It connects to multiple IRC
networks, runs in a ratatui TUI or headless mode, and loads Extism WASM modules from `modules/`.

## Current feature checklist

- [x] TLS, CAP negotiation, SASL PLAIN, NickServ fallback, channel auto-join
- [x] Multiple simultaneous networks with automatic reconnect and exponential backoff
- [x] Interactive server/admin/log management and headless operation
- [x] SQLite configuration, stable UUID user profiles, nick/account aliases, and retained logs
- [x] Hot-reloaded WASM modules with per-module capabilities, worker isolation, and time limits
- [x] Live `theme.toml` customization for every bundled module, including fishing
- [x] Admin, users, weather, fishing, Tavily search, DeepL translation, channel history/quotes,
      and channel-local memos modules
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

Runtime files default to `bot.db`, `modules/`, `theme.toml`, and
`module-capabilities.toml`. See `AGENTS.md` for the full development guide, `SPEC.md` for behavior,
`PLAN.md` for milestone history, and `MODULES_TODO.md` for the future module design backlog.

## Module security

Host access is controlled by the operator-owned `module-capabilities.toml`. Unknown modules receive
only `log`, `theme`, and `now`; privileged bot controls should remain restricted to trusted modules.
Each plugin runs on its own bounded worker with a 20-second execution deadline, so a slow plugin
does not stop unrelated modules.

## Possible next additions

- See [`MODULES_TODO.md`](MODULES_TODO.md) for planned memos, reminders, games, sed corrections,
  and local-time modules.
- [ ] Durable reminders and scheduler host functions
- [ ] Moderation actions and richer channel membership events
- [ ] Safe outbound HTTP capability for RSS, release notifications, and URL titles
- [ ] Factoids, polls, trivia, and channel statistics modules
- [ ] Profile privacy controls for birthday and location fields
- [ ] IRC casemapping negotiated from `005 CASEMAPPING`
