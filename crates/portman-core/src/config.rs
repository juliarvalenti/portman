//! `~/.config/portman/config.toml` — every key optional.
//!
//! ```toml
//! crawl_roots   = ["~/Documents/GitHub"]   # PORTMAN_ROOTS still overrides
//! refresh_secs  = 2
//! extra_dev_patterns = ["dagster", "uvicorn"]
//!
//! [stale]
//! after         = "48h"
//! idle_for      = "6h"
//! idle_cpu_pct  = 1.0
//! min_footprint = "1GB"
//!
//! [alerts]
//! disk_free_gb  = 10
//! swap_gb       = 20
//!
//! [notifications]   # opt-out per kind
//! stale = true
//! disk  = true
//! swap  = true
//!
//! [reap]
//! auto = false
//! ```

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
pub struct Config {
    pub crawl_roots: Option<Vec<String>>,
    pub refresh_secs: u64,
    pub extra_dev_patterns: Vec<String>,
    pub stale: StaleConfig,
    pub alerts: AlertConfig,
    pub notifications: NotificationConfig,
    pub reap: ReapConfig,
}

#[derive(Debug, Clone, Serialize)]
pub struct StaleConfig {
    #[serde(with = "secs")]
    pub after: Duration,
    #[serde(with = "secs")]
    pub idle_for: Duration,
    pub idle_cpu_pct: f32,
    pub min_footprint: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AlertConfig {
    pub disk_free_gb: f64,
    pub swap_gb: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct NotificationConfig {
    pub stale: bool,
    pub disk: bool,
    pub swap: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReapConfig {
    pub auto: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            crawl_roots: None,
            refresh_secs: 2,
            extra_dev_patterns: Vec::new(),
            stale: StaleConfig {
                after: Duration::from_secs(48 * 3600),
                idle_for: Duration::from_secs(6 * 3600),
                idle_cpu_pct: 1.0,
                min_footprint: 1 << 30,
            },
            alerts: AlertConfig { disk_free_gb: 10.0, swap_gb: 20.0 },
            notifications: NotificationConfig { stale: true, disk: true, swap: true },
            reap: ReapConfig { auto: false },
        }
    }
}

// Raw on-disk shape: everything optional, sizes/durations as strings.
#[derive(Deserialize, Default)]
#[serde(default)]
struct RawConfig {
    crawl_roots: Option<Vec<String>>,
    refresh_secs: Option<u64>,
    extra_dev_patterns: Option<Vec<String>>,
    stale: RawStale,
    alerts: RawAlerts,
    notifications: RawNotifications,
    reap: RawReap,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawStale {
    after: Option<String>,
    idle_for: Option<String>,
    idle_cpu_pct: Option<Num>,
    min_footprint: Option<toml::Value>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawAlerts {
    disk_free_gb: Option<Num>,
    swap_gb: Option<Num>,
}

/// TOML keeps `10` and `10.0` distinct; accept either.
#[derive(Deserialize, Clone, Copy)]
#[serde(untagged)]
enum Num {
    Int(i64),
    Float(f64),
}

impl Num {
    fn get(self) -> f64 {
        match self {
            Num::Int(i) => i as f64,
            Num::Float(f) => f,
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawNotifications {
    stale: Option<bool>,
    disk: Option<bool>,
    swap: Option<bool>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawReap {
    auto: Option<bool>,
}

impl Config {
    /// `PORTMAN_CONFIG` overrides the default location (handy for tests).
    pub fn path() -> PathBuf {
        if let Ok(p) = std::env::var("PORTMAN_CONFIG") {
            return crate::expand_user(&p);
        }
        crate::home().join(".config").join("portman").join("config.toml")
    }

    /// Load the config, falling back to defaults. A malformed file is
    /// reported on stderr rather than breaking `portman env`.
    pub fn load() -> Config {
        match Self::try_load() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("portman: ignoring {}: {e}", Self::path().display());
                Config::default()
            }
        }
    }

    pub fn try_load() -> Result<Config, String> {
        match std::fs::read_to_string(Self::path()) {
            Ok(text) => Self::parse(&text),
            Err(_) => Ok(Config::default()),
        }
    }

    pub fn parse(text: &str) -> Result<Config, String> {
        let raw: RawConfig = toml::from_str(text).map_err(|e| e.message().to_string())?;
        let mut c = Config::default();
        c.crawl_roots = raw.crawl_roots;
        if let Some(r) = raw.refresh_secs {
            c.refresh_secs = r.max(1);
        }
        if let Some(p) = raw.extra_dev_patterns {
            c.extra_dev_patterns = p;
        }
        if let Some(s) = raw.stale.after {
            c.stale.after = parse_duration(&s).ok_or(format!("stale.after: bad duration {s:?}"))?;
        }
        if let Some(s) = raw.stale.idle_for {
            c.stale.idle_for = parse_duration(&s).ok_or(format!("stale.idle_for: bad duration {s:?}"))?;
        }
        if let Some(p) = raw.stale.idle_cpu_pct {
            c.stale.idle_cpu_pct = p.get() as f32;
        }
        if let Some(v) = raw.stale.min_footprint {
            c.stale.min_footprint = match &v {
                toml::Value::Integer(n) => *n as u64,
                toml::Value::String(s) => parse_size(s).ok_or(format!("stale.min_footprint: bad size {s:?}"))?,
                _ => return Err("stale.min_footprint: expected a size like \"1GB\"".into()),
            };
        }
        if let Some(v) = raw.alerts.disk_free_gb {
            c.alerts.disk_free_gb = v.get();
        }
        if let Some(v) = raw.alerts.swap_gb {
            c.alerts.swap_gb = v.get();
        }
        c.notifications.stale = raw.notifications.stale.unwrap_or(true);
        c.notifications.disk = raw.notifications.disk.unwrap_or(true);
        c.notifications.swap = raw.notifications.swap.unwrap_or(true);
        c.reap.auto = raw.reap.auto.unwrap_or(false);
        Ok(c)
    }
}

/// `"90s"`, `"1m"`, `"6h"`, `"2d"`, `"1w"`, or compound `"1h30m"`.
/// A bare number is seconds.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if let Ok(n) = s.parse::<f64>() {
        return (n >= 0.0).then(|| Duration::from_secs_f64(n));
    }
    let mut total = 0f64;
    let mut num = String::new();
    let mut saw_unit = false;
    for c in s.chars() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
            continue;
        }
        if c.is_whitespace() {
            continue;
        }
        let n: f64 = num.parse().ok()?;
        num.clear();
        total += n * match c {
            's' => 1.0,
            'm' => 60.0,
            'h' => 3600.0,
            'd' => 86400.0,
            'w' => 7.0 * 86400.0,
            _ => return None,
        };
        saw_unit = true;
    }
    (saw_unit && num.is_empty()).then(|| Duration::from_secs_f64(total))
}

/// `"1GB"`, `"512MB"`, `"1.5G"`, `"300k"`; binary units. A bare number is bytes.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().ok()?;
    let mult = match unit.trim().to_ascii_lowercase().trim_end_matches("ib").trim_end_matches('b') {
        "" => 1u64,
        "k" => 1 << 10,
        "m" => 1 << 20,
        "g" => 1 << 30,
        "t" => 1 << 40,
        _ => return None,
    };
    Some((n * mult as f64) as u64)
}

