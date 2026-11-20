//! Finds the Claude Code login to use: WSL distros first (that's where VS Code runs Claude Code
//! here), then Windows. Reads the OAuth token Claude Code keeps in ~/.claude/.credentials.json.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::model::now;

pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Debug)]
pub enum Kind {
    Wsl { distro: String, user: String },
    Windows,
}

#[derive(Clone, Debug)]
pub struct Source {
    pub kind: Kind,
    pub creds: PathBuf,
    /// Path of the `claude` executable on that side (Linux path for WSL).
    pub claude: Option<String>,
    pub label: String,
}

pub struct Creds {
    pub token: String,
    /// Access-token expiry, unix seconds.
    pub expires_at: i64,
    pub plan: String,
    pub signed_out: bool,
}

pub fn read_creds(path: &Path) -> Option<Creds> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let o = v.get("claudeAiOauth")?;
    let s = |k: &str| o.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let token = s("accessToken");
    let expires_ms = o.get("expiresAt").and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f as i64))).unwrap_or(0);
    let ended = o.get("loginEnded").map(|x| !x.is_null() && x.as_bool() != Some(false)).unwrap_or(false);
    let has_refresh = !s("refreshToken").is_empty();
    Some(Creds {
        signed_out: ended || (token.is_empty() && !has_refresh),
        token,
        expires_at: expires_ms / 1000,
        plan: s("subscriptionType"),
    })
}

/// Run a hidden process, wait up to `timeout`, return (success, stdout, stderr).
pub fn run_hidden(mut cmd: Command, timeout: Duration) -> Result<(bool, Vec<u8>, String), String> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).creation_flags(CREATE_NO_WINDOW);
    let mut child = cmd.spawn().map_err(|e| format!("could not start: {e}"))?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("timed out after {} s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(e.to_string()),
        }
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    Ok((out.status.success(), out.stdout, String::from_utf8_lossy(&out.stderr).into_owned()))
}

fn wsl_distros() -> Vec<String> {
    let mut cmd = Command::new("wsl.exe");
    cmd.args(["-l", "-q"]);
    let Ok((_, out, _)) = run_hidden(cmd, Duration::from_secs(10)) else { return vec![] };
    // `wsl -l` prints UTF-16LE.
    let text = if out.len() >= 2 && out.iter().skip(1).step_by(2).all(|&b| b == 0) {
        let units: Vec<u16> = out.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(&out).into_owned()
    };
    text.lines()
        .map(|l| l.trim_matches(|c: char| c.is_whitespace() || c == '\0').to_string())
        .filter(|l| !l.is_empty() && !l.starts_with("docker-desktop"))
        .collect()
}

fn probe_wsl(distro: &str) -> Option<Source> {
    let script = r#"c=$(command -v claude 2>/dev/null || { [ -x "$HOME/.local/bin/claude" ] && echo "$HOME/.local/bin/claude"; }); printf 'CLUMP|%s|%s|%s\n' "$HOME" "$(id -un)" "$c""#;
    let mut cmd = Command::new("wsl.exe");
    cmd.args(["-d", distro, "-e", "bash", "-lc", script]);
    let (_, out, _) = run_hidden(cmd, Duration::from_secs(30)).ok()?;
    let text = String::from_utf8_lossy(&out);
    let line = text.lines().find(|l| l.starts_with("CLUMP|"))?;
    let parts: Vec<&str> = line.trim_end().splitn(4, '|').collect();
    let (home, user, claude) = (parts.get(1)?, parts.get(2)?, parts.get(3).copied().unwrap_or(""));
    if home.is_empty() {
        return None;
    }
    let creds = PathBuf::from(format!(r"\\wsl.localhost\{distro}{}", home.replace('/', "\\"))).join(".claude").join(".credentials.json");
    if !creds.exists() {
        return None;
    }
    Some(Source {
        kind: Kind::Wsl { distro: distro.to_string(), user: user.to_string() },
        creds,
        claude: (!claude.is_empty()).then(|| claude.to_string()),
        label: format!("WSL · {distro}"),
    })
}

fn probe_windows() -> Option<Source> {
    let home = PathBuf::from(std::env::var_os("USERPROFILE")?);
    let creds = home.join(".claude").join(".credentials.json");
    if !creds.exists() {
        return None;
    }
    let local = home.join(".local").join("bin").join("claude.exe");
    let claude = if local.exists() {
        Some(local)
    } else {
        std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join("claude.exe")).find(|f| f.exists()))
    };
    Some(Source {
        kind: Kind::Windows,
        creds,
        claude: claude.map(|p| p.to_string_lossy().into_owned()),
        label: "Windows".into(),
    })
}

/// Pick the login that works best: a live token beats an expired one (Claude Code refreshes it on
/// the next message), and anything beats a signed-out file.
pub fn discover() -> Option<Source> {
    let mut found: Vec<Source> = wsl_distros().iter().filter_map(|d| probe_wsl(d)).collect();
    found.extend(probe_windows());
    let t = now();
    found
        .into_iter()
        .map(|s| {
            let score = match read_creds(&s.creds) {
                Some(c) if !c.signed_out => {
                    let live = if !c.token.is_empty() && c.expires_at > t { 2 } else { 1 };
                    (live, s.claude.is_some() as i32, c.expires_at)
                }
                _ => (0, s.claude.is_some() as i32, 0),
            };
            (score, s)
        })
        .max_by_key(|(score, _)| *score)
        .map(|(_, s)| s)
}
