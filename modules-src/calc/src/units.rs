//! `!convert` parsing and physical-unit conversion.
//!
//! Input is `<amount> <unit> [<amount> <unit> …] <separator> <unit>`, where the separator is
//! `to`, `into`, `as`, `->`, `→`, `=`, or `in` (the last ` in ` is used, so `10 in in cm` works).
//! Several amounts of one kind add up (`5 ft 10 in`, `1 h 30 min`, `6 st 3 lb`). When either side
//! isn't a physical unit, the request is handed back as money for the host to resolve
//! (`50 usd to gbp`, `$20 in quid`, `0.5 btc to eur`), so `50 pounds to kg` stays mass while
//! `50 pounds to euros` is currency.

/// Which gallon/quart/pint/fluid ounce plain names mean.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PintSystem {
    Us,
    Uk,
}

#[derive(Debug, PartialEq)]
pub enum Plan {
    Physical {
        /// The amounts as the user wrote them, e.g. "5 ft 10 in".
        from: String,
        result: f64,
        to: String,
    },
    Money {
        amount: f64,
        from: String,
        to: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Category {
    Length,
    Mass,
    Volume,
    Speed,
    Data,
    DataRate,
    Area,
    Time,
    Pressure,
    Energy,
    Power,
}

struct UnitDef {
    names: &'static [&'static str],
    category: Category,
    /// Size of one unit in the category's base unit (m, g, ml, m/s, byte, bit/s, m², s, Pa, J, W).
    factor: f64,
}

const fn unit(names: &'static [&'static str], category: Category, factor: f64) -> UnitDef {
    UnitDef {
        names,
        category,
        factor,
    }
}

use Category::*;

const US_FL_OZ: f64 = 29.573_529_562_5;
const UK_FL_OZ: f64 = 28.413_062_5;
const US_PINT: f64 = 473.176_473;
const UK_PINT: f64 = 568.261_25;
const US_QUART: f64 = 946.352_946;
const UK_QUART: f64 = 1_136.522_5;
const US_GALLON: f64 = 3_785.411_784;
const UK_GALLON: f64 = 4_546.09;

