# CLAUDE.md

See **[AGENTS.md](./AGENTS.md)** — it is the source of truth for repo layout, build/run commands,
architecture, and conventions. Read **SPEC.md** for the spec and **PLAN.md** for live status.

## Claude-specific notes

- Keep `SPEC.md` and `PLAN.md` in sync with reality as you work — they are the live tracking docs
  the user relies on.
- Only the DB actor touches rusqlite; only the IRC actor touches `irc::Client`. Route everything
  else through channels.
- `jeeves-abi` is the single source of truth for the host/guest WASM ABI.
