//! Calculator and unit-conversion module for rustjeeves.
//!
//! `!calc <expr>` — safe arithmetic: `+ - * / %`, parentheses, and a small function set
//! (`sqrt pow abs round min max`). The evaluator is a hand-rolled shunting-yard parser with no
//! external dependency and no `eval`-style attack surface.
//!
//! `!convert <amount> <unit> to <unit>` — fixed-table unit conversion across temperature, length,
//! mass, volume, speed, data, area, and time.
//!
//! Stateless: no KV, no profiles, no network. The most locked-down module in the bot.

use extism_pdk::*;
use jeeves_abi::{CommandManifest, CommandSpec, Event, EventEnvelope, SendMessage, ThemeReq, COMMAND_MANIFEST_VERSION};

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn theme(input: String) -> String;
}

const MAX_INPUT_CHARS: usize = 200;

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![
            CommandSpec {
                name: "calc".into(),
                aliases: vec!["calculate".into()],
                description: "Evaluate an arithmetic expression.".into(),
                usage: "!calc <expression>".into(),
            },
            CommandSpec {
                name: "convert".into(),
                aliases: Vec::new(),
                description: "Convert a value between units.".into(),
                usage: "!convert <amount> <unit> to <unit>".into(),
            },
        ],
    })?)
}

// ── host helpers ────────────────────────────────────────────────────────────

fn reply(server: &str, target: &str, text: &str) -> Result<(), Error> {
    unsafe {
        send_message(serde_json::to_string(&SendMessage {
            server: server.into(),
            target: target.into(),
            text: text.into(),
        })?)?
    };
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

// ── dispatch ────────────────────────────────────────────────────────────────

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    let text = msg.text.trim();
    if !text.starts_with('!') {
        return Ok(());
    }
    let dest = if msg.is_private {
        msg.nick.as_str()
    } else {
        msg.target.as_str()
    };
    let caller: &str = if msg.display.is_empty() {
        msg.nick.as_str()
    } else {
        msg.display.as_str()
    };
    let mut parts = text.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();

    match cmd {
        "!calc" | "!calculate" => handle_calc(&env.server, dest, caller, arg)?,
        "!convert" => handle_convert(&env.server, dest, caller, arg)?,
        _ => {}
    }
    Ok(())
}

fn handle_calc(server: &str, dest: &str, caller: &str, arg: &str) -> Result<(), Error> {
    if arg.is_empty() {
        return reply(server, dest, &themed("calc.usage", &["Usage: !calc <expression>. Supports + - * / % ( ), sqrt, pow, abs, round, min, max."], &[])?);
    }
    if arg.chars().count() > MAX_INPUT_CHARS {
        return reply(server, dest, &themed("calc.error", &["{user}, that expression is too long."], &[("user", caller)])?);
    }
    match expr::evaluate(arg) {
        Ok(value) => {
            let formatted = format_number(value);
            reply(
                server,
                dest,
                &themed(
                    "calc.result",
                    &["{user}: {expr} = {result}"],
                    &[("user", caller), ("expr", arg), ("result", &formatted)],
                )?,
            )
        }
        Err(err) => reply(
            server,
            dest,
            &themed(
                "calc.error",
                &["{user}, I couldn't parse that: {error}"],
                &[("user", caller), ("error", err.message())],
            )?,
        ),
    }
}

fn handle_convert(server: &str, dest: &str, caller: &str, arg: &str) -> Result<(), Error> {
    if arg.is_empty() {
        return reply(server, dest, &themed("convert.usage", &["Usage: !convert <amount> <unit> to <unit>. e.g. !convert 72 F to C"], &[])?);
    }
    match units::parse_and_convert(arg) {
        Ok(units::Conversion {
            amount,
            from,
            to,
            result,
        }) => {
            let amt = format_number(amount);
            let res = format_number(result);
            reply(
                server,
                dest,
                &themed(
                    "convert.result",
                    &["{user}: {amount} {from} = {result} {to}"],
                    &[
                        ("user", caller),
                        ("amount", &amt),
                        ("from", &from),
                        ("result", &res),
                        ("to", &to),
                    ],
                )?,
            )
        }
        Err(err) => reply(
            server,
            dest,
            &themed(
                "convert.error",
                &["{user}, {error}"],
                &[("user", caller), ("error", &err)],
            )?,
        ),
    }
}

