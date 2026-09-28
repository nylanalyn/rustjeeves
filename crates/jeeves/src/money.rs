//! Currency and cryptocurrency prices for the `money` capability.
//!
//! Fiat rates are the ECB's working-day reference rates via Frankfurter (keyless), cached for six
//! hours. Crypto prices come from CoinGecko's keyless API, cached for five minutes per coin and
//! rate-gated well under its public limit. Everything converts through EUR, so crypto ↔ fiat and
//! crypto ↔ crypto work with no extra calls. Modules pass whatever the user typed ("$", "quid",
//! "btc", "euros"); resolution happens here.

use jeeves_abi::{
    CryptoQuoteRequest, CryptoQuoteResponse, MoneyConvertRequest, MoneyConvertResponse,
};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const FIAT_ENDPOINT: &str = "https://api.frankfurter.dev/v1/latest?base=EUR";
const COINGECKO: &str = "https://api.coingecko.com/api/v3";
const FIAT_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const CRYPTO_TTL: Duration = Duration::from_secs(5 * 60);
const SEARCH_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_COINGECKO_CALLS_PER_MINUTE: usize = 15;
const MAX_RESPONSE_BYTES: u64 = 256 * 1024;
const MAX_INPUT_CHARS: usize = 40;

/// Popular coins resolved without a search call: (ticker, CoinGecko id, display name).
const COINS: &[(&str, &str, &str)] = &[
    ("btc", "bitcoin", "Bitcoin"),
    ("eth", "ethereum", "Ethereum"),
    ("usdt", "tether", "Tether"),
    ("bnb", "binancecoin", "BNB"),
    ("sol", "solana", "Solana"),
    ("usdc", "usd-coin", "USD Coin"),
    ("xrp", "ripple", "XRP"),
    ("doge", "dogecoin", "Dogecoin"),
    ("ada", "cardano", "Cardano"),
    ("trx", "tron", "TRON"),
    ("ton", "the-open-network", "Toncoin"),
    ("avax", "avalanche-2", "Avalanche"),
    ("shib", "shiba-inu", "Shiba Inu"),
    ("dot", "polkadot", "Polkadot"),
    ("link", "chainlink", "Chainlink"),
    ("bch", "bitcoin-cash", "Bitcoin Cash"),
    ("ltc", "litecoin", "Litecoin"),
    ("xlm", "stellar", "Stellar"),
    ("xmr", "monero", "Monero"),
    ("atom", "cosmos", "Cosmos"),
    ("etc", "ethereum-classic", "Ethereum Classic"),
    ("near", "near", "NEAR"),
    ("uni", "uniswap", "Uniswap"),
    ("pepe", "pepe", "Pepe"),
    ("sui", "sui", "Sui"),
    ("apt", "aptos", "Aptos"),
    ("arb", "arbitrum", "Arbitrum"),
    ("op", "optimism", "Optimism"),
    ("fil", "filecoin", "Filecoin"),
    ("hbar", "hedera-hashgraph", "Hedera"),
    ("icp", "internet-computer", "Internet Computer"),
    ("kas", "kaspa", "Kaspa"),
    ("algo", "algorand", "Algorand"),
    ("xtz", "tezos", "Tezos"),
    ("zec", "zcash", "Zcash"),
    ("dai", "dai", "Dai"),
];

