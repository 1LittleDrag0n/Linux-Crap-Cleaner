//! Formatting helpers shared by every screen.

const UNITS: [&str; 6] = ["B", "kB", "MB", "GB", "TB", "PB"];

/// Formats a byte count the way GNOME Files does (decimal units), e.g.
/// `1.23 GB`, `45.6 MB`, `789 kB`.
///
/// Three significant digits are kept so lists of sizes line up nicely.
pub fn human_size(bytes: u64) -> String {
    if bytes < 1000 {
        return format!("{bytes} B");
    }

    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }

    // Rounding can push e.g. 999.96 kB to "1000 kB"; move up a unit instead.
    if round_to_precision(value) >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }

    let decimals = precision(value);
    format!("{value:.decimals$} {}", UNITS[unit])
}

fn precision(value: f64) -> usize {
    if value >= 99.95 {
        0
    } else if value >= 9.995 {
        1
    } else {
        2
    }
}

fn round_to_precision(value: f64) -> f64 {
    let factor = 10f64.powi(precision(value) as i32);
    (value * factor).round() / factor
}

/// Formats a count with thousands separators, e.g. `1,234,567`.
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Formats a duration in seconds, e.g. `0.4 s`, `12 s`, `3 min 5 s`.
pub fn duration(d: std::time::Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 10.0 {
        format!("{secs:.1} s")
    } else if secs < 60.0 {
        format!("{secs:.0} s")
    } else {
        let whole = d.as_secs();
        format!("{} min {} s", whole / 60, whole % 60)
    }
}

/// How long ago a Unix time was, in friendly words: "today", "yesterday",
/// "5 days ago", "3 months ago", "2 years ago".
pub fn ago(then: i64, now: i64) -> String {
    let days = (now - then).max(0) / 86_400;
    let plural = |n: i64, unit: &str| {
        if n == 1 {
            format!("1 {unit} ago")
        } else {
            format!("{n} {unit}s ago")
        }
    };
    match days {
        0 => "today".to_owned(),
        1 => "yesterday".to_owned(),
        2..=30 => plural(days, "day"),
        31..=364 => plural(days / 30, "month"),
        _ => plural(days / 365, "year"),
    }
}

/// Current Unix time in seconds.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Formats a percentage (0.0 to 1.0) as a whole number, e.g. `60%`.
pub fn percent(fraction: f64) -> String {
    format!("{:.0}%", (fraction * 100.0).clamp(0.0, 100.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(999), "999 B");
    }

    #[test]
    fn larger_units() {
        assert_eq!(human_size(1_000), "1.00 kB");
        assert_eq!(human_size(1_234_567), "1.23 MB");
        assert_eq!(human_size(45_600_000), "45.6 MB");
        assert_eq!(human_size(789_000), "789 kB");
        assert_eq!(human_size(281_000_000_000), "281 GB");
    }

    #[test]
    fn rounding_moves_to_next_unit() {
        assert_eq!(human_size(999_999), "1.00 MB");
        assert_eq!(human_size(999_960_000), "1.00 GB");
    }

    #[test]
    fn huge_values_stay_in_largest_unit() {
        assert_eq!(human_size(u64::MAX), "18447 PB");
    }

    #[test]
    fn counts() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1_000), "1,000");
        assert_eq!(count(1_234_567), "1,234,567");
    }

    #[test]
    fn durations() {
        use std::time::Duration;
        assert_eq!(duration(Duration::from_millis(420)), "0.4 s");
        assert_eq!(duration(Duration::from_secs(12)), "12 s");
        assert_eq!(duration(Duration::from_secs(185)), "3 min 5 s");
    }

    #[test]
    fn ages() {
        let day = 86_400;
        assert_eq!(ago(1000, 1000 + 3600), "today");
        assert_eq!(ago(0, day + 5), "yesterday");
        assert_eq!(ago(0, 5 * day), "5 days ago");
        assert_eq!(ago(0, 45 * day), "1 month ago");
        assert_eq!(ago(0, 200 * day), "6 months ago");
        assert_eq!(ago(0, 800 * day), "2 years ago");
        assert_eq!(ago(100, 0), "today", "future times don't go negative");
    }

    #[test]
    fn percentages() {
        assert_eq!(percent(0.6), "60%");
        assert_eq!(percent(1.5), "100%");
        assert_eq!(percent(-1.0), "0%");
    }
}
