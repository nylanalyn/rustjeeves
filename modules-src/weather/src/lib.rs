//! Weather module for rustjeeves.
//!
//! `!weather` reports current conditions. With no argument it uses the caller's stored location
//! (set via the users module's `!location`). `!weather <nick>` uses that user's saved location;
//! `!weather <place>` geocodes the text ad-hoc. Reads the shared profile store and uses the host
//! `geocode` / `weather` services (Open-Meteo). Replies are themed.

use extism_pdk::*;
use jeeves_abi::{
    AchievementManifest, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandSpec, Event, EventEnvelope, GeoQuery, GeoResult, KvGet, KvSet, ModuleDataDeletePlan,
    ModuleDataRequest, ModuleDataResponse, ModuleKvMutation, Profile, ProfileKey, SendMessage,
    StatIncrement, ThemeReq, WeatherQuery, WeatherResult, ACHIEVEMENT_MANIFEST_VERSION,
    COMMAND_MANIFEST_VERSION, DATA_LIFECYCLE_VERSION,
};

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn theme(input: String) -> String;
    fn profile_get(input: String) -> String;
    fn geocode(input: String) -> String;
    fn weather(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn award_stats(input: String) -> String;
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    let mut achievements = [
        ("weather_eye", "A Weather Eye", 1),
        ("prepared_anything", "Prepared for Anything", 25),
        ("resident_meteorologist", "Resident Meteorologist", 100),
    ]
    .into_iter()
    .map(|(id, name, threshold)| AchievementSpec {
        id: id.into(),
        name: name.into(),
        description: format!("Complete {threshold} successful weather lookups."),
        stat: "lookups".into(),
        threshold,
        optional: false,
        secret: false,
    })
    .collect::<Vec<_>>();
    achievements.push(AchievementSpec {
        id: "weather_ducks".into(),
        name: "Lovely Weather for Ducks".into(),
        description: "Check the weather during severe rain or a storm.".into(),
        stat: "severe_weather".into(),
        threshold: 1,
        optional: true,
        secret: true,
    });
    Ok(serde_json::to_string(&AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        catalog_version: 1,
        stats: vec![
            AchievementStat {
                id: "lookups".into(),
                description: "Successful weather lookups".into(),
            },
            AchievementStat {
                id: "severe_weather".into(),
                description: "Severe rain or storm lookups".into(),
            },
        ],
        achievements,
        prestige: Vec::new(),
    })?)
}

fn award(
    server: &str,
    profile_id: &str,
    display_name: &str,
    target: &str,
    severe: bool,
) -> Result<(), Error> {
    if profile_id.is_empty() {
        return Ok(());
    }
    let mut increments = vec![StatIncrement {
        stat: "lookups".into(),
        amount: 1,
    }];
    if severe {
        increments.push(StatIncrement {
            stat: "severe_weather".into(),
            amount: 1,
        });
    }
    unsafe {
        award_stats(serde_json::to_string(&AwardStatsRequest {
            server: server.into(),
            profile_id: profile_id.into(),
            display_name: display_name.into(),
            target: target.into(),
            increments,
            deduplication_id: None,
        })?)?;
    }
    Ok(())
}

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![CommandSpec {
            name: "weather".into(),
            aliases: vec!["w".into()],
            description: "Show weather and optional AQI for a saved or supplied location.".into(),
            usage: "!weather [location] | !weather aqi <on|off>".into(),
        }],
    })?)
}

fn reply(server: &str, target: &str, text: &str) -> Result<(), Error> {
    let req = SendMessage {
        server: server.into(),
        target: target.into(),
        text: text.into(),
    };
    unsafe { send_message(serde_json::to_string(&req)?)? };
    Ok(())
}

fn themed(key: &str, defaults: &[&str], vars: &[(&str, &str)]) -> Result<String, Error> {
    let req = ThemeReq {
        key: key.into(),
        default: defaults.iter().map(|s| s.to_string()).collect(),
        vars: vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    };
    Ok(unsafe { theme(serde_json::to_string(&req)?)? })
}

fn get_profile(server: &str, nick: &str) -> Result<Option<Profile>, Error> {
    let key = ProfileKey {
        server: server.into(),
        nick: nick.into(),
    };
    let out = unsafe { profile_get(serde_json::to_string(&key)?)? };
    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::from_str(&out)?))
    }
}

fn do_geocode(query: &str) -> Result<Option<GeoResult>, Error> {
    let out = unsafe {
        geocode(serde_json::to_string(&GeoQuery {
            query: query.into(),
        })?)?
    };
    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::from_str(&out)?))
    }
}

