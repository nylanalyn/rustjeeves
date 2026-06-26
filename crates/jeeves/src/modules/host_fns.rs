//! Host functions exposed to every module — the "base" capability API. Each takes a single JSON
//! string argument and returns a string (empty for void operations). Defined with extism's
//! `host_fn!` macro, which handles the WASM memory marshalling.

use super::HostCtx;
use crate::action::{Control, IrcAction};
use extism::host_fn;
use jeeves_abi::{Category, Channel, KvGet, KvSet, Level, LogReq, SendMessage, SendNotice};

host_fn!(pub send_message(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    let ctx = ctx.lock().unwrap();
    let req: SendMessage = serde_json::from_str(&input)?;
    if ctx.actions.try_send(IrcAction::Privmsg { target: req.target, text: req.text }).is_err() {
        ctx.log.error("modules", format!("{}: send_message dropped (channel full)", ctx.module));
    }
    Ok(String::new())
});

host_fn!(pub send_notice(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    let ctx = ctx.lock().unwrap();
    let req: SendNotice = serde_json::from_str(&input)?;
    let _ = ctx.actions.try_send(IrcAction::Notice { target: req.target, text: req.text });
    Ok(String::new())
});

host_fn!(pub join(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    let ctx = ctx.lock().unwrap();
    let req: Channel = serde_json::from_str(&input)?;
    let _ = ctx.actions.try_send(IrcAction::Join(req.channel));
    Ok(String::new())
});

host_fn!(pub part(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    let ctx = ctx.lock().unwrap();
    let req: Channel = serde_json::from_str(&input)?;
    let _ = ctx.actions.try_send(IrcAction::Part(req.channel));
    Ok(String::new())
});

host_fn!(pub kv_get(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    let ctx = ctx.lock().unwrap();
    let req: KvGet = serde_json::from_str(&input)?;
    let value = ctx.db.kv_get_blocking(&ctx.module, &req.key)?;
    Ok(value.unwrap_or_default())
});

host_fn!(pub kv_set(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    let ctx = ctx.lock().unwrap();
    let req: KvSet = serde_json::from_str(&input)?;
    ctx.db.kv_set_blocking(&ctx.module, &req.key, &req.value)?;
    Ok(String::new())
});

host_fn!(pub log(ud: HostCtx; input: String) -> String {
    let ctx = ud.get()?;
    let ctx = ctx.lock().unwrap();
    let req: LogReq = serde_json::from_str(&input)?;
    ctx.log.log(req.level, req.category, ctx.module.clone(), req.message);
    Ok(String::new())
});

host_fn!(pub bot_reload(ud: HostCtx; _input: String) -> String {
    let ctx = ud.get()?;
    let ctx = ctx.lock().unwrap();
    ctx.log.log(Level::Info, Category::Command, ctx.module.clone(), "requested reload");
    let _ = ctx.control.try_send(Control::Reload);
    Ok(String::new())
});

host_fn!(pub bot_refresh(ud: HostCtx; _input: String) -> String {
    let ctx = ud.get()?;
    let ctx = ctx.lock().unwrap();
    ctx.log.log(Level::Info, Category::Command, ctx.module.clone(), "requested refresh");
    let _ = ctx.control.try_send(Control::Refresh);
    Ok(String::new())
});

host_fn!(pub bot_shutdown(ud: HostCtx; _input: String) -> String {
    let ctx = ud.get()?;
    let ctx = ctx.lock().unwrap();
    ctx.log.log(Level::Info, Category::Command, ctx.module.clone(), "requested shutdown");
    let _ = ctx.control.try_send(Control::Shutdown);
    Ok(String::new())
});
