//! Finds the Claude Code login to use: WSL distros first (that's where VS Code runs Claude Code
//! here), then Windows. Reads the OAuth token Claude Code keeps in ~/.claude/.credentials.json.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{RegGetValueW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};

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
    Some(Source {
        kind: Kind::Windows,
        creds,
        claude: find_claude_windows(&home).map(|p| p.to_string_lossy().into_owned()),
        label: "Windows".into(),
    })
}

/// Where `claude` is on Windows, best first: the official installer's spot, then PATH (as Windows
/// has it now), WinGet's links, npm's wrapper, and last the copy the VS Code extension ships with.
fn find_claude_windows(home: &Path) -> Option<PathBuf> {
    let local = home.join(".local").join("bin").join("claude.exe");
    if local.is_file() {
        return Some(local);
    }
    let mut dirs = path_dirs();
    dirs.extend(std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Microsoft").join("WinGet").join("Links")));
    dirs.extend(std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("npm")));
    // A real exe anywhere beats npm's .cmd wrapper.
    ["claude.exe", "claude.cmd"]
        .iter()
        .find_map(|name| dirs.iter().map(|d| d.join(name)).find(|f| f.is_file()))
        .or_else(|| editor_claude(home))
}

/// Folders on PATH as Windows has them now. Clump's own PATH is fixed when it starts, so a
/// Claude Code installed later (WinGet, npm, ...) would stay invisible until a restart.
fn path_dirs() -> Vec<PathBuf> {
    let machine = reg_string(HKEY_LOCAL_MACHINE, w!(r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment"), w!("Path"));
    let user = reg_string(HKEY_CURRENT_USER, w!("Environment"), w!("Path"));
    let mut dirs: Vec<PathBuf> = Vec::new();
    for list in [machine, user, std::env::var("PATH").ok()].into_iter().flatten() {
        for d in std::env::split_paths(&list) {
            if !d.as_os_str().is_empty() && !dirs.contains(&d) {
                dirs.push(d);
            }
        }
    }
    dirs
}

/// A string registry value; `%VARS%` in an expandable one come back expanded.
fn reg_string(root: HKEY, key: PCWSTR, value: PCWSTR) -> Option<String> {
    let mut buf = vec![0u16; 32_768];
    let mut size = (buf.len() * 2) as u32;
    let r = unsafe { RegGetValueW(root, key, value, RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as *mut _), Some(&mut size)) };
    if r != ERROR_SUCCESS {
        return None;
    }
    let len = (size as usize / 2).min(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]).trim_end_matches('\0').to_string())
}

/// The `claude.exe` bundled with the Claude Code extension for VS Code, Cursor or Windsurf. Its
/// folder changes with every extension update, so the newest copy wins.
fn editor_claude(home: &Path) -> Option<PathBuf> {
    [".vscode", ".vscode-insiders", ".cursor", ".windsurf"]
        .iter()
        .filter_map(|editor| std::fs::read_dir(home.join(editor).join("extensions")).ok())
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("anthropic.claude-code-"))
        .map(|e| e.path().join("resources").join("native-binary").join("claude.exe"))
        .filter(|f| f.is_file())
        .max_by_key(|f| f.metadata().and_then(|m| m.modified()).ok())
}

/// Whether `claude` can be started for this login right now.
pub fn claude_usable(src: &Source) -> bool {
    match (&src.kind, src.claude.as_deref()) {
        (_, None) => false,
        // An extension update deletes the old folder, so check the file is still there.
        (Kind::Windows, Some(c)) => Path::new(c).is_file(),
        (Kind::Wsl { .. }, Some(_)) => true,
    }
}

/// Look for `claude` again (Windows side only, cheap). True when what was found changed.
pub fn recheck_claude(src: &mut Source) -> bool {
    if !matches!(src.kind, Kind::Windows) {
        return false;
    }
    let Some(home) = std::env::var_os("USERPROFILE") else { return false };
    let found = find_claude_windows(Path::new(&home)).map(|p| p.to_string_lossy().into_owned());
    if found == src.claude {
        return false;
    }
    src.claude = found;
    true
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_extension(home: &Path, editor: &str, version: &str) -> PathBuf {
        let bin = home.join(editor).join("extensions").join(format!("anthropic.claude-code-{version}-win32-x64")).join("resources").join("native-binary");
        std::fs::create_dir_all(&bin).unwrap();
        let exe = bin.join("claude.exe");
        std::fs::write(&exe, b"").unwrap();
        exe
    }

    #[test]
    fn finds_the_newest_extension_copy() {
        let home = std::env::temp_dir().join(format!("clump-test-ext-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(editor_claude(&home), None);
        std::fs::create_dir_all(home.join(".vscode").join("extensions").join("anthropic.claude-code-2.1.1-win32-x64")).unwrap();
        assert_eq!(editor_claude(&home), None, "a folder without the exe doesn't count");
        let old = fake_extension(&home, ".vscode", "2.1.300");
        let new = fake_extension(&home, ".cursor", "2.1.330");
        let hour_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options().write(true).open(&old).unwrap().set_modified(hour_ago).unwrap();
        assert_eq!(editor_claude(&home), Some(new));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_deleted_windows_claude_is_not_usable() {
        let src = |claude: Option<&str>| Source { kind: Kind::Windows, creds: PathBuf::new(), claude: claude.map(str::to_string), label: String::new() };
        assert!(!claude_usable(&src(None)));
        assert!(!claude_usable(&src(Some(r"C:\nowhere\claude.exe"))));
        let exe = std::env::current_exe().unwrap();
        assert!(claude_usable(&src(Some(exe.to_str().unwrap()))));
    }

    #[test]
    fn reads_the_live_path_from_the_registry() {
        let machine = reg_string(HKEY_LOCAL_MACHINE, w!(r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment"), w!("Path")).unwrap();
        assert!(!machine.to_ascii_lowercase().contains("%systemroot%"), "expandable value must come back expanded: {machine}");
        assert!(path_dirs().iter().any(|d| d.to_string_lossy().to_ascii_lowercase().ends_with(r"\system32")));
    }
}
