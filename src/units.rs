//! Sizes and durations as they are written in the configuration and printed
//! in the status.

use std::time::Duration;

use anyhow::{Result, bail};

/// `"10G"` is ten times 1024^3; a bare number is bytes.
pub fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let (digits, factor) = match s.chars().last() {
        Some('K') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M') => (&s[..s.len() - 1], 1 << 20),
        Some('G') => (&s[..s.len() - 1], 1 << 30),
        Some(c) if c.is_ascii_digit() => (s, 1),
        _ => bail!("not a size: {s:?} (use a number with K, M or G)"),
    };
    let n: u64 = digits
        .parse()
        .map_err(|_| anyhow::anyhow!("not a size: {s:?} (use a number with K, M or G)"))?;
    n.checked_mul(factor)
        .ok_or_else(|| anyhow::anyhow!("size too large: {s:?}"))
}

/// The unit is mandatory: a bare `30` is more often a mistake than a wish
/// for seconds.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let factor = match s.chars().last() {
        Some('s') => 1,
        Some('m') => 60,
        Some('h') => 3600,
        _ => bail!("not a duration: {s:?} (use a number with s, m or h)"),
    };
    let n: u64 = s[..s.len() - 1]
        .parse()
        .map_err(|_| anyhow::anyhow!("not a duration: {s:?} (use a number with s, m or h)"))?;
    Ok(Duration::from_secs(n * factor))
}

pub fn format_size(bytes: u64) -> String {
    const G: u64 = 1 << 30;
    const M: u64 = 1 << 20;
    if bytes >= G {
        format!("{:.1}G", bytes as f64 / G as f64)
    } else if bytes >= M {
        format!("{}M", bytes / M)
    } else {
        format!("{}K", bytes / 1024)
    }
}

pub fn format_duration(d: Duration) -> String {
    let s = d.as_secs();
    match (s / 3600, s % 3600 / 60, s % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m{s:02}s"),
        (h, m, _) => format!("{h}h{m:02}m"),
    }
}

/// `20260919-143000`, UTC, for file names that sort by time.
pub fn utc_stamp(epoch: u64) -> String {
    let (days, rest) = (epoch / 86_400, epoch % 86_400);
    // Civil date from a day count, after Howard Hinnant's `civil_from_days`.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps() {
        assert_eq!(utc_stamp(0), "19700101-000000");
        // 2026-09-19 14:30:00 UTC
        assert_eq!(utc_stamp(1_789_828_200), "20260919-143000");
        // A leap day.
        assert_eq!(utc_stamp(1_709_164_800), "20240229-000000");
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("10G").unwrap(), 10 << 30);
        assert_eq!(parse_size("512M").unwrap(), 512 << 20);
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert!(parse_size("1X").is_err());
        assert!(parse_size("G").is_err());
        assert!(parse_size("").is_err());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert!(parse_duration("30").is_err());
        assert!(parse_duration("m").is_err());
    }

    #[test]
    fn printing() {
        assert_eq!(format_size(10 << 30), "10.0G");
        assert_eq!(format_size(300 << 20), "300M");
        assert_eq!(format_duration(Duration::from_secs(252)), "4m12s");
        assert_eq!(format_duration(Duration::from_secs(3720)), "1h02m");
    }
}
