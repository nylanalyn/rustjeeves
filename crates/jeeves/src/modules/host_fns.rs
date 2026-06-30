//! Host functions exposed to every module — the "base" capability API. Each takes a single JSON
//! string argument and returns a string (empty for void operations). Defined with extism's
//! `host_fn!` macro, which handles the WASM memory marshalling.

use super::HostCtx;
use crate::action::{Control, IrcAction};
use extism::host_fn;
use jeeves_abi::{
    AiChatRequest, Category, Channel, CommandInfo, GeoQuery, IrcCasefold, KvGet, KvSet, Level,
    LocalTimeQuery, LogReq, ProfileClear, ProfileKey, ProfileUpdate, RandomBytesRequest,
    RandomBytesResponse, ScheduleCancel, ScheduleList, ScheduleSet, SearchQuery, SendMessage,
    SendNotice, ServerQuery, SettingGet, ThemeReq, TranslateQuery, WeatherQuery, YoutubeLookup,
    YoutubeSearch,
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

host_fn!(pub setting_get(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    let ctx = ctx.lock().unwrap();
    ctx.require("setting_get")?;
    let req: SettingGet = serde_json::from_str(&input)?;
    let value = ctx.settings
        .lock()
        .unwrap()
        .effective(
            &ctx.module,
            &req.key,
            req.server.as_deref(),
            req.channel.as_deref(),
        )
        .ok_or_else(|| anyhow::anyhow!("unknown setting '{}.{}'", ctx.module, req.key));
    value
});

host_fn!(pub schedule_set(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    let ctx = ctx.lock().unwrap();
    ctx.require("schedule")?;
    let request: ScheduleSet = serde_json::from_str(&input)?;
    ctx.scheduler.set_blocking(&ctx.module, request)?;
    Ok(String::new())
});

host_fn!(pub schedule_cancel(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    let ctx = ctx.lock().unwrap();
    ctx.require("schedule")?;
    let request: ScheduleCancel = serde_json::from_str(&input)?;
    Ok(ctx.scheduler.cancel_blocking(&ctx.module, &request.id)?.to_string())
});

host_fn!(pub schedule_list(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    let ctx = ctx.lock().unwrap();
    ctx.require("schedule")?;
    let request: ScheduleList = serde_json::from_str(&input)?;
    let jobs = ctx.scheduler.list_blocking(
        &ctx.module,
        request.server.as_deref(),
        request.channel.as_deref(),
    )?;
    Ok(serde_json::to_string(&jobs)?)
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

host_fn!(pub local_time(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    ctx.lock().unwrap().require("local_time")?;
    let req: LocalTimeQuery = serde_json::from_str(&input)?;
    let unix_seconds = req.unix_seconds.unwrap_or_else(now_secs);
    match crate::local_time::local_time(&req.timezone, unix_seconds) {
        Some(r) => Ok(serde_json::to_string(&r)?),
        None => Ok(String::new()),
    }
});

host_fn!(pub web_search(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    let db = {
        let ctx = ctx.lock().unwrap();
        ctx.require("web_search")?;
        ctx.db.clone()
    };
    let req: SearchQuery = serde_json::from_str(&input)?;
    let api_key = db.config_get_blocking(crate::search::API_KEY_CONFIG)?;
    Ok(serde_json::to_string(&crate::search::search(&req.query, api_key.as_deref()))?)
});

host_fn!(pub translate(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    let db = {
        let ctx = ctx.lock().unwrap();
        ctx.require("translate")?;
        ctx.db.clone()
    };
    let req: TranslateQuery = serde_json::from_str(&input)?;
    let api_key = db.config_get_blocking(crate::deepl::API_KEY_CONFIG)?;
    Ok(serde_json::to_string(&crate::deepl::translate(&req, api_key.as_deref()))?)
});

host_fn!(pub ai_chat(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    let db = {
        let ctx = ctx.lock().unwrap();
        ctx.require("ai_chat")?;
        ctx.db.clone()
    };
    let req: AiChatRequest = serde_json::from_str(&input)?;
    let provider = db.config_get_blocking(crate::ai::PROVIDER_CONFIG)?
        .or_else(|| std::env::var("RUSTJEEVES_AI_PROVIDER").ok())
        .unwrap_or_else(|| crate::ai::DEFAULT_PROVIDER.into());
    let endpoint = db.config_get_blocking(crate::ai::ENDPOINT_CONFIG)?
        .or_else(|| std::env::var("RUSTJEEVES_AI_ENDPOINT").ok())
        .unwrap_or_else(|| crate::ai::DEFAULT_ENDPOINT.into());
    let model = db.config_get_blocking(crate::ai::MODEL_CONFIG)?
        .or_else(|| std::env::var("RUSTJEEVES_AI_MODEL").ok())
        .unwrap_or_else(|| crate::ai::DEFAULT_MODEL.into());
    let soul_path = db.config_get_blocking(crate::ai::SOUL_PATH_CONFIG)?
        .or_else(|| std::env::var("RUSTJEEVES_AI_SOUL_PATH").ok())
        .unwrap_or_else(|| crate::ai::DEFAULT_SOUL_PATH.into());
    let configured_key = db.config_get_blocking(crate::ai::API_KEY_CONFIG)?;
    let api_key = configured_key
        .filter(|key| !key.trim().is_empty())
        .or_else(|| std::env::var("RUSTJEEVES_AI_API_KEY").ok())
        .or_else(|| (provider == "openai").then(|| std::env::var("OPENAI_API_KEY").ok()).flatten());
    let config = crate::ai::AiConfig { provider, endpoint, model, soul_path, api_key };
    Ok(serde_json::to_string(&crate::ai::chat(&req, &config))?)
});

host_fn!(pub bot_nick(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    let db = {
        let ctx = ctx.lock().unwrap();
        ctx.require("bot_nick")?;
        ctx.db.clone()
    };
    let req: ServerQuery = serde_json::from_str(&input)?;
    Ok(db.load_servers_blocking()?
        .into_iter()
        .find(|server| server.label == req.server)
        .map(|server| server.nick)
        .unwrap_or_default())
});

host_fn!(pub irc_casefold(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    let db = {
        let ctx = ctx.lock().unwrap();
        ctx.require("irc_casefold")?;
        ctx.db.clone()
    };
    let req: IrcCasefold = serde_json::from_str(&input)?;
    Ok(db.irc_casefold(&req.server, &req.value))
});

host_fn!(pub youtube_lookup(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    let db = {
        let ctx = ctx.lock().unwrap();
        ctx.require("youtube_lookup")?;
        ctx.db.clone()
    };
    let req: YoutubeLookup = serde_json::from_str(&input)?;
    let key = db.config_get_blocking(crate::youtube::API_KEY_CONFIG)?;
    Ok(serde_json::to_string(&crate::youtube::lookup(&req.ids, key.as_deref()))?)
});

host_fn!(pub youtube_search(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    let db = {
        let ctx = ctx.lock().unwrap();
        ctx.require("youtube_search")?;
        ctx.db.clone()
    };
    let req: YoutubeSearch = serde_json::from_str(&input)?;
    let key = db.config_get_blocking(crate::youtube::API_KEY_CONFIG)?;
    Ok(serde_json::to_string(&crate::youtube::search(&req.query, key.as_deref()))?)
});

host_fn!(pub random_bytes(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    ctx.lock().unwrap().require("random_bytes")?;
    let req: RandomBytesRequest = serde_json::from_str(&input)?;
    let count = req.count.min(64);
    let mut bytes = vec![0u8; count];
    for byte in &mut bytes {
        *byte = fastrand::u8(..);
    }
    Ok(serde_json::to_string(&RandomBytesResponse { bytes })?)
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

host_fn!(pub commands_list(ud: HostCtx; _input: String) -> String {
    let ctx = ud.get()?;
    let ctx = ctx.lock().unwrap();
    ctx.require("commands_list")?;
    let snapshot = ctx.commands.lock().unwrap().snapshot();
    let info: Vec<CommandInfo> = snapshot
        .iter()
        .map(|rc| CommandInfo {
            module: rc.module.clone(),
            name: rc.name.clone(),
            description: rc.description.clone(),
            usage: rc.usage.clone(),
            aliases: rc.aliases.clone(),
        })
        .collect();
    Ok(serde_json::to_string(&info)?)
});
