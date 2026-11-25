//! Cloud backup: Claude Code cloud tasks ("routines" on claude.ai) that say hi at the coming resets
//! in case this PC is off. Clump keeps them booked a little after each expected reset; when it says
//! hi itself, the new reset time moves every booking forward, so the cloud only fires when the PC
//! could not. Uses the same claude.ai endpoints as Claude Code's /schedule, with the same login.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::journal;
use crate::model::{load_json, now, save_json, Core, PingRecord};
use crate::source;

const BASE: &str = "https://api.anthropic.com";
const BETA_TRIGGERS: &str = "ccr-triggers-2026-01-30";
const BETA_ENVS: &str = "environments-2025-11-01";
pub const MODEL: &str = "claude-haiku-5-5";
const NAME: &str = "Clump hi";
/// Coming resets kept booked: four 5-hour windows ≈ 20 hours with the PC off.
pub const HOPS: usize = 4;
const FIVE_HOURS: i64 = 5 * 3600;
/// The first cloud hi goes this long after the reset. Clump's own hi (10 s after the reset) moves
/// the bookings well before that whenever the PC is on.
const FIRST_DELAY: i64 = 3 * 60;
/// A cloud run can start several minutes late, which pushes the following reset later too, so each
/// later booking gets this much more room. Late costs a few minutes; early would waste a window.
const HOP_SLACK: i64 = 15 * 60;
/// Bookings within this of the plan are left alone (estimated vs. confirmed reset times differ a little).
const TOLERANCE: i64 = 90;
const RETRY: [i64; 5] = [60, 120, 300, 600, 900];
/// Shown until the account has a cloud environment (claude.ai/code makes one on the first visit).
/// Checked every minute meanwhile, so the backup starts soon after.
const NO_ENV: &str = "no cloud environment yet. Open claude.ai/code once to make one";
/// The built-in tools a cloud task gets by default (Nov 2026). An empty `allowed_tools` means "the
/// default set", and a cloud session only leaves a tool out when `disallowed_tools` names it.
/// `lock_tools` goes by the list the server stores with the task; this is only the fallback.
const NO_TOOLS: [&str; 19] = [
    "Task", "Bash", "Glob", "Grep", "Read", "Edit", "MultiEdit", "Write", "NotebookEdit", "WebFetch", "TodoWrite",
    "WebSearch", "BashOutput", "KillBash", "Skill", "Tmux", "Monitor", "SendUserFile", "REPL",
];

/// When each cloud hi should run for a session that resets at `reset`.
pub fn plan(reset: i64) -> [i64; HOPS] {
    std::array::from_fn(|k| reset + FIRST_DELAY + k as i64 * (FIVE_HOURS + HOP_SLACK))
}

