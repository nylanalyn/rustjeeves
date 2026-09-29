//! Guest-side helpers shared by rustjeeves WASM modules.
//!
//! `jeeves-abi` is the ABI: plain serde types, no behaviour. This crate is the other half every
//! module used to copy by hand: the handful of host calls nearly all of them make (themed replies,
//! the clock, settings, KV) and small utilities that must behave identically everywhere (hex key
//! encoding, leaderboard names that don't ping, unbiased randomness). A fix here lands in every
//! module on its next build.
//!
//! Host calls still need the matching capability in `module-capabilities.toml`; importing a
//! function the policy doesn't grant is harmless until it is called.

use extism_pdk::*;
use jeeves_abi::{
    KvGet, KvList, KvSet, MessagePayload, ModuleKvEntry, RandomBytesRequest, RandomBytesResponse,
    SendMessage, SettingGet, ThemeReq,
};

mod host {
    use extism_pdk::*;

    #[host_fn]
    extern "ExtismHost" {
        pub fn send_message(input: String) -> String;
        pub fn theme(input: String) -> String;
        pub fn now(input: String) -> String;
        pub fn setting_get(input: String) -> String;
        pub fn kv_get(input: String) -> String;
        pub fn kv_set(input: String) -> String;
        pub fn kv_list(input: String) -> String;
        pub fn random_bytes(input: String) -> String;
    }
}

/// A line from `theme.toml`, seeding `defaults` on first use. Pass every dynamic value as a
/// `{placeholder}` var; never format it into the default.
pub fn themed(key: &str, defaults: &[&str], vars: &[(&str, &str)]) -> Result<String, Error> {
    Ok(unsafe {
        host::theme(serde_json::to_string(&ThemeReq {
            key: key.into(),
            default: defaults.iter().map(|value| (*value).into()).collect(),
            vars: vars
                .iter()
                .map(|(name, value)| ((*name).into(), (*value).into()))
                .collect(),
        })?)?
    })
}

/// Sends `text` to a channel or nick on `server`.
pub fn reply(server: &str, target: &str, text: &str) -> Result<(), Error> {
    unsafe {
        host::send_message(serde_json::to_string(&SendMessage {
            server: server.into(),
            target: target.into(),
            text: text.into(),
        })?)?
    };
    Ok(())
}

/// The host's current Unix time, in seconds.
pub fn timestamp() -> Result<i64, Error> {
    let raw = unsafe { host::now(String::new())? };
    raw.trim()
        .parse()
        .map_err(|_| Error::msg(format!("host clock returned {raw:?}")))
}

/// A setting's effective value for `server` and (optionally) `channel`.
pub fn setting(key: &str, server: &str, channel: Option<&str>) -> Result<String, Error> {
    Ok(unsafe {
        host::setting_get(serde_json::to_string(&SettingGet {
            key: key.into(),
            server: Some(server.into()),
            channel: channel.map(str::to_string),
        })?)?
    })
}

pub fn setting_bool(key: &str, server: &str, channel: Option<&str>) -> Result<bool, Error> {
    Ok(setting(key, server, channel)? == "true")
}

/// An integer setting, or `fallback` when it can't be read or parsed.
pub fn setting_i64(key: &str, server: &str, channel: Option<&str>, fallback: i64) -> i64 {
    setting(key, server, channel)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(fallback)
}

/// This module's value for `key`; empty when unset.
pub fn kv_load(key: &str) -> Result<String, Error> {
    Ok(unsafe { host::kv_get(serde_json::to_string(&KvGet { key: key.into() })?)? })
}

pub fn kv_save(key: &str, value: &str) -> Result<(), Error> {
    unsafe {
        host::kv_set(serde_json::to_string(&KvSet {
            key: key.into(),
            value: value.into(),
        })?)?
    };
    Ok(())
}

/// This module's entries whose keys start with `prefix` (all of them for `""`), ordered by key.
pub fn kv_list_prefix(prefix: &str) -> Result<Vec<ModuleKvEntry>, Error> {
    let request = KvList {
        prefix: (!prefix.is_empty()).then(|| prefix.to_string()),
    };
    Ok(serde_json::from_str(&unsafe {
        host::kv_list(serde_json::to_string(&request)?)?
    })?)
}

/// Where a per-person cooldown stands; see [`cooldown_check`].
#[derive(Debug, PartialEq, Eq)]
pub enum Cooldown {
    /// Free to go; call [`cooldown_start`] once the action happens.
    Ready,
    /// Still cooling down: tell them once, with the seconds left.
    Warn(i64),
    /// Still cooling down and already told: stay quiet.
    Quiet,
}

