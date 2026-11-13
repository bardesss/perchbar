use std::path::Path;

/// Commands the settings page may call. Each gets an `allow-<command>`
/// permission, granted in capabilities/settings.json.
const COMMANDS: &[&str] = &[
    "get_version",
    "get_state",
    "add_instance",
    "remove_instance",
    "update_settings",
    "set_secret",
    "clear_secret",
    "set_hidden",
    "set_max_width",
    "set_order",
    "create_group",
    "update_group",
    "ungroup",
    "refresh_now",
    "get_logs",
    "set_autostart",
    "open_log_folder",
    "inspect_folder",
    "install_local",
    "reload_widget",
    "approve_permissions",
    "remove_widget",
];

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