/// Everyday words and symbols for fiat currencies.
const FIAT_WORDS: &[(&str, &str)] = &[
    ("$", "USD"),
    ("us$", "USD"),
    ("dollar", "USD"),
    ("dollars", "USD"),
    ("buck", "USD"),
    ("bucks", "USD"),
    ("€", "EUR"),
    ("euro", "EUR"),
    ("euros", "EUR"),
    ("£", "GBP"),
    ("pound", "GBP"),
    ("pounds", "GBP"),
    ("quid", "GBP"),
    ("sterling", "GBP"),
    ("¥", "JPY"),
    ("yen", "JPY"),
    ("yuan", "CNY"),
    ("rmb", "CNY"),
    ("renminbi", "CNY"),
    ("₹", "INR"),
    ("rupee", "INR"),
    ("rupees", "INR"),
    ("₩", "KRW"),
    ("won", "KRW"),
    ("franc", "CHF"),
    ("francs", "CHF"),
    ("c$", "CAD"),
    ("a$", "AUD"),
    ("nz$", "NZD"),
    ("peso", "MXN"),
    ("pesos", "MXN"),
    ("real", "BRL"),
    ("reais", "BRL"),
    ("zloty", "PLN"),
    ("krona", "SEK"),
    ("kronor", "SEK"),
    ("krone", "NOK"),
    ("kroner", "NOK"),
    ("rand", "ZAR"),
    ("lira", "TRY"),
    ("₺", "TRY"),
    ("forint", "HUF"),
    ("koruna", "CZK"),
    ("shekel", "ILS"),
    ("shekels", "ILS"),
    ("₪", "ILS"),
    ("baht", "THB"),
    ("ringgit", "MYR"),
    ("rupiah", "IDR"),
    ("₱", "PHP"),
];

#[derive(Clone, Debug, PartialEq)]
enum Asset {
    Fiat(String),
    Crypto {
        id: String,
        symbol: String,
        name: String,
    },
}

impl Asset {
    fn code(&self) -> String {
        match self {
            Asset::Fiat(code) => code.clone(),
            Asset::Crypto { symbol, .. } => symbol.to_uppercase(),
        }
    }
}

struct FiatRates {
    fetched: Instant,
    date: String,
    /// Units per 1 EUR, including EUR itself.
    per_eur: HashMap<String, f64>,
}

#[derive(Clone, Copy)]
struct CryptoPrice {
    fetched: Instant,
    usd: f64,
    change_24h: Option<f64>,
}

#[derive(Default)]
struct State {
    fiat: Option<FiatRates>,
    crypto: HashMap<String, CryptoPrice>,
    searches: HashMap<String, (Instant, Option<Asset>)>,
    coingecko_calls: VecDeque<Instant>,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE
        .get_or_init(|| Mutex::new(State::default()))
        .lock()
        .unwrap()
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

#[derive(Debug, PartialEq)]
enum FetchError {
    Unavailable,
    RateLimited,
}

fn get_json(url: &str) -> Result<Value, FetchError> {
    let mut response = match agent().get(url).call() {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(429)) => return Err(FetchError::RateLimited),
        Err(_) => return Err(FetchError::Unavailable),
    };
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_string()
        .map_err(|_| FetchError::Unavailable)?;
    serde_json::from_str(&body).map_err(|_| FetchError::Unavailable)
}

fn admit_coingecko() -> bool {
    let now = Instant::now();
    let mut state = state();
    while state
        .coingecko_calls
        .front()
        .is_some_and(|at| now.duration_since(*at) >= Duration::from_secs(60))
    {
        state.coingecko_calls.pop_front();
    }
    if state.coingecko_calls.len() >= MAX_COINGECKO_CALLS_PER_MINUTE {
        return false;
    }
    state.coingecko_calls.push_back(now);
    true
}

/// ECB rates per EUR and their date, refreshed every six hours.
fn fiat_rates() -> Result<(String, HashMap<String, f64>), FetchError> {
    if let Some(rates) = state()
        .fiat
        .as_ref()
        .filter(|rates| rates.fetched.elapsed() < FIAT_TTL)
    {
        return Ok((rates.date.clone(), rates.per_eur.clone()));
    }
    let value = get_json(FIAT_ENDPOINT)?;
    let (date, per_eur) = parse_fiat(&value).ok_or(FetchError::Unavailable)?;
    state().fiat = Some(FiatRates {
        fetched: Instant::now(),
        date: date.clone(),
        per_eur: per_eur.clone(),
    });
    Ok((date, per_eur))
}

