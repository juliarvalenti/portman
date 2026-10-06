//! Menu-bar tray: `4 · 14.2 GB`, one line per dev server, reap, vitals.
//!
//! The menu is rebuilt only when its structure changes (which servers exist,
//! which actions they offer). Otherwise labels are updated in place, so an
//! open menu doesn't flicker or close every refresh.

use std::sync::Mutex;

use portman_core::fmt;
use portman_core::{DevProcess, PressureLevel, Snapshot, Worktree};
use tauri::image::Image;
use tauri::menu::{IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, Wry};

use crate::commands::{self, locate};
use crate::{show_window, AppState};

const TRAY_ID: &str = "main";

#[derive(Default)]
struct TrayState {
    signature: String,
    servers: Vec<(i32, Submenu<Wry>)>,
    reap: Option<MenuItem<Wry>>,
    vitals: Option<MenuItem<Wry>>,
    level: Option<PressureLevel>,
}

/// A server line in the tray, with what its submenu can offer.
struct Line {
    pid: i32,
    text: String,
    port: Option<u16>,
    has_lease: bool,
    has_worktree: bool,
}

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    app.manage(Mutex::new(TrayState::default()));
    let menu = Menu::with_items(app, &[&MenuItem::with_id(app, "loading", "Sampling…", false, None::<&str>)?])?;
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon(PressureLevel::Normal))
        .icon_as_template(true)
        .title("…")
        .tooltip("Portman")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| on_menu(app, event.id().as_ref()))
        .build(app)?;
    Ok(())
}

fn icon(level: PressureLevel) -> Image<'static> {
    let bytes: &'static [u8] = match level {
        PressureLevel::Normal => include_bytes!("../icons/tray-normal.png"),
        PressureLevel::Warn => include_bytes!("../icons/tray-warn.png"),
        PressureLevel::Critical => include_bytes!("../icons/tray-critical.png"),
    };
    Image::from_bytes(bytes).expect("bundled tray icon is a valid PNG")
}

fn lines(snap: &Snapshot) -> Vec<(u64, Line)> {
    let line = |label: &str, w: Option<&Worktree>, d: &DevProcess| {
        let port = d.listening.first().copied();
        let port_txt = port.map(|p| format!("  :{p}")).unwrap_or_default();
        let stale = if d.stale.is_some() { "  · stale" } else { "" };
        let dot = if d.stale.is_some() { "◉" } else { "●" };
        (
            d.footprint_bytes,
            Line {
                pid: d.pid,
                text: format!(
                    "{dot} {label}{port_txt}  {}  {}{stale}",
                    fmt::bytes(d.footprint_bytes),
                    fmt::duration(d.uptime_secs)
                ),
                port,
                has_lease: w.is_some_and(|w| w.lease.is_some()),
                has_worktree: w.is_some(),
            },
        )
    };
    let mut out: Vec<(u64, Line)> = snap
        .labeled()
        .into_iter()
        .filter(|(_, _, d)| d.stoppable)
        .map(|(label, w, d)| line(&label, Some(w), d))
        .collect();
    out.extend(snap.unattributed.iter().filter(|d| d.stoppable).map(|d| line(&d.name, None, d)));
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out
}

fn reap_text(snap: &Snapshot) -> String {
    format!(
        "Reap stale ({} · {})…",
        snap.summary.stale_count,
        fmt::bytes(snap.summary.stale_footprint)
    )
}

fn vitals_text(snap: &Snapshot) -> String {
    let v = &snap.vitals;
    format!(
        "Swap {}/{} GB · Disk {} GB free",
        fmt::gb(v.swap_used).trim_end_matches(".0"),
        fmt::gb(v.swap_total).trim_end_matches(".0"),
        format!("{:.0}", v.disk_free as f64 / (1u64 << 30) as f64)
    )
}

