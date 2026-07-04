# AGENTS.md — working agreement for rustjeeves

Do not read whole files unless necessary. Use symbols, outlines, ripgrep, git diff, and targeted ranges first.

This file orients any agent (or human) working in this repo. Read `SPEC.md` for what we're
building and `PLAN.md` for the live milestone status.

## What this is

`rustjeeves` (binary `jeeves`) is an IRCv3 bot framework in Rust. WASM plugins (extism) dropped in
`modules/` are auto-loaded. Config + per-module state live in SQLite. Two run modes: interactive
(ratatui TUI) and headless.

## Repo layout

```
Cargo.toml            # cargo workspace
README.md SPEC.md PLAN.md # overview + spec + live plan (keep current)
module-capabilities.toml # operator-owned WASM host capability policy
AGENTS.md CLAUDE.md   # this file + a pointer
crates/
  jeeves/             # main bot binary
    src/
      main.rs         # CLI (--interactive / --headless), bootstrap
      config.rs       # load/save config from SQLite
      db.rs           # rusqlite actor + migrations, including command alias overrides
      data_lifecycle.rs # versioned operator profile export
      irc/            # irc-crate client actor (CAP/SASL/account-tag, per-network)
      adminapi.rs     # localhost HTTP admin API (Discord router bridge: /v1/command, /v1/events)
      publicweb.rs    # optional read-only achievement gallery + sanitized versioned JSON API
      perms.rs        # permission resolver: stamps sender role onto messages
      theme.rs        # themable user-facing strings (theme.toml, {user} placeholders)
      geo.rs          # Open-Meteo geocoding (geocode host function)
      weather.rs      # Open-Meteo current conditions (weather host function)
      log_bus.rs      # broadcast LogEvent (levels + categories)
      commands.rs     # loaded command registry, validation, alias resolution
      settings.rs     # typed module setting registry, validation, scoped override resolution
      scheduler.rs    # host-owned persisted jobs and targeted timer delivery to modules
      ai.rs           # bounded OpenAI-compatible/Ollama chat provider and SOUL.md loader
      modules/        # extism host: load .wasm, command metadata, dispatch, host fns, hot reload
      tui/            # ratatui: servers/admins/logs/integrations/aliases/backups/profile repair
  jeeves-abi/         # shared serde types for host <-> guest
modules-src/
  admin/              # extism PDK plugin -> admin.wasm (bot commands)
  users/              # extism PDK plugin -> users.wasm (profiles: title/birthday/pronouns/location/clear)
  weather/            # extism PDK plugin -> weather.wasm (!weather via saved location or ad-hoc)
  clock/              # extism PDK plugin -> clock.wasm (!time via user profile or location)
  fishing/            # extism PDK plugin -> fishing.wasm (cast/reel mini-game; bundles fish_database.json)
  history/            # extism PDK plugin -> history.wasm (!seen, quotes, and sed corrections)
  ai/                 # addressed, stateless AI responder backed by the host ai_chat capability
  memos/              # extism PDK plugin -> memos.wasm (!tell and channel-local delivery)
  reminders/          # extism PDK plugin -> reminders.wasm (durable self-reminders)
  youtube/            # extism PDK plugin -> youtube.wasm (!yt + opt-in link metadata)
  banter/             # extism PDK plugin -> banter.wasm (sailing/crow channel rituals)
  achievements/       # collection/progress views over the host-owned achievement store
modules/              # RUNTIME: built .wasm files dropped here (auto-loaded)
```

## Build & run

```bash
cargo build --workspace            # build bot + abi
cargo run -p jeeves -- --headless  # run headless
cargo run -p jeeves -- --interactive

# Discord admin API (for ircbot_core's discord_admin.py router). Token-gated, localhost.
cargo run -p jeeves -- --headless \
  --admin-bind 127.0.0.1:9110 --admin-token "$RUSTJEEVES_ADMIN_TOKEN"
# then in discord_admin.yaml under bots::
#   rustjeeves:
#     url: "http://127.0.0.1:9110"
#     token_env: "RUSTJEEVES_ADMIN_TOKEN"

# build all modules to wasm and install them into modules/ (auto-discovers modules-src/*)
./build-modules.sh
./build-modules.sh weather        # or just one (or several) by name
```

`build-modules.sh` installs the wasm target if needed and copies each built `.wasm` into
`modules/`, where the running bot auto-loads it. Each module under `modules-src/` is its own
standalone cargo workspace.

## Architecture in one paragraph

