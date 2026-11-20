//! Shared state: the snapshot every view renders from, settings, and the command channel.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use crate::source::Source;

/// One usage limit as reported by the account: percent used and when the window resets (unix s).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct Limit {
    pub pct: f64,
    pub resets_at: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PingRecord {
    pub at: i64,
    pub ok: bool,
    pub ms: u64,
    pub detail: String,
    /// "reset", "startup" or "manual".
    pub trigger: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// Send "hi" as soon as the 5-hour window resets.
    pub auto_session: bool,
    /// Arm a wake timer so a sleeping PC wakes up for the reset.
    pub wake_from_sleep: bool,
    /// Show percent left instead of percent used.
    pub show_left: bool,
    pub show_widget: bool,
    /// Little animated buddy at the left of the taskbar widget.
    pub mascot: bool,
    /// "↻ 15:30" reset times in the taskbar widget.
    pub show_resets: bool,
    /// Panel colours: peach | mint | sky | lemon | night
    pub theme: String,
    /// Keep Claude Code cloud tasks booked to say hi at the resets while this PC is off.
    pub cloud_backup: bool,
    pub first_run_done: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            auto_session: true,
            wake_from_sleep: false,
            show_left: false,
            show_widget: true,
            mascot: true,
            show_resets: true,
            theme: "peach".into(),
            cloud_backup: false,
            first_run_done: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Loading,
    Ok,
    /// Last refresh failed; values (if any) are from `updated_at`.
    Stale,
    SignedOut,
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct Snapshot {
    pub status: Status,
    pub message: String,
    pub session: Option<Limit>,
    /// True right after we started a session and the API hasn't confirmed the new window yet.
    pub session_estimated: bool,
    pub weekly: Option<Limit>,
    pub updated_at: Option<i64>,
    /// Claude Code's access token has expired (it refreshes on its next message).
    pub token_expired: bool,
    pub plan: String,
    pub source: String,
    pub pinging: bool,
    pub pings: Vec<PingRecord>,
    pub settings: Settings,
    pub autostart: bool,
    pub demo: bool,
    /// Next cloud hi booked (only while cloud backup is on and booked).
    pub cloud_next: Option<i64>,
    /// Last cloud-backup problem, empty when fine.
    pub cloud_note: String,
}

/// Last good usage reading, kept on disk so a restart shows real numbers straight away.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct LastUsage {
    pub session: Option<Limit>,
    pub weekly: Option<Limit>,
    pub updated_at: Option<i64>,
}

pub enum Cmd {
    Refresh,
    PingNow,
    SettingsChanged,
    PanelShown,
}

pub struct Core {
    pub app: AppHandle,
    pub dir: PathBuf,
    snap: Mutex<Snapshot>,
    tx: Mutex<Sender<Cmd>>,
    source: Mutex<Option<Source>>,
}

impl Core {
    pub fn new(app: AppHandle, dir: PathBuf, snap: Snapshot, tx: Sender<Cmd>) -> Self {
        Self { app, dir, snap: Mutex::new(snap), tx: Mutex::new(tx), source: Mutex::new(None) }
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.tx.lock().unwrap().send(cmd);
    }

    /// The Claude Code login in use (found by the scheduler).
    pub fn source(&self) -> Option<Source> {
        self.source.lock().unwrap().clone()
    }

    pub fn set_source(&self, src: Source) {
        *self.source.lock().unwrap() = Some(src);
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snap.lock().unwrap().clone()
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut Snapshot) -> R) -> R {
        let mut guard = self.snap.lock().unwrap();
        f(&mut guard)
    }

    /// Push the current snapshot to the panel, the taskbar widget and the tray.
    /// Never call while holding the snapshot lock.
    pub fn publish(&self) {
        let s = self.snapshot();
        let _ = self.app.emit_to("panel", "snapshot", &s);
        crate::taskbar::update(crate::taskbar::Model::from_snapshot(&s));
        crate::tray::update(&self.app, &s);
    }

    pub fn save_settings(&self) {
        let s = self.snapshot().settings;
        save_json(&self.dir.join("settings.json"), &s);
    }

    pub fn save_pings(&self) {
        let p = self.snapshot().pings;
        save_json(&self.dir.join("pings.json"), &p);
    }
}

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub fn load_json<T: DeserializeOwned + Default>(path: &Path) -> T {
    std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

pub fn save_json<T: Serialize>(path: &Path, value: &T) {
    if let Ok(text) = serde_json::to_string_pretty(value) {
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}
