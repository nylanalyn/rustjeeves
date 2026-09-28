//! Safe arithmetic for `!calc`: a small recursive-descent evaluator with no `eval`.
//!
//! Grammar, loosest binding first:
//!
//! ```text
//! expr    := term (('+' | '-') term)*
//! term    := unary (('*' | '/' | '%' | implicit) unary)*     implicit: 2pi, 3(4+1)
//! unary   := ('+' | '-') unary | power
//! power   := postfix ('^' unary)?                           right-associative; -2^2 = -4
//! postfix := primary '!'*
//! primary := number | constant | function '(' args ')' | '(' expr ')'
//! ```
//!
//! Numbers accept `1e6`, `1,000,000` (outside function arguments), and `1_000`. `x`, `×`, `·`,
//! `÷`, `−`, and `**` are accepted as operators. Nesting depth and input length are bounded.

/// A user-facing parse or evaluation error.
#[derive(Debug, PartialEq, Eq)]
pub struct CalcError(pub &'static str);

const SYNTAX: CalcError = CalcError("syntax error");
const EMPTY: CalcError = CalcError("empty expression");
const UNBALANCED: CalcError = CalcError("unbalanced parentheses");
const DIV_ZERO: CalcError = CalcError("division by zero");
const TOO_LARGE: CalcError = CalcError("result too large");
const UNDEFINED: CalcError = CalcError("undefined result");
const TOO_DEEP: CalcError = CalcError("expression nested too deeply");
const NO_ANS: CalcError = CalcError("there is no previous answer yet");

const MAX_DEPTH: usize = 64;
const MAX_FACTORIAL: f64 = 170.0;

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Number(f64),
    Ident(String),
    Op(char),
    LParen,
    RParen,
    Comma,
}

/// Evaluate `input`; `ans` is the caller's previous result, if any.
pub fn evaluate(input: &str, ans: Option<f64>) -> Result<f64, CalcError> {
    let tokens = tokenize(input)?;
    if tokens.is_empty() {
        return Err(EMPTY);
    }
    let mut parser = Parser {
        tokens,
        position: 0,
        depth: 0,
        ans,
    };
    let value = parser.expr()?;
    match parser.peek() {
        None => finite(value),
        Some(Token::RParen) => Err(UNBALANCED),
        Some(_) => Err(SYNTAX),
    }
}

fn finite(value: f64) -> Result<f64, CalcError> {
    if value.is_nan() {
        Err(UNDEFINED)
    } else if value.is_infinite() {
        Err(TOO_LARGE)
    } else {
        Ok(value)
    }
}

// ── tokenizer ───────────────────────────────────────────────────────────────

fn tokenize(input: &str) -> Result<Vec<Token>, CalcError> {
    let chars = input.chars().collect::<Vec<_>>();
    let mut tokens: Vec<Token> = Vec::new();
    // For each open parenthesis: does it hold function arguments? Commas there separate
    // arguments; elsewhere a comma between digit groups is a thousands separator.
    let mut paren_is_call: Vec<bool> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        let ends_operand = matches!(
            tokens.last(),
            Some(Token::Number(_) | Token::RParen | Token::Op('!'))
        );
        match ch {
            c if c.is_whitespace() => i += 1,
            '0'..='9' | '.' => {
                let in_call = paren_is_call.last().copied().unwrap_or(false);
                let (number, next) = read_number(&chars, i, in_call)?;
                tokens.push(Token::Number(number));
                i = next;
            }
            '*' if chars.get(i + 1) == Some(&'*') => {
                tokens.push(Token::Op('^'));
                i += 2;
            }
            // `x` is multiplication between operands (2x3, 2 x 3), otherwise a name.
            'x' | 'X'
                if ends_operand
                    && !chars
                        .get(i + 1)
                        .is_some_and(|next| next.is_alphabetic() || *next == '_') =>
            {
                tokens.push(Token::Op('*'));
                i += 1;
            }
            '+' | '-' | '*' | '/' | '%' | '^' | '!' => {
                tokens.push(Token::Op(ch));
                i += 1;
            }
            '×' | '·' | '⋅' => {
                tokens.push(Token::Op('*'));
                i += 1;
            }
            '÷' => {
                tokens.push(Token::Op('/'));
                i += 1;
            }
            '−' => {
                tokens.push(Token::Op('-'));
                i += 1;
            }
            '(' => {
                paren_is_call.push(matches!(tokens.last(), Some(Token::Ident(_))));
                tokens.push(Token::LParen);
                i += 1;
            }
            ')' => {
                paren_is_call.pop();
                tokens.push(Token::RParen);
                i += 1;
            }
            ',' => {
                tokens.push(Token::Comma);
                i += 1;
            }
            'π' => {
                tokens.push(Token::Ident("pi".into()));
                i += 1;
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                tokens.push(Token::Ident(
                    chars[start..i].iter().collect::<String>().to_lowercase(),
                ));
            }
            _ => return Err(SYNTAX),
        }
    }
    Ok(tokens)
}

