//! Starts a new 5-hour window by sending one tiny message through the official Claude Code CLI:
//! Haiku, no tools, no MCP servers, no hooks/settings, nothing saved. Costs ~600 input tokens.

use std::process::Command;
use std::time::{Duration, Instant};

use crate::source::{run_hidden, Kind, Source};

const PROMPT: &str = "hi";
const SYSTEM: &str = "Reply with one short word.";
pub const NO_CLI: &str = "The claude command was not found. Install the Claude Code CLI.";

pub fn send_hi(src: &Source) -> Result<(u64, String), String> {
    let claude = src.claude.as_deref().ok_or(NO_CLI)?;
    let cmd = match &src.kind {
        Kind::Wsl { distro, user } => {
            let script = format!(
                "exec '{}' -p '{PROMPT}' --model haiku --restricted --tools '' --strict-mcp-config \
                 --no-session-persistence --disable-slash-commands --system-prompt '{SYSTEM}' \
                 --output-format json </dev/null",
                claude.replace('\'', "")
            );
            let mut c = Command::new("wsl.exe");
            c.args(["-d", distro, "-u", user, "--cd", "~", "-e", "bash", "-c", &script]);
            c
        }
        Kind::Windows => {
            let mut c = Command::new(claude);
            c.args([
                "-p", PROMPT, "--model", "haiku", "--restricted", "--tools", "", "--strict-mcp-config",
                "--no-session-persistence", "--disable-slash-commands", "--system-prompt", SYSTEM,
                "--output-format", "json",
            ]);
            c.current_dir(std::env::temp_dir());
            c
        }
    };
    let started = Instant::now();
    let (_, stdout, stderr) = run_hidden(cmd, Duration::from_secs(120))?;
    let ms = started.elapsed().as_millis() as u64;
    let text = String::from_utf8_lossy(&stdout);
    let json = text.find('{').and_then(|i| serde_json::from_str::<serde_json::Value>(text[i..].trim()).ok());
    match json {
        Some(v) if v.get("is_error").and_then(|x| x.as_bool()) == Some(false) => {
            let reply = v.get("result").and_then(|x| x.as_str()).unwrap_or("").trim().chars().take(40).collect();
            Ok((ms, reply))
        }
        Some(v) => Err(short(v.get("result").and_then(|x| x.as_str()).unwrap_or("Claude Code reported an error"))),
        None => {
            let msg = stderr.lines().chain(text.lines()).map(str::trim).filter(|l| !l.is_empty()).last().unwrap_or("no output");
            Err(short(msg))
        }
    }
}

fn short(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() > 120 {
        format!("{}…", s.chars().take(119).collect::<String>())
    } else {
        s.to_string()
    }
}