tokio tasks wired by channels. **One IRC actor per enabled network** owns its `irc::Client` (emits
`EventEnvelope`s tagged with the network label, runs `Action`s); a shared **registry**
(`label -> action sender`) lets host functions target a network. Events pass through the
**permission resolver** (`perms.rs`), which stamps the sender's role, before reaching the **module
host**, which loads `modules/*.wasm`, dispatches to guest hooks (`init`/`on_message`/`on_event`),
exposes **host functions** (server-aware send/join/kv/log + privileged reload/shutdown), and
enforces per-module capabilities, isolates plugins on bounded workers, and auto-reloads on directory
changes. The **DB actor** owns the single rusqlite connection. The **log
bus** broadcasts `LogEvent`s to the TUI and a stdout/DB sink.

## How to add a module

1. New crate under `modules-src/<name>` with `crate-type = ["cdylib"]`, standalone `[workspace]`,
   depending on `extism-pdk` and `jeeves-abi`.
2. Follow the **Module contract** below — every point is load-bearing.
3. Add the module to `module-capabilities.toml` with only the capabilities it actually uses.
4. Build with `./build-modules.sh <name>` — installs into `modules/` and auto-loads.

## Module contract

Every module **must** satisfy all of the following. Skipping any point means the module is only
half-integrated — it will silently miss features that operators and users expect.

### 1. Exports

| Export | Required | Notes |
|--------|----------|-------|
| `commands() -> FnResult<String>` | if the module handles any `!commands` | Returns `CommandManifest` JSON |
| `on_message(String) -> FnResult<()>` | if the module reacts to chat | Receives `EventEnvelope` JSON |
| `on_event(String) -> FnResult<()>` | if using scheduler / non-message events | Handles `Event::Timer` etc. |
| `settings() -> FnResult<String>` | if the module has configurable behaviour | Returns `SettingsManifest` JSON |
| `data_export(String) -> FnResult<String>` | if storing personal KV data | Pure, versioned subject export over host-supplied KV entries |
| `data_delete(String) -> FnResult<String>` | if storing personal KV data | Returns an idempotent, host-validated KV mutation plan |
| `achievements() -> FnResult<String>` | every user-facing module except admin | Versioned stats, finite milestones, and prestige metadata |
| `achievement_backfill(String) -> FnResult<String>` | if reliable historical totals exist | Pure, idempotent `set_max` values from host-supplied KV |
| `init() -> FnResult<()>` | optional | Good for startup logging |

**Never** export a function named `event` — the host calls `on_message` and `on_event`. A wrongly
named export compiles and loads without error but is silently ignored.

### 2. Command discovery — required for !help and the TUI alias editor

Every command the module handles **must** be declared in `commands()`:

```rust
CommandSpec {
    name: "mycommand".into(),          // without the leading !
    description: "One sentence.".into(), // shown in !help <module> <command>
    usage: "!mycommand <arg>".into(),  // shown in !help <module>
    aliases: vec!["mc".into()],        // built-in defaults; operators can override in TUI
}
```

- `name` + `aliases` drive the TUI alias editor and `!help` at all three levels.
- `description` and `usage` must be present — empty strings produce useless help output.
- Commands not declared here are invisible to `!help` and cannot be aliased.

### 3. Theming — required for all user-facing output

**Every string sent to IRC** (channel or PM) must go through `themed()`, never hardcoded:

```rust
fn themed(key: &str, defaults: &[&str], vars: &[(&str, &str)]) -> Result<String, Error> {
    Ok(unsafe { theme(serde_json::to_string(&ThemeReq {
        key: key.into(),
        default: defaults.iter().map(|s| s.to_string()).collect(),
        vars: vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
    })?)? })
}
```

Rules:
- Keys must be namespaced: `"mymodule.action_name"` (e.g. `"wordle.win"`, `"fishing.cast"`).
- `defaults` is what gets written to `theme.toml` on first use — write it as you'd want it to
  appear; operators replace it there to customise.
- Pass every dynamic value as a `{placeholder}` variable, never by string formatting into the
  default. This lets operators rewrite the sentence structure without losing the values.
- Internal logs, debug text, and error tracing are exempt — only what goes to IRC needs theming.

### 4. Settings — required for any configurable behaviour

If the module has knobs the operator should be able to turn, export `settings()` returning a
`SettingsManifest`. Read effective values at runtime with the `setting_get` host function.

- At minimum, modules that post spontaneously (unprompted channel output) **must** declare an
  `enabled` boolean setting defaulting to `false`, so operators can gate channel noise.