/// Read a number starting at `start`, returning it and the index after it.
fn read_number(chars: &[char], start: usize, in_call: bool) -> Result<(f64, usize), CalcError> {
    let mut text = String::new();
    let mut i = start;
    let mut seen_dot = false;
    while i < chars.len() {
        let ch = chars[i];
        if ch.is_ascii_digit() {
            text.push(ch);
        } else if ch == '.' && !seen_dot {
            seen_dot = true;
            text.push(ch);
        } else if ch == '_' && chars.get(i + 1).is_some_and(char::is_ascii_digit) {
            // 1_000_000
        } else if ch == ',' && !in_call && !seen_dot && is_thousands_group(chars, i + 1) {
            // 1,000,000 — exactly three digits follow, then not another digit.
        } else {
            break;
        }
        i += 1;
    }
    // Scientific notation: 1e6, 2.5E-3.
    if matches!(chars.get(i), Some('e' | 'E')) {
        let mut j = i + 1;
        let mut exponent = String::from("e");
        if matches!(chars.get(j), Some('+' | '-')) {
            exponent.push(chars[j]);
            j += 1;
        }
        let digits_start = j;
        while chars.get(j).is_some_and(char::is_ascii_digit) {
            exponent.push(chars[j]);
            j += 1;
        }
        // Only an exponent if digits follow and it isn't the start of a name (e.g. `2exp(1)`).
        if j > digits_start && !chars.get(j).is_some_and(|ch| ch.is_alphabetic()) {
            text.push_str(&exponent);
            i = j;
        }
    }
    if text == "." {
        return Err(SYNTAX);
    }
    let value = text.parse::<f64>().map_err(|_| SYNTAX)?;
    Ok((value, i))
}

fn is_thousands_group(chars: &[char], from: usize) -> bool {
    (0..3).all(|offset| chars.get(from + offset).is_some_and(char::is_ascii_digit))
        && !chars.get(from + 3).is_some_and(char::is_ascii_digit)
}

// ── parser / evaluator ──────────────────────────────────────────────────────

struct Parser {
    tokens: Vec<Token>,
    position: usize,
    depth: usize,
    ans: Option<f64>,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.position).cloned();
        self.position += 1;
        token
    }

    fn eat_op(&mut self, op: char) -> bool {
        if self.peek() == Some(&Token::Op(op)) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn enter(&mut self) -> Result<(), CalcError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            Err(TOO_DEEP)
        } else {
            Ok(())
        }
    }

    fn expr(&mut self) -> Result<f64, CalcError> {
        self.enter()?;
        let mut value = self.term()?;
        loop {
            if self.eat_op('+') {
                value = finite(value + self.term()?)?;
            } else if self.eat_op('-') {
                value = finite(value - self.term()?)?;
            } else {
                break;
            }
        }
        self.depth -= 1;
        Ok(value)
    }

    fn term(&mut self) -> Result<f64, CalcError> {
        let mut value = self.unary()?;
        loop {
            if self.eat_op('*') {
                value = finite(value * self.unary()?)?;
            } else if self.eat_op('/') {
                let divisor = self.unary()?;
                if divisor == 0.0 {
                    return Err(DIV_ZERO);
                }
                value = finite(value / divisor)?;
            } else if self.eat_op('%') {
                let divisor = self.unary()?;
                if divisor == 0.0 {
                    return Err(DIV_ZERO);
                }
                value = finite(value % divisor)?;
            } else if matches!(self.peek(), Some(Token::Ident(_) | Token::LParen)) {
                // Implicit multiplication: 2pi, 3(4+1), (1+2)(3+4).
                value = finite(value * self.unary()?)?;
            } else {
                break;
            }
        }
        Ok(value)
    }

    fn unary(&mut self) -> Result<f64, CalcError> {
        if self.eat_op('-') {
            self.enter()?;
            let value = -self.unary()?;
            self.depth -= 1;
            Ok(value)
        } else if self.eat_op('+') {
            self.enter()?;
            let value = self.unary()?;
            self.depth -= 1;
            Ok(value)
        } else {
            self.power()
        }
    }

    fn power(&mut self) -> Result<f64, CalcError> {
        let base = self.postfix()?;
        if self.eat_op('^') {
            self.enter()?;
            let exponent = self.unary()?;
            self.depth -= 1;
            return finite(base.powf(exponent));
        }
        Ok(base)
    }

    fn postfix(&mut self) -> Result<f64, CalcError> {
        let mut value = self.primary()?;
        while self.eat_op('!') {
            value = factorial(value)?;
        }
        Ok(value)
    }

    fn primary(&mut self) -> Result<f64, CalcError> {
        match self.next() {
            Some(Token::Number(value)) => Ok(value),
            Some(Token::LParen) => {
                let value = self.expr()?;
                match self.next() {
                    Some(Token::RParen) => Ok(value),
                    _ => Err(UNBALANCED),
                }
            }
            Some(Token::Ident(name)) => {
                if self.peek() == Some(&Token::LParen) {
                    self.position += 1;
                    let args = self.arguments()?;
                    call(&name, &args)
                } else {
                    self.constant(&name)
                }
            }
            Some(Token::RParen) => Err(UNBALANCED),
            None => Err(SYNTAX),
            Some(_) => Err(SYNTAX),
        }
    }

    /// Comma-separated arguments up to the closing parenthesis (already past the opening one).
    fn arguments(&mut self) -> Result<Vec<f64>, CalcError> {
        let mut args = Vec::new();
        if self.peek() == Some(&Token::RParen) {
            self.position += 1;
            return Ok(args);
        }
        loop {
            args.push(self.expr()?);
            match self.next() {
                Some(Token::Comma) => continue,
                Some(Token::RParen) => return Ok(args),
                _ => return Err(UNBALANCED),
            }
        }
    }

    fn constant(&self, name: &str) -> Result<f64, CalcError> {
        match name {
            "pi" => Ok(std::f64::consts::PI),
            "tau" => Ok(std::f64::consts::TAU),
            "e" => Ok(std::f64::consts::E),
            "phi" => Ok(1.618_033_988_749_895),
            "ans" => self.ans.ok_or(NO_ANS),
            _ => Err(CalcError("unknown name")),
        }
    }
}

