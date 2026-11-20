//! Small append-only log at %APPDATA%\com.clump.app\clump.log (rotated at 512 KB).

use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

static PATH: OnceLock<PathBuf> = OnceLock::new();

pub fn init(path: PathBuf) {
    let _ = PATH.set(path);
}

pub fn line(msg: impl AsRef<str>) {
    let Some(path) = PATH.get() else { return };
    if std::fs::metadata(path).map(|m| m.len() > 512 * 1024).unwrap_or(false) {
        let _ = std::fs::rename(path, path.with_extension("old.log"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let _ = writeln!(f, "{ts}  {}", msg.as_ref());
    }
}