- Declare per-channel scope for anything that should differ between channels.
- Never hardcode cooldowns, retention periods, or limits that an operator might want to change.

### 5. State and identity

- **Never key persistent state on a nick alone.** Nicks change. Use `profile_ensure` to obtain a
  stable profile UUID, then key all state on that UUID.
- KV keys are automatically namespaced per module by the host — use short, consistent key names
  within the module (e.g. `"game:#channel"`, `"stats:uuid"`).
- Cap stored values: bound queue sizes, stored text length, and number of records per user.
- For timers and scheduled delivery, use the durable scheduler host functions (`schedule_set`,
  `schedule_cancel`, `schedule_list`) and handle delivery in `on_event`. Do not poll via chat
  messages or invent a timing mechanism inside the module.
- Modules storing personal data must implement both lifecycle hooks. They must isolate by server,
  handle UUID and legacy alias ownership, fail on malformed relevant state, and never mutate KV
  directly from the hook; the host applies returned plans transactionally.

### 6. Randomness and time

- **Use `random_bytes`** for all randomness. Never seed your own PRNG from `now()` or a constant —
  it produces predictable sequences.
- **Use `now()`** for the current Unix timestamp. WASM modules have no access to the system clock.

### 7. Capabilities

Add the module to `module-capabilities.toml` listing only what it actually calls:

```toml
[mymodule]
capabilities = ["send_message", "theme", "kv_get", "kv_set", "now"]
```

Common capabilities: `send_message`, `theme`, `kv_get`, `kv_set`, `now`, `setting_get`,
`irc_casefold`,
`profile_ensure`, `profile_get`, `profile_set`, `log`, `schedule`, `random_bytes`, `commands_list`,
`ai_chat`, `bot_nick`. Omit any you don't use. Privileged ones (`bot_reload`, `bot_refresh`,
`bot_shutdown`) are for admin only.

### 8. Input validation and safety

- Validate user input at the boundary: check length before storing, reject non-alphabetic input
  where only letters are expected, etc.
- Apply per-user cooldowns for commands that touch external APIs or write to the DB.
- Bound module output for semantic correctness and readable IRC messages; the host's final
  line-length truncation is a safety net, not a substitute for module-level limits.
- Reject private-message use explicitly if the command is channel-only (or vice versa).
- Never assume the caller has any particular role unless you check `msg.role`.

### 9. Achievements

- Every applicable module exports an achievement manifest; the host owns counters, unlocks,
  prestige ranks, deduplication, and completion state.
- Emit `award_stats` only after the underlying operation and domain-state write succeeded. Awards
  require the host-stamped stable `msg.user_id`; never substitute a nick.
- Persistent/game modules with reliable historical totals export a pure, idempotent
  `achievement_backfill` using absolute `set_max` values.
- New finite, non-optional achievements automatically expand the dynamic completion catalog.
  Mark secrets, social/configuration-dependent milestones, and profile-detail milestones optional.

### Quick checklist before shipping a module

```
[ ] commands() exported with name, description, usage, aliases for every !command
[ ] on_message / on_event use those exact names (not "event")
[ ] Every IRC reply goes through themed("mymodule.key", &[default], &[vars])
[ ] All theme keys are namespaced: "modulename.action"
[ ] settings() exported if anything is configurable; enabled=false for spontaneous output
[ ] Persistent state keyed on stable profile UUIDs, not nicks
[ ] Personal KV state has pure `data_export` and idempotent `data_delete` hooks
[ ] Randomness via random_bytes, time via now()
[ ] module-capabilities.toml entry with only necessary capabilities
[ ] Input validated; per-user cooldowns on expensive ops
[ ] achievements() exported; successful events use stable UUIDs and award only after commit
[ ] Reliable historical totals have an idempotent achievement_backfill() hook
[ ] ./build-modules.sh <name> succeeds with no warnings
[ ] cargo test passes for any unit tests in the module crate
```

## Conventions (host / core code)

- `anyhow::Result` for app errors; `?` over `.unwrap()` outside tests/bootstrap.
- Host/guest payloads are JSON via `jeeves-abi` serde types — keep that crate the single source of
  truth for the ABI. Add new types there rather than defining them in module crates.
- rusqlite is only touched by the DB actor; everything else talks to it over a channel.
- The `irc::Client` is only touched by the IRC actor; everything else submits `Action`s.
- Keep `SPEC.md` and `PLAN.md` current as scope/status changes — they are the live record.

