//! Perchbar: Windows 11 taskbar widgets. The binary in `main.rs` wires these
//! modules together; integration tests drive them directly.

pub mod display;
pub mod instances;
pub mod logbook;
pub mod logger;
pub mod manifest;
pub mod paths;
pub mod prefs;
pub mod sandbox;
pub mod scheduler;
pub mod secrets;
pub mod settings_window;
pub mod setup;
pub mod strip;
