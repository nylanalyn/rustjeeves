//! Host functions exposed to every module — the "base" capability API. Each takes a single JSON
//! string argument and returns a string (empty for void operations). Defined with extism's
//! `host_fn!` macro, which handles the WASM memory marshalling.

use super::HostCtx;
use crate::action::{Control, IrcAction};
use extism::host_fn;
use jeeves_abi::{
    Category, Channel, GeoQuery, KvGet, KvSet, Level, LogReq, ProfileClear, ProfileKey,
    ProfileUpdate, SendMessage, SendNotice, ThemeReq, WeatherQuery,
};

fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Send an action to a named server's live IRC actor, logging if the server is unknown/offline.
fn dispatch_action(ctx: &HostCtx, server: &str, action: IrcAction) {
    let registry = ctx.registry.lock().unwrap();
    match registry.get(server) {
        Some(tx) => {
            if tx.try_send(action).is_err() {
                ctx.log.error(
                    "modules",
                    format!("{}: action dropped for '{server}'", ctx.module),
                );
            }
        }
        None => ctx.log.error(
            "modules",
            format!("{}: unknown/disconnected server '{server}'", ctx.module),
        ),
    }
}

host_fn!(pub send_message(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    let ctx = ctx.lock().unwrap();
    ctx.require("send_message")?;
    let req: SendMessage = serde_json::from_str(&input)?;
    dispatch_action(&ctx, &req.server, IrcAction::Privmsg { target: req.target, text: req.text });
    Ok(String::new())
});

host_fn!(pub send_notice(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    let ctx = ctx.lock().unwrap();
    ctx.require("send_notice")?;
    let req: SendNotice = serde_json::from_str(&input)?;
    dispatch_action(&ctx, &req.server, IrcAction::Notice { target: req.target, text: req.text });
    Ok(String::new())
});

host_fn!(pub join(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    let ctx = ctx.lock().unwrap();
    ctx.require("join")?;
    let req: Channel = serde_json::from_str(&input)?;
    dispatch_action(&ctx, &req.server, IrcAction::Join(req.channel));
    Ok(String::new())
});

host_fn!(pub part(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    let ctx = ctx.lock().unwrap();
    ctx.require("part")?;
    let req: Channel = serde_json::from_str(&input)?;
    dispatch_action(&ctx, &req.server, IrcAction::Part(req.channel));
    Ok(String::new())
});

host_fn!(pub kv_get(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    let ctx = ctx.lock().unwrap();
    ctx.require("kv_get")?;
    let req: KvGet = serde_json::from_str(&input)?;
    let value = ctx.db.kv_get_blocking(&ctx.module, &req.key)?;
    Ok(value.unwrap_or_default())
});

host_fn!(pub kv_set(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    let ctx = ctx.lock().unwrap();
    ctx.require("kv_set")?;
    let req: KvSet = serde_json::from_str(&input)?;
    ctx.db.kv_set_blocking(&ctx.module, &req.key, &req.value)?;
    Ok(String::new())
});

host_fn!(pub log(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    let ctx = ctx.lock().unwrap();
    ctx.require("log")?;
    let req: LogReq = serde_json::from_str(&input)?;
    ctx.log.log(req.level, req.category, ctx.module.clone(), req.message);
    Ok(String::new())
});

host_fn!(pub profile_ensure(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    let ctx = ctx.lock().unwrap();
    ctx.require("profile_ensure")?;
    let key: ProfileKey = serde_json::from_str(&input)?;
    ctx.db.profile_ensure_blocking(&key.server, &key.nick, now_secs())?;
    Ok(String::new())
});

host_fn!(pub profile_get(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    let ctx = ctx.lock().unwrap();
    ctx.require("profile_get")?;
    let key: ProfileKey = serde_json::from_str(&input)?;
    match ctx.db.profile_get_blocking(&key.server, &key.nick)? {
        Some(p) => Ok(serde_json::to_string(&p)?),
        None => Ok(String::new()),
    }
});

host_fn!(pub profile_set(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    let ctx = ctx.lock().unwrap();
    ctx.require("profile_set")?;
    let upd: ProfileUpdate = serde_json::from_str(&input)?;
    ctx.db.profile_set_blocking(upd)?;
    Ok(String::new())
});

host_fn!(pub profile_clear(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    let ctx = ctx.lock().unwrap();
    ctx.require("profile_clear")?;
    let req: ProfileClear = serde_json::from_str(&input)?;
    ctx.db.profile_clear_blocking(&req.server, &req.nick, &req.field)?;
    Ok(String::new())
});

host_fn!(pub geocode(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    ctx.lock().unwrap().require("geocode")?;
    let req: GeoQuery = serde_json::from_str(&input)?;
    match crate::geo::geocode(&req.query) {
        Some(r) => Ok(serde_json::to_string(&r)?),
        None => Ok(String::new()),
    }
});

host_fn!(pub weather(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    ctx.lock().unwrap().require("weather")?;
    let req: WeatherQuery = serde_json::from_str(&input)?;
    match crate::weather::weather(req.lat, req.lon) {
        Some(r) => Ok(serde_json::to_string(&r)?),
        None => Ok(String::new()),
    }
});

host_fn!(pub now(ud: HostCtx; _input: String) -> String {
    let ctx = ud.get()?;
    ctx.lock().unwrap().require("now")?;
    Ok(now_secs().to_string())
});

host_fn!(pub theme(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    let ctx = ctx.lock().unwrap();
    ctx.require("theme")?;
    let req: ThemeReq = serde_json::from_str(&input)?;
    let mut store = ctx.theme.lock().unwrap();
    Ok(store.resolve(&ctx.module, &req.key, &req.default, &req.vars))
});

host_fn!(pub bot_reload(ud: HostCtx; _input: String) -> String {
    let ctx = ud.get()?;
    let ctx = ctx.lock().unwrap();
    ctx.require("bot_reload")?;
    ctx.log.log(Level::Info, Category::Command, ctx.module.clone(), "requested reload");
    let _ = ctx.control.try_send(Control::Reload);
    Ok(String::new())
});

host_fn!(pub bot_refresh(ud: HostCtx; _input: String) -> String {
    let ctx = ud.get()?;
    let ctx = ctx.lock().unwrap();
    ctx.require("bot_refresh")?;
    ctx.log.log(Level::Info, Category::Command, ctx.module.clone(), "requested refresh");
    let _ = ctx.control.try_send(Control::Refresh);
    Ok(String::new())
});

host_fn!(pub bot_shutdown(ud: HostCtx; _input: String) -> String {
    let ctx = ud.get()?;
    let ctx = ctx.lock().unwrap();
    ctx.require("bot_shutdown")?;
    ctx.log.log(Level::Info, Category::Command, ctx.module.clone(), "requested shutdown");
    let _ = ctx.control.try_send(Control::Shutdown);
    Ok(String::new())
});
