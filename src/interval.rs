use std::time::Duration;

/// Parse durations such as `30s`, `5m`, `1h30m`, or a bare number of seconds.
pub fn parse_interval(raw: &str) -> Result<Duration, String> {
    let s: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    if s.is_empty() {
        return Err("interval is empty".into());
    }
    if s.chars().all(|c| c.is_ascii_digit()) {
        let secs: u64 = s.parse().map_err(|_| format!("bad interval `{raw}`"))?;
        return finish(secs);
    }
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut total: u64 = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            return Err(format!("bad interval `{raw}`"));
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let n: u64 = s[start..i]
            .parse()
            .map_err(|_| format!("bad interval `{raw}`"))?;
        let u_start = i;
        while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
            i += 1;
        }
        if u_start == i {
            return Err(format!("missing unit in interval `{raw}`"));
        }
        let mult = unit_mult(&s[u_start..i])?;
        let part = n
            .checked_mul(mult)
            .ok_or_else(|| format!("interval `{raw}` overflows"))?;
        total = total
            .checked_add(part)
            .ok_or_else(|| format!("interval `{raw}` overflows"))?;
    }
    finish(total)
}

fn unit_mult(unit: &str) -> Result<u64, String> {
    match unit.to_ascii_lowercase().as_str() {
        "s" | "sec" | "secs" | "second" | "seconds" => Ok(1),
        "m" | "min" | "mins" | "minute" | "minutes" => Ok(60),
        "h" | "hr" | "hrs" | "hour" | "hours" => Ok(3600),
        other => Err(format!("unknown interval unit `{other}`")),
    }
}

fn finish(secs: u64) -> Result<Duration, String> {
    if secs == 0 {
        return Err("interval must be at least 1s".into());
    }
    if secs > 7 * 24 * 3600 {
        return Err("interval must be at most 7d".into());
    }
    Ok(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_forms() {
        assert_eq!(parse_interval("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_interval("5min").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_interval("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_interval("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_interval("1h30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse_interval("2h30m").unwrap(), Duration::from_secs(9000));
        assert_eq!(parse_interval("5m30s").unwrap(), Duration::from_secs(330));
        assert_eq!(parse_interval("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_interval("  5 m ").unwrap(), Duration::from_secs(300));
    }

    #[test]
    fn rejects_bad_intervals() {
        assert!(parse_interval("").is_err());
        assert!(parse_interval("0").is_err());
        assert!(parse_interval("0s").is_err());
        assert!(parse_interval("m5").is_err());
        assert!(parse_interval("-5m").is_err());
        assert!(parse_interval("5d").is_err());
        assert!(parse_interval("8d").is_err());
    }
}
