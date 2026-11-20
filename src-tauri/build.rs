fn main() {
    // Own manifest: declares Windows 10/11 support, which a layered *child* window (the taskbar
    // widget) needs, plus per-monitor DPI awareness. Tauri's default one has neither.
    let windows = tauri_build::WindowsAttributes::new().app_manifest(include_str!("app.manifest"));
    tauri_build::try_build(tauri_build::Attributes::new().windows_attributes(windows)).expect("failed to run tauri-build");
}