/// Checks a cooldown kept under `key` (the last use, stored negated once a warning was given, so
/// someone hammering a command hears about it only once).
pub fn cooldown_check(key: &str, now: i64, seconds: i64) -> Result<Cooldown, Error> {
    let stored = kv_load(key)?.trim().parse::<i64>().unwrap_or(0);
    let (last_used, warned) = (stored.saturating_abs(), stored < 0);
    let remaining = seconds - now.saturating_sub(last_used);
    if now <= 0 || remaining <= 0 || remaining > seconds {
        return Ok(Cooldown::Ready);
    }
    if warned {
        return Ok(Cooldown::Quiet);
    }
    kv_save(key, &(-last_used).to_string())?;
    Ok(Cooldown::Warn(remaining))
}

/// Starts the cooldown under `key` from `now`.
pub fn cooldown_start(key: &str, now: i64) -> Result<(), Error> {
    kv_save(key, &now.to_string())
}

/// Lowercase hex of `value`'s bytes: a KV key segment that can't collide with separators.
pub fn encode(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    value
        .bytes()
        .flat_map(|byte| {
            [
                HEX[(byte >> 4) as usize] as char,
                HEX[(byte & 0x0f) as usize] as char,
            ]
        })
        .collect()
}

/// Breaks every word of a name with a zero-width space after its first character, so listing
/// someone (leaderboards, records) doesn't highlight them. Display names may carry a title
/// ("sir aureate"), so each word is broken.
pub fn no_highlight(name: &str) -> String {
    name.split(' ')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => format!("{first}\u{200B}{}", chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// How to name the sender: their display name (title included), or their nick.
pub fn display(msg: &MessagePayload) -> &str {
    if msg.display.is_empty() {
        &msg.nick
    } else {
        &msg.display
    }
}

/// How to address the sender: the host's pronoun-aware honorific, or their name from an older host
/// that doesn't send one.
pub fn honorific(msg: &MessagePayload) -> &str {
    if msg.honorific.is_empty() {
        display(msg)
    } else {
        &msg.honorific
    }
}

/// `{name}` placeholders in `template` filled from `vars` in one pass; unknown ones are kept.
/// Used to keep supplying a retired pass-through variable (`{text}`) with the default sentence.
pub fn fill(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let value = after.find('}').and_then(|close| {
            let name = &after[..close];
            vars.iter()
                .find(|(var, _)| *var == name)
                .map(|(_, value)| (*value, close))
        });
        match value {
            Some((value, close)) => {
                out.push_str(value);
                rest = &after[close + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Unbiased draws from host randomness, fetched in batches (the host gives at most 64 bytes a
/// call). Never seed a PRNG from the clock instead.
#[derive(Default)]
pub struct Entropy {
    bytes: Vec<u8>,
}

impl Entropy {
    fn next_u32(&mut self) -> Result<u32, Error> {
        if self.bytes.len() < 4 {
            let raw = unsafe {
                host::random_bytes(serde_json::to_string(&RandomBytesRequest { count: 64 })?)?
            };
            let response: RandomBytesResponse = serde_json::from_str(&raw)?;
            if response.bytes.len() < 4 {
                return Err(Error::msg("randomness host returned too few bytes"));
            }
            self.bytes = response.bytes;
        }
        let tail = self.bytes.split_off(self.bytes.len() - 4);
        Ok(u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]))
    }

    /// Uniform in `0..upper`, by rejection so no value is favoured.
    pub fn below(&mut self, upper: u32) -> Result<u32, Error> {
        let upper = upper.max(1);
        let zone = u32::MAX - (u32::MAX % upper);
        loop {
            let value = self.next_u32()?;
            if value < zone {
                return Ok(value % upper);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_matches_every_module_that_had_its_own() {
        // Keys already stored with the old copies must still be found.
        let old = |value: &str| {
            value
                .bytes()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        for value in ["", "net", "#Chan", "ünïcödé 🎣", "a:b/c"] {
            assert_eq!(encode(value), old(value), "{value}");
        }
    }

    #[test]
    fn names_are_broken_word_by_word() {
        assert_eq!(no_highlight("sir aureate"), "s\u{200B}ir a\u{200B}ureate");
        assert_eq!(no_highlight("é"), "é\u{200B}");
        assert_eq!(no_highlight(""), "");
    }

    #[test]
    fn fill_substitutes_known_placeholders_once() {
        assert_eq!(
            fill("{user} has {n} {unknown}", &[("user", "{n}"), ("n", "3")]),
            "{n} has 3 {unknown}"
        );
        assert_eq!(fill("{ {", &[]), "{ {");
    }

    #[test]
    fn names_and_honorifics_fall_back_sensibly() {
        let mut msg = MessagePayload {
            nick: "ann".into(),
            ..Default::default()
        };
        assert_eq!((display(&msg), honorific(&msg)), ("ann", "ann"));
        msg.display = "Dr ann".into();
        msg.honorific = "madam".into();
        assert_eq!((display(&msg), honorific(&msg)), ("Dr ann", "madam"));
    }
}
