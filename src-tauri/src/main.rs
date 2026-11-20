#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! Clump: shows the Claude 5-hour session limit in the Windows taskbar and starts a new
//! session (one tiny "hi" through Claude Code) the moment the previous one resets.

mod api;
mod cloud;
mod gauge;
mod journal;
mod model;
mod panel;
mod ping;
mod scheduler;
mod source;
mod taskbar;
mod tray;

use std::sync::{mpsc, Arc};

use model::{Cmd, Core, PingRecord, Settings, Snapshot};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

type CoreState<'a> = State<'a, Arc<Core>>;

#[tauri::command]
async fn get_snapshot(core: CoreState<'_>) -> Result<Snapshot, String> {
    Ok(core.snapshot())
}

#[tauri::command]
async fn update_settings(core: CoreState<'_>, patch: serde_json::Value) -> Result<Snapshot, String> {
    let mut merged = serde_json::to_value(core.snapshot().settings).map_err(|e| e.to_string())?;
    if let (Some(m), Some(p)) = (merged.as_object_mut(), patch.as_object()) {
        for (k, v) in p {
            if m.contains_key(k) && k != "first_run_done" {
                m.insert(k.clone(), v.clone());
            }
        }
    }
    let next: Settings = serde_json::from_value(merged).map_err(|e| e.to_string())?;
    core.with(|s| s.settings = next);
    core.save_settings();
    core.send(Cmd::SettingsChanged);
    core.publish();
    Ok(core.snapshot())
}

#[tauri::command]
async fn set_autostart(app: AppHandle, core: CoreState<'_>, on: bool) -> Result<Snapshot, String> {
    let al = app.autolaunch();
    if let Err(e) = if on { al.enable() } else { al.disable() } {
        journal::line(format!("start with Windows change failed: {e}"));
    }
    let enabled = al.is_enabled().unwrap_or(false);
    core.with(|s| s.autostart = enabled);
    core.publish();
    Ok(core.snapshot())
}

#[tauri::command]
async fn refresh(core: CoreState<'_>) -> Result<(), String> {
    core.send(Cmd::Refresh);
    Ok(())
}

#[tauri::command]
async fn start_session(core: CoreState<'_>) -> Result<(), String> {
    core.send(Cmd::PingNow);
    Ok(())
}

#[tauri::command]
async fn fit_panel(app: AppHandle, height: f64) -> Result<(), String> {
    panel::fit(&app, height);
    Ok(())
}

#[tauri::command]
async fn hide_panel(app: AppHandle) -> Result<(), String> {
    panel::hide(&app);
    Ok(())
}

#[tauri::command]
async fn quit(app: AppHandle) -> Result<(), String> {
    app.exit(0);
    Ok(())
}

fn main() {
    // --demo=<pct>: fake numbers for screenshots; no network, no messages sent.
    let demo = std::env::args().find_map(|a| a.strip_prefix("--demo=").and_then(|v| v.parse::<f64>().ok()));

    let mut builder = tauri::Builder::default();
    if demo.is_none() {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| panel::toggle(app, None, true)));
    }
    builder
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, Some(vec!["--background"])))
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            update_settings,
            set_autostart,
            refresh,
            start_session,
            fit_panel,
            hide_panel,
            quit
        ])
        .setup(move |app| {
            let mut dir = app.path().app_config_dir()?;
            if demo.is_some() {
                dir = dir.join("demo");
            }
            std::fs::create_dir_all(&dir)?;
            journal::init(dir.join("clump.log"));
            journal::line(format!("Clump {} started{}", env!("CARGO_PKG_VERSION"), if demo.is_some() { " (demo)" } else { "" }));

            let settings_path = dir.join("settings.json");
            let mut settings: Settings = model::load_json(&settings_path);
            let autolaunch = app.autolaunch();
            if !settings.first_run_done && demo.is_none() {
                // This app only helps while it runs, so it starts with Windows unless turned off.
                if let Err(e) = autolaunch.enable() {
                    journal::line(format!("could not enable start with Windows: {e}"));
                }
                settings.first_run_done = true;
                model::save_json(&settings_path, &settings);
            }
            let pings: Vec<PingRecord> = model::load_json(&dir.join("pings.json"));
            let last: model::LastUsage = model::load_json(&dir.join("last-usage.json"));
            let fresh = last.updated_at.map_or(false, |u| model::now() - u < 600);
            let snap = Snapshot {
                status: match (last.updated_at, fresh) {
                    (None, _) => model::Status::Loading,
                    (Some(_), true) => model::Status::Ok,
                    (Some(_), false) => model::Status::Stale,
                },
                session: last.session,
                weekly: last.weekly,
                updated_at: last.updated_at,
                settings,
                pings,
                autostart: autolaunch.is_enabled().unwrap_or(false),
                ..Default::default()
            };

            let (tx, rx) = mpsc::channel();
            let core = Arc::new(Core::new(app.handle().clone(), dir, snap, tx));
            app.manage(core.clone());
            tray::create(app, &core)?;
            panel::setup(app);
            let handle = app.handle().clone();
            taskbar::start(move |rect| {
                let h = handle.clone();
                let _ = handle.run_on_main_thread(move || panel::toggle(&h, Some(rect), false));
            });
            core.publish();
            if demo.is_none() {
                let c = core.clone();
                std::thread::Builder::new().name("cloud".into()).spawn(move || cloud::run(c))?;
            }
            std::thread::Builder::new().name("scheduler".into()).spawn(move || scheduler::run(core, rx, demo))?;
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to start Clump")
        .run(|_, event| {
            // Hiding the panel must never end the app; only Quit (exit code set) does.
            if let tauri::RunEvent::ExitRequested { api, code, .. } = event {
                if code.is_none() {
                    api.prevent_exit();
                }
            }
        });
}