fn same(booked: &[i64], want: &[i64; HOPS]) -> bool {
    booked.len() == HOPS && booked.iter().zip(want).all(|(a, b)| (a - b).abs() <= TOLERANCE)
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Task {
    id: String,
    /// `last_run.fired_at` already reported in Recent.
    seen_fire: String,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct State {
    env: String,
    tasks: Vec<Task>,
    /// Run times last sent, one per hop; empty when unknown.
    booked: Vec<i64>,
    /// Whether the tasks are switched on at claude.ai.
    enabled: bool,
}

struct Auth {
    token: String,
    org: String,
}

pub fn run(core: Arc<Core>) {
    let path = core.dir.join("cloud.json");
    // No cloud.json: this PC has no record of the tasks (first run, or Clump's data was deleted).
    // Tasks left on from before would keep firing with nothing moving them, so unless the backup
    // is on (then `verify` takes them over by name), switch them off once.
    let mut sweep = !path.exists();
    let mut st: State = load_json(&path);
    let (mut next_try, mut fails, mut next_check) = (0i64, 0usize, 0i64);
    loop {
        std::thread::sleep(Duration::from_secs(10));
        let t = now();
        let s = core.snapshot();
        // Signed out: keep whatever is booked; the login check below just waits.
        let on = s.settings.cloud_backup && s.settings.auto_session;
        sweep &= !on;
        let want = s.session.and_then(|l| l.resets_at).map(plan);
        // Never move a booking that is about to fire: if the reset passed and this PC hasn't said
        // hi yet, the cloud is the backup.
        let book = on && want.map_or(false, |w| w[0] >= t + 60 && !(st.enabled && same(&st.booked, &w)));
        let disable = !on && st.enabled;
        let check = on && t >= next_check;
        if t < next_try || !(book || disable || check || sweep) {
            let next = (on && st.enabled).then(|| st.booked.iter().copied().find(|&b| b > t - 60)).flatten();
            if s.cloud_next != next {
                core.with(|x| x.cloud_next = next);
                core.publish();
            }
            continue;
        }
        let Some(auth) = auth(&core) else {
            next_try = t + 60;
            continue;
        };
        let result = (|| -> Result<(), String> {
            if sweep {
                switch_off_leftovers(&auth, &mut st)?;
                return Ok(());
            }
            if check || st.env.is_empty() || st.tasks.len() < HOPS {
                verify(&core, &auth, &mut st)?;
                next_check = t + 1800;
            }
            if disable {
                for task in &st.tasks {
                    post(&auth, &format!("/v1/code/triggers/{}", task.id), &json!({ "enabled": false }))?;
                }
                st.enabled = false;
                st.booked.clear();
                journal::line("cloud backup off: tasks disabled");
            }
            if let Some(w) = want.filter(|w| on && w[0] >= t + 60) {
                if !(st.enabled && same(&st.booked, &w)) {
                    for (task, &at) in st.tasks.iter().zip(&w) {
                        post(&auth, &format!("/v1/code/triggers/{}", task.id), &json!({ "run_once_at": rfc3339(at), "enabled": true }))?;
                    }
                    st.booked = w.to_vec();
                    st.enabled = true;
                    journal::line(format!("cloud hi booked for {}", w.iter().map(|&a| crate::scheduler::fmt_local(a)).collect::<Vec<_>>().join(", ")));
                }
            }
            Ok(())
        })();
        let note = match result {
            Ok(()) => {
                fails = 0;
                next_try = 0;
                sweep = false;
                String::new()
            }
            Err(e) => {
                next_try = t + if e == NO_ENV { 60 } else { RETRY[fails.min(RETRY.len() - 1)] };
                fails += 1;
                journal::line(format!("cloud backup: {e}"));
                // A half-applied booking must be redone.
                st.booked.clear();
                e
            }
        };
        save_json(&path, &st);
        let next = (on && st.enabled).then(|| st.booked.iter().copied().find(|&b| b > t - 60)).flatten();
        core.with(|x| {
            x.cloud_next = next;
            x.cloud_note = note;
        });
        core.publish();
    }
}

/// The Claude Code login (token + organization) Clump already reads usage with.
fn auth(core: &Core) -> Option<Auth> {
    let src = core.source()?;
    let c = source::read_creds(&src.creds)?;
    if c.signed_out || c.token.is_empty() || c.expires_at <= now() + 30 {
        return None;
    }
    // ~/.claude/.credentials.json → ~/.claude.json
    let cfg = src.creds.parent()?.parent()?.join(".claude.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(cfg).ok()?).ok()?;
    let org = v.pointer("/oauthAccount/organizationUuid")?.as_str()?.to_string();
    Some(Auth { token: c.token, org })
}

/// Find (or create) the cloud environment and the tasks; report cloud runs in Recent; drop the
/// bookings if the tasks no longer match them (fired, edited or deleted on claude.ai).
fn verify(core: &Core, auth: &Auth, st: &mut State) -> Result<(), String> {
    if st.env.is_empty() {
        let envs = get(auth, "/v1/environments", BETA_ENVS)?;
        let list = envs["data"].as_array().cloned().unwrap_or_default();
        let cloud: Vec<&Value> = list.iter().filter(|e| e["config"]["type"] == "anthropic_cloud" && e["state"] == "active").collect();
        let env = cloud.iter().find(|e| e["name"] == "Default").or(cloud.first()).ok_or(NO_ENV)?;
        st.env = env["id"].as_str().unwrap_or_default().to_string();
    }
    let list = get(auth, "/v1/code/triggers", BETA_TRIGGERS)?;
    let triggers = list["data"].as_array().cloned().unwrap_or_default();
    let mut tasks = Vec::with_capacity(HOPS);
    let mut fired = Vec::new();
    let mut matches = true;
    for k in 0..HOPS {
        let name = format!("{NAME} {}", k + 1);
        let known = st.tasks.get(k).cloned().unwrap_or_default();
        let found = triggers.iter().find(|x| !known.id.is_empty() && x["id"] == known.id.as_str()).or_else(|| triggers.iter().find(|x| x["name"] == name.as_str()));
        let task = match found {
            Some(x) => {
                let id = x["id"].as_str().unwrap_or_default().to_string();
                let run = &x["last_run"];
                let fire = run["fired_at"].as_str().unwrap_or_default().to_string();
                let done = !run["finished_at"].as_str().unwrap_or_default().is_empty();
                let mut seen = if id == known.id { known.seen_fire.clone() } else { fire.clone() };
                if !fire.is_empty() && fire != seen && done {
                    let ok = run["status"].as_str().unwrap_or_default().ends_with("SUCCEEDED");
                    let (a, b) = (ts(&fire), ts(run["finished_at"].as_str().unwrap_or_default()));
                    fired.push(PingRecord {
                        at: a,
                        ok,
                        ms: ((b - a).max(0) * 1000) as u64,
                        detail: if ok { "Cloud hi (Haiku)".into() } else { format!("Cloud hi: {}", run["failure_reason"].as_str().unwrap_or("failed")) },
                        trigger: "cloud".into(),
                    });
                    journal::line(format!("cloud hi ran at {} ({})", crate::scheduler::fmt_local(a), if ok { "ok" } else { "failed" }));
                    seen = fire;
                }
                // Also fixes tasks made by Clump 1.3.0, which still have every default tool.
                lock_tools(auth, x);
                let booked_at = st.booked.get(k).copied();
                let next = x["next_run_at"].as_str().map(ts);
                if x["enabled"] != true || booked_at.zip(next).map_or(true, |(b, n)| (b - n).abs() > TOLERANCE) {
                    matches = false;
                }
                Task { id, seen_fire: seen }
            }
            None => {
                matches = false;
                Task { id: create(auth, &st.env, &name)?, seen_fire: String::new() }
            }
        };
        tasks.push(task);
    }
    st.tasks = tasks;
    if !matches && st.enabled {
        st.booked.clear();
    }
    if !fired.is_empty() {
        core.with(|s| {
            s.pings.extend(fired);
            s.pings.sort_by(|a, b| b.at.cmp(&a.at));
            s.pings.truncate(10);
        });
        core.save_pings();
    }
    Ok(())
}

/// New task: Haiku, no tools, no connectors, prompt "hi". Created switched off; booking arms it.
fn create(auth: &Auth, env: &str, name: &str) -> Result<String, String> {
    let body = json!({
        "name": name,
        "run_once_at": rfc3339(now() + 86_400),
        "enabled": false,
        "clear_mcp_connections": true,
        "job_config": { "ccr": {
            "environment_id": env,
            // Tools are switched off right after, from the list the server stores (`lock_tools`).
            "session_context": { "model": MODEL, "sources": [] },
            "events": [{ "data": {
                "uuid": uuid4(), "session_id": "", "type": "user", "parent_tool_use_id": null,
                "message": { "content": "hi", "role": "user" }
            }}]
        }}
    });
    let v = post(auth, "/v1/code/triggers", &body)?;
    let task = if v["trigger"].is_object() { &v["trigger"] } else { &v };
    let id = task["id"].as_str().ok_or("claude.ai did not return the new task")?.to_string();
    journal::line(format!("cloud task created: {name}"));
    lock_tools(auth, task);
    Ok(id)
}

/// The tools a task can still use: what its stored `allowed_tools` lists (the server writes the
/// full default set there; `NO_TOOLS` if it lists none) minus its `disallowed_tools`.
fn tools_left(task: &Value) -> Vec<String> {
    let ctx = &task["job_config"]["ccr"]["session_context"];
    let names = |v: &Value| v.as_array().map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect::<Vec<_>>()).unwrap_or_default();
    let denied = names(&ctx["disallowed_tools"]);
    let mut allowed: Vec<String> = names(&ctx["allowed_tools"]).into_iter().filter(|t| !t.starts_with("preset:")).collect();
    if allowed.is_empty() {
        allowed = NO_TOOLS.map(str::to_string).to_vec();
    }
    let mut left: Vec<String> = Vec::new();
    for tool in allowed {
        if !tool.starts_with("preset:") && !denied.contains(&tool) && !left.contains(&tool) {
            left.push(tool);
        }
    }
    left
}

/// Make sure a task can't use any tool, including tools added to the default set after this
/// version of Clump. Sends the task's own job config back with the missing names disallowed.
/// A failure is logged and tried again at the next check; it never holds up the booking.
fn lock_tools(auth: &Auth, task: &Value) {
    let left = tools_left(task);
    let mut ccr = task["job_config"]["ccr"].clone();
    if left.is_empty() || !ccr["session_context"].is_object() {
        return;
    }
    let ctx = &mut ccr["session_context"];
    let mut denied = ctx["disallowed_tools"].as_array().cloned().unwrap_or_default();
    denied.extend(left.into_iter().map(Value::from));
    ctx["disallowed_tools"] = Value::Array(denied);
    if let Some(c) = ctx.as_object_mut() {
        // The server's expanded default list; leaving it out lets the server work it out again.
        c.remove("allowed_tools");
    }
    let id = task["id"].as_str().unwrap_or_default();
    let name = task["name"].as_str().unwrap_or(id);
    match post(auth, &format!("/v1/code/triggers/{id}"), &json!({ "job_config": { "ccr": ccr } })) {
        Ok(_) => journal::line(format!("cloud task {name}: tools switched off")),
        Err(e) => journal::line(format!("cloud task {name}: could not switch tools off: {e}")),
    }
}

/// Switch off every "Clump hi" task still on at claude.ai and remember them, so a later
/// `verify` takes them over instead of making new ones.
fn switch_off_leftovers(auth: &Auth, st: &mut State) -> Result<(), String> {
    let list = get(auth, "/v1/code/triggers", BETA_TRIGGERS)?;
    let triggers = list["data"].as_array().cloned().unwrap_or_default();
    let mut tasks = Vec::with_capacity(HOPS);
    for k in 0..HOPS {
        let name = format!("{NAME} {}", k + 1);
        // Keep slot k for hop k even when a task is missing: `verify` matches by position.
        let Some(x) = triggers.iter().find(|x| x["name"] == name.as_str()) else {
            tasks.push(Task::default());
            continue;
        };
        let id = x["id"].as_str().unwrap_or_default().to_string();
        if x["enabled"] == true {
            post(auth, &format!("/v1/code/triggers/{id}"), &json!({ "enabled": false }))?;
            journal::line(format!("cloud task left on from before switched off: {name}"));
        }
        // Earlier runs are old news; don't list them in Recent.
        tasks.push(Task { id, seen_fire: x["last_run"]["fired_at"].as_str().unwrap_or_default().to_string() });
    }
    st.tasks = tasks;
    st.enabled = false;
    st.booked.clear();
    Ok(())
}

fn get(auth: &Auth, path: &str, beta: &str) -> Result<Value, String> {
    send(auth, "GET", path, beta, None)
}

fn post(auth: &Auth, path: &str, body: &Value) -> Result<Value, String> {
    send(auth, "POST", path, BETA_TRIGGERS, Some(body))
}

fn send(auth: &Auth, method: &str, path: &str, beta: &str, body: Option<&Value>) -> Result<Value, String> {
    let req = crate::api::agent()
        .request(method, &format!("{BASE}{path}"))
        .set("Authorization", &format!("Bearer {}", auth.token))
        .set("anthropic-version", "2023-06-01")
        .set("anthropic-beta", beta)
        .set("x-organization-uuid", &auth.org)
        .set("Content-Type", "application/json");
    let resp = match body {
        Some(b) => req.send_string(&b.to_string()),
        None => req.call(),
    };
    match resp {
        Ok(r) => serde_json::from_str(&r.into_string().map_err(|e| e.to_string())?).map_err(|_| "unexpected answer from claude.ai".into()),
        Err(ureq::Error::Status(code, r)) => {
            let msg = r.into_string().ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v["error"]["message"].as_str().map(str::to_string));
            Err(match (code, msg) {
                (401 | 403, _) => "claude.ai refused the login (it refreshes with the next message)".into(),
                (429, _) => "claude.ai is busy; trying again shortly".into(),
                (_, Some(m)) => format!("claude.ai: {}", m.chars().take(120).collect::<String>()),
                (c, None) => format!("claude.ai answered HTTP {c}"),
            })
        }
        Err(ureq::Error::Transport(_)) => Err("no connection to claude.ai".into()),
    }
}

fn rfc3339(t: i64) -> String {
    chrono::DateTime::from_timestamp(t, 0).map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string()).unwrap_or_default()
}