fn factorial(value: f64) -> Result<f64, CalcError> {
    if value < 0.0 || value.fract() != 0.0 {
        return Err(CalcError("factorial needs a whole number"));
    }
    if value > MAX_FACTORIAL {
        return Err(TOO_LARGE);
    }
    Ok((1..=value as u64).fold(1.0, |product, n| product * n as f64))
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

fn whole(value: f64) -> Result<u64, CalcError> {
    if value.fract() != 0.0 || value.abs() > 9.0e15 {
        return Err(CalcError("gcd and lcm need whole numbers"));
    }
    Ok(value.abs() as u64)
}

fn call(name: &str, args: &[f64]) -> Result<f64, CalcError> {
    let one = |f: fn(f64) -> f64| match args {
        [x] => finite(f(*x)),
        _ => Err(CalcError("that function takes one argument")),
    };
    let two = |f: fn(f64, f64) -> f64| match args {
        [a, b] => finite(f(*a, *b)),
        _ => Err(CalcError("that function takes two arguments")),
    };
    let some = || {
        if args.is_empty() {
            Err(CalcError("that function needs at least one argument"))
        } else {
            Ok(args)
        }
    };
    match name {
        "sqrt" => one(f64::sqrt),
        "cbrt" => one(f64::cbrt),
        "abs" => one(f64::abs),
        "round" => match args {
            [x] => Ok(x.round()),
            [x, places] if (0.0..=12.0).contains(places) && places.fract() == 0.0 => {
                let scale = 10f64.powi(*places as i32);
                finite((x * scale).round() / scale)
            }
            _ => Err(CalcError("round takes a number and optional 0-12 places")),
        },
        "floor" => one(f64::floor),
        "ceil" => one(f64::ceil),
        "exp" => one(f64::exp),
        "ln" => one(f64::ln),
        "log" => match args {
            [x] => finite(x.log10()),
            [x, base] => finite(x.log(*base)),
            _ => Err(CalcError("log takes a number and optional base")),
        },
        "log10" => one(f64::log10),
        "log2" => one(f64::log2),
        "sin" => one(f64::sin),
        "cos" => one(f64::cos),
        "tan" => one(f64::tan),
        "asin" => one(f64::asin),
        "acos" => one(f64::acos),
        "atan" => one(f64::atan),
        "sind" => one(|x| x.to_radians().sin()),
        "cosd" => one(|x| x.to_radians().cos()),
        "tand" => one(|x| x.to_radians().tan()),
        "deg" => one(f64::to_degrees),
        "rad" => one(f64::to_radians),
        "pow" => two(f64::powf),
        "hypot" => two(f64::hypot),
        "atan2" => two(f64::atan2),
        "min" => Ok(some()?.iter().copied().fold(f64::INFINITY, f64::min)),
        "max" => Ok(some()?.iter().copied().fold(f64::NEG_INFINITY, f64::max)),
        "avg" | "mean" => {
            let args = some()?;
            finite(args.iter().sum::<f64>() / args.len() as f64)
        }
        "sum" => finite(some()?.iter().sum()),
        "fact" | "factorial" => match args {
            [x] => factorial(*x),
            _ => Err(CalcError("that function takes one argument")),
        },
        "gcd" | "lcm" => match args {
            [a, b] => {
                let (a, b) = (whole(*a)?, whole(*b)?);
                let divisor = gcd(a, b);
                Ok(if name == "gcd" {
                    divisor as f64
                } else {
                    // lcm(0, n) is 0; a zero divisor only happens when both are 0.
                    a.checked_div(divisor)
                        .map_or(0.0, |quotient| quotient as f64 * b as f64)
                })
            }
            _ => Err(CalcError("that function takes two arguments")),
        },
        _ => Err(CalcError("unknown function")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(input: &str) -> f64 {
        evaluate(input, None).unwrap_or_else(|error| panic!("{input}: {}", error.0))
    }

    fn close(input: &str, expected: f64) {
        let value = eval(input);
        assert!(
            (value - expected).abs() <= 1e-9 * expected.abs().max(1.0),
            "{input} = {value}, expected {expected}"
        );
    }

    #[test]
    fn precedence_and_associativity() {
        close("2+3*4", 14.0);
        close("(2+3)*4", 20.0);
        close("10-4-3", 3.0);
        close("2^10", 1024.0);
        close("2**3", 8.0);
        close("2^3^2", 512.0);
        close("-2^2", -4.0);
        close("2^-1", 0.5);
        close("7 % 3", 1.0);
        close("2 x 3", 6.0);
        close("2x3", 6.0);
        close("6 ÷ 4", 1.5);
        close("3 − 5", -2.0);
    }

    #[test]
    fn numbers_constants_and_implicit_multiplication() {
        close("1e6 / 1,000", 1000.0);
        close("1_000 + 2.5E-1", 1000.25);
        close("1,234,567", 1234567.0);
        close("2pi", 2.0 * std::f64::consts::PI);
        close("3(4+1)", 15.0);
        close("(1+2)(3+4)", 21.0);
        close("π", std::f64::consts::PI);
        close("e^1", std::f64::consts::E);
    }

    #[test]
    fn functions_check_their_arguments() {
        close("max(1, 2, 3)", 3.0);
        close("min(4,-1)", -1.0);
        close("avg(2,4,6)", 4.0);
        close("sqrt(16)", 4.0);
        close("log(1000)", 3.0);
        close("log(8, 2)", 3.0);
        close("ln(e)", 1.0);
        close("sind(30)", 0.5);
        close("round(2.71828, 2)", 2.72);
        close("5!", 120.0);
        close("gcd(12, 18)", 6.0);
        close("lcm(4, 6)", 12.0);
        close("max(1,000, 2)", 2.0);
        assert!(evaluate("pow(2)", None).is_err());
        assert!(evaluate("sqrt(1, 2)", None).is_err());
        assert!(evaluate("sqrt 16", None).is_err());
        assert!(evaluate("3 pow(2)", None).is_err());
    }

    #[test]
    fn large_numbers_are_fine_until_infinite() {
        close("2^60", 1_152_921_504_606_846_976.0);
        close("170!", (1..=170).fold(1.0, |p, n| p * n as f64));
        assert_eq!(evaluate("171!", None), Err(TOO_LARGE));
        assert_eq!(evaluate("10^400", None), Err(TOO_LARGE));
    }

    #[test]
    fn errors_are_reported_not_panicked() {
        assert_eq!(evaluate("1/0", None), Err(DIV_ZERO));
        assert_eq!(evaluate("5 % 0", None), Err(DIV_ZERO));
        assert_eq!(evaluate("sqrt(-1)", None), Err(UNDEFINED));
        assert_eq!(evaluate("(1+2", None), Err(UNBALANCED));
        assert_eq!(evaluate("1+2)", None), Err(UNBALANCED));
        assert_eq!(evaluate("", None), Err(EMPTY));
        assert_eq!(evaluate("2 3", None), Err(SYNTAX));
        assert!(evaluate("2 & 3", None).is_err());
        assert!(evaluate("foo(1)", None).is_err());
        assert!(evaluate(&"(".repeat(100), None).is_err());
        assert_eq!(evaluate(&"-".repeat(100), None), Err(TOO_DEEP));
    }

    #[test]
    fn ans_recalls_the_previous_result() {
        assert_eq!(evaluate("ans * 2", Some(21.0)), Ok(42.0));
        assert_eq!(evaluate("ans", None), Err(NO_ANS));
    }
}
