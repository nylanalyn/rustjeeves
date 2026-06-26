# AGENTS.md — working agreement for rustjeeves

This file orients any agent (or human) working in this repo. Read `SPEC.md` for what we're
building and `PLAN.md` for the live milestone status.

## What this is

`rustjeeves` (binary `jeeves`) is an IRCv3 bot framework in Rust. WASM plugins (extism) dropped in
`modules/` are auto-loaded. Config + per-module state live in SQLite. Two run modes: interactive
(ratatui TUI) and headless.

## Repo layout

```
Cargo.toml            # cargo workspace
SPEC.md PLAN.md       # spec + live plan (keep current)
AGENTS.md CLAUDE.md   # this file + a pointer
crates/
  jeeves/             # main bot binary
    src/
      main.rs         # CLI (--interactive / --headless), bootstrap
      config.rs       # load/save config from SQLite
      db.rs           # rusqlite actor + migrations
      irc/            # irc-crate client actor (CAP/SASL/actions)
      log_bus.rs      # broadcast LogEvent (levels + categories)
      modules/        # extism host: load .wasm, dispatch, host fns
      tui/            # ratatui: settings + logs screens
  jeeves-abi/         # shared serde types for host <-> guest
modules-src/
  admin/              # extism PDK plugin -> admin.wasm
modules/              # RUNTIME: built .wasm files dropped here (auto-loaded)
```

## Build & run

```bash
cargo build --workspace            # build bot + abi
cargo run -p jeeves -- --headless  # run headless
cargo run -p jeeves -- --interactive

# build a module to wasm and install it
# (each module under modules-src/ is its own standalone cargo workspace)
cd modules-src/admin
cargo build --release --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/admin.wasm ../../modules/
```

(One-time: `rustup target add wasm32-unknown-unknown`.)

## Architecture in one paragraph

tokio tasks wired by channels. The **IRC actor** owns the `irc::Client` (emits `Event`s, runs
`Action`s). The **DB actor** owns the single rusqlite connection. The **module host** loads
`modules/*.wasm`, dispatches events to guest hooks (`init`/`on_message`/`on_event`), and exposes
**host functions** (send/join/kv/log + privileged reload/shutdown) that route back to the Action
channel and DB actor. The **log bus** broadcasts `LogEvent`s to the TUI and a stdout/DB sink.

## How to add a module

1. New crate under `modules-src/<name>` with `crate-type = ["cdylib"]`, depending on
   `extism-pdk` and `jeeves-abi`.
2. Implement any of `init` / `on_message` / `on_event` with `#[plugin_fn]`; call host functions
   for side effects.
3. Build to `wasm32-unknown-unknown` and drop the `.wasm` into `modules/`. It auto-loads on next
   start (or via `!reload`).

## Conventions

- `anyhow::Result` for app errors; `?` over `.unwrap()` outside tests/bootstrap.
- Host/guest payloads are JSON via `jeeves-abi` serde types — keep that crate the single source of
  truth for the ABI.
- rusqlite is only touched by the DB actor; everything else talks to it over a channel.
- The `irc::Client` is only touched by the IRC actor; everything else submits `Action`s.
- Keep `SPEC.md` and `PLAN.md` current as scope/status changes — they are the live record.

## Status

See `PLAN.md`.