fn get_weather(lat: f64, lon: f64) -> Result<Option<WeatherResult>, Error> {
    let out = unsafe { weather(serde_json::to_string(&WeatherQuery { lat, lon })?)? };
    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::from_str(&out)?))
    }
}

fn encode(value: &str) -> String {
    value.bytes().map(|byte| format!("{byte:02x}")).collect()
}

fn aqi_key(server: &str, profile_id: &str) -> String {
    format!("aqi:{}:{}", encode(server), encode(profile_id))
}

fn aqi_enabled(server: &str, profile_id: &str) -> Result<bool, Error> {
    if profile_id.is_empty() {
        return Ok(true);
    }
    let value = unsafe {
        kv_get(serde_json::to_string(&KvGet {
            key: aqi_key(server, profile_id),
        })?)?
    };
    Ok(value != "off")
}

fn set_aqi_enabled(server: &str, profile_id: &str, enabled: bool) -> Result<(), Error> {
    unsafe {
        kv_set(serde_json::to_string(&KvSet {
            key: aqi_key(server, profile_id),
            value: if enabled { "on" } else { "off" }.into(),
        })?)?
    };
    Ok(())
}

fn aqi_category(aqi: f64) -> &'static str {
    match aqi.round() as i64 {
        ..=50 => "Good",
        51..=100 => "Moderate",
        101..=150 => "Unhealthy for sensitive groups",
        151..=200 => "Unhealthy",
        201..=300 => "Very unhealthy",
        _ => "Hazardous",
    }
}

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let server = env.server;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };

    let text = msg.text.trim();
    let mut parts = text.splitn(2, char::is_whitespace);
    if parts.next() != Some("!weather") {
        return Ok(());
    }
    let arg = parts.next().unwrap_or("").trim();
    let dest = if msg.is_private {
        msg.nick.as_str()
    } else {
        msg.target.as_str()
    };
    let nick = msg.nick.as_str();
    let addr = if msg.display.is_empty() {
        nick
    } else {
        msg.display.as_str()
    };

    if arg.eq_ignore_ascii_case("aqi") {
        let enabled = aqi_enabled(&server, &msg.user_id)?;
        let state = if enabled { "on" } else { "off" };
        reply(
            &server,
            dest,
            &themed(
                "weather.aqi_status",
                &["AQI is {state} for your weather reports, {user}. Use !weather aqi on|off."],
                &[("state", state), ("user", addr)],
            )?,
        )?;
        return Ok(());
    }
    let normalized_arg = arg.to_ascii_lowercase();
    if let Some(value) = normalized_arg.strip_prefix("aqi ") {
        let enabled = match value.trim() {
            "on" => true,
            "off" => false,
            _ => {
                reply(
                    &server,
                    dest,
                    &themed(
                        "weather.aqi_usage",
                        &["Choose whether AQI appears with !weather aqi on or !weather aqi off, {user}."],
                        &[("user", addr)],
                    )?,
                )?;
                return Ok(());
            }
        };
        if msg.user_id.is_empty() {
            reply(
                &server,
                dest,
                &themed(
                    "weather.aqi_profile_error",
                    &["I couldn't save that AQI preference right now, {user}."],
                    &[("user", addr)],
                )?,
            )?;
            return Ok(());
        }
        set_aqi_enabled(&server, &msg.user_id, enabled)?;
        let state = if enabled { "on" } else { "off" };
        reply(
            &server,
            dest,
            &themed(
                "weather.aqi_saved",
                &["AQI is now {state} for your weather reports, {user}."],
                &[("state", state), ("user", addr)],
            )?,
        )?;
        return Ok(());
    }

    // Resolve a (display label, lat, lon) to look up.
    let resolved: Option<(String, f64, f64)> = if arg.is_empty() {
        // Caller's own saved location.
        match get_profile(&server, nick)? {
            Some(p) if p.lat.is_some() && p.lon.is_some() => Some((
                p.location_display.unwrap_or_else(|| "your location".into()),
                p.lat.unwrap(),
                p.lon.unwrap(),
            )),
            _ => {
                reply(
                    &server,
                    dest,
                    &themed(
                        "weather_noloc",
                        &["Set your location first, {user}: !location <place>."],
                        &[("user", addr)],
                    )?,
                )?;
                return Ok(());
            }
        }
    } else {
        // A known user's saved location, else geocode the text.
        match get_profile(&server, arg)? {
            Some(p) if p.lat.is_some() && p.lon.is_some() => Some((
                p.location_display.unwrap_or_else(|| arg.into()),
                p.lat.unwrap(),
                p.lon.unwrap(),
            )),
            _ => match do_geocode(arg)? {
                Some(g) => Some((arg.to_string(), g.lat, g.lon)),
                None => {
                    reply(
                        &server,
                        dest,
                        &themed(
                            "weather_notfound",
                            &["I couldn't find '{query}', {user}."],
                            &[("user", addr), ("query", arg)],
                        )?,
                    )?;
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
            let mut vars: Vec<(&str, &str)> = vec![
                ("location", location.as_str()),
                ("desc", desc),
                ("tempc", tempc.as_str()),
                ("tempf", tempf.as_str()),
                ("feelc", feelc.as_str()),
                ("feelf", feelf.as_str()),
                ("humidity", humidity.as_str()),
                ("windk", windk.as_str()),
                ("windm", windm.as_str()),
            ];
            let show_aqi = aqi_enabled(&server, &msg.user_id)?;
            let aqi = w.us_aqi.map(|value| format!("{:.0}", value));
            let pm25 = w.pm2_5.map(|value| format!("{value:.1}"));
            let category = w.us_aqi.map(aqi_category);
            let out = if show_aqi {
                if let (Some(aqi), Some(category)) = (aqi.as_deref(), category) {
                    vars.extend([("aqi", aqi), ("aqi_category", category)]);
                    if let Some(pm25) = pm25.as_deref() {
                        vars.push(("pm25", pm25));
                        themed(
                            "weather.report_with_aqi_pm25",
                            &["Weather for {location}: {desc}, {tempc}°C/{tempf}°F (feels {feelc}°C/{feelf}°F), humidity {humidity}%, wind {windk} km/h ({windm} mph). Air quality: {aqi} US AQI ({aqi_category}), PM2.5 {pm25} µg/m³."],
                            &vars,
                        )?
                    } else {
                        themed(
                        "weather.report_with_aqi",
                            &["Weather for {location}: {desc}, {tempc}°C/{tempf}°F (feels {feelc}°C/{feelf}°F), humidity {humidity}%, wind {windk} km/h ({windm} mph). Air quality: {aqi} US AQI ({aqi_category})."],
                        &vars,
                    )?
                    }
                } else {
                    themed("report", &["Weather for {location}: {desc}, {tempc}°C/{tempf}°F (feels {feelc}°C/{feelf}°F), humidity {humidity}%, wind {windk} km/h ({windm} mph)."], &vars)?
                }
            } else {
                themed("report", &["Weather for {location}: {desc}, {tempc}°C/{tempf}°F (feels {feelc}°C/{feelf}°F), humidity {humidity}%, wind {windk} km/h ({windm} mph)."], &vars)?
            };
            reply(&server, dest, &out)?;
            award(
                &server,
                &msg.user_id,
                addr,
                dest,
                matches!(w.code, 65..=67 | 80..=82 | 95..=99),
            )?;
        }
        None => reply(
            &server,
            dest,
            &themed(
                "weather_error",
                &["The weather service isn't answering right now, {user}."],
                &[("user", addr)],
            )?,
        )?,
    }
    Ok(())
}

#[plugin_fn]
pub fn data_export(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let key = aqi_key(&request.subject.server, &request.subject.profile_id);
    let preference = request
        .entries
        .iter()
        .find(|entry| entry.key == key)
        .map(|entry| entry.value.clone());
    Ok(serde_json::to_string(&ModuleDataResponse {
        version: DATA_LIFECYCLE_VERSION,
        data: preference
            .map(|value| serde_json::json!({"aqi_preference": value}))
            .unwrap_or(serde_json::Value::Null),
    })?)
}

#[plugin_fn]
pub fn data_delete(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let key = aqi_key(&request.subject.server, &request.subject.profile_id);
    let mutations = request
        .entries
        .iter()
        .filter(|entry| entry.key == key)
        .map(|entry| ModuleKvMutation {
            key: entry.key.clone(),
            value: None,
        })
        .collect();
    Ok(serde_json::to_string(&ModuleDataDeletePlan {
        version: DATA_LIFECYCLE_VERSION,
        mutations,
    })?)
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
        assert_eq!(aqi_category(50.0), "Good");
        assert_eq!(aqi_category(101.0), "Unhealthy for sensitive groups");
        assert_eq!(aqi_category(301.0), "Hazardous");
    }
}
