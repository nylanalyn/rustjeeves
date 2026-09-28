//! Pure formatting for weather reports: units, compass points, WMO descriptions, and the
//! compact forecast line.

use jeeves_abi::DailyWeather;

/// How a person likes their numbers. `Both` is the default and matches the original reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Units {
    Both,
    Metric,
    Imperial,
}

impl Units {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "both" | "default" => Some(Units::Both),
            "metric" | "si" | "c" | "celsius" => Some(Units::Metric),
            "imperial" | "us" | "f" | "fahrenheit" => Some(Units::Imperial),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Units::Both => "both",
            Units::Metric => "metric",
            Units::Imperial => "imperial",
        }
    }
}

pub fn c_to_f(c: f64) -> f64 {
    c * 9.0 / 5.0 + 32.0
}

pub fn mm_to_inches(mm: f64) -> f64 {
    mm / 25.4
}

pub fn kmh_to_mph(kmh: f64) -> f64 {
    kmh * 0.621_371
}

/// "18°C/64°F", "18°C", or "64°F".
pub fn temperature(celsius: f64, units: Units) -> String {
    match units {
        Units::Both => format!("{:.0}°C/{:.0}°F", celsius, c_to_f(celsius)),
        Units::Metric => format!("{celsius:.0}°C"),
        Units::Imperial => format!("{:.0}°F", c_to_f(celsius)),
    }
}

/// "11 km/h (7 mph)", "11 km/h", or "7 mph".
pub fn speed(kmh: f64, units: Units) -> String {
    match units {
        Units::Both => format!("{kmh:.0} km/h ({:.0} mph)", kmh_to_mph(kmh)),
        Units::Metric => format!("{kmh:.0} km/h"),
        Units::Imperial => format!("{:.0} mph", kmh_to_mph(kmh)),
    }
}

/// "4.5 mm (0.18 in)", "4.5 mm", or "0.18 in".
pub fn rain(mm: f64, units: Units) -> String {
    match units {
        Units::Both => format!("{mm:.1} mm ({:.2} in)", mm_to_inches(mm)),
        Units::Metric => format!("{mm:.1} mm"),
        Units::Imperial => format!("{:.2} in", mm_to_inches(mm)),
    }
}

/// The direction wind blows *from*, as one of 16 compass points.
pub fn compass(degrees: f64) -> &'static str {
    const POINTS: [&str; 16] = [
        "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE", "S", "SSW", "SW", "WSW", "W", "WNW",
        "NW", "NNW",
    ];
    let index = ((degrees.rem_euclid(360.0) + 11.25) / 22.5) as usize % 16;
    POINTS[index]
}

/// "11 km/h (7 mph) E, gusts 24 km/h (15 mph)". Gusts only when notably above the wind.
pub fn wind(kmh: f64, direction: Option<f64>, gusts_kmh: Option<f64>, units: Units) -> String {
    let mut text = speed(kmh, units);
    if let Some(direction) = direction.filter(|_| kmh >= 1.0) {
        text.push(' ');
        text.push_str(compass(direction));
    }
    if let Some(gusts) = gusts_kmh.filter(|gusts| *gusts >= kmh + 10.0) {
        text.push_str(", gusts ");
        text.push_str(&speed(gusts, units));
    }
    text
}

/// WMO weather interpretation code → short description (factual, not themed).
pub fn wmo_text(code: i64) -> &'static str {
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

/// A small symbol for a WMO code, for the compact forecast.
pub fn wmo_symbol(code: i64) -> &'static str {
    match code {
        0 => "☀",
        1 | 2 => "⛅",
        3 => "☁",
        45 | 48 => "🌫",
        51..=57 | 80..=82 => "🌦",
        61..=67 => "🌧",
        71..=77 | 85 | 86 => "🌨",
        95..=99 => "⛈",
        _ => "·",
    }
}

/// "Mon ☁ 20°/14°C 25%" — one forecast day.
pub fn forecast_day(day: &DailyWeather, units: Units) -> String {
    let short = day.weekday.chars().take(3).collect::<String>();
    let range = match (day.max_c, day.min_c) {
        (Some(max), Some(min)) => match units {
            Units::Metric => format!("{max:.0}°/{min:.0}°C"),
            Units::Imperial => format!("{:.0}°/{:.0}°F", c_to_f(max), c_to_f(min)),
            Units::Both => format!(
                "{max:.0}°/{min:.0}°C ({:.0}°/{:.0}°F)",
                c_to_f(max),
                c_to_f(min)
            ),
        },
        _ => "?".into(),
    };
    let rain = day
        .precipitation_probability
        .filter(|chance| *chance >= 10.0)
        .map(|chance| format!(" 💧{chance:.0}%"))
        .unwrap_or_default();
    format!("{short} {} {range}{rain}", wmo_symbol(day.code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions_and_codes() {
        assert_eq!(c_to_f(0.0), 32.0);
        assert_eq!(c_to_f(100.0), 212.0);
        assert!((mm_to_inches(25.4) - 1.0).abs() < f64::EPSILON);
        assert_eq!(wmo_text(0), "clear sky");
        assert_eq!(wmo_text(95), "thunderstorm");
        assert_eq!(wmo_text(12345), "unknown conditions");
    }

    #[test]
    fn units_shape_every_number() {
        assert_eq!(temperature(18.3, Units::Both), "18°C/65°F");
        assert_eq!(temperature(18.3, Units::Metric), "18°C");
        assert_eq!(temperature(18.3, Units::Imperial), "65°F");
        assert_eq!(speed(10.8, Units::Imperial), "7 mph");
        assert_eq!(rain(4.5, Units::Metric), "4.5 mm");
        assert_eq!(Units::parse("Imperial"), Some(Units::Imperial));
        assert_eq!(Units::parse("kelvin"), None);
    }

    #[test]
    fn wind_includes_direction_and_notable_gusts() {
        assert_eq!(compass(0.0), "N");
        assert_eq!(compass(90.0), "E");
        assert_eq!(compass(350.0), "N");
        assert_eq!(compass(225.0), "SW");
        assert_eq!(
            wind(10.8, Some(90.0), Some(23.8), Units::Metric),
            "11 km/h E, gusts 24 km/h"
        );
        assert_eq!(
            wind(10.8, Some(90.0), Some(14.0), Units::Metric),
            "11 km/h E"
        );
        assert_eq!(wind(0.2, Some(90.0), None, Units::Metric), "0 km/h");
    }

    #[test]
    fn forecast_days_are_compact() {
        let day = DailyWeather {
            date: "2026-09-30".into(),
            weekday: "Wednesday".into(),
            code: 63,
            max_c: Some(23.2),
            min_c: Some(17.3),
            precipitation_probability: Some(91.0),
            rain_mm: Some(4.5),
            sunrise: None,
            sunset: None,
        };
        assert_eq!(forecast_day(&day, Units::Metric), "Wed 🌧 23°/17°C 💧91%");
        assert_eq!(forecast_day(&day, Units::Imperial), "Wed 🌧 74°/63°F 💧91%");
    }
}