pub fn update(app: &AppHandle, snap: &Snapshot) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else { return };
    let title = format!("{} · {}", snap.summary.dev_count, fmt::bytes(snap.summary.dev_footprint));
    let _ = tray.set_title(Some(title));

    let state = app.state::<Mutex<TrayState>>();
    let mut st = state.lock().unwrap();

    if st.level != Some(snap.summary.level) {
        let _ = tray.set_icon(Some(icon(snap.summary.level)));
        let _ = tray.set_icon_as_template(snap.summary.level == PressureLevel::Normal);
        st.level = Some(snap.summary.level);
    }

    let lines = lines(snap);
    let signature: String = lines
        .iter()
        .map(|(_, l)| format!("{}:{:?}:{}:{};", l.pid, l.port, l.has_lease, l.has_worktree))
        .collect();

    if signature == st.signature && st.servers.len() == lines.len() {
        for ((_, l), (_, sub)) in lines.iter().zip(&st.servers) {
            let _ = sub.set_text(&l.text);
        }
        if let Some(r) = &st.reap {
            let _ = r.set_text(reap_text(snap));
            let _ = r.set_enabled(snap.summary.stale_count > 0);
        }
        if let Some(v) = &st.vitals {
            let _ = v.set_text(vitals_text(snap));
        }
        return;
    }

    match build_menu(app, snap, &lines) {
        Ok((menu, servers, reap, vitals)) => {
            let _ = tray.set_menu(Some(menu));
            st.signature = signature;
            st.servers = servers;
            st.reap = Some(reap);
            st.vitals = Some(vitals);
        }
        Err(e) => eprintln!("portman: tray menu: {e}"),
    }
}

type Built = (Menu<Wry>, Vec<(i32, Submenu<Wry>)>, MenuItem<Wry>, MenuItem<Wry>);

fn build_menu(app: &AppHandle, snap: &Snapshot, lines: &[(u64, Line)]) -> tauri::Result<Built> {
    let mut servers = Vec::new();
    for (_, l) in lines {
        let pid = l.pid;
        let sub = Submenu::with_id_and_items(
            app,
            format!("server:{pid}"),
            &l.text,
            true,
            &[
                &MenuItem::with_id(app, format!("open:{pid}"), "Open in browser", l.port.is_some(), None::<&str>)?,
                &MenuItem::with_id(app, format!("stop:{pid}"), "Stop…", true, None::<&str>)?,
                &MenuItem::with_id(app, format!("reveal:{pid}"), "Reveal worktree", l.has_worktree, None::<&str>)?,
                &MenuItem::with_id(app, format!("env:{pid}"), "Copy env", l.has_lease, None::<&str>)?,
            ],
        )?;
        servers.push((pid, sub));
    }
    let empty = MenuItem::with_id(app, "none", "No dev servers running", false, None::<&str>)?;
    let reap = MenuItem::with_id(app, "reap", reap_text(snap), snap.summary.stale_count > 0, None::<&str>)?;
    let vitals = MenuItem::with_id(app, "vitals", vitals_text(snap), false, None::<&str>)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let sep3 = PredefinedMenuItem::separator(app)?;
    let show = MenuItem::with_id(app, "show", "Open Portman", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Portman", true, Some("CmdOrCtrl+Q"))?;

    let mut items: Vec<&dyn IsMenuItem<Wry>> = Vec::new();
    if servers.is_empty() {
        items.push(&empty);
    }
    for (_, s) in &servers {
        items.push(s);
    }
    items.extend([&sep1 as &dyn IsMenuItem<Wry>, &reap, &sep2, &vitals, &sep3, &show, &quit]);
    let menu = Menu::with_items(app, &items)?;
    Ok((menu, servers, reap, vitals))
}

fn on_menu(app: &AppHandle, id: &str) {
    match id {
        "show" => show_window(app),
        "quit" => app.exit(0),
        "reap" => {
            show_window(app);
            let _ = app.emit("confirm-reap", ());
        }
        _ => {
            let Some((action, pid)) = id.split_once(':') else { return };
            let Ok(pid) = pid.parse::<i32>() else { return };
            server_action(app, action, pid);
        }
    }
}

fn server_action(app: &AppHandle, action: &str, pid: i32) {
    let state = app.state::<AppState>();
    let Some(snap) = state.latest() else { return };
    let Some((wt, d)) = locate(&snap, pid) else { return };
    match action {
        "open" => {
            if let Some(&port) = d.listening.first() {
                let _ = commands::open_url(port);
            }
        }
        // Stopping always confirms; the window shows the inline prompt.
        "stop" => {
            show_window(app);
            let _ = app.emit("confirm-stop", d.pid);
        }
        "reveal" => {
            if let Some(w) = wt {
                let _ = commands::reveal_path(w.path.clone());
            }
        }
        "env" => {
            if let Some(lease) = wt.and_then(|w| w.lease.as_ref()) {
                let _ = commands::pbcopy(&(lease.export_lines().join("\n") + "\n"));
            }
        }
        _ => {}
    }
}
