//! Random animal pictures for the `animal_image` host function.
//!
//! The host owns a fixed catalogue: a handful of dedicated keyless picture APIs (capybaras,
//! foxes, dogs and breeds, cats, ducks, bunnies) and, for everything else, photos from the
//! Wikimedia Commons category of the animal's scientific name. Modules only name an animal from
//! this catalogue; they never supply URLs or search terms, so user input can't steer the bot to
//! arbitrary images. Commons file lists are cached for a day to stay well within its rate limits.

mod catalog;

use jeeves_abi::{AnimalImageRequest, AnimalImageResponse};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub use catalog::CATALOG;

const MAX_RESPONSE_BYTES: u64 = 512 * 1024;
const MAX_URL_BYTES: usize = 400;
const COMMONS_ENDPOINT: &str = "https://commons.wikimedia.org/w/api.php";
const COMMONS_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const COMMONS_POOL: &str = "50";
const MAX_REQUESTS_PER_MINUTE: usize = 30;
const RANDOM_DOG_ATTEMPTS: usize = 3;

/// Where a catalogue entry's pictures come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    CapyLol,
    RandomFox,
    RandomDog,
    TheCatApi,
    RandomDuck,
    Bunnies,
    /// A dog.ceo breed path such as `retriever/golden`.
    DogBreed(&'static str),
    /// A Wikimedia Commons category, normally the species' scientific name.
    Commons(&'static str),
}

pub struct Animal {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub emoji: &'static str,
    pub source: Source,
}

/// Commons category → (when fetched, thumbnail URLs).
type CommonsCache = HashMap<&'static str, (Instant, Vec<String>)>;

static COMMONS_CACHE: OnceLock<Mutex<CommonsCache>> = OnceLock::new();
static REQUEST_LOG: OnceLock<Mutex<VecDeque<Instant>>> = OnceLock::new();

pub fn fetch(request: &AnimalImageRequest) -> AnimalImageResponse {
    if request.list {
        return AnimalImageResponse {
            kinds: CATALOG
                .iter()
                .map(|animal| animal.name.to_string())
                .collect(),
            ..AnimalImageResponse::default()
        };
    }
    let animal = if request.kind.trim().is_empty() {
        &CATALOG[fastrand::usize(..CATALOG.len())]
    } else {
        match find(&request.kind) {
            Some(animal) => animal,
            None => return failure("unknown"),
        }
    };
    if !admit_request() {
        return failure("rate_limited");
    }
    match image_url(animal.source) {
        Some(url) => AnimalImageResponse {
            kind: Some(animal.name.into()),
            emoji: Some(animal.emoji.into()),
            url: Some(url),
            ..AnimalImageResponse::default()
        },
        None => failure("unavailable"),
    }
}

/// Look an animal up by name or alias, forgiving case, spacing, hyphens, and a plural `s`.
pub fn find(kind: &str) -> Option<&'static Animal> {
    let wanted = kind
        .to_lowercase()
        .replace('-', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let matches = |candidate: &str| {
        CATALOG
            .iter()
            .find(|animal| animal.name == candidate || animal.aliases.contains(&candidate))
    };
    matches(&wanted)
        .or_else(|| wanted.strip_suffix("es").and_then(matches))
        .or_else(|| wanted.strip_suffix('s').and_then(matches))
}

fn failure(kind: &str) -> AnimalImageResponse {
    AnimalImageResponse {
        error: Some(kind.into()),
        ..AnimalImageResponse::default()
    }
}

/// A global cap on outbound picture requests, independent of per-user module cooldowns.
fn admit_request() -> bool {
    let now = Instant::now();
    let mut log = REQUEST_LOG
        .get_or_init(|| Mutex::new(VecDeque::new()))
        .lock()
        .unwrap();
    while log
        .front()
        .is_some_and(|at| now.duration_since(*at) >= Duration::from_secs(60))
    {
        log.pop_front();
    }
    if log.len() >= MAX_REQUESTS_PER_MINUTE {
        return false;
    }
    log.push_back(now);
    true
}

fn agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(8)))
            .user_agent(concat!(
                "rustjeeves-bot/",
                env!("CARGO_PKG_VERSION"),
                " (https://github.com/nylanalyn/rustjeeves)"
            ))
            .build(),
    )
}

fn get_json(agent: &ureq::Agent, url: &str) -> Option<Value> {
    let mut response = agent.get(url).call().ok()?;
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_string()
        .ok()?;
    serde_json::from_str(&body).ok()
}

fn image_url(source: Source) -> Option<String> {
    let agent = agent();
    let pick = |url: &str, pointer: &str| {
        get_json(&agent, url).and_then(|value| value.pointer(pointer)?.as_str().map(str::to_string))
    };
    let url = match source {
        Source::CapyLol => pick("https://api.capy.lol/v1/capybara?json=true", "/data/url"),
        Source::RandomFox => pick("https://randomfox.ca/floof/", "/image"),
        Source::TheCatApi => pick("https://api.thecatapi.com/v1/images/search", "/0/url"),
        Source::RandomDuck => pick("https://random-d.uk/api/v2/random", "/url"),
        Source::Bunnies => pick(
            "https://api.bunnies.io/v2/loop/random/?media=gif",
            "/media/gif",
        ),
        Source::DogBreed(breed) => pick(
            &format!("https://dog.ceo/api/breed/{breed}/images/random"),
            "/message",
        ),
        // random.dog also serves videos; ask again for a still or a GIF.
        Source::RandomDog => (0..RANDOM_DOG_ATTEMPTS)
            .filter_map(|_| pick("https://random.dog/woof.json", "/url"))
            .find(|url| is_picture(url)),
        Source::Commons(category) => commons_picture(&agent, category),
    }?;
    safe_url(&url)
}

