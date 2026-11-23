//! Small, pure helpers. Most of them mirror the lenient parsing the legacy
//! service did (`shopcore/utils.py`) so clients keep getting the same results.

use chrono::{NaiveDate, NaiveDateTime, Timelike};
use serde_json::Value;

/// Python `to_int`: accepts ints, floats (truncated), bools and strings with a
/// leading integer ("12", " 7 ", "5abc"). Anything else yields `default`.
pub fn to_int(v: Option<&Value>, default: i64) -> i64 {
    match v {
        None | Some(Value::Null) => default,
        Some(Value::Bool(b)) => *b as i64,
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| {
                n.as_f64()
                    .filter(|f| f.is_finite() && f.abs() < 9.0e18)
                    .map(|f| f.trunc() as i64)
            })
            .unwrap_or(default),
        Some(Value::String(s)) => str_to_int(s).unwrap_or(default),
        Some(_) => default,
    }
}

/// `to_int` for query-string values.
pub fn query_int(v: Option<&String>, default: i64) -> i64 {
    v.and_then(|s| str_to_int(s)).unwrap_or(default)
}

fn str_to_int(s: &str) -> Option<i64> {
    let t = s.trim();
    if let Ok(n) = t.parse::<i64>() {
        return Some(n);
    }
    // fallback: leading "-?\d+" after optional whitespace (legacy regex)
    let start = s.trim_start();
    let (neg, rest) = match start.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, start),
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let n: i64 = digits.parse().ok()?;
    Some(if neg { -n } else { n })
}

/// Parse a money amount from JSON (number or numeric string).
/// Returns `None` for anything that is not a finite number.
pub fn parse_money(v: &Value) -> Option<f64> {
    let f = match v {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    f.is_finite().then_some(f)
}

/// Round to 2 decimals exactly like Python's `round(x, 2)` (correctly rounded,
/// ties to even on the exact binary value).
pub fn round2(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{x:.2}").parse().unwrap_or(x)
}

/// Legacy `pricing.calc_total`.
pub fn calc_total(subtotal: f64, percent: i32) -> f64 {
    let total = if percent != 0 {
        subtotal * (1.0 - percent as f64 / 100.0)
    } else {
        subtotal
    };
    round2(total)
}

/// Python's `str(float)`: `15.0` stays `15.0`, `89.99` stays `89.99`.
pub fn py_float(x: f64) -> String {
    if x.is_finite() && x.fract() == 0.0 && x.abs() < 1e16 {
        format!("{x:.1}")
    } else {
        format!("{x}")
    }
}

/// Python's `str(datetime)`.
pub fn py_datetime(dt: &NaiveDateTime) -> String {
    if dt.nanosecond() / 1000 == 0 {
        dt.format("%Y-%m-%d %H:%M:%S").to_string()
    } else {
        dt.format("%Y-%m-%d %H:%M:%S%.6f").to_string()
    }
}

/// The format Flask's `jsonify` used for datetimes (RFC 1123), kept so the
/// `created_at` fields look the same as before. Timestamps are UTC.
pub fn http_date(dt: &NaiveDateTime) -> String {
    dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

/// Parse a report date bound: `YYYY-MM-DD` or an ISO date-time.
pub fn parse_bound(s: &str) -> Option<NaiveDateTime> {
    let s = s.trim();
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return d.and_hms_opt(0, 0, 0);
    }
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(dt);
        }
    }
    None
}

/// Legacy `clean`: strip a value, `None` becomes "".
pub fn clean(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.trim().to_string(),
        Some(other) => other.to_string().trim().to_string(),
    }
}

/// A nullable text field from JSON: strings as-is, null/missing as None,
/// any other scalar as its JSON text.
pub fn opt_text(v: Option<&Value>) -> Option<String> {
    match v {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => Some(other.to_string()),
    }
}

/// Export file names: a single path component made of `[A-Za-z0-9._-]`,
/// not starting with a dot. Blocks path traversal and shell metacharacters.
pub fn valid_file_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Escape `%`, `_` and `\` for use inside a LIKE/ILIKE pattern.
pub fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Path ids follow Flask's `<int:...>` converter: digits only, otherwise 404.
pub fn parse_path_id(s: &str) -> Option<i32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<i32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn to_int_matches_legacy() {
        assert_eq!(to_int(Some(&json!(5)), 0), 5);
        assert_eq!(to_int(Some(&json!("12")), 0), 12);
        assert_eq!(to_int(Some(&json!(" 7 ")), 0), 7);
        assert_eq!(to_int(Some(&json!("5abc")), 0), 5);
        assert_eq!(to_int(Some(&json!("-3x")), 0), -3);
        assert_eq!(to_int(Some(&json!(2.9)), 0), 2);
        assert_eq!(to_int(Some(&json!("abc")), 9), 9);
        assert_eq!(to_int(None, 1), 1);
        assert_eq!(to_int(Some(&Value::Null), 1), 1);
        assert_eq!(query_int(Some(&"10".to_string()), 20), 10);
        assert_eq!(query_int(None, 20), 20);
    }

    #[test]
    fn money_parsing() {
        assert_eq!(parse_money(&json!(10)), Some(10.0));
        assert_eq!(parse_money(&json!("12.5")), Some(12.5));
        assert_eq!(parse_money(&json!("nan")), None);
        assert_eq!(parse_money(&json!(true)), None);
        assert_eq!(parse_money(&json!(null)), None);
    }

    #[test]
    fn rounding_matches_python() {
        assert_eq!(round2(89.99 * 2.0 * 0.9), 161.98);
        assert_eq!(round2(0.125), 0.12); // ties to even, like Python
        assert_eq!(round2(0.1 + 0.2), 0.3);
        assert_eq!(calc_total(89.99 * 2.0, 10), round2(89.99 * 2.0 * 0.9));
        assert_eq!(calc_total(24.5, 0), 24.5);
        assert_eq!(calc_total(100.0, 50), 50.0);
    }

    #[test]
    fn python_float_and_dates() {
        assert_eq!(py_float(15.0), "15.0");
        assert_eq!(py_float(89.99), "89.99");
        let dt = NaiveDate::from_ymd_opt(2026, 1, 2)
            .unwrap()
            .and_hms_opt(3, 4, 5)
            .unwrap();
        assert_eq!(py_datetime(&dt), "2026-01-02 03:04:05");
        assert_eq!(http_date(&dt), "Fri, 02 Jan 2026 03:04:05 GMT");
        assert_eq!(
            parse_bound("2026-01-02"),
            NaiveDate::from_ymd_opt(2026, 1, 2)
                .unwrap()
                .and_hms_opt(0, 0, 0)
        );
        assert!(parse_bound("2026-01-02T10:00:00").is_some());
        assert!(parse_bound("yesterday'; drop table orders;--").is_none());
    }

    #[test]
    fn file_names() {
        assert!(valid_file_name("sales.csv"));
        assert!(valid_file_name("smoke_ab12.csv.gz"));
        assert!(!valid_file_name("../etc/passwd"));
        assert!(!valid_file_name("a/b.csv"));
        assert!(!valid_file_name("x; rm -rf /"));
        assert!(!valid_file_name(".hidden"));
        assert!(!valid_file_name(""));
    }

    #[test]
    fn path_ids() {
        assert_eq!(parse_path_id("42"), Some(42));
        assert_eq!(parse_path_id("-1"), None);
        assert_eq!(parse_path_id("abc"), None);
        assert_eq!(parse_path_id("99999999999"), None);
    }

    #[test]
    fn like_escaping() {
        assert_eq!(escape_like("100%_a\\"), "100\\%\\_a\\\\");
    }
}