/// Case-insensitive units, matched after normalization (lowercase, no spaces or dots). Plain
/// pint/quart/gallon/fl oz are resolved separately through the pint setting.
const UNITS: &[UnitDef] = &[
    // Length (m)
    unit(&["nm", "nanometer", "nanometre"], Length, 1e-9),
    unit(
        &["µm", "um", "micron", "micrometer", "micrometre"],
        Length,
        1e-6,
    ),
    unit(&["mm", "millimeter", "millimetre"], Length, 0.001),
    unit(&["cm", "centimeter", "centimetre"], Length, 0.01),
    unit(&["m", "meter", "metre"], Length, 1.0),
    unit(&["km", "kilometer", "kilometre", "klick"], Length, 1000.0),
    unit(&["in", "inch", "inches", "\""], Length, 0.0254),
    unit(&["ft", "foot", "feet", "'"], Length, 0.3048),
    unit(&["yd", "yard"], Length, 0.9144),
    unit(&["mi", "mile"], Length, 1609.344),
    unit(&["nmi", "nauticalmile"], Length, 1852.0),
    unit(&["au", "astronomicalunit"], Length, 149_597_870_700.0),
    unit(&["ly", "lightyear"], Length, 9_460_730_472_580_800.0),
    unit(&["pc", "parsec"], Length, 30_856_775_814_913_673.0),
    unit(&["furlong"], Length, 201.168),
    unit(&["fathom"], Length, 1.8288),
    // Mass (g)
    unit(&["mg", "milligram", "milligramme"], Mass, 0.001),
    unit(&["g", "gram", "gramme"], Mass, 1.0),
    unit(&["kg", "kilogram", "kilogramme", "kilo"], Mass, 1000.0),
    unit(&["t", "tonne", "metricton"], Mass, 1_000_000.0),
    unit(&["oz", "ounce"], Mass, 28.349_523_125),
    unit(&["lb", "lbs", "pound"], Mass, 453.592_37),
    unit(&["st", "stone"], Mass, 6_350.293_18),
    unit(&["uston", "shortton"], Mass, 907_184.74),
    unit(&["ukton", "longton"], Mass, 1_016_046.908_8),
    unit(&["ct", "carat"], Mass, 0.2),
    // Volume (ml)
    unit(&["ml", "milliliter", "millilitre", "cc"], Volume, 1.0),
    unit(&["cl", "centiliter", "centilitre"], Volume, 10.0),
    unit(&["dl", "deciliter", "decilitre"], Volume, 100.0),
    unit(&["l", "liter", "litre"], Volume, 1000.0),
    unit(
        &["m3", "m^3", "cubicmeter", "cubicmetre"],
        Volume,
        1_000_000.0,
    ),
    unit(&["tsp", "teaspoon"], Volume, 4.928_921_593_75),
    unit(&["tbsp", "tablespoon"], Volume, 14.786_764_781_25),
    unit(&["cup"], Volume, 236.588_236_5),
    unit(&["usfloz", "usfluidounce"], Volume, US_FL_OZ),
    unit(
        &["ukfloz", "imperialfloz", "impfloz", "ukfluidounce"],
        Volume,
        UK_FL_OZ,
    ),
    unit(&["uspint", "uspt"], Volume, US_PINT),
    unit(
        &["ukpint", "imperialpint", "imppint", "ukpt"],
        Volume,
        UK_PINT,
    ),
    unit(&["usquart", "usqt"], Volume, US_QUART),
    unit(&["ukquart", "imperialquart", "ukqt"], Volume, UK_QUART),
    unit(&["usgallon", "usgal"], Volume, US_GALLON),
    unit(
        &["ukgallon", "imperialgallon", "impgal", "ukgal"],
        Volume,
        UK_GALLON,
    ),
    unit(&["barrel", "bbl"], Volume, 158_987.294_928),
    // Speed (m/s)
    unit(
        &["m/s", "mps", "meterpersecond", "metrepersecond"],
        Speed,
        1.0,
    ),
    unit(
        &[
            "km/h",
            "kmh",
            "kph",
            "kmph",
            "kilometerperhour",
            "kilometreperhour",
        ],
        Speed,
        1.0 / 3.6,
    ),
    unit(&["mph", "mileperhour"], Speed, 0.447_04),
    unit(&["kn", "kt", "knot"], Speed, 0.514_444_444_444),
    unit(&["ft/s", "fps"], Speed, 0.3048),
    unit(&["mach"], Speed, 343.0),
    unit(&["speedoflight"], Speed, 299_792_458.0),
    // Data (bytes). Lowercase and uppercase abbreviations mean bytes; `Mb`/`Gb` bits live below.
    unit(&["byte", "b"], Data, 1.0),
    unit(&["bit"], Data, 0.125),
    unit(&["kb", "kilobyte"], Data, 1024.0),
    unit(&["mb", "megabyte"], Data, 1_048_576.0),
    unit(&["gb", "gigabyte"], Data, 1_073_741_824.0),
    unit(&["tb", "terabyte"], Data, 1_099_511_627_776.0),
    unit(&["pb", "petabyte"], Data, 1_125_899_906_842_624.0),
    unit(&["kib", "kibibyte"], Data, 1024.0),
    unit(&["mib", "mebibyte"], Data, 1_048_576.0),
    unit(&["gib", "gibibyte"], Data, 1_073_741_824.0),
    unit(&["tib", "tebibyte"], Data, 1_099_511_627_776.0),
    unit(&["kbit", "kilobit"], Data, 125.0),
    unit(&["mbit", "megabit"], Data, 125_000.0),
    unit(&["gbit", "gigabit"], Data, 125_000_000.0),
    // Data rate (bits per second)
    unit(&["bps", "bit/s"], DataRate, 1.0),
    unit(&["kbps", "kbit/s"], DataRate, 1e3),
    unit(&["kb/s", "kbyte/s"], DataRate, 8.0 * 1024.0),
    unit(&["mbps", "mbit/s"], DataRate, 1e6),
    unit(&["gbps", "gbit/s"], DataRate, 1e9),
    unit(&["mb/s", "mbyte/s"], DataRate, 8.0 * 1_048_576.0),
    unit(&["gb/s", "gbyte/s"], DataRate, 8.0 * 1_073_741_824.0),
    // Area (m²)
    unit(&["mm2", "mm^2", "sqmm"], Area, 1e-6),
    unit(&["cm2", "cm^2", "sqcm"], Area, 1e-4),
    unit(
        &["m2", "m^2", "sqm", "squaremeter", "squaremetre"],
        Area,
        1.0,
    ),
    unit(
        &["km2", "km^2", "sqkm", "squarekilometer", "squarekilometre"],
        Area,
        1e6,
    ),
    unit(&["in2", "in^2", "sqin", "squareinch"], Area, 0.000_645_16),
    unit(
        &["ft2", "ft^2", "sqft", "squarefoot", "squarefeet"],
        Area,
        0.092_903_04,
    ),
    unit(&["yd2", "yd^2", "sqyd", "squareyard"], Area, 0.836_127_36),
    unit(
        &["mi2", "mi^2", "sqmi", "squaremile"],
        Area,
        2_589_988.110_336,
    ),
    unit(&["acre"], Area, 4_046.856_422_4),
    unit(&["ha", "hectare"], Area, 10_000.0),
    // Time (s)
    unit(&["ns", "nanosecond"], Time, 1e-9),
    unit(&["µs", "us", "microsecond"], Time, 1e-6),
    unit(&["ms", "millisecond"], Time, 0.001),
    unit(&["s", "sec", "second"], Time, 1.0),
    unit(&["min", "minute"], Time, 60.0),
    unit(&["h", "hr", "hour"], Time, 3600.0),
    unit(&["d", "day"], Time, 86_400.0),
    unit(&["wk", "week"], Time, 604_800.0),
    unit(&["fortnight"], Time, 1_209_600.0),
    unit(&["mo", "month"], Time, 2_629_746.0),
    unit(&["yr", "year"], Time, 31_556_952.0),
    unit(&["decade"], Time, 315_569_520.0),
    unit(&["century"], Time, 3_155_695_200.0),
    // Pressure (Pa)
    unit(&["pa", "pascal"], Pressure, 1.0),
    unit(&["hpa", "hectopascal"], Pressure, 100.0),
    unit(&["kpa", "kilopascal"], Pressure, 1000.0),
    unit(&["mbar", "millibar"], Pressure, 100.0),
    unit(&["bar"], Pressure, 100_000.0),
    unit(&["atm", "atmosphere"], Pressure, 101_325.0),
    unit(&["psi"], Pressure, 6_894.757_293_168),
    unit(&["mmhg", "torr"], Pressure, 133.322_387_415),
    unit(&["inhg"], Pressure, 3_386.388_666_67),
    // Energy (J)
    unit(&["j", "joule"], Energy, 1.0),
    unit(&["kj", "kilojoule"], Energy, 1000.0),
    unit(&["mj", "megajoule"], Energy, 1e6),
    unit(&["cal", "calorie"], Energy, 4.184),
    unit(&["kcal", "kilocalorie"], Energy, 4184.0),
    unit(&["wh", "watthour"], Energy, 3600.0),
    unit(&["kwh", "kilowatthour"], Energy, 3_600_000.0),
    unit(&["btu"], Energy, 1_055.055_852_62),
    unit(&["ev", "electronvolt"], Energy, 1.602_176_634e-19),
    // Power (W)
    unit(&["w", "watt"], Power, 1.0),
    unit(&["kw", "kilowatt"], Power, 1000.0),
    unit(&["mw", "megawatt"], Power, 1e6),
    unit(&["hp", "horsepower"], Power, 745.699_871_582_27),
    unit(&["ps", "metrichorsepower"], Power, 735.498_75),
];