fn parse_fiat(value: &Value) -> Option<(String, HashMap<String, f64>)> {
    let date = value.get("date")?.as_str()?.to_string();
    let mut per_eur = value
        .get("rates")?
        .as_object()?
        .iter()
        .filter_map(|(code, rate)| {
            let rate = rate.as_f64()?;
            (code.len() == 3 && rate.is_finite() && rate > 0.0).then(|| (code.clone(), rate))
        })
        .collect::<HashMap<_, _>>();
    per_eur.insert("EUR".into(), 1.0);
    (per_eur.len() > 1).then_some((date, per_eur))
}

fn crypto_price(id: &str) -> Result<CryptoPrice, FetchError> {
    if let Some(price) = state()
        .crypto
        .get(id)
        .filter(|price| price.fetched.elapsed() < CRYPTO_TTL)
    {
        return Ok(*price);
    }
    if !admit_coingecko() {
        return Err(FetchError::RateLimited);
    }
    let value = get_json(&format!(
        "{COINGECKO}/simple/price?ids={id}&vs_currencies=usd&include_24hr_change=true"
    ))?;
    let entry = value.get(id).ok_or(FetchError::Unavailable)?;
    let usd = entry
        .get("usd")
        .and_then(Value::as_f64)
        .filter(|usd| usd.is_finite() && *usd > 0.0)
        .ok_or(FetchError::Unavailable)?;
    let price = CryptoPrice {
        fetched: Instant::now(),
        usd,
        change_24h: entry.get("usd_24h_change").and_then(Value::as_f64),
    };
    state().crypto.insert(id.to_string(), price);
    Ok(price)
}

fn normalize(input: &str) -> String {
    input
        .trim()
        .trim_end_matches('.')
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Resolve without network calls: fiat symbols/names/codes and the curated coin list.
fn resolve_known(input: &str, fiat: &HashMap<String, f64>) -> Option<Asset> {
    let key = normalize(input);
    if let Some((_, code)) = FIAT_WORDS.iter().find(|(word, _)| *word == key) {
        return Some(Asset::Fiat((*code).into()));
    }
    let upper = key.to_uppercase();
    if fiat.contains_key(&upper) {
        return Some(Asset::Fiat(upper));
    }
    COINS
        .iter()
        .find(|(ticker, id, name)| *ticker == key || *id == key || name.to_lowercase() == key)
        .map(|(ticker, id, name)| Asset::Crypto {
            id: (*id).into(),
            symbol: (*ticker).into(),
            name: (*name).into(),
        })
}

/// Resolve a less common coin through CoinGecko search (cached a day, including misses).
fn resolve_by_search(input: &str) -> Result<Option<Asset>, FetchError> {
    let key = normalize(input);
    if key.is_empty()
        || key.chars().count() > 20
        || !key
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == ' ')
    {
        return Ok(None);
    }
    if let Some((_, asset)) = state()
        .searches
        .get(&key)
        .filter(|(at, _)| at.elapsed() < SEARCH_TTL)
    {
        return Ok(asset.clone());
    }
    if !admit_coingecko() {
        return Err(FetchError::RateLimited);
    }
    let value = get_json(&format!(
        "{COINGECKO}/search?query={}",
        key.replace(' ', "%20")
    ))?;
    let asset = best_search_match(&key, &value);
    let mut state = state();
    if state.searches.len() > 500 {
        state.searches.clear();
    }
    state.searches.insert(key, (Instant::now(), asset.clone()));
    Ok(asset)
}

