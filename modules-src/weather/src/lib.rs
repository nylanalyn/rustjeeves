//! Weather module for rustjeeves.
//!
//! `!weather` reports current conditions. With no argument it uses the caller's stored location
//! (set via the users module's `!location`). `!weather <nick>` uses that user's saved location;
//! `!weather <place>` geocodes the text ad-hoc. Reads the shared profile store and uses the host
//! `geocode` / `weather` services (Open-Meteo). Replies are themed.

use extism_pdk::*;
use jeeves_abi::{Event, EventEnvelope, GeoQuery, GeoResult, Profile, ProfileKey, SendMessage, ThemeReq, WeatherQuery, WeatherResult};

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn theme(input: String) -> String;
    fn profile_get(input: String) -> String;
    fn geocode(input: String) -> String;
    fn weather(input: String) -> String;
}

fn reply(server: &str, target: &str, text: &str) -> Result<(), Error> {
    let req = SendMessage { server: server.into(), target: target.into(), text: text.into() };
    unsafe { send_message(serde_json::to_string(&req)?)? };
    Ok(())
}

fn themed(key: &str, defaults: &[&str], vars: &[(&str, &str)]) -> Result<String, Error> {
    let req = ThemeReq {
        key: key.into(),
        default: defaults.iter().map(|s| s.to_string()).collect(),
        vars: vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
    };
    Ok(unsafe { theme(serde_json::to_string(&req)?)? })
}

fn get_profile(server: &str, nick: &str) -> Result<Option<Profile>, Error> {
    let key = ProfileKey { server: server.into(), nick: nick.into() };
    let out = unsafe { profile_get(serde_json::to_string(&key)?)? };
    if out.is_empty() { Ok(None) } else { Ok(Some(serde_json::from_str(&out)?)) }
}

fn do_geocode(query: &str) -> Result<Option<GeoResult>, Error> {
    let out = unsafe { geocode(serde_json::to_string(&GeoQuery { query: query.into() })?)? };
    if out.is_empty() { Ok(None) } else { Ok(Some(serde_json::from_str(&out)?)) }
}

fn get_weather(lat: f64, lon: f64) -> Result<Option<WeatherResult>, Error> {
    let out = unsafe { weather(serde_json::to_string(&WeatherQuery { lat, lon })?)? };
    if out.is_empty() { Ok(None) } else { Ok(Some(serde_json::from_str(&out)?)) }
}

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let server = env.server;
    let Event::Message(msg) = env.event else { return Ok(()) };

    let text = msg.text.trim();
    let mut parts = text.splitn(2, char::is_whitespace);
    if parts.next() != Some("!weather") {
        return Ok(());
    }
    let arg = parts.next().unwrap_or("").trim();
    let dest = if msg.is_private { msg.nick.as_str() } else { msg.target.as_str() };
    let who = msg.nick.as_str();

    // Resolve a (display label, lat, lon) to look up.
    let resolved: Option<(String, f64, f64)> = if arg.is_empty() {
        // Caller's own saved location.
        match get_profile(&server, who)? {
            Some(p) if p.lat.is_some() && p.lon.is_some() => {
                Some((p.location_display.unwrap_or_else(|| "your location".into()), p.lat.unwrap(), p.lon.unwrap()))
            }
            _ => {
                reply(&server, dest, &themed("weather_noloc", &["Set your location first, {user}: !location <place>."], &[("user", who)])?)?;
                return Ok(());
            }
        }
    } else {
        // A known user's saved location, else geocode the text.
        match get_profile(&server, arg)? {
            Some(p) if p.lat.is_some() && p.lon.is_some() => {
                Some((p.location_display.unwrap_or_else(|| arg.into()), p.lat.unwrap(), p.lon.unwrap()))
            }
            _ => match do_geocode(arg)? {
                Some(g) => Some((arg.to_string(), g.lat, g.lon)),
                None => {
                    reply(&server, dest, &themed("weather_notfound", &["I couldn't find '{query}', {user}."], &[("user", who), ("query", arg)])?)?;
                    return Ok(());
                }
            },
        }
    };

    let (location, lat, lon) = resolved.unwrap();
    match get_weather(lat, lon)? {
        Some(w) => {
            let tempc = format!("{:.0}", w.temp_c);
            let tempf = format!("{:.0}", c_to_f(w.temp_c));
            let feelc = format!("{:.0}", w.apparent_c);
            let feelf = format!("{:.0}", c_to_f(w.apparent_c));
            let humidity = format!("{:.0}", w.humidity);
            let windk = format!("{:.0}", w.wind_kmh);
            let windm = format!("{:.0}", w.wind_kmh * 0.621_371);
            let desc = wmo_text(w.code);
            let out = themed(
                "report",
                &["Weather for {location}: {desc}, {tempc}\u{00b0}C/{tempf}\u{00b0}F (feels {feelc}\u{00b0}C/{feelf}\u{00b0}F), humidity {humidity}%, wind {windk} km/h ({windm} mph)."],
                &[
                    ("location", &location),
                    ("desc", desc),
                    ("tempc", &tempc),
                    ("tempf", &tempf),
                    ("feelc", &feelc),
                    ("feelf", &feelf),
                    ("humidity", &humidity),
                    ("windk", &windk),
                    ("windm", &windm),
                ],
            )?;
            reply(&server, dest, &out)?;
        }
        None => reply(&server, dest, &themed("weather_error", &["The weather service isn't answering right now, {user}."], &[("user", who)])?)?,
    }
    Ok(())
}

fn c_to_f(c: f64) -> f64 {
    c * 9.0 / 5.0 + 32.0
}

/// WMO weather interpretation code -> short description (factual, not themed).
fn wmo_text(code: i64) -> &'static str {
    match code {
        0 => "clear sky",
        1 => "mainly clear",
        2 => "partly cloudy",
        3 => "overcast",
        45 => "fog",
        48 => "depositing rime fog",
        51 => "light drizzle",
        53 => "moderate drizzle",
        55 => "dense drizzle",
        56 | 57 => "freezing drizzle",
        61 => "slight rain",
        63 => "moderate rain",
        65 => "heavy rain",
        66 | 67 => "freezing rain",
        71 => "slight snow",
        73 => "moderate snow",
        75 => "heavy snow",
        77 => "snow grains",
        80 => "slight rain showers",
        81 => "moderate rain showers",
        82 => "violent rain showers",
        85 | 86 => "snow showers",
        95 => "thunderstorm",
        96 | 99 => "thunderstorm with hail",
        _ => "unknown conditions",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions_and_codes() {
        assert_eq!(c_to_f(0.0), 32.0);
        assert_eq!(c_to_f(100.0), 212.0);
        assert_eq!(wmo_text(0), "clear sky");
        assert_eq!(wmo_text(95), "thunderstorm");
        assert_eq!(wmo_text(12345), "unknown conditions");
    }
}