fn ts(s: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(s).map(|d| d.timestamp()).unwrap_or(0)
}

/// Random v4 UUID for the task's message (only needs to be unique).
fn uuid4() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut b = [0u8; 16];
    for (i, chunk) in b.chunks_mut(8).enumerate() {
        let mut h = RandomState::new().build_hasher();
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        h.write_usize(i);
        chunk.copy_from_slice(&h.finish().to_le_bytes());
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let x: String = b.iter().map(|v| format!("{v:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &x[0..8], &x[8..12], &x[12..16], &x[16..20], &x[20..32])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_books_each_reset_with_growing_room() {
        let p = plan(1_000_000);
        assert_eq!(p[0], 1_000_000 + 180, "first hi 3 min after the reset");
        assert_eq!(p[1] - p[0], 5 * 3600 + 15 * 60);
        assert_eq!(p[3] - p[0], 3 * (5 * 3600 + 15 * 60));
    }

    #[test]
    fn small_shifts_do_not_rebook() {
        let p = plan(1_000_000);
        assert!(same(&p.map(|x| x + 60), &p));
        assert!(!same(&p.map(|x| x + 600), &p));
        assert!(!same(&p[..2], &p));
    }

    fn task(allowed: Value, disallowed: Value) -> Value {
        json!({ "id": "trig_1", "job_config": { "ccr": { "session_context": {
            "allowed_tools": allowed, "disallowed_tools": disallowed, "model": MODEL
        }}}})
    }

    #[test]
    fn a_task_made_by_1_3_0_still_has_every_default_tool() {
        // What claude.ai stored for `"allowed_tools": []` (Nov 2026).
        let stored = json!(["preset:default", "Task", "Bash", "Glob", "Grep", "Read", "Edit", "MultiEdit", "Write",
            "NotebookEdit", "WebFetch", "TodoWrite", "WebSearch", "BashOutput", "KillBash", "Skill", "Tmux", "Monitor",
            "SendUserFile", "REPL"]);
        let left = tools_left(&task(stored.clone(), Value::Null));
        assert_eq!(left.len(), 19, "every tool but the preset marker");
        assert!(!left.iter().any(|t| t.starts_with("preset:")));
        assert!(tools_left(&task(stored, json!(NO_TOOLS))).is_empty(), "locked once, nothing left to do");
    }

    #[test]
    fn tools_added_by_the_server_later_are_locked_too() {
        let left = tools_left(&task(json!(["preset:default", "Bash", "NewTool"]), json!(NO_TOOLS)));
        assert_eq!(left, vec!["NewTool".to_string()]);
    }

    #[test]
    fn no_stored_list_falls_back_to_the_known_tools() {
        assert_eq!(tools_left(&task(Value::Null, Value::Null)).len(), NO_TOOLS.len());
        assert!(tools_left(&task(json!([]), json!(NO_TOOLS))).is_empty());
    }

    #[test]
    fn uuid_and_time_formats() {
        let u = uuid4();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert_ne!(u, uuid4());
        assert_eq!(rfc3339(1_795_156_200), "2026-11-20T06:30:00Z");
        assert_eq!(ts("2026-11-20T06:30:00Z"), 1_795_156_200);
    }
}