/// Format a number for IRC: integers without a trailing `.0`, otherwise trim to a tidy precision.
fn format_number(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf" } else { "-inf" }.into();
    }
    // Avoid "-0".
    if value == 0.0 {
        return "0".into();
    }
    let rounded = (value * 10_000.0).round() / 10_000.0;
    if rounded.fract() == 0.0 && rounded.abs() < 1e16 {
        return format!("{}", rounded as i64);
    }
    format!("{rounded}")
}

// ── expression evaluator (shunting-yard) ────────────────────────────────────

mod expr {
    /// A parse/eval failure with a user-facing message.
    #[derive(Debug)]
    pub struct CalcError(&'static str);

    impl CalcError {
        pub fn message(&self) -> &'static str {
            self.0
        }
    }

    const EMPTY: CalcError = CalcError("empty expression");
    const SYNTAX: CalcError = CalcError("syntax error");
    const UNBALANCED: CalcError = CalcError("unbalanced parentheses");
    const DIV_ZERO: CalcError = CalcError("division by zero");
    const UNKNOWN_FN: CalcError = CalcError("unknown function");
    const OVERFLOW: CalcError = CalcError("result too large");
    const ARITY: CalcError = CalcError("wrong number of arguments");

    /// Bounded magnitude. Anything beyond this is rejected as overflow rather than letting f64
    /// silently produce `inf`, which would print badly and serve no real purpose for IRC math.
    const MAX_MAGNITUDE: f64 = 1e15;

    #[derive(Debug, Clone, PartialEq)]
    enum Token {
        Number(f64),
        Op(char),
        LParen,
        RParen,
        Ident(String),
        Comma,
    }