/// Case-sensitive bit abbreviations checked before the table (`Mb` = megabit, `MB` = megabyte).
const BIT_UNITS: &[(&str, f64)] = &[
    ("Kb", 125.0),
    ("Mb", 125_000.0),
    ("Gb", 125_000_000.0),
    ("Tb", 125_000_000_000.0),
];

/// Case-sensitive bit rates (`Mb/s` = megabits per second, `MB/s` = megabytes per second).
const BIT_RATE_UNITS: &[(&str, f64)] = &[("Kb/s", 1e3), ("Mb/s", 1e6), ("Gb/s", 1e9)];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Temp {
    C,
    F,
    K,
}

#[derive(Clone, Copy, Debug)]
enum Resolved {
    Unit { category: Category, factor: f64 },
    Temp(Temp),
}

/// Lowercase, drop spaces, dots, and a trailing plural `s`/`es` when that finds a unit.
fn resolve(name: &str, pints: PintSystem) -> Option<Resolved> {
    let trimmed = name.trim().trim_end_matches('.');
    if let Some((_, factor)) = BIT_UNITS.iter().find(|(unit, _)| *unit == trimmed) {
        return Some(Resolved::Unit {
            category: Data,
            factor: *factor,
        });
    }
    if let Some((_, factor)) = BIT_RATE_UNITS.iter().find(|(unit, _)| *unit == trimmed) {
        return Some(Resolved::Unit {
            category: DataRate,
            factor: *factor,
        });
    }
    let key = trimmed
        .to_lowercase()
        .replace(['.', ' '], "")
        .replace('²', "2")
        .replace('³', "3");
    if key.is_empty() {
        return None;
    }
    let lookup = |key: &str| resolve_exact(key, pints);
    lookup(&key)
        .or_else(|| key.strip_suffix("es").and_then(lookup))
        .or_else(|| key.strip_suffix('s').and_then(lookup))
}

