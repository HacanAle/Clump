//! Background loop: refreshes usage, sends "hi" the moment the 5-hour window resets, and
//! (optionally) keeps a wake timer armed so a sleeping PC wakes up for the reset.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED};
use windows::Win32::System::Threading::{CancelWaitableTimer, CreateWaitableTimerExW, SetWaitableTimer};

use crate::api::{self, ApiErr};
use crate::journal;
use crate::model::{now, save_json, Cmd, Core, LastUsage, Limit, PingRecord, Snapshot, Status};
use crate::ping;
use crate::source::{self, Source};

const FIVE_HOURS: i64 = 5 * 3600;
/// Seconds after the reported reset before we send "hi": lets the server roll the window over
/// and covers a PC clock that runs a few seconds behind.
const RESET_SLACK: i64 = 10;
const PING_BACKOFF: [i64; 6] = [20, 40, 90, 180, 300, 600];

/// Does the account need a new session started right now?
pub fn needs_session(s: &Snapshot, t: i64) -> bool {
    if matches!(s.status, Status::SignedOut | Status::Loading) {
        return false;
    }
    match s.session {
        Some(Limit { resets_at: Some(r), .. }) => t >= r + RESET_SLACK,
        // The account reports no active window.
        Some(Limit { resets_at: None, .. }) => true,
        // Unknown. Only act when the cause is an expired token: a "hi" also refreshes it.
        // (Network errors / rate limits just wait for data.)
        None => s.token_expired,
    }
}