/// The best-ranked coin whose ticker or name matches exactly.
fn best_search_match(key: &str, value: &Value) -> Option<Asset> {
    value
        .get("coins")?
        .as_array()?
        .iter()
        .filter(|coin| {
            let symbol = coin.get("symbol").and_then(Value::as_str).unwrap_or("");
            let name = coin.get("name").and_then(Value::as_str).unwrap_or("");
            symbol.eq_ignore_ascii_case(key) || name.eq_ignore_ascii_case(key)
        })
        .min_by_key(|coin| {
            coin.get("market_cap_rank")
                .and_then(Value::as_u64)
                .unwrap_or(u64::MAX)
        })
        .and_then(|coin| {
            Some(Asset::Crypto {
                id: coin.get("id")?.as_str()?.to_string(),
                symbol: coin.get("symbol")?.as_str()?.to_lowercase(),
                name: coin.get("name")?.as_str()?.to_string(),
            })
        })
}

fn resolve(input: &str, fiat: &HashMap<String, f64>) -> Result<Option<Asset>, FetchError> {
    if input.chars().count() > MAX_INPUT_CHARS {
        return Ok(None);
    }
    match resolve_known(input, fiat) {
        Some(asset) => Ok(Some(asset)),
        None => resolve_by_search(input),
    }
}

/// Value of one unit of `asset` in EUR.
fn eur_value(asset: &Asset, per_eur: &HashMap<String, f64>) -> Result<f64, FetchError> {
    match asset {
        Asset::Fiat(code) => per_eur
            .get(code)
            .map(|rate| 1.0 / rate)
            .ok_or(FetchError::Unavailable),
        Asset::Crypto { id, .. } => {
            let usd_per_eur = per_eur.get("USD").ok_or(FetchError::Unavailable)?;
            Ok(crypto_price(id)?.usd / usd_per_eur)
        }
    }
}

fn error_name(error: FetchError) -> String {
    match error {
        FetchError::Unavailable => "unavailable".into(),
        FetchError::RateLimited => "rate_limited".into(),
    }
}

pub fn convert(request: &MoneyConvertRequest) -> MoneyConvertResponse {
    let failure = |error: &str| MoneyConvertResponse {
        error: Some(error.into()),
        ..MoneyConvertResponse::default()
    };
    if !request.amount.is_finite() {
        return failure("unavailable");
    }
    let (date, per_eur) = match fiat_rates() {
        Ok(rates) => rates,
        Err(error) => return failure(&error_name(error)),
    };
    let from = match resolve(&request.from, &per_eur) {
        Ok(Some(asset)) => asset,
        Ok(None) => return failure("unknown_from"),
        Err(error) => return failure(&error_name(error)),
    };
    let to = match resolve(&request.to, &per_eur) {
        Ok(Some(asset)) => asset,
        Ok(None) => return failure("unknown_to"),
        Err(error) => return failure(&error_name(error)),
    };
    let rate = match (eur_value(&from, &per_eur), eur_value(&to, &per_eur)) {
        (Ok(from_eur), Ok(to_eur)) if to_eur > 0.0 => from_eur / to_eur,
        (Err(error), _) | (_, Err(error)) => return failure(&error_name(error)),
        _ => return failure("unavailable"),
    };
    // Every conversion runs through ECB rates; crypto prices add CoinGecko.
    let mut sources = vec!["ECB".to_string()];
    if [&from, &to]
        .iter()
        .any(|asset| matches!(asset, Asset::Crypto { .. }))
    {
        sources.push("CoinGecko".to_string());
    }
    let result = request.amount * rate;
    if !result.is_finite() {
        return failure("unavailable");
    }
    MoneyConvertResponse {
        result: Some(result),
        from: Some(from.code()),
        to: Some(to.code()),
        rate: Some(rate),
        sources,
        as_of: Some(date),
        error: None,
    }
}