fn resolve_exact(key: &str, pints: PintSystem) -> Option<Resolved> {
    let temp = match key {
        "c" | "°c" | "degc" | "celsius" | "centigrade" | "degreesc" | "degreescelsius" => {
            Some(Temp::C)
        }
        "f" | "°f" | "degf" | "fahrenheit" | "degreesf" | "degreesfahrenheit" => Some(Temp::F),
        "k" | "°k" | "kelvin" | "kelvins" => Some(Temp::K),
        _ => None,
    };
    if let Some(temp) = temp {
        return Some(Resolved::Temp(temp));
    }
    let volume = |us: f64, uk: f64| {
        Some(Resolved::Unit {
            category: Volume,
            factor: match pints {
                PintSystem::Us => us,
                PintSystem::Uk => uk,
            },
        })
    };
    match key {
        "pint" | "pt" => return volume(US_PINT, UK_PINT),
        "quart" | "qt" => return volume(US_QUART, UK_QUART),
        "gallon" | "gal" => return volume(US_GALLON, UK_GALLON),
        "floz" | "fluidounce" => return volume(US_FL_OZ, UK_FL_OZ),
        _ => {}
    }
    UNITS
        .iter()
        .find(|unit| unit.names.contains(&key))
        .map(|unit| Resolved::Unit {
            category: unit.category,
            factor: unit.factor,
        })
}

fn to_celsius(value: f64, unit: Temp) -> f64 {
    match unit {
        Temp::C => value,
        Temp::F => (value - 32.0) * 5.0 / 9.0,
        Temp::K => value - 273.15,
    }
}

fn from_celsius(value: f64, unit: Temp) -> f64 {
    match unit {
        Temp::C => value,
        Temp::F => value * 9.0 / 5.0 + 32.0,
        Temp::K => value + 273.15,
    }
}

const SEPARATORS: &[&str] = &[" to ", " into ", " as ", "->", "→", "=", " in "];

/// Split `input` into (amounts, target) at the separator. ` in ` is tried last and split at its
/// final occurrence, since `in` is also inches.
fn split_target(input: &str) -> Option<(&str, &str)> {
    let lower = input.to_lowercase();
    for separator in SEPARATORS {
        if let Some(index) = lower.rfind(separator) {
            // Byte offsets match: lowercasing these separators never changes their length, and
            // `lower` only differs from `input` in letter case.
            if lower.len() == input.len() {
                let (left, right) = (&input[..index], &input[index + separator.len()..]);
                if !left.trim().is_empty() && !right.trim().is_empty() {
                    return Some((left.trim(), right.trim()));
                }
            }
        }
    }
    None
}

/// Currency symbols that may lead an amount: `$50`, `£3.20`.
const LEADING_SYMBOLS: &[char] = &['$', '£', '€', '¥', '₹', '₩', '₺', '₪', '₱'];

/// One `<amount><unit>` group of the left-hand side.
#[derive(Debug, PartialEq)]
struct Segment {
    amount: f64,
    unit: String,
}