pub fn run(core: Arc<Core>, rx: Receiver<Cmd>, demo: Option<f64>) {
    if let Some(pct) = demo {
        return run_demo(core, rx, pct);
    }
    let mut src: Option<Source> = None;
    let mut discovered_at = i64::MIN / 2;
    let mut next_poll = 0i64;
    let mut poll_backoff = 0u32;
    let mut next_auto_ping = 0i64;
    let mut ping_fails = 0usize;
    let mut wake = WakeTimer::default();
    let mut keep_awake = false;
    let mut claude_checked = 0i64;
    // A due hi that couldn't go out for lack of `claude` was already reported.
    let mut skip_reported = false;

    loop {
        let mut force_ping = false;
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Cmd::Refresh) => {
                next_poll = 0;
                if src.is_none() {
                    discovered_at = i64::MIN / 2;
                }
            }
            Ok(Cmd::PingNow) => force_ping = true,
            Ok(Cmd::SettingsChanged) => {}
            Ok(Cmd::PanelShown) => {
                // The usage endpoint rate-limits hard: only refresh on open if data is a minute old.
                if now() - core.snapshot().updated_at.unwrap_or(0) > 60 {
                    next_poll = 0;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        let t = now();

        // Find (or re-check) the Claude Code login.
        let every = if src.is_some() { 1800 } else { 60 };
        if t - discovered_at >= every {
            discovered_at = t;
            match source::discover() {
                Some(found) => {
                    if src.as_ref().map_or(true, |s| s.creds != found.creds || s.claude != found.claude) {
                        journal::line(format!(
                            "using Claude Code login: {} ({}), claude: {}",
                            found.label,
                            found.creds.display(),
                            found.claude.as_deref().unwrap_or("not found")
                        ));
                    }
                    core.with(|s| s.source = found.label.clone());
                    core.set_source(found.clone());
                    src = Some(found);
                }
                None if src.is_none() => journal::line("no Claude Code login found"),
                None => {}
            }
            next_poll = next_poll.min(t);
        }

        // No `claude` yet (or an extension update took its folder away): look again every minute,
        // so a CLI installed while Clump runs is used without a restart.
        if let Some(s) = src.as_mut().filter(|s| !source::claude_usable(s)) {
            if t - claude_checked >= 60 {
                claude_checked = t;
                if source::recheck_claude(s) {
                    journal::line(format!("claude: {}", s.claude.as_deref().unwrap_or("not found")));
                    core.set_source(s.clone());
                }
            }
        }

        if t >= next_poll {
            next_poll = t + poll(&core, src.as_ref(), &mut poll_backoff);
            core.publish();
        }

        // Wake timer for the next reset + stay awake while a reset is being handled.
        let snap = core.snapshot();
        let wake_mode = snap.settings.wake_from_sleep && snap.settings.auto_session;
        let reset_at = snap.session.and_then(|l| l.resets_at);
        wake.arm(if wake_mode { reset_at.filter(|&r| r > t).map(|r| r + 15) } else { None });
        let due = needs_session(&snap, t);
        let awake = wake_mode && due && reset_at.map_or(false, |r| t - r < 600);
        if awake != keep_awake {
            keep_awake = awake;
            unsafe {
                SetThreadExecutionState(if awake { ES_CONTINUOUS | ES_SYSTEM_REQUIRED } else { ES_CONTINUOUS });
            }
        }

        let can_ping = src.as_ref().map_or(false, source::claude_usable);
        let no_cli = src.is_some() && !can_ping;
        if snap.no_cli != no_cli {
            core.with(|s| s.no_cli = no_cli);
            core.publish();
        }
        // A hi is due but there is nothing to send it with: say so once per missed reset
        // (in the log and in Recent) instead of skipping it silently.
        if !(snap.settings.auto_session && due && no_cli) {
            skip_reported = false;
        } else if !skip_reported {
            skip_reported = true;
            let trigger = if snap.session.is_none() { "startup" } else { "reset" };
            journal::line(format!("hi skipped ({trigger}): {}", ping::NO_CLI));
            core.with(|s| {
                s.pings.insert(0, PingRecord { at: t, ok: false, ms: 0, detail: ping::NO_CLI.into(), trigger: trigger.into() });
                s.pings.truncate(10);
            });
            core.save_pings();
            core.publish();
        }
        if !(force_ping || (snap.settings.auto_session && due && can_ping && t >= next_auto_ping)) {
            continue;
        }
        let trigger = if force_ping { "manual" } else if snap.session.is_none() { "startup" } else { "reset" };
        core.with(|s| s.pinging = true);
        core.publish();
        let result = match &src {
            Some(s) => ping::send_hi(s),
            None => Err("No Claude Code login found".into()),
        };
        let t2 = now();
        let record = match &result {
            Ok((ms, reply)) => {
                ping_fails = 0;
                // If the API still shows no new window after this, try again in 5 minutes.
                next_auto_ping = t2 + 300;
                next_poll = t2 + 20;
                journal::line(format!("hi sent ({trigger}) in {ms} ms, reply: {reply}"));
                PingRecord { at: t2, ok: true, ms: *ms, detail: "Session started".into(), trigger: trigger.into() }
            }
            Err(e) => {
                next_auto_ping = t2 + PING_BACKOFF[ping_fails.min(PING_BACKOFF.len() - 1)];
                ping_fails += 1;
                journal::line(format!("hi failed ({trigger}): {e}"));
                PingRecord { at: t2, ok: false, ms: 0, detail: e.clone(), trigger: trigger.into() }
            }
        };
        let ok = record.ok;
        core.with(|s| {
            s.pinging = false;
            s.pings.insert(0, record);
            s.pings.truncate(10);
            // Until the API confirms it, assume a fresh window started now.
            if ok && due {
                s.session = Some(Limit { pct: 0.0, resets_at: Some(t2 + FIVE_HOURS) });
                s.session_estimated = true;
            }
        });
        core.save_pings();
        core.publish();
    }
}

/// Refresh usage. Returns seconds until the next refresh.
fn poll(core: &Core, src: Option<&Source>, backoff: &mut u32) -> i64 {
    let normal = if crate::panel::is_visible() { 60 } else { 180 };
    let Some(src) = src else {
        core.with(|s| {
            s.status = Status::SignedOut;
            s.message = "No Claude Code login found. Sign in to Claude Code, then refresh.".into();
        });
        return 60;
    };
    let Some(c) = source::read_creds(&src.creds) else {
        stale(core, "Couldn't read the Claude Code login file");
        return 30;
    };
    if !c.plan.is_empty() {
        core.with(|s| s.plan = c.plan.clone());
    }
    if c.signed_out {
        core.with(|s| {
            s.status = Status::SignedOut;
            s.message = "Claude Code is signed out. Run claude and sign in again.".into();
        });
        return 60;
    }
    if c.token.is_empty() || c.expires_at <= now() + 30 {
        // Claude Code refreshes its token on the next message; the next "hi" does that too.
        core.with(|s| s.token_expired = true);
        stale(core, "Login token expired; it refreshes with the next message");
        return 60;
    }
    match api::fetch(&c.token) {
        Ok(u) => {
            *backoff = 0;
            let last = core.with(|s| {
                s.status = Status::Ok;
                s.message.clear();
                s.token_expired = false;
                s.session = u.session;
                s.session_estimated = false;
                s.weekly = u.weekly;
                s.updated_at = Some(now());
                LastUsage { session: s.session, weekly: s.weekly, updated_at: s.updated_at }
            });
            save_json(&core.dir.join("last-usage.json"), &last);
            normal
        }
        Err(ApiErr::Unauthorized) => {
            core.with(|s| s.token_expired = true);
            stale(core, "The usage service refused the login token");
            60
        }
        Err(ApiErr::RateLimited(retry_after)) => {
            *backoff = (*backoff + 1).min(3);
            stale(core, "Usage service is busy; trying again shortly");
            retry_after.unwrap_or(120i64 << *backoff).clamp(60, 900)
        }
        Err(ApiErr::Other(m)) => {
            stale(core, &m);
            60
        }
    }
}

/// A failed refresh only counts as stale once the last good data is 10+ minutes old.
fn stale(core: &Core, msg: &str) {
    core.with(|s| {
        let fresh = s.status == Status::Ok && s.updated_at.map_or(false, |u| now() - u < 600);
        if !fresh {
            s.status = Status::Stale;
        }
        s.message = msg.to_string();
    });
}

#[derive(Default)]
struct WakeTimer {
    handle: Option<HANDLE>,
    armed: Option<i64>,
}

impl WakeTimer {
    fn arm(&mut self, at: Option<i64>) {
        // The reported reset time wobbles by a second between readings.
        let close = match (self.armed, at) {
            (Some(a), Some(b)) => (a - b).abs() <= 60,
            (a, b) => a == b,
        };
        if close {
            return;
        }
        unsafe {
            if self.handle.is_none() {
                // TIMER_ALL_ACCESS
                self.handle = CreateWaitableTimerExW(None, PCWSTR::null(), 0, 0x1F_0003).ok();
            }
            let Some(h) = self.handle else { return };
            match at {
                Some(t) => {
                    // Absolute UTC FILETIME (100 ns since 1601).
                    let due: i64 = (t + 11_644_473_600) * 10_000_000;
                    match SetWaitableTimer(h, &due, 0, None, None, true) {
                        Ok(()) => journal::line(format!("wake timer set for {}", fmt_local(t))),
                        Err(e) => journal::line(format!("wake timer failed: {e}")),
                    }
                }
                None => {
                    let _ = CancelWaitableTimer(h);
                    if self.armed.is_some() {
                        journal::line("wake timer cleared");
                    }
                }
            }
        }
        self.armed = at;
    }
}

pub fn fmt_local(t: i64) -> String {
    use chrono::TimeZone;
    chrono::Local.timestamp_opt(t, 0).single().map(|d| d.format("%H:%M").to_string()).unwrap_or_default()
}

/// `--demo=<pct>`: fake data for screenshots; never touches the network or sends anything.
fn run_demo(core: Arc<Core>, rx: Receiver<Cmd>, pct: f64) {
    let t = now();
    core.with(|s| {
        s.demo = true;
        s.status = Status::Ok;
        s.plan = "pro".into();
        s.source = "WSL · Ubuntu".into();
        s.session = Some(Limit { pct, resets_at: Some(t + 83 * 60 + 20) });
        s.weekly = Some(Limit { pct: 66.0, resets_at: Some(t + 4 * 86400 + 5 * 3600) });
        s.updated_at = Some(t);
        if s.settings.cloud_backup {
            s.cloud_next = Some(crate::cloud::plan(t + 83 * 60 + 20)[0]);
        }
        if s.pings.is_empty() {
            s.pings = vec![PingRecord { at: t - 3 * 3600 - 37 * 60, ok: true, ms: 4120, detail: "Session started".into(), trigger: "reset".into() }];
        }
    });
    core.publish();
    loop {
        match rx.recv_timeout(Duration::from_secs(30)) {
            Err(RecvTimeoutError::Disconnected) => return,
            _ => core.publish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(status: Status, session: Option<Limit>, token_expired: bool) -> Snapshot {
        Snapshot { status, session, token_expired, ..Default::default() }
    }

    #[test]
    fn active_session_waits_for_reset_plus_slack() {
        let s = snap(Status::Ok, Some(Limit { pct: 40.0, resets_at: Some(1000) }), false);
        assert!(!needs_session(&s, 900));
        assert!(!needs_session(&s, 1009));
        assert!(needs_session(&s, 1010));
    }

    #[test]
    fn no_active_window_needs_a_session() {
        assert!(needs_session(&snap(Status::Ok, Some(Limit { pct: 0.0, resets_at: None }), false), 5));
    }

    #[test]
    fn unknown_usage_only_pings_when_the_token_expired() {
        assert!(!needs_session(&snap(Status::Ok, None, false), 5), "plan without a 5-hour limit");
        assert!(!needs_session(&snap(Status::Stale, None, false), 5), "rate limited / offline: wait");
        assert!(needs_session(&snap(Status::Stale, None, true), 5), "expired token: hi refreshes it");
    }

    #[test]
    fn signed_out_or_loading_never_pings() {
        let ended = Some(Limit { pct: 10.0, resets_at: Some(1) });
        assert!(!needs_session(&snap(Status::SignedOut, ended, true), 100));
        assert!(!needs_session(&snap(Status::Loading, None, true), 100));
    }

    #[test]
    fn stale_but_known_reset_still_pings_on_time() {
        let s = snap(Status::Stale, Some(Limit { pct: 80.0, resets_at: Some(1000) }), true);
        assert!(!needs_session(&s, 999));
        assert!(needs_session(&s, 1010));
    }
}
