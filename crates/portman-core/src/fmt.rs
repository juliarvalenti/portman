//! Human-readable sizes and durations, shared by CLI and app.

/// `6.7 GB`, `383 MB`, `12 KB` — binary units, like `top` and Activity Monitor.
pub fn bytes(b: u64) -> String {
    const K: f64 = 1024.0;
    let b = b as f64;
    if b >= K * K * K {
        format!("{:.1} GB", b / (K * K * K))
    } else if b >= K * K {
        format!("{:.0} MB", b / (K * K))
    } else if b >= K {
        format!("{:.0} KB", b / K)
    } else {
        format!("{b:.0} B")
    }
}

/// GB with one decimal and no unit, for `26/28 GB`-style pairs.
pub fn gb(b: u64) -> String {
    format!("{:.1}", b as f64 / (1u64 << 30) as f64)
}

/// Coarse uptime: `45s`, `12m`, `5h`, `8d`.
pub fn duration(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(bytes(7_194_000_000), "6.7 GB");
        assert_eq!(bytes(383 << 20), "383 MB");
        assert_eq!(duration(8 * 86400 + 5), "8d");
        assert_eq!(duration(59), "59s");
        assert_eq!(gb(26 << 30), "26.0");
    }
}
