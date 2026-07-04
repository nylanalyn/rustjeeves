//! Read-only public achievement gallery and JSON API.

use crate::db::{DbHandle, PublicAchievementHolder};
use crate::log_bus::LogBus;
use crate::modules::AchievementRegistry;
use jeeves_abi::{AchievementManifest, AchievementProfileSummary};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

const MAX_RESPONSE_BYTES: usize = 512 * 1024;
const RATE_LIMIT_PER_MINUTE: u32 = 120;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
type RouteResponse = (u16, &'static str, String, u32);

#[derive(Clone)]
pub struct PublicWebState {
    pub db: DbHandle,
    pub achievements: AchievementRegistry,
}

#[derive(Default)]
struct RateLimiter {
    clients: HashMap<IpAddr, (Instant, u32)>,
}

#[derive(Serialize)]
struct CatalogResponse {
    version: u32,
    modules: Vec<PublicModule>,
    totals: PublicTotals,
}

#[derive(Serialize)]
struct PublicModule {
    module: String,
    achievements: Vec<PublicAchievement>,
    prestige: Vec<PublicPrestige>,
}

#[derive(Serialize)]
struct PublicAchievement {
    id: String,
    name: String,
    description: Option<String>,
    optional: bool,
    secret: bool,
    earned: Option<bool>,
    current: Option<u64>,
    threshold: Option<u64>,
}

#[derive(Serialize)]
struct PublicPrestige {
    id: String,
    name: String,
    first_threshold: u64,
    every: u64,
    rank: Option<u64>,
}

#[derive(Default, Serialize)]
struct PublicTotals {
    achievements: usize,
    modules: usize,
    prestige_tracks: usize,
    earned: Option<usize>,
}

#[derive(Serialize)]
struct UsersResponse {
    version: u32,
    users: Vec<PublicUser>,
}

#[derive(Clone, Serialize)]
struct PublicUser {
    server: String,
    profile: String,
    nick: String,
    label: String,
}

#[derive(Serialize)]
struct CollectionResponse {
    version: u32,
    user: PublicUser,
    modules: Vec<PublicModule>,
    totals: PublicTotals,
}

pub fn serve(bind: String, state: PublicWebState, log: LogBus) {
    std::thread::Builder::new()
        .name("jeeves-public-web".into())
        .spawn(move || {
            let server = match Server::http(&bind) {
                Ok(server) => server,
                Err(error) => {
                    log.error("publicweb", format!("failed to bind {bind}: {error}"));
                    return;
                }
            };
            log.info(
                "publicweb",
                format!("public gallery listening on http://{bind}"),
            );
            let limiter = Arc::new(Mutex::new(RateLimiter::default()));
            for request in server.incoming_requests() {
                handle(request, &state, &limiter, &log);
            }
        })
        .ok();
}

fn handle(request: Request, state: &PublicWebState, limiter: &Mutex<RateLimiter>, log: &LogBus) {
    let (path, query) = split_url(request.url());
    if path != "/health" && !rate_allowed(&request, limiter) {
        respond_text(
            request,
            429,
            "text/plain; charset=utf-8",
            "rate limit exceeded",
            0,
        );
        return;
    }
    if request.method() != &Method::Get && request.method() != &Method::Head {
        respond_text(
            request,
            405,
            "text/plain; charset=utf-8",
            "method not allowed",
            0,
        );
        return;
    }
    let head = request.method() == &Method::Head;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let route_state = state.clone();
    std::thread::spawn(move || {
        let _ = tx.send(route(&route_state, &path, &query));
    });
    match rx.recv_timeout(REQUEST_TIMEOUT) {
        Ok(Ok((status, content_type, body, max_age))) => {
            respond_cached(request, status, content_type, body, max_age, head)
        }
        Ok(Err(error)) => {
            log.error("publicweb", format!("request failed: {error}"));
            respond_text(
                request,
                500,
                "application/json",
                r#"{"error":"internal error"}"#,
                0,
            );
        }
        Err(_) => respond_text(
            request,
            503,
            "application/json",
            r#"{"error":"request timeout"}"#,
            0,
        ),
    }
}

fn route(state: &PublicWebState, path: &str, query: &str) -> anyhow::Result<RouteResponse> {
    match path {
        "/health" => Ok((200, "application/json", r#"{"ok":true}"#.into(), 0)),
        "/ready" => {
            let ready = !state.achievements.lock().unwrap().is_empty();
            Ok((
                if ready { 200 } else { 503 },
                "application/json",
                format!(r#"{{"ready":{ready}}}"#),
                0,
            ))
        }
        "/style.css" => Ok((200, "text/css; charset=utf-8", STYLE.into(), 3600)),
        "/v1/catalog" => catalog(state).and_then(|value| json_body(&value, 60)),
        "/v1/users" => users(state).and_then(|value| json_body(&value, 15)),
        "/v1/collection" => {
            let server = query_param(query, "server");
            let profile = query_param(query, "profile");
            match (server, profile) {
                (Some(server), Some(profile)) => collection(state, &server, &profile),
                _ => Ok((
                    400,
                    "application/json",
                    r#"{"error":"missing server or profile"}"#.into(),
                    0,
                )),
            }
        }
        "/" => page(state, query).map(|body| (200, "text/html; charset=utf-8", body, 15)),
        _ => Ok((404, "text/plain; charset=utf-8", "not found".into(), 0)),
    }
}

fn manifests(state: &PublicWebState) -> Vec<(String, AchievementManifest)> {
    let mut values = state
        .achievements
        .lock()
        .unwrap()
        .iter()
        .map(|(name, manifest)| (name.clone(), manifest.clone()))
        .collect::<Vec<_>>();
    values.sort_by(|a, b| a.0.cmp(&b.0));
    values
}

fn catalog(state: &PublicWebState) -> anyhow::Result<CatalogResponse> {
    let modules = manifests(state)
        .into_iter()
        .map(|(module, manifest)| public_module(&module, &manifest, None))
        .collect::<Vec<_>>();
    Ok(CatalogResponse {
        version: 1,
        totals: totals(&modules, None),
        modules,
    })
}

fn eligible_users(state: &PublicWebState) -> anyhow::Result<Vec<PublicUser>> {
    let catalogs = manifests(state);
    let mut users = Vec::new();
    for holder in state.db.public_achievement_holders_blocking()? {
        if owns_current_finite(&holder, &catalogs) {
            users.push(public_user(holder));
        }
    }
    Ok(users)
}

fn owns_current_finite(
    holder: &PublicAchievementHolder,
    catalogs: &[(String, AchievementManifest)],
) -> bool {
    holder.unlocks.iter().any(|(module, achievement_id)| {
        catalogs.iter().any(|(catalog_module, manifest)| {
            catalog_module == module
                && manifest
                    .achievements
                    .iter()
                    .any(|spec| &spec.id == achievement_id)
        })
    })
}

fn public_user(holder: PublicAchievementHolder) -> PublicUser {
    PublicUser {
        label: format!("{} — {}", holder.nick, holder.server),
        server: holder.server,
        profile: holder.profile_id,
        nick: holder.nick,
    }
}

fn users(state: &PublicWebState) -> anyhow::Result<UsersResponse> {
    Ok(UsersResponse {
        version: 1,
        users: eligible_users(state)?,
    })
}

fn collection(
    state: &PublicWebState,
    server: &str,
    profile: &str,
) -> anyhow::Result<(u16, &'static str, String, u32)> {
    if server.len() > 100 || profile.len() > 100 {
        return Ok((
            404,
            "application/json",
            r#"{"error":"not found"}"#.into(),
            0,
        ));
    }
    let Some(user) = eligible_users(state)?
        .into_iter()
        .find(|user| user.server == server && user.profile == profile)
    else {
        return Ok((
            404,
            "application/json",
            r#"{"error":"not found"}"#.into(),
            0,
        ));
    };
    let catalogs = manifests(state);
    let summary = state
        .db
        .achievements_get_blocking(server, profile, catalogs.clone())?;
    let modules = catalogs
        .iter()
        .map(|(module, manifest)| public_module(module, manifest, Some(&summary)))
        .collect::<Vec<_>>();
    let response = CollectionResponse {
        version: 1,
        totals: totals(&modules, Some(&summary)),
        modules,
        user,
    };
    json_body(&response, 15)
}

fn public_module(
    module: &str,
    manifest: &AchievementManifest,
    summary: Option<&AchievementProfileSummary>,
) -> PublicModule {
    let progress = summary.and_then(|summary| summary.modules.iter().find(|m| m.module == module));
    let mut achievements = manifest
        .achievements
        .iter()
        .filter_map(|spec| {
            let status = progress
                .and_then(|module| module.achievements.iter().find(|item| item.id == spec.id));
            let earned = status.is_some_and(|item| item.earned);
            if spec.secret && !earned {
                return None;
            }
            Some(PublicAchievement {
                id: spec.id.clone(),
                name: spec.name.clone(),
                description: (!spec.secret).then(|| spec.description.clone()),
                optional: spec.optional,
                secret: spec.secret,
                earned: summary.map(|_| earned),
                current: (!spec.secret).then(|| status.map_or(0, |item| item.current)),
                threshold: (!spec.secret).then_some(spec.threshold),
            })
        })
        .collect::<Vec<_>>();
    achievements.sort_by(|a, b| a.name.cmp(&b.name));
    let prestige = manifest
        .prestige
        .iter()
        .map(|spec| PublicPrestige {
            id: spec.id.clone(),
            name: spec.name.clone(),
            first_threshold: spec.first_threshold,
            every: spec.every,
            rank: progress.and_then(|module| {
                module
                    .prestige
                    .iter()
                    .find(|rank| rank.id == spec.id)
                    .map(|rank| rank.rank)
            }),
        })
        .collect();
    PublicModule {
        module: module.into(),
        achievements,
        prestige,
    }
}

fn totals(modules: &[PublicModule], summary: Option<&AchievementProfileSummary>) -> PublicTotals {
    PublicTotals {
        achievements: modules.iter().map(|module| module.achievements.len()).sum(),
        modules: modules.len(),
        prestige_tracks: modules.iter().map(|module| module.prestige.len()).sum(),
        earned: summary.map(|_| {
            modules
                .iter()
                .flat_map(|module| &module.achievements)
                .filter(|item| item.earned == Some(true))
                .count()
        }),
    }
}

fn page(state: &PublicWebState, query: &str) -> anyhow::Result<String> {
    let users = eligible_users(state)?;
    let selected = query_param(query, "holder")
        .and_then(|holder| {
            holder
                .split_once('|')
                .map(|(server, profile)| (server.to_string(), profile.to_string()))
        })
        .and_then(|(server, profile)| {
            users
                .iter()
                .find(|user| user.server == server && user.profile == profile)
                .cloned()
        });
    let catalogs = manifests(state);
    let summary = selected
        .as_ref()
        .map(|user| {
            state
                .db
                .achievements_get_blocking(&user.server, &user.profile, catalogs.clone())
        })
        .transpose()?;
    let modules = catalogs
        .iter()
        .map(|(module, manifest)| public_module(module, manifest, summary.as_ref()))
        .collect::<Vec<_>>();
    let totals = totals(&modules, summary.as_ref());
    let mut html = String::from("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Jeeves Achievement Gallery</title><link rel=\"stylesheet\" href=\"/style.css\"></head><body><header><p class=\"eyebrow\">JEEVES</p><h1>Achievement Gallery</h1><p>Collections earned across the IRC fleet.</p></header><main>");
    html.push_str("<form method=\"get\" action=\"/\"><label for=\"holder\">Achievement holder</label><select id=\"holder\" name=\"holder\"><option value=\"\">Catalog overview</option>");
    for user in &users {
        let is_selected = selected.as_ref().is_some_and(|current| {
            current.profile == user.profile && current.server == user.server
        });
        let selected_attr = if is_selected { " selected" } else { "" };
        html.push_str(&format!(
            "<option value=\"{}|{}\"{}>{}</option>",
            escape(&user.server),
            escape(&user.profile),
            selected_attr,
            escape(&user.label)
        ));
    }
    html.push_str("</select><button type=\"submit\">View collection</button></form>");
    if users.is_empty() {
        html.push_str(
            "<p class=\"notice\">No achievement holders have published a collection yet.</p>",
        );
    }
    html.push_str(&format!("<section class=\"summary\"><strong>{}</strong> achievements · <strong>{}</strong> modules · <strong>{}</strong> prestige tracks", totals.achievements, totals.modules, totals.prestige_tracks));
    if let Some(earned) = totals.earned {
        html.push_str(&format!(" · <strong>{earned}</strong> earned"));
    }
    html.push_str("</section><nav aria-label=\"Modules\">");
    for module in &modules {
        html.push_str(&format!(
            "<a href=\"#module-{}\">{}</a>",
            escape(&module.module),
            escape(&module.module)
        ));
    }
    html.push_str("</nav>");
    for module in &modules {
        html.push_str(&format!(
            "<section class=\"module\" id=\"module-{}\"><h2>{}</h2><div class=\"grid\">",
            escape(&module.module),
            escape(&module.module)
        ));
        for item in &module.achievements {
            let class = match (item.earned, item.secret, item.optional) {
                (Some(true), true, _) => "card earned secret",
                (Some(true), _, true) => "card earned optional",
                (Some(true), _, _) => "card earned",
                (Some(false), _, true) => "card locked optional",
                (Some(false), _, _) => "card locked",
                (None, _, true) => "card optional",
                _ => "card",
            };
            html.push_str(&format!(
                "<article class=\"{class}\"><h3>{}</h3>",
                escape(&item.name)
            ));
            if item.secret {
                html.push_str("<span class=\"tag\">Earned secret</span>");
            }
            if item.optional {
                html.push_str("<span class=\"tag\">Optional</span>");
            }
            if let Some(description) = &item.description {
                html.push_str(&format!("<p>{}</p>", escape(description)));
            }
            if let (Some(current), Some(threshold)) = (item.current, item.threshold) {
                html.push_str(&format!("<progress value=\"{current}\" max=\"{threshold}\"></progress><small>{current} / {threshold}</small>"));
            }
            html.push_str("</article>");
        }
        for prestige in &module.prestige {
            html.push_str(&format!("<article class=\"card prestige\"><h3>{}</h3><span class=\"tag\">Prestige</span><p>Begins at {} and advances every {}.</p>", escape(&prestige.name), prestige.first_threshold, prestige.every));
            if let Some(rank) = prestige.rank.filter(|rank| *rank > 0) {
                html.push_str(&format!("<strong>Rank {rank}</strong>"));
            }
            html.push_str("</article>");
        }
        html.push_str("</div></section>");
    }
    html.push_str("</main><footer>Read-only gallery · Profiles appear only by explicit opt-in.</footer></body></html>");
    Ok(html)
}

fn rate_allowed(request: &Request, limiter: &Mutex<RateLimiter>) -> bool {
    let Some(peer) = request.remote_addr().map(|address| address.ip()) else {
        return true;
    };
    // Trust Cloudflare's client address only from the loopback reverse proxy/tunnel.
    let ip = if peer.is_loopback() {
        request
            .headers()
            .iter()
            .find(|header| header.field.equiv("CF-Connecting-IP"))
            .and_then(|header| header.value.as_str().parse().ok())
            .unwrap_or(peer)
    } else {
        peer
    };
    let now = Instant::now();
    let mut limiter = limiter.lock().unwrap();
    limiter
        .clients
        .retain(|_, (start, _)| now.duration_since(*start) < Duration::from_secs(120));
    let entry = limiter.clients.entry(ip).or_insert((now, 0));
    if now.duration_since(entry.0) >= Duration::from_secs(60) {
        *entry = (now, 0);
    }
    entry.1 += 1;
    entry.1 <= RATE_LIMIT_PER_MINUTE
}

fn json_body<T: Serialize>(
    value: &T,
    max_age: u32,
) -> anyhow::Result<(u16, &'static str, String, u32)> {
    let body = serde_json::to_string(value)?;
    if body.len() > MAX_RESPONSE_BYTES {
        anyhow::bail!("public response exceeds size limit");
    }
    Ok((200, "application/json", body, max_age))
}

fn respond_cached(
    request: Request,
    status: u16,
    content_type: &str,
    body: String,
    max_age: u32,
    head: bool,
) {
    if body.len() > MAX_RESPONSE_BYTES {
        respond_text(
            request,
            500,
            "text/plain; charset=utf-8",
            "response too large",
            0,
        );
        return;
    }
    let etag = format!("\"{:x}\"", Sha256::digest(body.as_bytes()));
    let not_modified = request.headers().iter().any(|header| {
        header.field.equiv("If-None-Match") && header.value.as_str() == etag.as_str()
    });
    let actual_status = if not_modified { 304 } else { status };
    let actual_body = if not_modified || head {
        String::new()
    } else {
        body
    };
    let mut response =
        Response::from_string(actual_body).with_status_code(StatusCode(actual_status));
    for (name, value) in security_headers(content_type, max_age) {
        if let Ok(header) = Header::from_bytes(name, value) {
            response.add_header(header);
        }
    }
    if let Ok(header) = Header::from_bytes("ETag", etag) {
        response.add_header(header);
    }
    let _ = request.respond(response);
}

fn respond_text(request: Request, status: u16, content_type: &str, body: &str, max_age: u32) {
    respond_cached(request, status, content_type, body.into(), max_age, false)
}

fn security_headers(content_type: &str, max_age: u32) -> Vec<(&'static str, String)> {
    vec![
        ("Content-Type", content_type.into()),
        ("Cache-Control", format!("public, max-age={max_age}")),
        ("Content-Security-Policy", "default-src 'none'; style-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'".into()),
        ("X-Content-Type-Options", "nosniff".into()),
        ("Referrer-Policy", "no-referrer".into()),
        ("X-Frame-Options", "DENY".into()),
        ("Permissions-Policy", "camera=(), microphone=(), geolocation=()".into()),
    ]
}

fn split_url(url: &str) -> (String, String) {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    (path.to_string(), query.to_string())
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        (name == key).then(|| percent_decode(value))
    })
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let decoded = std::str::from_utf8(&bytes[index + 1..index + 3])
                    .ok()
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok());
                if let Some(decoded) = decoded {
                    out.push(decoded);
                    index += 3;
                } else {
                    out.push(bytes[index]);
                    index += 1;
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

const STYLE: &str = r#"
:root{color-scheme:dark;--bg:#07110f;--panel:#10201c;--ink:#f1f5df;--muted:#9caf9f;--gold:#e6bf65;--line:#29443b;--accent:#69c5a4}*{box-sizing:border-box}body{margin:0;background:radial-gradient(circle at top,#15362d 0,var(--bg) 42%);color:var(--ink);font:16px/1.5 system-ui,sans-serif}header,main,footer{width:min(1120px,92vw);margin:auto}header{padding:4rem 0 2rem}.eyebrow{color:var(--gold);letter-spacing:.35em;font-weight:800}h1{font-family:Georgia,serif;font-size:clamp(2.5rem,7vw,5.5rem);line-height:.95;margin:.2rem 0}form,.summary,.notice{background:rgba(16,32,28,.9);border:1px solid var(--line);border-radius:14px;padding:1rem;margin:1rem 0}label{display:block;font-weight:700;margin-bottom:.35rem}select,input,button{font:inherit;padding:.7rem;border-radius:8px;border:1px solid var(--line);background:#07110f;color:var(--ink)}select{min-width:min(100%,22rem)}button{background:var(--gold);color:#171207;font-weight:800;cursor:pointer}nav{display:flex;gap:.6rem;flex-wrap:wrap;margin:2rem 0}nav a,.tag{color:var(--accent);border:1px solid var(--line);border-radius:999px;padding:.25rem .65rem;text-decoration:none}.module{scroll-margin-top:1rem;margin:2.5rem 0}.module h2{text-transform:capitalize;font-family:Georgia,serif;font-size:2rem}.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(230px,1fr));gap:1rem}.card{background:linear-gradient(145deg,#142a24,#0d1916);border:1px solid var(--line);border-radius:16px;padding:1.1rem;min-height:180px;box-shadow:0 10px 28px #0005}.card h3{margin-top:0}.card.locked{opacity:.45;filter:saturate(.45)}.card.earned{border-color:var(--gold);box-shadow:0 0 0 1px #e6bf6544,0 10px 28px #0006}.card.secret{background:linear-gradient(145deg,#2d2240,#12101c)}.card.optional{border-style:dashed}.card.prestige{border-color:var(--accent)}progress{width:100%;accent-color:var(--gold)}small{display:block;color:var(--muted)}footer{color:var(--muted);padding:4rem 0 2rem}@media(max-width:620px){header{padding-top:2.5rem}select,input,button{width:100%;margin:.25rem 0}.grid{grid-template-columns:1fr}}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_and_decoding_are_safe() {
        assert_eq!(escape("<script>&\"'"), "&lt;script&gt;&amp;&quot;&#39;");
        assert_eq!(
            query_param("server=libera%20chat", "server").as_deref(),
            Some("libera chat")
        );
    }

    #[test]
    fn undiscovered_secrets_are_not_serialized() {
        let manifest = AchievementManifest {
            achievements: vec![jeeves_abi::AchievementSpec {
                id: "hidden".into(),
                name: "Secret Name".into(),
                description: "Secret rule".into(),
                stat: "secret_stat".into(),
                threshold: 99,
                optional: false,
                secret: true,
            }],
            ..Default::default()
        };
        let module = public_module("game", &manifest, None);
        let json = serde_json::to_string(&module).unwrap();
        assert!(!json.contains("Secret Name"));
        assert!(!json.contains("Secret rule"));
        assert!(!json.contains("secret_stat"));
        assert!(!json.contains("99"));
    }

    #[test]
    fn gallery_is_opt_in_escaped_network_isolated_and_hides_secret_rules() {
        let db = DbHandle::open(":memory:").unwrap();
        db.profile_ensure_blocking("net-a", "<Alice>", 1).unwrap();
        db.profile_ensure_blocking("net-b", "<Alice>", 1).unwrap();
        let profile = db
            .profile_get_blocking("net-a", "<Alice>")
            .unwrap()
            .unwrap();
        let manifest = AchievementManifest {
            version: jeeves_abi::ACHIEVEMENT_MANIFEST_VERSION,
            catalog_version: 1,
            stats: vec![jeeves_abi::AchievementStat {
                id: "wins".into(),
                description: "Wins".into(),
            }],
            achievements: vec![
                jeeves_abi::AchievementSpec {
                    id: "first".into(),
                    name: "First".into(),
                    description: "Win once".into(),
                    stat: "wins".into(),
                    threshold: 1,
                    optional: false,
                    secret: false,
                },
                jeeves_abi::AchievementSpec {
                    id: "hidden".into(),
                    name: "Hidden Crown".into(),
                    description: "Secret rule 987654".into(),
                    stat: "wins".into(),
                    threshold: 2,
                    optional: true,
                    secret: true,
                },
            ],
            prestige: Vec::new(),
        };
        db.achievement_backfill_apply_blocking(
            "net-a",
            "game",
            manifest.clone(),
            vec![jeeves_abi::AchievementSetMax {
                profile_id: profile.id.clone(),
                stat: "wins".into(),
                value: 2,
            }],
            2,
        )
        .unwrap();
        let state = PublicWebState {
            db: db.clone(),
            achievements: Arc::new(Mutex::new(HashMap::from([("game".into(), manifest)]))),
        };
        assert!(eligible_users(&state).unwrap().is_empty());
        db.achievement_public_blocking("net-a", &profile.id, true)
            .unwrap();
        let users = eligible_users(&state).unwrap();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].server, "net-a");
        let html = page(&state, "").unwrap();
        assert!(html.contains("&lt;Alice&gt;"));
        assert!(!html.contains("<Alice>"));

        let (_, _, body, _) = collection(&state, "net-a", &profile.id).unwrap();
        assert!(body.contains("Hidden Crown"));
        assert!(!body.contains("Secret rule 987654"));
        assert!(!body.contains("\"stat\""));

        db.achievement_opt_out_blocking("net-a", &profile.id, true)
            .unwrap();
        assert!(eligible_users(&state).unwrap().is_empty());
    }
}
