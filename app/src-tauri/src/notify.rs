//! Notifications (each kind opt-out via `[notifications]`) and auto-reap.
//!
//! - Stale server: at most once per process per day.
//! - Disk free below `alerts.disk_free_gb` / swap above `alerts.swap_gb`:
//!   at most once an hour while the condition lasts. The swap alert names
//!   the top three footprints so the cause is right there.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime};

use portman_core::{fmt, Config, DevProcess, Snapshot, StaleReason};
use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

use crate::AppState;

const STALE_REPEAT: Duration = Duration::from_secs(24 * 3600);
const VITAL_REPEAT: Duration = Duration::from_secs(3600);

type Key = (i32, SystemTime);

#[derive(Default)]
pub struct Notifier {
    stale_sent: HashMap<Key, Instant>,
    reaped: HashSet<Key>,
    disk_sent: Option<Instant>,
    swap_sent: Option<Instant>,
}

fn gb(b: u64) -> f64 {
    b as f64 / (1u64 << 30) as f64
}

fn due(last: Option<Instant>, every: Duration) -> bool {
    last.is_none_or(|t| t.elapsed() >= every)
}

fn send(app: &AppHandle, title: &str, body: &str) {
    if let Err(e) = app.notification().builder().title(title).body(body).show() {
        eprintln!("portman: notification failed: {e}");
    }
}

fn long_duration(secs: u64) -> String {
    let plural = |n: u64, unit: &str| format!("{n} {unit}{}", if n == 1 { "" } else { "s" });
    match secs {
        s if s >= 86400 => plural(s / 86400, "day"),
        s if s >= 3600 => plural(s / 3600, "hour"),
        s => plural((s / 60).max(1), "minute"),
    }
}

/// Reap candidates (`Snapshot::stale`) with a `polaris/web`-style label.
fn stale_servers(snap: &Snapshot) -> Vec<(String, DevProcess)> {
    let mut out: Vec<(String, DevProcess)> = snap
        .labeled()
        .into_iter()
        .filter(|(_, _, d)| d.stale.is_some() && d.reapable)
        .map(|(l, _, d)| (l, d.clone()))
        .collect();
    out.extend(
        snap.unattributed
            .iter()
            .filter(|d| d.stale.is_some() && d.reapable)
            .map(|d| (d.cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), d.clone())),
    );
    out
}

impl Notifier {
    pub fn check(&mut self, app: &AppHandle, config: &Config, snap: &Snapshot) {
        let stale = stale_servers(snap);
        let live: HashSet<Key> = snap.dev_processes().map(|d| (d.pid, d.started_at)).collect();
        self.stale_sent.retain(|k, _| live.contains(k));
        self.reaped.retain(|k| live.contains(k));

        if config.reap.auto {
            self.auto_reap(app, &stale);
        } else if config.notifications.stale {
            for (label, d) in &stale {
                let key = (d.pid, d.started_at);
                if !due(self.stale_sent.get(&key).copied(), STALE_REPEAT) {
                    continue;
                }
                self.stale_sent.insert(key, Instant::now());
                let how = match d.stale {
                    Some(StaleReason::Idle) => {
                        format!("has been idle for {}", long_duration(d.idle_secs.unwrap_or(0)))
                    }
                    _ => format!("has been up {}", long_duration(d.uptime_secs)),
                };
                send(
                    app,
                    "Stale dev server",
                    &format!("{label} {} {how}, using {}. Open Portman to stop it.", d.name, fmt::bytes(d.footprint_bytes)),
                );
            }
        }

        let v = &snap.vitals;
        if gb(v.disk_free) < config.alerts.disk_free_gb {
            if config.notifications.disk && due(self.disk_sent, VITAL_REPEAT) {
                self.disk_sent = Some(Instant::now());
                send(
                    app,
                    &format!("Disk almost full: {} free", fmt::bytes(v.disk_free)),
                    &format!("Below your {} GB alert. Swap lives on disk: {} GB in use.", config.alerts.disk_free_gb, fmt::gb(v.swap_used)),
                );
            }
        } else {
            self.disk_sent = None;
        }

        if gb(v.swap_used) > config.alerts.swap_gb {
            if config.notifications.swap && due(self.swap_sent, VITAL_REPEAT) {
                self.swap_sent = Some(Instant::now());
                send(
                    app,
                    &format!("Swap at {} GB", fmt::gb(v.swap_used)),
                    &format!("Top footprints: {}", top_footprints(snap, 3)),
                );
            }
        } else {
            self.swap_sent = None;
        }
    }

    fn auto_reap(&mut self, app: &AppHandle, stale: &[(String, DevProcess)]) {
        for (label, d) in stale {
            let key = (d.pid, d.started_at);
            if !self.reaped.insert(key) {
                continue;
            }
            let (app, label, d) = (app.clone(), label.clone(), d.clone());
            std::thread::spawn(move || {
                let res = portman_core::stop::stop_pids(&d.pids, true, portman_core::stop::GRACE);
                let body = match (&res.error, res.stopped) {
                    (Some(e), _) => format!("Couldn't stop {label} {}: {e}", d.name),
                    (None, true) => format!("Stopped {label} {}, freeing {}.", d.name, fmt::bytes(d.footprint_bytes)),
                    (None, false) => format!("{label} {} is still running after SIGKILL.", d.name),
                };
                send(&app, "Auto-reaped stale server", &body);
                app.state::<AppState>().refresh_now();
            });
        }
    }
}

/// "Firefox 19 GB, polaris/web next-server 6.7 GB, Docker VM 9 GB"
fn top_footprints(snap: &Snapshot, n: usize) -> String {
    let mut all: Vec<(String, u64)> = snap
        .labeled()
        .into_iter()
        .map(|(l, _, d)| (format!("{l} {}", d.name), d.footprint_bytes))
        .chain(snap.unattributed.iter().map(|d| (d.name.clone(), d.footprint_bytes)))
        .chain(snap.heavy_hitters.iter().map(|h| (h.app.clone(), h.footprint_bytes)))
        .collect();
    all.sort_by(|a, b| b.1.cmp(&a.1));
    all.into_iter()
        .take(n)
        .map(|(name, b)| format!("{name} {}", fmt::bytes(b)))
        .collect::<Vec<_>>()
        .join(", ")
}
