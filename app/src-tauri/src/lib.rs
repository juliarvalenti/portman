//! Portman menu-bar app: a tray with the running dev servers and their true
//! footprint, plus a window with the full project → worktree view.
//!
//! One background thread owns the `Sampler`. Every `refresh_secs` it takes a
//! `Snapshot`, stores it, pushes it to the window as a `snapshot` event,
//! refreshes the tray, and runs the notification / auto-reap rules.

mod commands;
mod notify;
mod tray;

use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::time::Duration;

use portman_core::{Config, Sampler, Snapshot};
use tauri::{AppHandle, Emitter, Manager, WindowEvent};

pub struct AppState {
    pub config: Config,
    pub snapshot: Mutex<Option<Snapshot>>,
    pub sampler: Mutex<Sampler>,
    /// Wakes the sampler thread for an immediate refresh (after a stop).
    pub wake: Mutex<Sender<()>>,
}

impl AppState {
    pub fn latest(&self) -> Option<Snapshot> {
        self.snapshot.lock().unwrap().clone()
    }

    /// Drop cached crawl/docker data and resample now.
    pub fn refresh_now(&self) {
        self.sampler.lock().unwrap().invalidate();
        let _ = self.wake.lock().unwrap().send(());
    }
}

pub fn show_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

fn sampler_loop(app: AppHandle, wake: std::sync::mpsc::Receiver<()>) {
    let mut notifier = notify::Notifier::default();
    loop {
        let state = app.state::<AppState>();
        let snap = state.sampler.lock().unwrap().sample();
        *state.snapshot.lock().unwrap() = Some(snap.clone());
        let _ = app.emit("snapshot", &snap);
        tray::update(&app, &snap);
        notifier.check(&app, &state.config, &snap);

        let refresh = Duration::from_secs(state.config.refresh_secs.max(1));
        match wake.recv_timeout(refresh) {
            Ok(()) => {
                // Coalesce bursts of wake-ups (e.g. a reap stopping several).
                while wake.try_recv().is_ok() {}
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let config = Config::load();
    let (wake_tx, wake_rx) = channel();
    let state = AppState {
        sampler: Mutex::new(Sampler::new(config.clone()).with_history()),
        config,
        snapshot: Mutex::new(None),
        wake: Mutex::new(wake_tx),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_window(app)))
        .plugin(tauri_plugin_notification::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            commands::get_snapshot,
            commands::stop_process,
            commands::kill_process,
            commands::reap,
            commands::open_url,
            commands::reveal_path,
            commands::copy_env,
        ])
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            tray::create(app.handle())?;
            let handle = app.handle().clone();
            std::thread::Builder::new()
                .name("portman-sampler".into())
                .spawn(move || sampler_loop(handle, wake_rx))?;
            Ok(())
        })
        .on_window_event(|window, event| {
            // A menu-bar app keeps running when its window closes.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running Portman");
}
