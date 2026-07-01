# rustjeeves

`rustjeeves` (`jeeves`) is an IRCv3 bot framework written in Rust. It connects to multiple IRC
networks, runs in a ratatui TUI or headless mode, and loads Extism WASM modules from `modules/`.

## Current feature checklist

- [x] TLS, CAP negotiation, SASL PLAIN, NickServ fallback, channel auto-join
- [x] Per-network nickname folding negotiated from `005 CASEMAPPING`
- [x] Multiple simultaneous networks with automatic reconnect and exponential backoff
- [x] Interactive server/admin/log management, API credentials, command aliases, and scoped module
      settings
- [x] SQLite configuration, stable UUID user profiles, nick/account aliases, and retained logs
- [x] Hot-reloaded WASM modules with per-module capabilities, worker isolation, and time limits
- [x] Live `theme.toml` customization for every bundled module, including fishing
- [x] Admin, users, weather, local time, fishing, Tavily search, DeepL translation, YouTube search
      and opt-in link metadata, sailing/crow banter, channel history/quotes/sed corrections,
      channel-local memos, and durable reminders modules
- [x] Host-owned durable scheduler with restart recovery and targeted module timer events
- [x] Token-protected localhost HTTP admin bridge
- [x] Verified local SQLite backups with tiered retention and encrypted weekly Backblaze replication
- [x] Context-aware addressed AI responder for Ollama, OpenAI, and compatible chat-completions servers

## Build and run

```bash
cargo build --workspace
./build-modules.sh
cargo run -p jeeves -- --headless
# Interactive mode is the default:
cargo run -p jeeves -- --interactive
```

Run the root workspace and every standalone module test suite with `./test-all.sh`. Pull requests
also run formatting, strict Clippy, all tests, and release WASM builds in GitHub Actions.

In interactive mode, enter Tavily, DeepL, and YouTube keys under **Integrations (F3)** and save with
`Ctrl-S`. Keys are masked in the TUI and stored in `bot.db` (like the IRC passwords; SQLite is not
encrypted). Settings apply immediately and take precedence over environment variables. For
headless deployments, the modules use these fallbacks:

```bash
RUSTJEEVES_TAVILY_API_KEY="..." \
RUSTJEEVES_DEEPL_API_KEY="..." \
RUSTJEEVES_YOUTUBE_API_KEY="..." \
  cargo run -p jeeves -- --headless
```

`TAVILY_API_KEY`, `DEEPL_AUTH_KEY`, and `YOUTUBE_API_KEY` are also accepted as provider-standard
aliases.

## YouTube

The bundled `youtube` module provides `!yt <query>` (alias `!youtube`) and optional metadata for
YouTube links posted in channels. Search remains available when the module's scoped `enabled`
setting is false; that switch gates only passive announcements, which are off by default. Search
cooldowns, repeated-link suppression, maximum links per message, and like-count display are
configurable under **Modules (F5)**. The API key stays host-owned and is never passed to WASM.

## Channel banter

The optional `banter` module answers a whole-word `sail` from the configured `sailor_nick`
(`witeshark2` by default) with sailing banter, and a whole-word `caw` or `kaw` from anyone with crow
lore. It is disabled by default; enable it at the desired network/channel under **Modules (F5)**.
The two response pools live under `[banter]` in `theme.toml`, and separate scoped cooldowns prevent
one ritual from suppressing the other.

## AI responder

The bundled `ai` module responds to private messages and, when enabled per channel under
**Modules (F5)**, to explicit addressing such as `jeeves, explain this` or `JeevesBot: hello`.
The configured IRC nick is always recognized; additional comma-separated aliases are configurable
globally or per network. Embedded mentions in ordinary conversation do not trigger it.
Each enabled room keeps a bounded recent transcript (25 lines and three hours by default) so the
bot can resolve references in a question. Rooms, networks, and PM conversations are isolated;
line count and maximum age are scoped module settings. Stored lines participate in profile export
and deletion, and the provider receives them explicitly labelled as untrusted context.

