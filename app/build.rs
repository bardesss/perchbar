use std::path::Path;

/// Commands the settings page may call. Each gets an `allow-<command>`
/// permission, granted in capabilities/settings.json.
const COMMANDS: &[&str] = &["get_version"];

fn main() {
    println!("cargo:rerun-if-changed=windows-app-manifest.xml");
    println!("cargo:rerun-if-changed=../ui/dist");
    // The page is embedded from ui/dist; tauri-build's own error for a
    // missing folder does not say how to make it.
    let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("../ui/dist");
    if !dist.join("index.html").is_file() {
        panic!(
            r#"ui/dist is missing: run "npm --prefix ui ci" and "npm --prefix ui run build" first"#
        );
    }
    let windows = tauri_build::WindowsAttributes::new()
        .app_manifest(include_str!("windows-app-manifest.xml"));
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .windows_attributes(windows)
            .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS)),
    )
    .expect("failed to run tauri-build");
}