/// Parse "5 ft 10 in", "72F", "$50", "1,000 km" into segments. A new segment starts at each
/// whitespace-separated word that begins with a number.
fn segments(left: &str) -> Result<Vec<Segment>, String> {
    let mut out: Vec<Segment> = Vec::new();
    for word in left.split_whitespace() {
        let (symbol, rest) = match word.chars().next() {
            Some(first) if LEADING_SYMBOLS.contains(&first) => {
                (Some(first), &word[first.len_utf8()..])
            }
            _ => (None, word),
        };
        let number_len = numeric_prefix_len(rest);
        if number_len > 0 {
            let amount = parse_amount(&rest[..number_len])
                .ok_or_else(|| format!("'{word}' is not a number I can read"))?;
            let mut unit = rest[number_len..].to_string();
            if let Some(symbol) = symbol {
                unit = format!("{symbol}{unit}");
            }
            out.push(Segment { amount, unit });
        } else if let Some(last) = out.last_mut() {
            if !last.unit.is_empty() {
                last.unit.push(' ');
            }
            last.unit.push_str(word);
        } else {
            return Err("start with an amount, e.g. !convert 5 ft to m".into());
        }
    }
    if out.is_empty() {
        return Err("start with an amount, e.g. !convert 5 ft to m".into());
    }
    Ok(out)
}

fn numeric_prefix_len(word: &str) -> usize {
    let bytes = word.as_bytes();
    let mut end = 0;
    let mut seen_digit = false;
    while end < bytes.len() {
        let byte = bytes[end];
        let ok = byte.is_ascii_digit()
            || byte == b'.'
            || byte == b'_'
            || (byte == b',' && seen_digit)
            || ((byte == b'-' || byte == b'+') && end == 0)
            || ((byte == b'e' || byte == b'E')
                && seen_digit
                && bytes.get(end + 1).is_some_and(|next| {
                    next.is_ascii_digit()
                        || ((*next == b'-' || *next == b'+')
                            && bytes.get(end + 2).is_some_and(u8::is_ascii_digit))
                }));
        if !ok {
            break;
        }
        if byte.is_ascii_digit() {
            seen_digit = true;
        }
        if byte == b'e' || byte == b'E' {
            // Consume the exponent sign with the `e`.
            if matches!(bytes.get(end + 1), Some(b'-' | b'+')) {
                end += 1;
            }
        }
        end += 1;
    }
    if seen_digit {
        end
    } else {
        0
    }
}

fn parse_amount(text: &str) -> Option<f64> {
    let cleaned = text.replace([',', '_'], "");
    cleaned
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
}

