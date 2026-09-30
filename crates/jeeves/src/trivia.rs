//! Open Trivia DB (opentdb.com) for the `trivia_fetch` host function. Only this one service is
//! ever contacted; a session token keeps questions from repeating until the pool runs dry, and
//! requests are spaced at least five seconds apart as the service asks.

use jeeves_abi::{FetchedQuestion, TriviaFetchRequest, TriviaFetchResponse};
use serde_json::Value;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const API: &str = "https://opentdb.com/api.php";
const TOKEN_API: &str = "https://opentdb.com/api_token.php";
const MIN_INTERVAL: Duration = Duration::from_secs(5);
const MAX_RESPONSE_BYTES: u64 = 256 * 1024;
const MAX_FIELD_CHARS: usize = 300;

struct Session {
    token: Option<String>,
    last_request: Option<Instant>,
}

static SESSION: Mutex<Session> = Mutex::new(Session {
    token: None,
    last_request: None,
});

pub fn fetch(request: &TriviaFetchRequest) -> TriviaFetchResponse {
    let failure = |error: &str| TriviaFetchResponse {
        questions: Vec::new(),
        error: Some(error.into()),
    };
    let amount = request.amount.clamp(1, 50);
    let mut session = SESSION.lock().unwrap();
    if session
        .last_request
        .is_some_and(|last| last.elapsed() < MIN_INTERVAL)
    {
        return failure("rate_limited");
    }
    if session.token.is_none() {
        session.last_request = Some(Instant::now());
        session.token = get_json(&format!("{TOKEN_API}?command=request"))
            .ok()
            .and_then(|value| value.get("token")?.as_str().map(str::to_string));
        // The token request counts toward the rate limit; ask for questions next time.
        return failure("rate_limited");
    }
    session.last_request = Some(Instant::now());
    let token = session.token.clone().unwrap_or_default();
    let url = format!("{API}?amount={amount}&encode=url3986&token={token}");
    let Ok(value) = get_json(&url) else {
        return failure("unavailable");
    };
    match value.get("response_code").and_then(Value::as_i64) {
        Some(0) => {}
        // Token spent (every question seen) or unknown: start a fresh session next time.
        Some(3 | 4) => {
            session.token = None;
            return failure("empty");
        }
        Some(5) => return failure("rate_limited"),
        _ => return failure("empty"),
    }
    let questions = parse(&value);
    if questions.is_empty() {
        return failure("empty");
    }
    TriviaFetchResponse {
        questions,
        error: None,
    }
}

fn get_json(url: &str) -> Result<Value, ()> {
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(6)))
            .build(),
    );
    let mut response = agent.get(url).call().map_err(|_| ())?;
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_string()
        .map_err(|_| ())?;
    serde_json::from_str(&body).map_err(|_| ())
}

fn parse(value: &Value) -> Vec<FetchedQuestion> {
    let text = |item: &Value, key: &str| item.get(key).and_then(Value::as_str).map(decode);
    value
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let kind = text(item, "type")?;
            let incorrect = item
                .get("incorrect_answers")?
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .map(decode)
                .collect::<Vec<_>>();
            let question = FetchedQuestion {
                category: text(item, "category")?,
                difficulty: text(item, "difficulty").unwrap_or_default(),
                question: text(item, "question")?,
                correct: text(item, "correct_answer")?,
                incorrect,
                kind,
            };
            let usable = matches!(question.kind.as_str(), "multiple" | "boolean")
                && !question.question.is_empty()
                && !question.correct.is_empty()
                && !question.incorrect.is_empty();
            usable.then_some(question)
        })
        .collect()
}

/// Decodes the RFC 3986 percent-encoding the API is asked for, bounded, without control characters.
fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Some(byte) = std::str::from_utf8(&bytes[index + 1..index + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out)
        .chars()
        .filter(|ch| !ch.is_control())
        .take(MAX_FIELD_CHARS)
        .collect::<String>()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Talks to the real service; run by hand with `cargo test -p jeeves live_opentdb -- --ignored`.
    #[test]
    #[ignore]
    fn live_opentdb() {
        let request = TriviaFetchRequest { amount: 5 };
        assert_eq!(
            fetch(&request).error.as_deref(),
            Some("rate_limited"),
            "token first"
        );
        std::thread::sleep(MIN_INTERVAL + Duration::from_millis(500));
        let response = fetch(&request);
        assert_eq!(response.error, None);
        assert_eq!(response.questions.len(), 5);
        assert_eq!(
            fetch(&request).error.as_deref(),
            Some("rate_limited"),
            "spaced out"
        );
    }

    #[test]
    fn decodes_and_keeps_only_usable_questions() {
        assert_eq!(
            decode("Who%20wrote%20%22Hamlet%22%3F"),
            "Who wrote \"Hamlet\"?"
        );
        assert_eq!(decode("50%25"), "50%");
        assert_eq!(decode("bad%zzend%"), "bad%zzend%");
        let value = serde_json::json!({
            "response_code": 0,
            "results": [
                {"type": "multiple", "difficulty": "easy", "category": "Science%3A%20Computers",
                 "question": "What%20does%20CPU%20stand%20for%3F",
                 "correct_answer": "Central%20Processing%20Unit",
                 "incorrect_answers": ["Computer%20Personal%20Unit", "Central%20Process%20Unit"]},
                {"type": "boolean", "difficulty": "hard", "category": "History",
                 "question": "The%20sky%20is%20green.", "correct_answer": "False",
                 "incorrect_answers": ["True"]},
                {"type": "multiple", "category": "Broken", "question": "No%20answers",
                 "correct_answer": "", "incorrect_answers": []}
            ]
        });
        let questions = parse(&value);
        assert_eq!(questions.len(), 2);
        assert_eq!(questions[0].category, "Science: Computers");
        assert_eq!(questions[0].correct, "Central Processing Unit");
        assert_eq!(questions[1].kind, "boolean");
    }
}
