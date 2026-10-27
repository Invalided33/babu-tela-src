//! Errors, input coercion and small helpers shared by the API and the
//! gateway.

use serde_json::{Number, Value};
use sha2::{Digest, Sha256};

/// An error that becomes `{"error": msg}` with `status` (or an ERROR event on
/// the gateway). `status` 500 also covers database failures, which are logged.
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub msg: String,
}

pub type ApiResult<T = Value> = Result<T, ApiError>;

pub const INTERNAL: &str = "Erro interno do servidor";

pub fn fail<T>(status: u16, msg: impl Into<String>) -> ApiResult<T> {
    Err(ApiError {
        status,
        msg: msg.into(),
    })
}

/// Milliseconds since the Unix epoch.
pub fn now() -> i64 {
    worker::Date::now().as_millis() as i64
}

/// Counts like JavaScript's String.length (UTF-16 units), which the earlier
/// server and the app's maxlength attributes use.
pub fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// The first `n` characters of `s`.
pub fn truncate(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((i, _)) => s[..i].to_owned(),
        None => s.to_owned(),
    }
}

/// Lowercases one character for one, the way Go's strings.ToLower did: "İ"
/// becomes "i" and a final "Σ" stays "σ".
pub fn to_lower(s: &str) -> String {
    s.chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect()
}

/// Mirrors JavaScript's `String(value ?? '')` for JSON values.
pub fn str(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => number_string(n),
        Some(Value::Bool(b)) => b.to_string(),
        Some(other) => other.to_string(),
    }
}

/// `f` as an int64 when it is a whole number in range.
fn whole(f: f64) -> Option<i64> {
    (f.fract() == 0.0 && (-9.223_372_036_854_776e18..9.223_372_036_854_776e18).contains(&f))
        .then_some(f as i64)
}

/// A JSON number as text, the way the earlier servers printed it (they read
/// every number as a float64): whole numbers that fit an int64 without a
/// decimal point, others in the shortest form, with an exponent below 1e-4
/// or from 1e6 on.
pub fn number_string(n: &Number) -> String {
    let f = n.as_f64().unwrap_or(0.0);
    if let Some(i) = whole(f) {
        return i.to_string();
    }
    let sci = format!("{f:e}");
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if !(-4..6).contains(&exp) {
        format!(
            "{mantissa}e{}{:02}",
            if exp < 0 { '-' } else { '+' },
            exp.abs()
        )
    } else {
        f.to_string()
    }
}

/// JavaScript truthiness for JSON values.
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(_) => true,
    }
}

/// Accepts a whole number given as a JSON number or a numeric string.
pub fn to_int(v: Option<&Value>) -> ApiResult<i64> {
    let n = match v {
        Some(Value::Number(n)) => n.as_f64().and_then(whole),
        Some(Value::String(s)) => parse_int(s),
        _ => None,
    };
    match n {
        Some(n) => Ok(n),
        None => fail(400, "ID inválido"),
    }
}

/// A path or query value as a whole number ("" is 0).
pub fn param_int(s: &str) -> ApiResult<i64> {
    match parse_int(s) {
        Some(n) => Ok(n),
        None => fail(400, "ID inválido"),
    }
}

/// Canonical decimal integers only ("12", "+12", "-3"), so "012" or "1e3" never pass.
fn parse_int(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return Some(0);
    }
    let body = s.strip_prefix('+').unwrap_or(s);
    if body.starts_with('+') || (body.len() != s.len() && body.starts_with('-')) {
        return None;
    }
    let n: i64 = body.parse().ok()?;
    (n.to_string() == body).then_some(n)
}

/// Required text between 1 and `max` characters, trimmed.
pub fn text(v: Option<&Value>, max: usize, label: &str) -> ApiResult<String> {
    let s = str(v).trim().to_owned();
    if s.is_empty() || js_len(&s) > max {
        return fail(
            400,
            format!("{label} precisa ter entre 1 e {max} caracteres"),
        );
    }
    Ok(s)
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    // getrandom uses crypto.getRandomValues here, which cannot fail.
    getrandom::fill(&mut b).expect("sorteio");
    b
}

pub fn random_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b[..]).expect("sorteio");
    hex::encode(b)
}

pub fn pick<'a>(list: &[&'a str]) -> &'a str {
    let n = u32::from_le_bytes(random_bytes::<4>()) as usize;
    list[n % list.len()]
}

pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Base name of a path, the way Go's filepath.Base worked on Windows.
pub fn base_name(path: &str) -> &str {
    let is_sep = |c: char| c == '/' || c == '\\';
    let trimmed = path.trim_end_matches(is_sep);
    if trimmed.is_empty() {
        return if path.is_empty() { "." } else { &path[..1] };
    }
    match trimmed.rfind(is_sep) {
        Some(i) => &trimmed[i + 1..],
        None => trimmed,
    }
}

/// Extension with its dot (".png"), like Go's filepath.Ext.
pub fn extension(path: &str) -> &str {
    for (i, c) in path.char_indices().rev() {
        if c == '/' || c == '\\' {
            break;
        }
        if c == '.' {
            return &path[i..];
        }
    }
    ""
}
