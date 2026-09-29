//! Page titles for posted links (`link_title` host function).
//!
//! Fetching arbitrary URLs on behalf of chat is a classic SSRF risk, so every connection goes
//! through [`PublicResolver`]: names are resolved once, only public unicast addresses survive, and
//! the connection is made to exactly those addresses (so DNS can't be rebound between the check
//! and the connect). Redirects resolve through the same resolver. Only HTML is read, and only the
//! first part of it.

use jeeves_abi::LinkTitleResponse;
use std::collections::HashMap;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use ureq::http::Uri;
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};

const MAX_URL_CHARS: usize = 2_000;
const MAX_BODY_BYTES: u64 = 512 * 1024;
const MAX_TITLE_CHARS: usize = 200;
const CACHE_TTL: Duration = Duration::from_secs(60 * 60);
const CACHE_CAP: usize = 512;
/// Fetches per minute across every channel.
const RATE_PER_MINUTE: usize = 30;

type Cache = HashMap<String, (Instant, LinkTitleResponse)>;
static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
static RECENT: OnceLock<Mutex<Vec<Instant>>> = OnceLock::new();

pub fn fetch(url: &str) -> LinkTitleResponse {
    let url = url.trim();
    let Some(uri) = valid_url(url) else {
        return failure("invalid_url");
    };
    if let Some(host) = uri.host() {
        if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>() {
            if !is_public(ip) {
                return failure("blocked");
            }
        }
    }
    let cache = CACHE.get_or_init(Default::default);
    if let Some((at, cached)) = cache.lock().unwrap().get(url) {
        if at.elapsed() < CACHE_TTL {
            return cached.clone();
        }
    }
    if !rate_allowed() {
        return failure("rate_limited");
    }
    let result = fetch_uncached(url);
    // Provider hiccups aren't cached; answers (including "no title") are.
    if result.error.as_deref() != Some("unavailable") {
        let mut cache = cache.lock().unwrap();
        if cache.len() >= CACHE_CAP {
            cache.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
            if cache.len() >= CACHE_CAP {
                cache.clear();
            }
        }
        cache.insert(url.to_string(), (Instant::now(), result.clone()));
    }
    result
}

fn valid_url(url: &str) -> Option<Uri> {
    if url.chars().count() > MAX_URL_CHARS || url.chars().any(char::is_control) {
        return None;
    }
    let uri = url.parse::<Uri>().ok()?;
    let scheme_ok = matches!(uri.scheme_str(), Some("http" | "https"));
    let authority = uri.authority()?;
    // No credentials in links; they'd be sent to whoever the link points at.
    let clean = !authority.as_str().contains('@') && !authority.host().is_empty();
    (scheme_ok && clean).then_some(uri)
}

fn rate_allowed() -> bool {
    let mut recent = RECENT.get_or_init(Default::default).lock().unwrap();
    recent.retain(|at| at.elapsed() < Duration::from_secs(60));
    if recent.len() >= RATE_PER_MINUTE {
        return false;
    }
    recent.push(Instant::now());
    true
}

/// Only globally routable unicast addresses.
pub(crate) fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => public_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return public_v4(v4);
            }
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00 // unique local
                || (first & 0xffc0) == 0xfe80 // link local
                || (first & 0xffc0) == 0xfec0 // site local (deprecated)
                || (first == 0x2001 && v6.segments()[1] == 0x0db8) // documentation
                || v6.segments()[..6] == [0, 0, 0, 0, 0, 0]) // IPv4-compatible
        }
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || a == 0
        || (a == 100 && (64..128).contains(&b)) // carrier-grade NAT
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
        || (a == 198 && (18..20).contains(&b)) // benchmarking
        || a >= 240) // reserved
}

/// Resolves normally, then keeps only public addresses; the connector dials exactly these.
#[derive(Debug, Default)]
struct PublicResolver(DefaultResolver);