mod secs {
    use serde::Serializer;
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_secs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("48h"), Some(Duration::from_secs(172800)));
        assert_eq!(parse_duration("1m"), Some(Duration::from_secs(60)));
        assert_eq!(parse_duration("1h30m"), Some(Duration::from_secs(5400)));
        assert_eq!(parse_duration("30"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("5x"), None);
        assert_eq!(parse_duration("5h3"), None);
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("1GB"), Some(1 << 30));
        assert_eq!(parse_size("512MB"), Some(512 << 20));
        assert_eq!(parse_size("1.5G"), Some(3 << 29));
        assert_eq!(parse_size("2GiB"), Some(2 << 30));
        assert_eq!(parse_size("7"), Some(7));
        assert_eq!(parse_size("7q"), None);
    }

    #[test]
    fn parses_spec_example() {
        let c = Config::parse(
            r#"
            crawl_roots = ["~/Documents/GitHub"]
            refresh_secs = 2
            extra_dev_patterns = ["dagster", "uvicorn"]
            [stale]
            after = "1m"
            idle_for = "6h"
            idle_cpu_pct = 1.0
            min_footprint = "1GB"
            [alerts]
            disk_free_gb = 10
            swap_gb = 20
            [reap]
            auto = false
            "#,
        )
        .unwrap();
        assert_eq!(c.stale.after, Duration::from_secs(60));
        assert_eq!(c.alerts.swap_gb, 20.0);
        assert_eq!(c.extra_dev_patterns, ["dagster", "uvicorn"]);
    }
}