pub fn quote(request: &CryptoQuoteRequest) -> CryptoQuoteResponse {
    let failure = |error: &str| CryptoQuoteResponse {
        error: Some(error.into()),
        ..CryptoQuoteResponse::default()
    };
    // Fiat rates are optional here: they only add the GBP/EUR prices.
    let per_eur = fiat_rates().map(|(_, rates)| rates).unwrap_or_default();
    let asset = match resolve(&request.symbol, &per_eur) {
        Ok(Some(asset @ Asset::Crypto { .. })) => asset,
        Ok(_) => return failure("unknown"),
        Err(error) => return failure(&error_name(error)),
    };
    let Asset::Crypto { id, symbol, name } = &asset else {
        return failure("unknown");
    };
    let price = match crypto_price(id) {
        Ok(price) => price,
        Err(error) => return failure(&error_name(error)),
    };
    let in_currency = |code: &str| {
        let usd_per_eur = per_eur.get("USD")?;
        Some(price.usd / usd_per_eur * per_eur.get(code)?)
    };
    CryptoQuoteResponse {
        symbol: Some(symbol.to_uppercase()),
        name: Some(name.clone()),
        price_usd: Some(price.usd),
        change_24h: price.change_24h,
        price_gbp: in_currency("GBP"),
        price_eur: in_currency("EUR"),
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rates() -> HashMap<String, f64> {
        parse_fiat(&serde_json::json!({
            "date": "2026-09-28",
            "rates": {"USD": 1.1378, "GBP": 0.85785, "JPY": 178.5}
        }))
        .unwrap()
        .1
    }

    #[test]
    fn resolves_codes_symbols_names_and_coins() {
        let rates = rates();
        assert_eq!(
            resolve_known("usd", &rates),
            Some(Asset::Fiat("USD".into()))
        );
        assert_eq!(resolve_known("$", &rates), Some(Asset::Fiat("USD".into())));
        assert_eq!(
            resolve_known("Quid", &rates),
            Some(Asset::Fiat("GBP".into()))
        );
        assert_eq!(
            resolve_known("EUR", &rates),
            Some(Asset::Fiat("EUR".into()))
        );
        assert!(matches!(
            resolve_known("BTC", &rates),
            Some(Asset::Crypto { ref id, .. }) if id == "bitcoin"
        ));
        assert!(matches!(
            resolve_known("dogecoin", &rates),
            Some(Asset::Crypto { ref symbol, .. }) if symbol == "doge"
        ));
        assert_eq!(resolve_known("zzz", &rates), None);
    }

    #[test]
    fn fiat_values_go_through_eur() {
        let rates = rates();
        let usd = eur_value(&Asset::Fiat("USD".into()), &rates).unwrap();
        let gbp = eur_value(&Asset::Fiat("GBP".into()), &rates).unwrap();
        let usd_to_gbp = usd / gbp;
        assert!((usd_to_gbp - 0.85785 / 1.1378).abs() < 1e-9);
    }

    #[test]
    fn search_prefers_the_best_ranked_exact_match() {
        let value = serde_json::json!({"coins": [
            {"id": "fake-doge", "symbol": "DOGE", "name": "Fake", "market_cap_rank": 900},
            {"id": "dogecoin", "symbol": "DOGE", "name": "Dogecoin", "market_cap_rank": 9},
            {"id": "doge-killer", "symbol": "LEASH", "name": "Doge Killer", "market_cap_rank": 300}
        ]});
        assert_eq!(
            best_search_match("doge", &value),
            Some(Asset::Crypto {
                id: "dogecoin".into(),
                symbol: "doge".into(),
                name: "Dogecoin".into()
            })
        );
        assert_eq!(best_search_match("nothing", &value), None);
    }

    /// Live check: `cargo test -p jeeves live_money -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_money_converts_and_quotes() {
        for (amount, from, to) in [
            (50.0, "usd", "gbp"),
            (0.5, "btc", "eur"),
            (100.0, "£", "doge"),
        ] {
            let response = convert(&MoneyConvertRequest {
                amount,
                from: from.into(),
                to: to.into(),
            });
            println!("{amount} {from} -> {to}: {response:?}");
            assert!(response.result.is_some());
        }
        let quote = quote(&CryptoQuoteRequest {
            symbol: "eth".into(),
        });
        println!("{quote:?}");
        assert!(quote.price_usd.is_some());
    }
}