/// Plan a `!convert` request.
pub fn plan(input: &str, pints: PintSystem) -> Result<Plan, String> {
    let Some((left, target)) = split_target(input) else {
        return Err(
            "use !convert <amount> <unit> to <unit>, e.g. !convert 5 ft 10 in to cm".into(),
        );
    };
    let parts = segments(left)?;
    let target_resolved = resolve(target, pints);
    let resolved = parts
        .iter()
        .map(|part| resolve(&part.unit, pints))
        .collect::<Vec<_>>();

    // Any side that isn't a physical unit goes to the host as money (single amounts only).
    if target_resolved.is_none() || resolved.iter().any(Option::is_none) {
        if parts.len() == 1 {
            return Ok(Plan::Money {
                amount: parts[0].amount,
                from: parts[0].unit.clone(),
                to: target.to_string(),
            });
        }
        let unknown = parts
            .iter()
            .zip(&resolved)
            .find(|(_, resolved)| resolved.is_none())
            .map(|(part, _)| part.unit.as_str())
            .unwrap_or(target);
        return Err(format!("I don't know the unit '{unknown}'"));
    }
    let target_resolved = target_resolved.expect("checked above");
    let from_text = parts
        .iter()
        .map(|part| format!("{} {}", crate::format_number(part.amount), part.unit))
        .collect::<Vec<_>>()
        .join(" ");

    match target_resolved {
        Resolved::Temp(target_unit) => {
            let [Some(Resolved::Temp(source_unit))] = resolved.as_slice() else {
                return Err("temperatures only convert to other temperatures".into());
            };
            let celsius = to_celsius(parts[0].amount, *source_unit);
            if celsius < -273.15 - 1e-9 {
                return Err("that's colder than absolute zero".into());
            }
            Ok(Plan::Physical {
                from: from_text,
                result: from_celsius(celsius, target_unit),
                to: target.to_string(),
            })
        }
        Resolved::Unit {
            category,
            factor: target_factor,
        } => {
            let mut base = 0.0;
            for (part, resolved) in parts.iter().zip(&resolved) {
                match resolved {
                    Some(Resolved::Unit {
                        category: part_category,
                        factor,
                    }) if *part_category == category => base += part.amount * factor,
                    Some(Resolved::Temp(_)) => {
                        return Err("temperatures only convert to other temperatures".into())
                    }
                    _ => {
                        return Err(format!(
                            "'{}' and '{target}' measure different things",
                            part.unit
                        ))
                    }
                }
            }
            let result = base / target_factor;
            if !result.is_finite() {
                return Err("that result is too large".into());
            }
            Ok(Plan::Physical {
                from: from_text,
                result,
                to: target.to_string(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn physical(input: &str) -> f64 {
        match plan(input, PintSystem::Us) {
            Ok(Plan::Physical { result, .. }) => result,
            other => panic!("{input}: {other:?}"),
        }
    }

    fn close(input: &str, expected: f64) {
        let value = physical(input);
        assert!(
            (value - expected).abs() <= 1e-6 * expected.abs().max(1.0),
            "{input} = {value}, expected {expected}"
        );
    }

    #[test]
    fn separators_and_compact_forms() {
        close("5 km to m", 5000.0);
        close("5 km into m", 5000.0);
        close("5km -> m", 5000.0);
        close("5 km = m", 5000.0);
        close("5 ft in m", 1.524);
        close("10 in in cm", 25.4);
        close("72F to C", 22.222_222_2);
        close("1,000 m to km", 1.0);
        close("1e3 g to kg", 1.0);
    }

    #[test]
    fn compound_amounts_add_up() {
        close("5 ft 10 in to cm", 177.8);
        close("1 h 30 min to s", 5400.0);
        close("6 st 3 lb to kg", 39.462_536_19);
        assert!(plan("5 ft 3 kg to m", PintSystem::Us).is_err());
    }

    #[test]
    fn temperatures_accept_symbols_and_respect_absolute_zero() {
        close("100 °C to °F", 212.0);
        close("0 degC to K", 273.15);
        close("-40 celsius to fahrenheit", -40.0);
        assert!(plan("-500 C to K", PintSystem::Us).is_err());
        assert!(plan("10 C to m", PintSystem::Us).is_err());
    }

    #[test]
    fn pints_follow_the_setting_but_explicit_names_win() {
        close("1 pint to ml", US_PINT);
        match plan("1 pint to ml", PintSystem::Uk) {
            Ok(Plan::Physical { result, .. }) => assert!((result - UK_PINT).abs() < 1e-6),
            other => panic!("{other:?}"),
        }
        close("1 uk pint to ml", UK_PINT);
        close("2 imperial gallons to l", 9.09218);
        close("8 fl oz to ml", 8.0 * US_FL_OZ);
    }

    #[test]
    fn bits_and_bytes_are_distinguished() {
        close("100 Mb to MB", 100.0 * 125_000.0 / 1_048_576.0);
        close("1 GB to MB", 1024.0);
        close("100 mbps to MB/s", 100e6 / (8.0 * 1_048_576.0));
        close("100 Mb/s to Mbps", 100.0);
    }

    #[test]
    fn more_categories() {
        close("30 psi to bar", 2.068_427);
        close("2000 kcal to kj", 8368.0);
        close("100 hp to kw", 74.569_987);
        close("30 knots to mph", 34.523_4);
        close("2 weeks to days", 14.0);
        close("1 acre to sq m", 4_046.856_422_4);
        close("3 miles to km", 4.828_032);
    }

    #[test]
    fn non_physical_units_become_money() {
        assert_eq!(
            plan("50 usd to gbp", PintSystem::Us),
            Ok(Plan::Money {
                amount: 50.0,
                from: "usd".into(),
                to: "gbp".into()
            })
        );
        assert_eq!(
            plan("$20 in quid", PintSystem::Us),
            Ok(Plan::Money {
                amount: 20.0,
                from: "$".into(),
                to: "quid".into()
            })
        );
        assert!(matches!(
            plan("50 pounds to euros", PintSystem::Us),
            Ok(Plan::Money { .. })
        ));
        close("50 pounds to kg", 22.679_618_5);
        assert!(plan("5 ft 3 zorbs to m", PintSystem::Us).is_err());
    }

    #[test]
    fn malformed_input_is_explained() {
        assert!(plan("5 km", PintSystem::Us).is_err());
        assert!(plan("km to m", PintSystem::Us).is_err());
        assert!(plan("5 km to", PintSystem::Us).is_err());
    }
}
