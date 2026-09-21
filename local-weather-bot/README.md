# Local weather bot

A deliberately small IRC bot that handles only `!local`. It connects with TLS by default,
authenticates using SASL PLAIN, sets user mode `+B`, joins the configured channels, and reports the
same Davis WeatherLink observations that Jeeves used to report.

## Configure and run

Set these environment variables (do not commit the real secrets):

```bash
export LOCALBOT_IRC_HOST="irc.example.net"
export LOCALBOT_IRC_PORT="6697"
export LOCALBOT_IRC_TLS="true"
export LOCALBOT_IRC_NICK="LocalWeather"
export LOCALBOT_IRC_SASL_ACCOUNT="LocalWeather"
export LOCALBOT_IRC_SASL_PASSWORD="..."
export LOCALBOT_IRC_CHANNELS="#weather"

export LOCALBOT_WEATHERLINK_API_KEY="..."
export LOCALBOT_WEATHERLINK_API_SECRET="..."
export LOCALBOT_WEATHERLINK_STATION_ID="..."
export LOCALBOT_WEATHERLINK_STATION_NAME="Back Garden"

cargo run --release --manifest-path local-weather-bot/Cargo.toml
```

`LOCALBOT_IRC_USERNAME` and `LOCALBOT_IRC_REALNAME` are optional. Separate multiple channels with
commas. The process reconnects ten seconds after a disconnect; use your normal service manager to
restart it after a crash or machine reboot.