impl Resolver for PublicResolver {
    fn resolve(
        &self,
        uri: &Uri,
        config: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let resolved = self.0.resolve(uri, config, timeout)?;
        let mut public = self.empty();
        for address in resolved
            .iter()
            .filter(|address: &&SocketAddr| is_public(address.ip()))
        {
            public.push(*address);
        }
        if public.is_empty() {
            return Err(ureq::Error::HostNotFound);
        }
        Ok(public)
    }
}

fn agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(6)))
        .max_redirects(3)
        .user_agent(concat!(
            "rustjeeves-link-titles/",
            env!("CARGO_PKG_VERSION"),
            " (https://github.com/nylanalyn/rustjeeves)"
        ))
        .build();
    ureq::Agent::with_parts(config, DefaultConnector::new(), PublicResolver::default())
}

fn fetch_uncached(url: &str) -> LinkTitleResponse {
    let mut response = match agent()
        .get(url)
        .header("Accept", "text/html,application/xhtml+xml;q=0.9,*/*;q=0.1")
        .header("Accept-Language", "en;q=1.0, *;q=0.5")
        .call()
    {
        Ok(response) => response,
        Err(ureq::Error::HostNotFound) => return failure("blocked"),
        Err(_) => return failure("unavailable"),
    };
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !(content_type.starts_with("text/html") || content_type.starts_with("application/xhtml")) {
        return failure("not_html");
    }
    let mut bytes = Vec::new();
    if response
        .body_mut()
        .as_reader()
        .take(MAX_BODY_BYTES)
        .read_to_end(&mut bytes)
        .is_err()
        && bytes.is_empty()
    {
        return failure("unavailable");
    }
    let html = String::from_utf8_lossy(&bytes);
    let host = url.parse::<Uri>().ok().and_then(|uri| {
        uri.host()
            .map(|host| host.trim_start_matches("www.").to_string())
    });
    let (title, site) = extract(&html);
    match title {
        Some(title) => LinkTitleResponse {
            title: Some(title),
            site,
            host,
            error: None,
        },
        None => LinkTitleResponse {
            host,
            error: Some("no_title".into()),
            ..LinkTitleResponse::default()
        },
    }
}

/// `og:title` (or `twitter:title`), then `<title>`; plus `og:site_name`.
fn extract(html: &str) -> (Option<String>, Option<String>) {
    let head = match html.to_ascii_lowercase().find("</head>") {
        Some(end) => &html[..end],
        None => html,
    };
    // Empty tags are common ("og:title" content=""), so they never win over a real title.
    let meta = |names: &[&str]| {
        meta_tags(head).into_iter().find_map(|attributes| {
            let key = attributes
                .get("property")
                .or_else(|| attributes.get("name"))?
                .to_ascii_lowercase();
            names
                .contains(&key.as_str())
                .then(|| attributes.get("content").map(|content| clean(content)))
                .flatten()
                .filter(|content| !content.is_empty())
        })
    };
    let title = meta(&["og:title", "twitter:title"])
        .or_else(|| title_tag(head).map(|title| clean(&title)))
        .filter(|title| !title.is_empty());
    let site = meta(&["og:site_name"]);
    (title, site)
}

fn title_tag(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let open = lower.find("<title")?;
    let start = open + lower[open..].find('>')? + 1;
    let end = start + lower[start..].find("</title")?;
    Some(html[start..end].to_string())
}

/// Attributes of each `<meta …>` tag, keys lower-cased, values entity-decoded.
fn meta_tags(html: &str) -> Vec<HashMap<String, String>> {
    let lower = html.to_ascii_lowercase();
    let mut tags = Vec::new();
    let mut from = 0;
    while let Some(offset) = lower[from..].find("<meta") {
        let start = from + offset + 5;
        let Some(length) = lower[start..].find('>') else {
            break;
        };
        tags.push(attributes(&html[start..start + length]));
        from = start + length;
        if tags.len() >= 200 {
            break;
        }
    }
    tags
}