Provider configuration lives under **Integrations (F3)**. The defaults target Ollama at
`http://127.0.0.1:11434/v1/chat/completions` using `llama3.2`; change the endpoint to the Ollama
machine's LAN address when it runs elsewhere. Select `openai` for OpenAI's Chat Completions API or
`compatible` for another compatible server. API keys are optional for local Ollama and remain in
the host rather than crossing into WASM. Headless equivalents are:

```bash
RUSTJEEVES_AI_PROVIDER=ollama \
RUSTJEEVES_AI_ENDPOINT=http://192.168.1.10:11434/v1/chat/completions \
RUSTJEEVES_AI_MODEL=llama3.2 \
RUSTJEEVES_AI_SOUL_PATH=SOUL.md \
  cargo run -p jeeves -- --headless
```

`RUSTJEEVES_AI_API_KEY` supplies a remote-provider key; OpenAI mode also accepts
`OPENAI_API_KEY`. The host reloads the size-bounded `SOUL.md` for each request. AI requests are
stateless, tool-free, concurrency-limited, time-bounded, and subject to stable-profile cooldowns.

Open **Commands (F4)** to view commands advertised by loaded modules. Select a command and press
Enter to edit its comma-separated aliases without the leading `!`; save with `Ctrl-S`. An empty
saved list disables all aliases for that command, while `r` restores the module defaults. Alias
changes are persisted in SQLite and apply immediately.

Open **Modules (F5)** to configure settings advertised by loaded modules. Overrides can be global,
per network, or per channel; precedence is channel → network → global → module default. Every
module has a standard `enabled` switch. Save with `Ctrl-S`, or remove the selected override with
`Ctrl-D`. Changes apply immediately.

Open **Profiles (F8)** to filter known profiles by network, nick, or stable UUID. A profile view
shows read-only aliases/account bindings, validated editable host fields, and each lifecycle-aware
module's profile-owned export. Module JSON is intentionally read-only; `r` previews a reset of only
that profile's contribution through the module's deletion hook. Every confirmed host edit or module
reset first creates and verifies a `backups/jeeves-pre-repair-*.sqlite` snapshot, logs field names
without values, and aborts if chat changed the affected data after the preview.

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

## Backups and restore

Open **Backups (F7)** to enable daily local snapshots, choose the UTC run hour and retention counts,
configure the Backblaze bucket/prefix/weekday, or press `r` to run immediately. The defaults retain
3 daily, 4 weekly, and 3 monthly local restore points. Every snapshot is opened, migrated, checked
with SQLite `integrity_check`, and accompanied by a SHA-256 manifest.

Backblaze credentials and the client encryption key live in masked **Integrations (F3)** fields;
focus the encryption-key field and press `Ctrl-G` to generate a key. Preserve that key outside the
bot database: a disk loss destroys both `bot.db` and its stored key, making remote backups
unrecoverable. Headless deployments may instead set `RUSTJEEVES_B2_KEY_ID`,
`RUSTJEEVES_B2_APPLICATION_KEY`, and `RUSTJEEVES_BACKUP_ENCRYPTION_KEY`. The B2 application key
needs `listBuckets`, `listFiles`, `writeFiles`, and `deleteFiles` access to the configured bucket and
prefix. Remote copies are scrubbed of IRC/API credentials, vacuumed, encrypted locally, uploaded
with a manifest, and pruned to the configured weekly retention count.

Verify a local snapshot without starting the bot:

```bash
cargo run -p jeeves -- --verify-backup backups/jeeves-daily-YYYYMMDD-HHMMSS.sqlite
```

Decrypt and verify a remote `.rjb` object to a new file:

```bash
RUSTJEEVES_BACKUP_ENCRYPTION_KEY="..." \
  cargo run -p jeeves -- \
  --decrypt-backup jeeves-YYYYMMDD-HHMMSS.sqlite.rjb \
  --decrypt-output restored.sqlite
```

Stop Jeeves before replacing `bot.db`. Keep the old database until the restored copy has passed
`--verify-backup`; remote restores intentionally require IRC and API secrets to be re-entered.

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