    /// Tokenize the input string into numbers, operators, parens, identifiers, and commas.
    fn tokenize(input: &str) -> Result<Vec<Token>, CalcError> {
        let chars: Vec<char> = input.chars().collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            match c {
                ' ' | '\t' => {
                    i += 1;
                }
                '0'..='9' | '.' => {
                    let start = i;
                    while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                        i += 1;
                    }
                    let slice: String = chars[start..i].iter().collect();
                    let n: f64 = slice.parse().map_err(|_| SYNTAX)?;
                    if n.abs() > MAX_MAGNITUDE {
                        return Err(OVERFLOW);
                    }
                    out.push(Token::Number(n));
                }
                '+' | '-' | '*' | '/' | '%' => {
                    out.push(Token::Op(c));
                    i += 1;
                }
                '(' => {
                    out.push(Token::LParen);
                    i += 1;
                }
                ')' => {
                    out.push(Token::RParen);
                    i += 1;
                }
                ',' => {
                    out.push(Token::Comma);
                    i += 1;
                }
                c if c.is_ascii_alphabetic() || c == '_' => {
                    let start = i;
                    while i < chars.len()
                        && (chars[i].is_ascii_alphanumeric() || chars[i] == '_')
                    {
                        i += 1;
                    }
                    let name: String = chars[start..i].iter().collect();
                    out.push(Token::Ident(name.to_ascii_lowercase()));
                }
                _ => return Err(SYNTAX),
            }
        }
        Ok(out)
    }

    /// Operator precedence for the shunting-yard algorithm. Higher binds tighter.
    fn prec(op: char) -> u8 {
        match op {
            '+' | '-' => 1,
            '*' | '/' | '%' => 2,
            _ => 0,
        }
    }

    fn is_right_assoc(_op: char) -> bool {
        false // none of our binary operators are right-associative
    }

    /// Whether an identifier names a recognized function.
    fn is_known_function(name: &str) -> bool {
        matches!(name, "sqrt" | "abs" | "round" | "pow" | "min" | "max")
    }

    /// Rewrite the token stream to insert implicit unary-minus handling and convert identifiers to
    /// function-call form, then produce RPN via shunting-yard.
    fn to_rpn(tokens: Vec<Token>) -> Result<Vec<Token>, CalcError> {
        let mut output: Vec<Token> = Vec::new();
        let mut ops: Vec<Token> = Vec::new();
        let mut prev: Option<&Token> = None;

        let top_op_prec = |ops: &Vec<Token>| {
            ops.last().and_then(|t| match t {
                Token::Op(c) => Some(prec(*c)),
                // Unary minus binds tighter than any binary operator.
                Token::Ident(name) if name == "_neg" => Some(3),
                _ => None,
            })
        };

        for tok in &tokens {
            match tok {
                Token::Number(_) => {
                    output.push(tok.clone());
                }
                Token::Ident(name) => {
                    if !is_known_function(name) {
                        return Err(UNKNOWN_FN);
                    }
                    // A function name must be followed by '('.
                    ops.push(tok.clone());
                }
                Token::Comma => {
                    // Pop until we reach the matching function's '('.
                    while let Some(top) = ops.last() {
                        let is_op = matches!(top, Token::Op(_));
                        let is_neg = matches!(top, Token::Ident(n) if n == "_neg");
                        if matches!(top, Token::LParen) {
                            break;
                        }
                        if is_op || is_neg {
                            output.push(ops.pop().unwrap());
                        } else {
                            return Err(SYNTAX);
                        }
                    }
                }
                Token::Op(op @ ('+' | '-' | '*' | '/' | '%')) => {
                    // Unary detection: operator at start, or after another operator / '(' / comma.
                    let is_unary = matches!(
                        prev,
                        None | Some(Token::Op(_)) | Some(Token::LParen) | Some(Token::Comma)
                    );
                    if is_unary && *op == '-' {
                        // Encode unary minus as a synthetic '(' + '-1' + '*' + ')'? Simpler: push a
                        // special unary-minus marker token. We use a sentinel function "_neg".
                        ops.push(Token::Ident("_neg".into()));
                    } else if is_unary && *op == '+' {
                        // Unary plus is a no-op; skip it.
                    } else {
                        while let Some(top_prec) = top_op_prec(&ops) {
                            let cur = prec(*op);
                            if top_prec > cur || (!is_right_assoc(*op) && top_prec == cur) {
                                output.push(ops.pop().unwrap());
                            } else {
                                break;
                            }
                        }
                        ops.push(Token::Op(*op));
                    }
                }
                Token::LParen => {
                    ops.push(Token::LParen);
                }
                Token::RParen => {
                    // Pop until '(', pushing operators and unary markers to output.
                    let mut found_paren = false;
                    while let Some(top) = ops.pop() {
                        match top {
                            Token::LParen => {
                                found_paren = true;
                                break;
                            }
                            Token::Op(_) => output.push(top),
                            Token::Ident(ref name) if name == "_neg" => output.push(top),
                            _ => return Err(SYNTAX),
                        }
                    }
                    if !found_paren {
                        return Err(UNBALANCED);
                    }
                    // If the '(' belonged to a function call, pop the function onto output.
                    if let Some(Token::Ident(name)) = ops.last() {
                        if name == "_neg" || is_known_function(name) {
                            output.push(ops.pop().unwrap());
                        }
                    }
                }
                Token::Op(_) => unreachable!("only arithmetic ops handled above"),
            }
            prev = Some(tok);
        }

        // Drain remaining operators.
        while let Some(top) = ops.pop() {
            match top {
                Token::Op(_) => output.push(top),
                Token::LParen | Token::RParen => return Err(UNBALANCED),
                Token::Ident(name) if is_known_function(&name) || name == "_neg" => {
                    output.push(Token::Ident(name));
                }
                _ => return Err(SYNTAX),
            }
        }
        Ok(output)
    }

    /// Evaluate an RPN token stream.
    fn eval_rpn(rpn: Vec<Token>) -> Result<f64, CalcError> {
        let mut stack: Vec<f64> = Vec::new();
        for tok in rpn {
            match tok {
                Token::Number(n) => stack.push(n),
                Token::Op(op) => {
                    // Binary operators.
                    let rhs = stack.pop().ok_or(SYNTAX)?;
                    let lhs = stack.pop().ok_or(SYNTAX)?;
                    let val = match op {
                        '+' => lhs + rhs,
                        '-' => lhs - rhs,
                        '*' => lhs * rhs,
                        '/' => {
                            if rhs == 0.0 {
                                return Err(DIV_ZERO);
                            }
                            lhs / rhs
                        }
                        '%' => {
                            if rhs == 0.0 {
                                return Err(DIV_ZERO);
                            }
                            lhs % rhs
                        }
                        _ => return Err(SYNTAX),
                    };
                    if val.is_nan() {
                        return Err(SYNTAX);
                    }
                    if val.abs() > MAX_MAGNITUDE {
                        return Err(OVERFLOW);
                    }
                    stack.push(val);
                }
                Token::Ident(name) if name == "_neg" => {
                    let v = stack.pop().ok_or(SYNTAX)?;
                    stack.push(-v);
                }
                Token::Ident(name) => {
                    // Function call. Arity is determined by the function name.
                    let (result, arity) = match name.as_str() {
                        "sqrt" => {
                            let x = stack.pop().ok_or(ARITY)?;
                            (x.sqrt(), 1)
                        }
                        "abs" => {
                            let x = stack.pop().ok_or(ARITY)?;
                            (x.abs(), 1)
                        }
                        "round" => {
                            let x = stack.pop().ok_or(ARITY)?;
                            (x.round(), 1)
                        }
                        "pow" => {
                            let exp = stack.pop().ok_or(ARITY)?;
                            let base = stack.pop().ok_or(ARITY)?;
                            // Guard against blowup before f64 produces inf.
                            if base.abs() > 1e4 || exp.abs() > 1e3 {
                                return Err(OVERFLOW);
                            }
                            (base.powf(exp), 2)
                        }
                        "min" => {
                            let b = stack.pop().ok_or(ARITY)?;
                            let a = stack.pop().ok_or(ARITY)?;
                            (a.min(b), 2)
                        }
                        "max" => {
                            let b = stack.pop().ok_or(ARITY)?;
                            let a = stack.pop().ok_or(ARITY)?;
                            (a.max(b), 2)
                        }
                        _ => return Err(UNKNOWN_FN),
                    };
                    let _ = arity; // arity validated implicitly by the pops above
                    if result.is_nan() {
                        return Err(SYNTAX);
                    }
                    if result.abs() > MAX_MAGNITUDE {
                        return Err(OVERFLOW);
                    }
                    stack.push(result);
                }
                Token::LParen | Token::RParen | Token::Comma => return Err(SYNTAX),
            }
        }
        match stack.len() {
            1 => Ok(stack[0]),
            0 => Err(EMPTY),
            _ => Err(SYNTAX),
        }
    }

    /// Evaluate an arithmetic expression. Pure, no allocations beyond the token vectors.
    pub fn evaluate(input: &str) -> Result<f64, CalcError> {
        let tokens = tokenize(input)?;
        if tokens.is_empty() {
            return Err(EMPTY);
        }
        let rpn = to_rpn(tokens)?;
        eval_rpn(rpn)
    }
}

