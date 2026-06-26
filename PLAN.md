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

`cargo build --workspace`, `cargo clippy --workspace`, and `cargo test -p jeeves` (7 tests) clean.