fn attributes(tag: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let chars = tag.char_indices().collect::<Vec<_>>();
    let mut index = 0;
    while index < chars.len() {
        while index < chars.len() && !chars[index].1.is_alphanumeric() {
            index += 1;
        }
        let key_start = index;
        while index < chars.len()
            && (chars[index].1.is_alphanumeric() || matches!(chars[index].1, ':' | '-' | '_'))
        {
            index += 1;
        }
        if key_start == index {
            break;
        }
        let key = tag[chars[key_start].0..chars.get(index).map_or(tag.len(), |c| c.0)]
            .to_ascii_lowercase();
        while index < chars.len() && chars[index].1.is_whitespace() {
            index += 1;
        }
        if index >= chars.len() || chars[index].1 != '=' {
            continue;
        }
        index += 1;
        while index < chars.len() && chars[index].1.is_whitespace() {
            index += 1;
        }
        let Some(&(_, quote)) = chars.get(index) else {
            break;
        };
        let (value_start, terminator) = if quote == '"' || quote == '\'' {
            index += 1;
            (index, Some(quote))
        } else {
            (index, None)
        };
        while index < chars.len()
            && match terminator {
                Some(quote) => chars[index].1 != quote,
                None => !chars[index].1.is_whitespace(),
            }
        {
            index += 1;
        }
        let start = chars.get(value_start).map_or(tag.len(), |c| c.0);
        let end = chars.get(index).map_or(tag.len(), |c| c.0);
        map.insert(key, decode_entities(&tag[start..end.max(start)]));
        index += 1;
    }
    map
}

fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        let Some(semi) = tail[..tail.len().min(12)].find(';') else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let entity = &tail[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            "ndash" => Some('–'),
            "mdash" => Some('—'),
            "hellip" => Some('…'),
            "rsquo" => Some('’'),
            "lsquo" => Some('‘'),
            "rdquo" => Some('”'),
            "ldquo" => Some('“'),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|dec| dec.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(ch) => {
                out.push(ch);
                rest = &tail[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn clean(text: &str) -> String {
    let text = decode_entities(text)
        .chars()
        .filter(|ch| !ch.is_control())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if text.chars().count() <= MAX_TITLE_CHARS {
        return text;
    }
    let cut = text.chars().take(MAX_TITLE_CHARS - 1).collect::<String>();
    format!("{}…", cut.trim_end())
}

fn failure(kind: &str) -> LinkTitleResponse {
    LinkTitleResponse {
        error: Some(kind.into()),
        ..LinkTitleResponse::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_public_addresses_are_reachable() {
        for private in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:192.168.0.1",
        ] {
            assert!(!is_public(private.parse().unwrap()), "{private}");
        }
        for public in ["1.1.1.1", "93.184.216.34", "2606:4700:4700::1111"] {
            assert!(is_public(public.parse().unwrap()), "{public}");
        }
    }

    #[test]
    fn urls_must_be_plain_http() {
        assert!(valid_url("https://example.com/page?a=1").is_some());
        assert!(valid_url("ftp://example.com/").is_none());
        assert!(valid_url("https://user:pass@example.com/").is_none());
        assert!(valid_url("javascript:alert(1)").is_none());
        assert_eq!(
            fetch("http://127.0.0.1:8080/admin").error.as_deref(),
            Some("blocked")
        );
        assert_eq!(fetch("http://[::1]/").error.as_deref(), Some("blocked"));
    }

    #[test]
    fn titles_prefer_open_graph_and_decode_entities() {
        let html = r#"<html><head><title>Fallback &amp; Co</title>
            <meta property="og:site_name" content="The Paper">
            <meta content='A &quot;Great&quot;   Story &#8211; Part&nbsp;2' property='og:title'>
            </head><body><title>not this</title></body></html>"#;
        assert_eq!(
            extract(html),
            (
                Some("A \"Great\" Story – Part 2".into()),
                Some("The Paper".into())
            )
        );
        assert_eq!(
            extract("<TITLE>\n  Plain   Title\n</TITLE>"),
            (Some("Plain Title".into()), None)
        );
        assert_eq!(extract("<p>no title here</p>"), (None, None));
        assert_eq!(
            extract(r#"<meta name="twitter:title" content=""><title>Real</title>"#).0,
            Some("Real".into()),
            "an empty meta title doesn't hide the real one"
        );
        assert!(clean(&"x".repeat(500)).ends_with('…'));
    }
}