// ── unit conversion ─────────────────────────────────────────────────────────

mod units {
    /// A successful conversion, with the original and result values plus the unit names as the
    /// caller typed them (for display).
    pub struct Conversion {
        pub amount: f64,
        pub from: String,
        pub to: String,
        pub result: f64,
    }

    /// One convertible unit. Temperature is handled separately because it is affine, not linear.
    #[derive(Clone, Copy)]
    struct UnitDef {
        aliases: &'static [&'static str],
        category: &'static str,
        /// Factor to the category's base unit (e.g. km -> 1000 meters).
        factor: f64,
    }

    /// All non-temperature units. Sorted by category. The factor converts one unit into the base
    /// unit for its category.
    const UNITS: &[UnitDef] = &[
        // Length (base: meter)
        UnitDef { aliases: &["mm", "millimeter", "millimetre"], category: "length", factor: 0.001 },
        UnitDef { aliases: &["cm", "centimeter", "centimetre"], category: "length", factor: 0.01 },
        UnitDef { aliases: &["m", "meter", "metre"], category: "length", factor: 1.0 },
        UnitDef { aliases: &["km", "kilometer", "kilometre"], category: "length", factor: 1000.0 },
        UnitDef { aliases: &["in", "inch", "inches"], category: "length", factor: 0.0254 },
        UnitDef { aliases: &["ft", "feet", "foot"], category: "length", factor: 0.3048 },
        UnitDef { aliases: &["yd", "yard", "yards"], category: "length", factor: 0.9144 },
        UnitDef { aliases: &["mi", "mile", "miles"], category: "length", factor: 1609.344 },
        // Mass (base: gram)
        UnitDef { aliases: &["mg", "milligram"], category: "mass", factor: 0.001 },
        UnitDef { aliases: &["g", "gram", "gramme"], category: "mass", factor: 1.0 },
        UnitDef { aliases: &["kg", "kilogram", "kilo"], category: "mass", factor: 1000.0 },
        UnitDef { aliases: &["oz", "ounce", "ounces"], category: "mass", factor: 28.349523125 },
        UnitDef { aliases: &["lb", "lbs", "pound", "pounds"], category: "mass", factor: 453.59237 },
        // Volume (base: milliliter)
        UnitDef { aliases: &["ml", "milliliter", "millilitre"], category: "volume", factor: 1.0 },
        UnitDef { aliases: &["l", "liter", "litre"], category: "volume", factor: 1000.0 },
        UnitDef { aliases: &["tsp", "teaspoon"], category: "volume", factor: 4.92892159375 },
        UnitDef { aliases: &["tbsp", "tablespoon"], category: "volume", factor: 14.78676478125 },
        UnitDef { aliases: &["cup", "cups"], category: "volume", factor: 236.5882365 },
        UnitDef { aliases: &["pt", "pint", "pints"], category: "volume", factor: 473.176473 },
        UnitDef { aliases: &["qt", "quart", "quarts"], category: "volume", factor: 946.352946 },
        UnitDef { aliases: &["gal", "gallon", "gallons"], category: "volume", factor: 3785.411784 },
        // Speed (base: meter/second)
        UnitDef { aliases: &["m/s", "mps"], category: "speed", factor: 1.0 },
        UnitDef { aliases: &["km/h", "kmh", "kph"], category: "speed", factor: 0.277777778 },
        UnitDef { aliases: &["mph"], category: "speed", factor: 0.44704 },
        // Data (base: byte, base-1024 — the binary convention IRC/networking people expect)
        UnitDef { aliases: &["b", "byte", "bytes"], category: "data", factor: 1.0 },
        UnitDef { aliases: &["kb", "kilobyte", "kilobytes"], category: "data", factor: 1024.0 },
        UnitDef { aliases: &["mb", "megabyte", "megabytes"], category: "data", factor: 1024.0 * 1024.0 },
        UnitDef { aliases: &["gb", "gigabyte", "gigabytes"], category: "data", factor: 1024.0 * 1024.0 * 1024.0 },
        UnitDef { aliases: &["tb", "terabyte", "terabytes"], category: "data", factor: 1_099_511_627_776.0 },
        // Area (base: square meter)
        UnitDef { aliases: &["sqm", "m2", "m^2"], category: "area", factor: 1.0 },
        UnitDef { aliases: &["sqft", "ft2", "ft^2"], category: "area", factor: 0.09290304 },
        UnitDef { aliases: &["acre", "acres"], category: "area", factor: 4046.8564224 },
        UnitDef { aliases: &["hectare", "ha"], category: "area", factor: 10000.0 },
        // Time (base: second)
        UnitDef { aliases: &["s", "sec", "second", "seconds"], category: "time", factor: 1.0 },
        UnitDef { aliases: &["min", "minute", "minutes"], category: "time", factor: 60.0 },
        UnitDef { aliases: &["h", "hr", "hour", "hours"], category: "time", factor: 3600.0 },
        UnitDef { aliases: &["day", "days"], category: "time", factor: 86400.0 },
    ];

    /// Temperature aliases — handled via the affine path, not the factor table.
    const TEMP_ALIASES: &[&str] = &["c", "celsius", "f", "fahrenheit", "k", "kelvin"];

    fn is_temp(unit: &str) -> bool {
        TEMP_ALIASES.contains(&unit.to_ascii_lowercase().as_str())
    }

    fn find_unit(name: &str) -> Option<&'static UnitDef> {
        let lower = name.to_ascii_lowercase();
        UNITS.iter().find(|u| u.aliases.contains(&lower.as_str()))
    }

    /// Convert a temperature value between C, F, and K via Celsius as the canonical unit.
    fn convert_temp(value: f64, from: &str, to: &str) -> Result<f64, String> {
        let celsius = match from.to_ascii_lowercase().as_str() {
            "c" | "celsius" => value,
            "f" | "fahrenheit" => (value - 32.0) * 5.0 / 9.0,
            "k" | "kelvin" => value - 273.15,
            _ => return Err(format!("unknown temperature unit '{from}'")),
        };
        let result = match to.to_ascii_lowercase().as_str() {
            "c" | "celsius" => celsius,
            "f" | "fahrenheit" => celsius * 9.0 / 5.0 + 32.0,
            "k" | "kelvin" => celsius + 273.15,
            _ => return Err(format!("unknown temperature unit '{to}'")),
        };
        Ok(result)
    }

    /// Parse `<amount> <from> to <to>` and perform the conversion. Returns a user-facing error
    /// string on failure.
    pub fn parse_and_convert(input: &str) -> Result<Conversion, String> {
        // We split on " to " (case-insensitive) to separate the source from the target. This lets
        // unit names contain slashes (km/h, m/s) without ambiguity.
        let lower = input.to_ascii_lowercase();
        let split = lower.find(" to ").ok_or_else(|| {
            "usage: !convert <amount> <unit> to <unit> (note the ' to ')".to_string()
        })?;
        let left = input[..split].trim();
        let right = input[split + 4..].trim();
        if right.is_empty() {
            return Err("missing target unit".into());
        }
        // left is "<amount> <unit>". Split on the first whitespace.
        let mut left_parts = left.splitn(2, char::is_whitespace);
        let amount_str = left_parts.next().unwrap_or("");
        let from_unit = left_parts.next().unwrap_or("").trim();
        if from_unit.is_empty() {
            return Err("missing source unit".into());
        }
        let amount: f64 = amount_str.parse().map_err(|_| "amount must be a number".to_string())?;

        let to_unit = right;
        let result = if is_temp(from_unit) || is_temp(to_unit) {
            if !(is_temp(from_unit) && is_temp(to_unit)) {
                return Err("temperature units only convert to other temperature units".into());
            }
            convert_temp(amount, from_unit, to_unit)?
        } else {
            let from = find_unit(from_unit)
                .ok_or_else(|| format!("unknown unit '{from_unit}'"))?;
            let to = find_unit(to_unit)
                .ok_or_else(|| format!("unknown unit '{to_unit}'"))?;
            if from.category != to.category {
                return Err(format!(
                    "can't convert {} to {} (different categories)",
                    from.category, to.category
                ));
            }
            amount * from.factor / to.factor
        };
        Ok(Conversion {
            amount,
            from: from_unit.to_string(),
            to: to_unit.to_string(),
            result,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── expression evaluator ────────────────────────────────────────────────

    #[test]
    fn precedence_multiplies_before_adding() {
        assert_eq!(expr::evaluate("2+3*4").unwrap(), 14.0);
    }

    #[test]
    fn parentheses_override_precedence() {
        assert_eq!(expr::evaluate("(2+3)*4").unwrap(), 20.0);
    }

    #[test]
    fn modulo_works() {
        assert_eq!(expr::evaluate("10%3").unwrap(), 1.0);
    }

    #[test]
    fn functions_evaluate() {
        assert_eq!(expr::evaluate("sqrt(16)").unwrap(), 4.0);
        assert_eq!(expr::evaluate("pow(2,10)").unwrap(), 1024.0);
        assert_eq!(expr::evaluate("max(3,5)").unwrap(), 5.0);
        assert_eq!(expr::evaluate("min(3,5)").unwrap(), 3.0);
        assert_eq!(expr::evaluate("abs(-7)").unwrap(), 7.0);
        assert_eq!(expr::evaluate("round(2.6)").unwrap(), 3.0);
    }

    #[test]
    fn division_by_zero_errors_not_panics() {
        assert!(expr::evaluate("1/0").is_err());
        assert!(expr::evaluate("5%0").is_err());
    }

    #[test]
    fn unbalanced_parens_error() {
        assert!(expr::evaluate("(2+3").is_err());
        assert!(expr::evaluate("2+3)").is_err());
    }

    #[test]
    fn empty_input_errors() {
        assert!(expr::evaluate("").is_err());
        assert!(expr::evaluate("   ").is_err());
    }

    #[test]
    fn garbage_errors_not_panics() {
        assert!(expr::evaluate("hello").is_err());
        assert!(expr::evaluate("2 + * 3").is_err()); // two operators in a row
        assert!(expr::evaluate("2 2").is_err()); // missing operator
    }

    #[test]
    fn overflow_is_rejected() {
        assert!(expr::evaluate("pow(9,99999)").is_err());
        assert!(expr::evaluate("9999999999999999").is_err()); // > 1e15
    }

    #[test]
    fn unary_minus_and_implicit_negation() {
        assert_eq!(expr::evaluate("-5+3").unwrap(), -2.0);
        assert_eq!(expr::evaluate("3*-2").unwrap(), -6.0);
        assert_eq!(expr::evaluate("-(2+3)").unwrap(), -5.0);
    }

    // ── unit conversion ─────────────────────────────────────────────────────

    #[test]
    fn temperature_converts_via_affine_path() {
        let c = units::parse_and_convert("72 F to C").unwrap();
        assert!((c.result - 22.2222).abs() < 0.01);
        let f = units::parse_and_convert("0 C to F").unwrap();
        assert!((f.result - 32.0).abs() < 0.001);
        let k = units::parse_and_convert("0 C to K").unwrap();
        assert!((k.result - 273.15).abs() < 0.001);
    }

    #[test]
    fn length_converts() {
        let r = units::parse_and_convert("5 km to mi").unwrap();
        assert!((r.result - 3.10686).abs() < 0.001);
    }

    #[test]
    fn mass_converts() {
        let r = units::parse_and_convert("1 kg to lb").unwrap();
        assert!((r.result - 2.20462).abs() < 0.01);
    }

    #[test]
    fn mismatched_category_errors() {
        assert!(units::parse_and_convert("5 km to lbs").is_err());
        assert!(units::parse_and_convert("10 g to cm").is_err());
    }

    #[test]
    fn volume_converts() {
        let r = units::parse_and_convert("1 cup to ml").unwrap();
        assert!((r.result - 236.588).abs() < 0.01);
    }

    #[test]
    fn data_uses_base_1024() {
        let r = units::parse_and_convert("1 mb to kb").unwrap();
        assert_eq!(r.result, 1024.0);
    }

    #[test]
    fn speed_converts() {
        let r = units::parse_and_convert("60 mph to km/h").unwrap();
        assert!((r.result - 96.5606).abs() < 0.01);
    }

    #[test]
    fn unknown_unit_errors() {
        assert!(units::parse_and_convert("5 floobs to blargs").is_err());
    }

    #[test]
    fn case_insensitive() {
        let r = units::parse_and_convert("5 KM to mi").unwrap();
        assert!((r.result - 3.10686).abs() < 0.001);
    }

    #[test]
    fn missing_to_keyword_errors() {
        assert!(units::parse_and_convert("5 km mi").is_err());
    }

    // ── output formatting ───────────────────────────────────────────────────

    #[test]
    fn integers_render_without_decimal() {
        assert_eq!(format_number(4.0), "4");
        assert_eq!(format_number(-7.0), "-7");
    }

    #[test]
    fn decimals_trim_to_four_places() {
        assert_eq!(format_number(1.23456), "1.2346");
        assert_eq!(format_number(0.5), "0.5");
    }

    #[test]
    fn zero_renders_cleanly() {
        assert_eq!(format_number(0.0), "0");
        assert_eq!(format_number(-0.0), "0");
    }
}