fn is_picture(url: &str) -> bool {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    [".jpg", ".jpeg", ".png", ".gif", ".webp"]
        .iter()
        .any(|extension| path.ends_with(extension))
}

/// Accept only a bounded, single-token web URL, upgrading the few providers that still hand out
/// `http://` links (they all serve the same files over HTTPS).
fn safe_url(url: &str) -> Option<String> {
    let url = url.trim();
    let url = match url.strip_prefix("http://") {
        Some(rest) => format!("https://{rest}"),
        None => url.to_string(),
    };
    (url.starts_with("https://")
        && url.len() <= MAX_URL_BYTES
        && !url.chars().any(|ch| ch.is_whitespace() || ch.is_control()))
    .then_some(url)
}

fn commons_picture(agent: &ureq::Agent, category: &'static str) -> Option<String> {
    let cache = COMMONS_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let cached = cache
        .lock()
        .unwrap()
        .get(category)
        .filter(|(fetched, urls)| fetched.elapsed() < COMMONS_CACHE_TTL && !urls.is_empty())
        .map(|(_, urls)| urls.clone());
    let urls = match cached {
        Some(urls) => urls,
        None => {
            let urls = commons_files(agent, category)?;
            cache
                .lock()
                .unwrap()
                .insert(category, (Instant::now(), urls.clone()));
            urls
        }
    };
    (!urls.is_empty()).then(|| urls[fastrand::usize(..urls.len())].clone())
}

/// Up to 50 bitmap files filed directly in the category, as 800px-wide thumbnails.
fn commons_files(agent: &ureq::Agent, category: &str) -> Option<Vec<String>> {
    let search = format!("filetype:bitmap incategory:\"{category}\"");
    let mut response = agent
        .get(COMMONS_ENDPOINT)
        .query("action", "query")
        .query("generator", "search")
        .query("gsrsearch", &search)
        .query("gsrnamespace", "6")
        .query("gsrlimit", COMMONS_POOL)
        .query("prop", "imageinfo")
        .query("iiprop", "url|mime")
        .query("iiurlwidth", "800")
        .query("format", "json")
        .query("formatversion", "2")
        .call()
        .ok()?;
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_string()
        .ok()?;
    Some(parse_commons(&serde_json::from_str(&body).ok()?))
}

fn parse_commons(value: &Value) -> Vec<String> {
    value
        .pointer("/query/pages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|page| {
            let info = page.pointer("/imageinfo/0")?;
            let mime = info.get("mime")?.as_str()?;
            if !matches!(
                mime,
                "image/jpeg" | "image/png" | "image/gif" | "image/webp"
            ) {
                return None;
            }
            let url = info.get("thumburl").or_else(|| info.get("url"))?.as_str()?;
            // Drop Commons' tracking query string; the path alone serves the image.
            safe_url(url.split('?').next()?)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_names_aliases_and_plurals() {
        assert_eq!(find("Capybara").unwrap().name, "capybara");
        assert_eq!(find("capy").unwrap().name, "capybara");
        assert_eq!(find("capybaras").unwrap().name, "capybara");
        assert_eq!(find("  Red-Panda ").unwrap().name, "red panda");
        assert_eq!(find("foxes").unwrap().name, "fox");
        assert!(find("dragon").is_none());
        assert!(find("../../etc").is_none());
    }

    #[test]
    fn catalogue_names_are_unique_and_lowercase() {
        let mut seen = std::collections::HashSet::new();
        for animal in CATALOG {
            for name in std::iter::once(&animal.name).chain(animal.aliases) {
                assert_eq!(*name, name.to_lowercase(), "{name}");
                assert!(seen.insert(*name), "duplicate catalogue name {name}");
            }
        }
    }

    #[test]
    fn urls_are_upgraded_and_bounded() {
        assert_eq!(
            safe_url("http://api.capy.lol/v1/capybara/51").as_deref(),
            Some("https://api.capy.lol/v1/capybara/51")
        );
        assert!(safe_url("javascript:alert(1)").is_none());
        assert!(safe_url("https://a b").is_none());
        assert!(is_picture("https://random.dog/x.JPG"));
        assert!(!is_picture("https://random.dog/x.mp4"));
    }

    #[test]
    fn commons_results_keep_bitmaps_without_tracking() {
        let value = serde_json::json!({"query": {"pages": [
            {"imageinfo": [{"mime": "image/jpeg",
                "thumburl": "https://upload.wikimedia.org/a/800px-Q.jpg?utm_source=x"}]},
            {"imageinfo": [{"mime": "application/pdf", "url": "https://upload.wikimedia.org/b.pdf"}]}
        ]}});
        assert_eq!(
            parse_commons(&value),
            ["https://upload.wikimedia.org/a/800px-Q.jpg"]
        );
    }

    /// Live check against the real services: `cargo test -p jeeves live_animal -- --ignored`.
    #[test]
    #[ignore]
    fn live_animal_sources_return_pictures() {
        for kind in [
            "capybara", "fox", "dog", "cat", "duck", "bunny", "pug", "quokka", "leopard",
        ] {
            let response = fetch(&AnimalImageRequest {
                kind: kind.into(),
                list: false,
            });
            println!("{kind}: {:?} {:?}", response.url, response.error);
            assert!(response.url.is_some(), "{kind} returned no picture");
        }
    }
}
