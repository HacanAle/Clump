//! The flyout panel: opens above the taskbar widget (or the tray), hides when it loses focus.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tauri::{AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, WebviewWindow, WindowEvent};

use crate::model::{Cmd, Core};

/// Window width; the visible card is inset by INSET on every side (room for its soft shadow).
const WIDTH: f64 = 388.0;
const INSET: f64 = 14.0;

static VISIBLE: AtomicBool = AtomicBool::new(false);
static LAST_HIDE_MS: AtomicI64 = AtomicI64::new(0);
/// Screen rect (x, y, w, h) of whatever opened the panel; None = tray / no anchor.
static ANCHOR: Mutex<Option<(i32, i32, i32, i32)>> = Mutex::new(None);

pub fn is_visible() -> bool {
    VISIBLE.load(Ordering::SeqCst)
}

fn ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

pub fn setup(app: &tauri::App) {
    if let Some(w) = app.get_webview_window("panel") {
        let w2 = w.clone();
        w.on_window_event(move |e| match e {
            WindowEvent::Focused(false) => hide_window(&w2),
            WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                hide_window(&w2);
            }
            _ => {}
        });
    }
}

pub fn toggle(app: &AppHandle, anchor: Option<(i32, i32, i32, i32)>, force_show: bool) {
    let Some(w) = app.get_webview_window("panel") else { return };
    if is_visible() {
        if force_show {
            let _ = w.set_focus();
        } else {
            hide_window(&w);
        }
        return;
    }
    // The click that took focus away from the panel also lands here: don't reopen it.
    if !force_show && ms() - LAST_HIDE_MS.load(Ordering::SeqCst) < 350 {
        return;
    }
    *ANCHOR.lock().unwrap() = anchor;
    place(&w);
    let _ = w.show();
    let _ = w.set_focus();
    VISIBLE.store(true, Ordering::SeqCst);
    let _ = w.emit("panel-shown", ());
    if let Some(core) = app.try_state::<Arc<Core>>() {
        core.send(Cmd::PanelShown);
    }
}

pub fn hide(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("panel") {
        hide_window(&w);
    }
}

fn hide_window(w: &WebviewWindow) {
    if VISIBLE.swap(false, Ordering::SeqCst) {
        let _ = w.hide();
        let _ = w.emit("panel-hidden", ());
        LAST_HIDE_MS.store(ms(), Ordering::SeqCst);
    }
}

/// The page reports its content height; size the window to it and keep it docked.
pub fn fit(app: &AppHandle, height: f64) {
    let Some(w) = app.get_webview_window("panel") else { return };
    let _ = w.set_size(LogicalSize::new(WIDTH, height.clamp(120.0, 760.0).ceil()));
    place(&w);
}

fn place(w: &WebviewWindow) {
    let (Ok(size), Ok(Some(mon))) = (w.outer_size(), w.primary_monitor()) else { return };
    let scale = mon.scale_factor();
    let wa = mon.work_area();
    let (left, right) = (wa.position.x, wa.position.x + wa.size.width as i32);
    let bottom = wa.position.y + wa.size.height as i32;
    let margin = (12.0 * scale).round() as i32 - (INSET * scale).round() as i32;
    let (pw, ph) = (size.width as i32, size.height as i32);
    let x = match *ANCHOR.lock().unwrap() {
        Some((ax, _, aw, _)) => ax + aw / 2 - pw / 2,
        None => right - pw - margin,
    };
    let x = x.clamp(left + margin, (right - pw - margin).max(left + margin));
    // margin is negative here: the transparent inset may overlap the taskbar edge.
    let _ = w.set_position(PhysicalPosition::new(x, bottom - ph - margin));
}
