//! Notification-area icon: live ring gauge, tooltip, and a short menu.

use std::sync::{Arc, Mutex, OnceLock};

use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSMICON};

use crate::model::{Cmd, Core, Snapshot, Status};
use crate::scheduler::fmt_local;
use crate::{gauge, panel, taskbar};

static AUTO_ITEM: OnceLock<CheckMenuItem<Wry>> = OnceLock::new();
static LAST: Mutex<String> = Mutex::new(String::new());

fn core_of(app: &AppHandle) -> Arc<Core> {
    app.state::<Arc<Core>>().inner().clone()
}

pub fn create(app: &tauri::App, core: &Arc<Core>) -> tauri::Result<()> {
    let s = core.snapshot();
    let open = MenuItem::with_id(app, "open", "Open Clump", true, None::<&str>)?;
    let start = MenuItem::with_id(app, "start", "Say hi now (start a session)", true, None::<&str>)?;
    let auto = CheckMenuItem::with_id(app, "auto", "Say hi at each reset", true, s.settings.auto_session, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Clump", true, None::<&str>)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&open, &start, &sep1, &auto, &sep2, &quit])?;
    let (rgba, n, _) = icon(&s);
    TrayIconBuilder::with_id("main")
        .icon(Image::new_owned(rgba, n, n))
        .tooltip("Clump")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, ev| match ev.id().as_ref() {
            "open" => panel::toggle(app, None, true),
            "start" => core_of(app).send(Cmd::PingNow),
            "auto" => {
                let c = core_of(app);
                c.with(|s| s.settings.auto_session = !s.settings.auto_session);
                c.save_settings();
                c.send(Cmd::SettingsChanged);
                c.publish();
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, ev| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = ev {
                panel::toggle(tray.app_handle(), None, false);
            }
        })
        .build(app)?;
    let _ = AUTO_ITEM.set(auto);
    Ok(())
}

fn icon(s: &Snapshot) -> (Vec<u8>, u32, String) {
    let n = unsafe { GetSystemMetrics(SM_CXSMICON) }.clamp(16, 64) as u32;
    let light = taskbar::light_taskbar();
    let level = match s.session {
        Some(l) if l.resets_at.is_some() => Some(taskbar::level(l.pct)),
        _ => None,
    };
    let rgb = |v: u32| [(v >> 16) as u8, (v >> 8) as u8, v as u8];
    let dot = level.map(|l| rgb(taskbar::level_rgb(l, light)));
    let key = format!("{n}|{light}|{level:?}");
    (gauge::tray_icon(n, dot), n, key)
}

pub fn tooltip(s: &Snapshot) -> String {
    if s.pinging {
        return "Claude: starting a new session…".into();
    }
    match s.status {
        Status::SignedOut => "Claude: sign in to Claude Code".into(),
        Status::Loading => "Claude: loading usage…".into(),
        _ => {
            let stale = if s.status == Status::Stale { " (not updated)" } else { "" };
            match s.session {
                Some(l) if l.resets_at.is_some() => {
                    format!("Claude: {}% used · resets {}{stale}", l.pct.round(), fmt_local(l.resets_at.unwrap_or(0)))
                }
                Some(_) => format!("Claude: no active session{stale}"),
                None => format!("Claude: usage unavailable{stale}"),
            }
        }
    }
}

pub fn update(app: &AppHandle, s: &Snapshot) {
    let (rgba, n, ikey) = icon(s);
    let tip = tooltip(s);
    let key = format!("{ikey}|{tip}");
    let changed = {
        let mut last = LAST.lock().unwrap();
        if *last == key {
            false
        } else {
            *last = key;
            true
        }
    };
    if changed {
        if let Some(t) = app.tray_by_id("main") {
            let _ = t.set_icon(Some(Image::new_owned(rgba, n, n)));
            let _ = t.set_tooltip(Some(tip));
        }
    }
    if let Some(item) = AUTO_ITEM.get() {
        let _ = item.set_checked(s.settings.auto_session);
    }
}
