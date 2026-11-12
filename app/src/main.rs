// Wired up by later tasks.
#[allow(dead_code)]
mod display;
#[allow(dead_code)]
mod instances;
#[allow(dead_code)]
mod manifest;
#[allow(dead_code)]
mod paths;

fn main() {
    tauri::Builder::default()
        .build(tauri::generate_context!())
        .expect("failed to build Perchbar")
        .run(|_app, event| {
            if let tauri::RunEvent::ExitRequested { api, code, .. } = event {
                // Perchbar has no windows; only an explicit exit (code Some) quits.
                if code.is_none() {
                    api.prevent_exit();
                }
            }
        });
}