## Status

See `PLAN.md`.


<!-- headroom:rtk-instructions -->
# RTK (Rust Token Killer) - Token-Optimized Commands

When running shell commands, **always prefix with `rtk`**. This reduces context
usage by 60-90% with zero behavior change. If rtk has no filter for a command,
it passes through unchanged — so it is always safe to use.

## Key Commands
```bash
# Git (59-80% savings)
rtk git status          rtk git diff            rtk git log

# Files & Search (60-75% savings)
rtk ls <path>           rtk read <file>         rtk grep <pattern>
rtk find <pattern>      rtk diff <file>

# Test (90-99% savings) — shows failures only
rtk pytest tests/       rtk cargo test          rtk test <cmd>

# Build & Lint (80-90% savings) — shows errors only
rtk tsc                 rtk lint                rtk cargo build
rtk prettier --check    rtk mypy                rtk ruff check

# Analysis (70-90% savings)
rtk err <cmd>           rtk log <file>          rtk json <file>
rtk summary <cmd>       rtk deps                rtk env

# GitHub (26-87% savings)
rtk gh pr view <n>      rtk gh run list         rtk gh issue list

# Infrastructure (85% savings)
rtk docker ps           rtk kubectl get         rtk docker logs <c>

# Package managers (70-90% savings)
rtk pip list            rtk pnpm install        rtk npm run <script>
```

## Rules
- In command chains, prefix each segment: `rtk git add . && rtk git commit -m "msg"`
- For debugging, use raw command without rtk prefix
- `rtk proxy <cmd>` runs command without filtering but tracks usage
<!-- /headroom:rtk-instructions -->
# CLAUDE.md

See **[AGENTS.md](./AGENTS.md)** — it is the source of truth for repo layout, build/run commands,
architecture, and conventions. Read **SPEC.md** for the spec and **PLAN.md** for live status.

## Claude-specific notes

- Keep `SPEC.md` and `PLAN.md` in sync with reality as you work — they are the live tracking docs
  the user relies on.
- Only the DB actor touches rusqlite; only the IRC actor touches `irc::Client`. Route everything
  else through channels.
- `jeeves-abi` is the single source of truth for the host/guest WASM ABI.

<!-- grove:start -->
## Code navigation: grove for structure, shell for the rest

**grove** is a tree-sitter engine for *structural* code questions — byte-precise,
token-cheap (languages: bash, json, rust). Its tools are **deferred** MCP tools; load them in
one ToolSearch when a code question lands (don't default to a search agent or grep):
`mcp__grove__outline`, `mcp__grove__symbols`, `mcp__grove__source`, `mcp__grove__callers`, `mcp__grove__definition`, `mcp__grove__map`, `mcp__grove__check`.

**Use grove for named symbols and relationships** (every result carries a stable
`symbol-id`, `<lang>:<relpath>#<name>@<row>`, to pass forward; lines 1-based):
- What's in a file (skeleton, not the whole file) → `mcp__grove__outline` (`detail:0` if > 500 lines).
- Where a fn / type / struct / macro is defined → `mcp__grove__symbols` with `name` → `mcp__grove__source` with the id.
- One symbol's exact body → `mcp__grove__source`.
- Who calls it → `mcp__grove__callers`.
- Go-to-def from a usage (scope-aware, follows imports cross-file) → `mcp__grove__definition` with `at` (file:line:col).
- How a directory connects → `mcp__grove__map` (one call; prefer over many `mcp__grove__source`).
- Syntax after an edit → `mcp__grove__check`.

**Use the shell — the right tool, not a fallback — when grove can't see the target:**
- Text, not a symbol (a string, log / error message, config key, a macro's *value*,
  a constant, a flag, a TODO) → `grep -rn` / `rg`. grove finds definitions, not text.
- Non-code files (Makefiles, configs, data, docs) → `grep` / `read`.
- A quick fact (path exists, `ls`, `wc -l`, `find`, read a small file) → shell.

**Combine** (same 1-based lines, same bytes): `grep` a literal's line → `mcp__grove__definition`
`at` to resolve its symbol · `mcp__grove__outline` → bounded `read` (`offset`/`limit`) for
adjacent symbols · `mcp__grove__map` / `mcp__grove__symbols` to locate → `grep` a constant inside.

Rule of thumb: want a **symbol** → grove first (don't `grep` / `read` for it). Want
**text or a quick fact** → shell. Combining is fine.
<!-- grove:end -->
