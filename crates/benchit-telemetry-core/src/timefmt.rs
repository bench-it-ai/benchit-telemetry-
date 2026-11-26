//! Small time helpers that avoid pulling in a date/time crate.
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_f64() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

pub fn now() -> i64 {
    now_f64() as i64
}

/// Parse a duration such as `90`, `90s`, `15m`, `2h`, `7d` into seconds.
pub fn parse_duration(s: &str) -> Option<f64> {
    let s = s.trim();
    let (num, mul) = match s.chars().last()? {
        's' => (&s[..s.len() - 1], 1.0),
        'm' => (&s[..s.len() - 1], 60.0),
        'h' => (&s[..s.len() - 1], 3600.0),
        'd' => (&s[..s.len() - 1], 86400.0),
        'w' => (&s[..s.len() - 1], 604800.0),
        _ => (s, 1.0),
    };
    num.trim().parse::<f64>().ok().filter(|v| *v >= 0.0).map(|v| v * mul)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn durations() {
        assert_eq!(parse_duration("90"), Some(90.0));
        assert_eq!(parse_duration("15m"), Some(900.0));
        assert_eq!(parse_duration("2h"), Some(7200.0));
        assert_eq!(parse_duration("7d"), Some(604800.0));
        assert_eq!(parse_duration("x"), None);
        assert_eq!(parse_duration("-5m"), None);
    }
}
