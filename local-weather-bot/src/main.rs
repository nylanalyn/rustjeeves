use anyhow::{bail, Context, Result};
use base64::Engine;
use futures_util::StreamExt;
use irc::client::prelude::{Capability, Client, Command, Config, Response};
use irc::proto::CapSubCommand;
use jeeves_abi::WeatherLinkResult;
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[allow(dead_code)] // Jeeves' integration-key constants are unused by this standalone binary.
#[path = "../../crates/jeeves/src/weatherlink.rs"]
mod weatherlink;

const COOLDOWN: Duration = Duration::from_secs(15);
const RECONNECT_DELAY: Duration = Duration::from_secs(10);

#[derive(Clone)]
struct BotConfig {
    host: String,
    port: u16,
    tls: bool,
    nick: String,
    username: String,
    realname: String,
    sasl_account: String,
    sasl_password: String,
    channels: Vec<String>,
    weather_api_key: String,
    weather_api_secret: String,
    weather_station_id: String,
    weather_station_name: Option<String>,
}

impl BotConfig {
    fn from_env() -> Result<Self> {
        let nick = required("LOCALBOT_IRC_NICK")?;
        let channels = required("LOCALBOT_IRC_CHANNELS")?
            .split(',')
            .map(str::trim)
            .filter(|channel| !channel.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if channels.is_empty() {
            bail!("LOCALBOT_IRC_CHANNELS must contain at least one channel");
        }
        Ok(Self {
            host: required("LOCALBOT_IRC_HOST")?,
            port: optional("LOCALBOT_IRC_PORT")
                .unwrap_or_else(|| "6697".into())
                .parse()
                .context("LOCALBOT_IRC_PORT must be a number from 1 to 65535")?,
            tls: parse_bool("LOCALBOT_IRC_TLS", true)?,
            username: optional("LOCALBOT_IRC_USERNAME").unwrap_or_else(|| nick.clone()),
            realname: optional("LOCALBOT_IRC_REALNAME")
                .unwrap_or_else(|| "Local weather bot".into()),
            sasl_account: optional("LOCALBOT_IRC_SASL_ACCOUNT").unwrap_or_else(|| nick.clone()),
            sasl_password: required("LOCALBOT_IRC_SASL_PASSWORD")?,
            channels,
            weather_api_key: required("LOCALBOT_WEATHERLINK_API_KEY")?,
            weather_api_secret: required("LOCALBOT_WEATHERLINK_API_SECRET")?,
            weather_station_id: required("LOCALBOT_WEATHERLINK_STATION_ID")?,
            weather_station_name: optional("LOCALBOT_WEATHERLINK_STATION_NAME"),
            nick,
        })
    }
}

fn required(name: &str) -> Result<String> {
    optional(name).with_context(|| format!("{name} is required"))
}

fn optional(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn parse_bool(name: &str, default: bool) -> Result<bool> {
    match optional(name).as_deref() {
        None => Ok(default),
        Some("1" | "true" | "yes" | "on") => Ok(true),
        Some("0" | "false" | "no" | "off") => Ok(false),
        Some(_) => bail!("{name} must be true or false"),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = BotConfig::from_env()?;
    loop {
        eprintln!("connecting to {}:{}", config.host, config.port);
        tokio::select! {
            result = run(&config) => match result {
                Ok(()) => eprintln!("disconnected"),
                Err(error) => eprintln!("connection failed: {error:#}"),
            },
            _ = tokio::signal::ctrl_c() => return Ok(()),
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

async fn run(config: &BotConfig) -> Result<()> {
    let irc_config = Config {
        nickname: Some(config.nick.clone()),
        username: Some(config.username.clone()),
        realname: Some(config.realname.clone()),
        server: Some(config.host.clone()),
        port: Some(config.port),
        use_tls: Some(config.tls),
        channels: config.channels.clone(),
        umodes: Some("+B".into()),
        ..Config::default()
    };
    let mut client = Client::from_config(irc_config).await?;
    let mut stream = client.stream()?;
    let sender = client.sender();
    sender.send_cap_req(&[Capability::Sasl])?;
    sender.send(Command::NICK(config.nick.clone()))?;
    sender.send(Command::USER(
        config.username.clone(),
        "0".into(),
        config.realname.clone(),
    ))?;

    let payload = base64::engine::general_purpose::STANDARD.encode(format!(
        "\0{}\0{}",
        config.sasl_account, config.sasl_password
    ));
    let mut caps_ended = false;
    let mut cooldowns = HashMap::<String, Instant>::new();

    while let Some(message) = stream.next().await {
        let message = message?;
        match &message.command {
            Command::CAP(_, CapSubCommand::ACK, middle, trailing)
                if cap_list(middle, trailing)
                    .any(|cap| cap == "sasl" || cap.starts_with("sasl=")) =>
            {
                sender.send_sasl_plain()?;
            }
            Command::CAP(_, CapSubCommand::NAK, _, _) => bail!("server rejected SASL"),
            Command::AUTHENTICATE(data) if data == "+" => sender.send_sasl(&payload)?,
            Command::Response(Response::RPL_SASLSUCCESS, _)
            | Command::Response(Response::RPL_LOGGEDIN, _)
                if !caps_ended =>
            {
                sender.send(Command::CAP(None, CapSubCommand::END, None, None))?;
                caps_ended = true;
                eprintln!("authenticated as {}", config.sasl_account);
            }
            Command::Response(Response::ERR_SASLFAIL, _) => bail!("SASL authentication failed"),
            Command::PRIVMSG(target, text) => {
                let Some(arg) = local_arg(text) else {
                    continue;
                };
                let nick = message.source_nickname().unwrap_or("friend").to_owned();
                let destination = if target.starts_with(['#', '&']) {
                    target.clone()
                } else {
                    nick.clone()
                };
                if !arg.is_empty() {
                    sender.send_privmsg(
                        destination,
                        format!("The local station command takes no arguments, {nick}: !local"),
                    )?;
                    continue;
                }
                if let Some(remaining) = cooldowns
                    .get(&nick)
                    .and_then(|last| COOLDOWN.checked_sub(last.elapsed()))
                {
                    sender.send_privmsg(
                        destination,
                        format!(
                            "Give the local station {}s before asking again, {nick}.",
                            remaining.as_secs().max(1)
                        ),
                    )?;
                    continue;
                }

                let weather_config = config.clone();
                let weather = tokio::task::spawn_blocking(move || {
                    weatherlink::current(
                        Some(weather_config.weather_api_key),
                        Some(weather_config.weather_api_secret),
                        Some(weather_config.weather_station_id),
                        weather_config.weather_station_name,
                    )
                })
                .await?;
                let (reply, succeeded) = format_reply(&weather, &nick);
                sender.send_privmsg(destination, reply)?;
                if succeeded {
                    cooldowns.insert(nick, Instant::now());
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn cap_list<'a>(
    middle: &'a Option<String>,
    trailing: &'a Option<String>,
) -> impl Iterator<Item = &'a str> {
    middle
        .iter()
        .chain(trailing.iter())
        .flat_map(|caps| caps.split_whitespace())
}

fn local_arg(text: &str) -> Option<&str> {
    let mut parts = text.trim().splitn(2, char::is_whitespace);
    (parts.next() == Some("!local")).then(|| parts.next().unwrap_or("").trim())
}

fn format_reply(weather: &WeatherLinkResult, nick: &str) -> (String, bool) {
    if let Some(error) = weather.error.as_deref() {
        let message = match error {
            "not_configured" | "invalid_configuration" => {
                "The local weather station is not configured yet"
            }
            "authentication" => "The local weather station rejected its credentials",
            "station_not_found" => "The configured local weather station could not be found",
            "rate_limited" => "The local weather station is rate-limited right now",
            "no_observations" => "The local station has no current outdoor observations",
            _ => "The local weather station is not answering right now",
        };
        return (format!("{message}, {nick}."), false);
    }
    let details = format_details(weather);
    if details.is_empty() {
        return (
            format!("The local station has no current outdoor observations, {nick}."),
            false,
        );
    }
    (
        format!("Local weather at {}: {details}.", weather.station),
        true,
    )
}

fn format_details(weather: &WeatherLinkResult) -> String {
    let mut details = Vec::new();
    if let Some(temp) = weather.temp_f {
        details.push(format!("{temp:.1}°F/{:.1}°C", (temp - 32.0) * 5.0 / 9.0));
    }
    if let Some(apparent) = weather.apparent_f.filter(|apparent| {
        weather
            .temp_f
            .is_none_or(|temp| (apparent - temp).abs() >= 1.0)
    }) {
        details.push(format!(
            "feels {apparent:.1}°F/{:.1}°C",
            (apparent - 32.0) * 5.0 / 9.0
        ));
    }
    if let Some(humidity) = weather.humidity {
        details.push(format!("humidity {humidity:.0}%"));
    }
    if let Some(speed) = weather.wind_mph {
        let direction = weather
            .wind_dir_degrees
            .map(wind_direction)
            .unwrap_or("variable");
        let gust = weather
            .wind_gust_mph
            .filter(|gust| *gust > speed + 0.5)
            .map(|gust| format!(", gusting {gust:.1} mph"))
            .unwrap_or_default();
        details.push(format!(
            "wind {direction} {speed:.1} mph/{:.1} km/h{gust}",
            speed * 1.609_344
        ));
    }
    if let Some(pressure) = weather.pressure_inhg {
        details.push(format!(
            "pressure {pressure:.2} inHg/{:.0} hPa",
            pressure * 33.863_9
        ));
    }
    if let Some(rain) = weather.rain_daily_in {
        details.push(format!("rain today {rain:.2} in/{:.1} mm", rain * 25.4));
    }
    if let Some(rate) = weather.rain_rate_in_hr.filter(|rate| *rate > 0.0) {
        details.push(format!("rain rate {rate:.2} in/h/{:.1} mm/h", rate * 25.4));
    }
    details.join(", ")
}

fn wind_direction(degrees: f64) -> &'static str {
    const DIRECTIONS: [&str; 16] = [
        "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE", "S", "SSW", "SW", "WSW", "W", "WNW",
        "NW", "NNW",
    ];
    DIRECTIONS[((degrees.rem_euclid(360.0) / 22.5 + 0.5).floor() as usize) % DIRECTIONS.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_local_and_formats_observations() {
        assert_eq!(local_arg(" !local "), Some(""));
        assert_eq!(local_arg("!local extra"), Some("extra"));
        assert_eq!(local_arg("!weather"), None);

        let weather = WeatherLinkResult {
            station: "Back Garden".into(),
            temp_f: Some(68.0),
            humidity: Some(52.0),
            wind_mph: Some(5.0),
            wind_gust_mph: Some(9.0),
            wind_dir_degrees: Some(225.0),
            ..WeatherLinkResult::default()
        };
        let (reply, succeeded) = format_reply(&weather, "alice");
        assert!(succeeded);
        assert!(reply.contains("68.0°F/20.0°C"));
        assert!(reply.contains("wind SW 5.0 mph/8.0 km/h, gusting 9.0 mph"));
    }
}
