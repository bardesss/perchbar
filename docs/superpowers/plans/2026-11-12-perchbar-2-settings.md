# Perchbar Plan 2: Settings window Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** People can manage Perchbar without touching JSON. A settings window, opened from the strip, lists the widgets on the taskbar and lets the user add, remove, reorder, hide, group and configure them. Setting forms are generated from each widget's manifest. Secrets are stored encrypted, Perchbar starts with Windows, a local widget folder can be loaded after a permission review, and "+N" opens a real flyout.

**Architecture:** The Rust core stays the single source of truth. The scheduler gains a live-editing API: apply a new `Instances`, add/replace/block widgets, read statuses and per-instance logs. A thin Tauri command layer validates every change against the manifest before applying it. The settings window is a Tauri WebView2 window built with Svelte 5, created on demand and destroyed on close. It talks to the core only through those commands and one state event. The strip asks the main thread to open the window and never waits for it. Secrets live outside `instances.json`, encrypted with Windows DPAPI. The "+N" flyout is a native layered popup drawn with the strip's own renderer.

**Tech Stack:** Rust (stable), Tauri 2 (window, events, commands, capabilities), the `windows` crate (DPAPI `CryptProtectData`, registry), Svelte 5 + Vite + TypeScript for `ui/`, Vitest for UI logic tests, Node 22 LTS for the UI build. Use the latest stable version of each package and pin it in `Cargo.lock` / `ui/package-lock.json`.

**Spec:** `docs/superpowers/specs/2026-11-12-taskbar-widgets-design.md`

**Plan series:** Plan 1 (merged) is the core. Plan 2 is this file. Plan 3 covers the catalog, updates with permission diffs for catalog widgets, Share, AGENTS.md and the docs, the remaining templates, the `files`/`battery`/`media` sources, media click actions, and app self-update.

## Global Constraints

- Windows 11 only. App name **Perchbar**. Data folder `%APPDATA%\Perchbar\` (`paths::data_dir()`, which honors `PERCHBAR_DATA_DIR`).
- New files in the data folder: `app.json` (app preferences), `approvals.json` (approved permissions per widget id), `local.json` (local widget folders by widget id), `secrets\<instance-id>.bin` (DPAPI blob).
- The settings window is closed by default, created on demand and destroyed on close. Closing it never quits Perchbar. With it closed, memory stays well under 100 MB and idle CPU about 0%.
- The settings window must feel like Windows 11: Segoe UI Variable, Mica backdrop, rounded corners, light/dark following Windows live, keyboard accessible.
- Settings marked `secret` are stored encrypted with Windows DPAPI (user scope), are never written to `instances.json`, are never sent to the UI, and are never shown in logs.
- "Start with Windows": a toggle, on by default after the first run.
- Approved permissions belong to the widget, not the instance. A widget runs only with permissions the user approved. Bundled templates count as approved.
- The strip thread never waits on the settings window, the main thread, the disk, or Explorer (Plan 1 rule, unchanged).
- The UI loads only bundled files: strict CSP, no remote content, Tauri capabilities allow only Perchbar's own commands, and only in the `settings` window.
- Widget-provided strings (names, descriptions, labels, log lines, status text) are shown as text, never as HTML.
- Repo writing rules: no em or en dashes (U+2014, U+2013) in any committed text; no AI or Claude signatures or `Co-Authored-By` trailers; commits use the noreply address already in the repo's git config.

## Review Focus

1. **Opening and closing the settings window many times:** Perchbar never exits when it closes, memory returns close to the closed-window baseline (the WebView2 processes end), and the strip keeps updating while it is open. Pinned by `closing_settings_does_not_exit` (Task 4) and Task 10's live check.
2. **Changing settings while that instance is mid-run:** runs never overlap, the old run's result is discarded, and the next run uses the new settings. Pinned by `apply_restarts_changed_instance_and_drops_stale_result` (Task 1).
3. **Bad form input** (text in a number field, an option not in the select, a 1 MB string, a list with 10,000 entries, an unknown key): rejected by the core with a per-field message; nothing is saved. Pinned by the `validate_*` tests (Task 5).
4. **Secrets leaking:** a secret never appears in `instances.json`, `get_state`, the log file or the per-instance log, even when a widget logs it. Pinned by `secret_never_reaches_instances_json`, `state_reports_secret_as_set_only`, `widget_log_redacts_secret_values` (Tasks 2 and 5).
5. **A registered local folder that was deleted or broken:** Perchbar starts, the widget shows an error in the settings window, its instances and settings are kept, and fixing the folder plus "Reload" brings it back. Pinned by `missing_local_folder_keeps_instances` (Task 8).

---

## File Structure

```
app/src/
  scheduler.rs        + live editing API, widget registry, statuses, log book hookup
  logbook.rs          per-instance ring buffer of log lines (new)
  secrets.rs          DPAPI secret store + redaction (new)
  prefs.rs            app.json + autostart registry (new)
  approvals.rs        approvals.json + local.json + permission diff (new)
  commands.rs         Tauri commands, validation, state snapshot (new)
  validate.rs         settings validation against a manifest (new, pure)
  settings_window.rs  open/focus/destroy the WebView window, state event bridge (new)
  sandbox/mod.rs      RunContext.permissions (approved) replaces manifest permissions for checks
  sandbox/host.rs     widget log lines go through redaction and into the log book
  strip/window.rs     left click opens settings, new menu items, "+N" opens the flyout
  strip/flyout.rs     native "+N" flyout window (new)
  main.rs             wiring: prefs, secrets, approvals, local widgets, commands, window
app/capabilities/settings.json   Tauri capability for the settings window (new)
app/tauri.conf.json   CSP, frontendDist ../ui/dist, beforeBuildCommand
ui/                   Svelte 5 + Vite + TS app (replaces the placeholder)
  package.json, vite.config.ts, tsconfig.json, index.html
  src/main.ts, src/App.svelte, src/api.ts (typed invoke wrappers), src/theme.css
  src/lib/form.ts     pure: manifest fields -> form model, client-side checks (mirrors validate.rs)
  src/lib/*.svelte    WidgetList, WidgetEditor, SettingField, SecretField, ListField,
                      GroupEditor, AddWidgetDialog, PermissionReview, LogView, AppSettings
  src/lib/form.test.ts
.github/workflows/ci.yml        + Node setup, npm ci, npm test, npm run build
.github/workflows/release.yml   + the same UI build before `cargo tauri build`
.github/dependabot.yml          + npm for /ui
```

---

### Task 1: Scheduler live editing, widget registry, statuses, log book

**Files:**
- Modify: `app/src/scheduler.rs`, `app/src/sandbox/mod.rs`, `app/src/sandbox/host.rs`, `app/src/lib.rs`
- Create: `app/src/logbook.rs`
- Test: `app/src/scheduler.rs`, `app/src/logbook.rs`

**Interfaces:**
- Consumes: Plan 1 `Scheduler`, `SchedulerHandle`, `Instances`, `RunContext`, `SlotState`.
- Produces:
  - `sandbox::RunContext` gains `pub permissions: manifest::Permissions` (the approved set). `sandbox::run` checks URLs against `ctx.permissions.network`, not `ctx.manifest.permissions`.
  - `scheduler::WidgetEntry { manifest: Arc<Manifest>, dir: PathBuf, approved: Permissions, blocked: Option<String> }`. A blocked widget's instances do not run and show `SlotState::Error { message: <reason>, last: None }`.
  - `Scheduler::start*` takes `widgets: HashMap<String, WidgetEntry>` instead of `(Arc<Manifest>, PathBuf)`. The widget map moves into `Inner` so it can change at runtime.
  - `SchedulerHandle::snapshot(&self) -> Instances`
  - `SchedulerHandle::apply(&self, next: Instances)`: normalizes `next`, diffs it against the current data, stops removed or newly hidden instances, starts new or newly shown ones, restarts instances whose `settings` changed, republishes and saves in the background. A run still in flight for a restarted or removed instance finishes, and its result is discarded (per-instance run epoch checked before storing a result).
  - `SchedulerHandle::upsert_widget(&self, id: &str, entry: WidgetEntry)` (restarts that widget's running instances) and `SchedulerHandle::remove_widget(&self, id: &str)` (stops them; instances stay in the data).
  - `SchedulerHandle::statuses(&self) -> HashMap<String, SlotState>` (every instance id; hidden ones absent).
  - `SchedulerHandle::set_hidden` is reimplemented on top of `apply`.
  - `logbook::LogBook` (cheap `Clone`, `Arc` inside): `push(&self, instance_id: &str, level: LogLevel, line: String)`, `lines(&self, instance_id: &str) -> Vec<LogLine>`, `remove(&self, instance_id: &str)`. `LogLine { at: chrono::DateTime<Local>, level: LogLevel, text: String }`, `LogLevel { Info, Warn, Error }`. At most **200** lines per instance, oldest dropped.
  - `RunContext` gains `pub logbook: LogBook`. Widget `log`/`console.*` lines go to the log file (as now) and to the book; run failures are pushed by the scheduler as `Error` lines with the `RunError` message.

- [ ] **Step 1: Write the failing tests**

```rust
// scheduler.rs (fake runner, tokio::time::pause)
#[tokio::test] async fn apply_starts_added_instance()                  // apply with one new visible instance -> runner called for it, slot published
#[tokio::test] async fn apply_stops_removed_instance()                 // remove -> no further runner calls after advancing 10 min
#[tokio::test] async fn apply_restarts_changed_instance_and_drops_stale_result()
    // runner blocks on a gate for run #1 (settings {coin:"BTC"}); apply settings {coin:"ETH"};
    // release gate -> run #1 result never published; next run sees coin == "ETH"; never two runs at once
#[tokio::test] async fn apply_reorder_and_group_only_republishes()     // order/group change -> no restart (runner call count unchanged), slots reordered
#[tokio::test] async fn apply_saves_in_background()                    // writer receives the applied data
#[tokio::test] async fn blocked_widget_shows_reason_and_does_not_run() // upsert_widget with blocked Some("Waiting for permission review")
#[tokio::test] async fn upsert_widget_restarts_its_instances()
#[tokio::test] async fn run_uses_approved_permissions()                // RunContext.permissions == entry.approved, not manifest.permissions
#[tokio::test] async fn statuses_reports_every_visible_instance()
#[tokio::test] async fn run_failure_goes_to_logbook()                  // Err(Script("boom")) -> lines(id).last().text contains "boom", level Error

// logbook.rs
#[test] fn logbook_keeps_last_200_lines()
#[test] fn logbook_remove_forgets_instance()
```

- [ ] **Step 2:** Run `cargo test -p perchbar scheduler logbook`. Expected: FAIL (missing items).
- [ ] **Step 3:** Implement the API above. Keep Plan 1's guarantees: per-instance run guard, global `Semaphore(4)`, publish outside the state lock, background saver, `runtime.spawn` from any thread. Update `main.rs`, `setup::load_widgets` callers and existing tests to build `WidgetEntry` (bundled templates: `approved = manifest.permissions.clone()`, `blocked = None`).
- [ ] **Step 4:** Run `cargo test -p perchbar`. Expected: all PASS. Run `cargo clippy --all-targets -- -D warnings`. Expected: clean.
- [ ] **Step 5:** Commit: `feat: live editing API for the scheduler`.

---

### Task 2: Secrets with DPAPI, and redaction

**Files:**
- Create: `app/src/secrets.rs`
- Modify: `app/src/scheduler.rs` (merge secrets into run settings), `app/src/sandbox/host.rs` and `app/src/scheduler.rs` (redact before logging), `app/src/paths.rs`, `app/Cargo.toml` (`Win32_Security_Cryptography`)
- Test: `app/src/secrets.rs`, `app/src/scheduler.rs`

**Interfaces:**
- Consumes: Task 1 `RunContext`, `LogBook`.
- Produces:
  - `paths::secrets_file(instance_id: &str) -> PathBuf` = `data_dir()/secrets/<instance-id>.bin`.
  - `secrets::SecretStore` (cheap `Clone`): `load(&self, instance_id: &str) -> SecretLoad`, `set(&self, instance_id: &str, key: &str, value: &str) -> io::Result<()>`, `clear(&self, instance_id: &str, key: &str) -> io::Result<()>`, `remove_instance(&self, instance_id: &str)`, `keys_set(&self, instance_id: &str) -> Vec<String>`.
  - `secrets::SecretLoad { values: Map<String, Value>, unreadable: bool }`. `unreadable` is true when the file exists but cannot be decrypted or parsed (copied from another PC or user, corrupt). Then `values` is empty, the file is left in place, and the UI says "Re-enter this secret".
  - The file content is `CryptProtectData` (user scope, `CRYPTPROTECT_UI_FORBIDDEN`, optional entropy bytes `b"Perchbar secrets v1"`) over the JSON object `{ "<key>": "<value>" }`. Writes are atomic (temp file + rename). Values are at most **4,096** chars.
  - `secrets::protect(plain: &[u8]) -> io::Result<Vec<u8>>`, `secrets::unprotect(blob: &[u8]) -> io::Result<Vec<u8>>`.
  - `secrets::Redactor::new(values: impl IntoIterator<Item = String>) -> Redactor` and `Redactor::apply(&self, text: &str) -> String`: replaces every occurrence of each secret value of 4 or more chars with `[secret]`. Used for widget log lines, run error messages and the log book.
  - `RunContext` gains `pub redactor: Arc<Redactor>`. The scheduler builds the run settings as `effective_settings` overlaid with `SecretStore::load(id).values`, and the redactor from those values.
  - Migration in `main.rs`: for each instance, any `secret`-kind key found in `instances.json` is moved into the store and removed from `instances.json` (then saved).

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn protect_round_trips()                          // unprotect(protect(b"abc")) == b"abc"; blob != b"abc"
#[test] fn store_set_load_clear()                         // set k=v -> load().values[k]=="v"; clear -> absent; keys_set lists set keys
#[test] fn corrupt_file_is_unreadable_not_error()         // write b"garbage" -> load(): unreadable=true, values empty, file still present
#[test] fn secret_value_length_capped()                   // 4,097 chars -> Err(InvalidInput)
#[test] fn redactor_replaces_values()                     // values ["sk-12345"] -> "key sk-12345 ok" => "key [secret] ok"; short values (<4) untouched
#[tokio::test] async fn run_settings_include_secrets()    // fake runner sees settings["apiKey"] == stored secret
#[tokio::test] async fn widget_log_redacts_secret_values()// runner logs the secret -> log book line shows "[secret]"
#[test] fn secret_never_reaches_instances_json()          // migration: instances.json with apiKey moved -> saved file has no apiKey, store has it
```

- [ ] **Step 2:** Run `cargo test -p perchbar secrets`. Expected: FAIL.
- [ ] **Step 3:** Implement. `LocalFree` the DPAPI output blob. Never log a secret value or a decrypted buffer, including in error messages.
- [ ] **Step 4:** Run `cargo test -p perchbar`. Expected: all PASS.
- [ ] **Step 5:** Commit: `feat: store secret settings with DPAPI`.

---

### Task 3: App preferences and Start with Windows

**Files:**
- Create: `app/src/prefs.rs`
- Modify: `app/src/main.rs`, `app/src/paths.rs`
- Test: `app/src/prefs.rs`

**Interfaces:**
- Produces:
  - `paths::prefs_file() -> PathBuf` = `data_dir()/app.json`.
  - `prefs::AppPrefs { autostart: bool }` (camelCase JSON, unknown fields ignored). `prefs::load(path) -> Option<AppPrefs>` (None when the file is missing or corrupt) and `prefs::save(path, &AppPrefs) -> io::Result<()>`.
  - `prefs::Autostart` trait with `fn enabled(&self) -> bool` and `fn set(&self, on: bool) -> io::Result<()>`; `prefs::RegistryAutostart { exe: PathBuf }` implements it with value name `Perchbar` under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, data `"<exe path>"` in quotes. `enabled()` is false when the value is missing, points at another path, or Windows' Startup Apps page turned it off (`HKCU\...\Explorer\StartupApproved\Run\Perchbar` with first byte odd, i.e. 0x03).
  - `prefs::startup(prefs_path, &dyn Autostart, exe: &Path) -> AppPrefs`: no `app.json` yet (first run of Plan 2 or later) -> turn autostart on and write `{autostart: true}`; `autostart: true` and the Run value points at a different exe (moved portable exe) -> rewrite it; otherwise leave Windows alone.
  - `--autostart` is not used: started at sign-in or by hand behaves the same.

- [ ] **Step 1: Write the failing tests** with a fake `Autostart`:

```rust
#[test] fn first_run_turns_autostart_on()           // no app.json -> fake.set(true) called, app.json {autostart:true}
#[test] fn user_off_is_respected()                  // app.json {autostart:false} -> set never called
#[test] fn moved_exe_is_rewritten()                 // app.json true, fake reports a different exe path -> set(true) called again
#[test] fn corrupt_prefs_count_as_missing()
```

- [ ] **Step 2:** Run `cargo test -p perchbar prefs`. Expected: FAIL.
- [ ] **Step 3:** Implement, and call `prefs::startup` in `main.rs` after logging is up. Registry failures are logged as warnings, never fatal.
- [ ] **Step 4:** Run `cargo test -p perchbar`. Expected: all PASS.
- [ ] **Step 5:** Commit: `feat: start with Windows`.

---

### Task 4: Settings window shell (UI toolchain, window lifecycle, strip wiring)

**Files:**
- Create: `ui/` Svelte 5 + Vite + TypeScript project (replaces `ui/index.html`), `app/src/settings_window.rs`, `app/capabilities/settings.json`
- Modify: `app/tauri.conf.json`, `app/src/main.rs`, `app/src/strip/mod.rs`, `app/src/strip/window.rs`, `.github/workflows/ci.yml`, `.github/workflows/release.yml`, `.github/dependabot.yml`, `RELEASING.md`
- Test: `app/src/settings_window.rs`, `app/src/strip/window.rs` (menu spec tests), `ui/src/lib/smoke.test.ts`

**Interfaces:**
- Consumes: Task 1 `SchedulerHandle::statuses`.
- Produces:
  - `settings_window::open(app: &AppHandle, focus: Option<String>)`: creates the window labelled `settings` (title "Perchbar", 960x680 logical, min 720x520, Mica effect, decorations on, centered on the primary monitor) or, if it exists, unminimizes and focuses it. With `focus = Some(instance_id)` it emits `perchbar://focus` with that id after the page signals ready.
  - Closing the window destroys it (default Tauri behavior with `windows: []` in config). `main.rs` keeps preventing exit for `ExitRequested { code: None }`.
  - `settings_window::StateBridge`: holds a `tokio::sync::watch::Sender<u64>`; `notify(&self)` is called from the scheduler publish callback (never blocks). A task on the Tauri runtime waits on the watch, throttles to at most one emit per **300 ms**, and emits `perchbar://state-changed` (no payload) to the `settings` window if it exists.
  - `strip::spawn` takes `on_open_settings: impl Fn(Option<String>) + Send + Sync + 'static` in addition to `on_quit`. The strip calls it and returns immediately; `main.rs` implements it with `AppHandle::run_on_main_thread(move || settings_window::open(..))`.
  - Strip input changes (spec 5.8): left click on a single slot opens settings focused on that instance (Plan 1's refresh + dim is removed). Left click on a group still advances. Right-click menu: `Refresh now`, `Widget settings`, `Hide this widget`, separator, `Open Perchbar settings`, separator, `Quit Perchbar`. On empty strip area: `Open Perchbar settings`, `Quit Perchbar`.
  - UI build: `ui/` with `npm run build` -> `ui/dist`, `npm test` -> Vitest. `tauri.conf.json`: `"frontendDist": "../ui/dist"`, `"beforeBuildCommand": "npm --prefix ../ui ci && npm --prefix ../ui run build"`, CSP `default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src ipc: http://ipc.localhost; object-src 'none'; base-uri 'none'; frame-ancestors 'none'`. `build.rs` declares Perchbar's commands with `tauri_build::Attributes::new().app_manifest(tauri_build::AppManifest::new().commands(&[...]))` so each gets an `allow-<command>` permission. `capabilities/settings.json` grants `core:event:default`, `core:window:allow-close`, `core:window:allow-minimize`, and those `allow-<command>` permissions, for window `settings` only. Each later task that adds a command adds it to both lists.
  - The page is a placeholder in this task: a title bar area, the app name and version from a `get_version` command, and light/dark from `prefers-color-scheme`.
  - CI: `actions/setup-node` (SHA-pinned, Node 22, npm cache for `ui/package-lock.json`), then `npm ci`, `npm test`, `npm run build` in `ui/` before the Rust steps (the Rust build embeds `ui/dist`). The required check name stays `CI / build`. Release workflow: the same Node setup before `cargo tauri build`. Dependabot: add `npm` for `/ui`, weekly Monday, prefix `deps`, minor+patch grouped.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn closing_settings_does_not_exit()   // the RunEvent handler as a pure fn: ExitRequested{code:None} -> prevent; code Some(0) -> allow
#[test] fn state_bridge_throttles()           // 10 notify() within 100 ms -> 1 emit; another after 300 ms -> 2 emits (fake emitter, paused time)
#[test] fn menu_for_single_slot()             // labels == ["Refresh now","Widget settings","Hide this widget","-","Open Perchbar settings","-","Quit Perchbar"]
#[test] fn menu_for_empty_area()              // ["Open Perchbar settings","-","Quit Perchbar"]
#[test] fn left_click_single_opens_settings() // on_left_click on a single slot -> on_open_settings(Some(id)) called, no refresh_now
```
```ts
// ui/src/lib/smoke.test.ts
test("app module loads", async () => { expect(await import("../App.svelte")).toBeTruthy(); });
```

- [ ] **Step 2:** Run `cargo test -p perchbar settings_window strip` and `npm --prefix ui test`. Expected: FAIL.
- [ ] **Step 3:** Implement. Extract the RunEvent decision into a pure fn so it can be tested. The strip calls `on_open_settings` only; it never touches Tauri.
- [ ] **Step 4:** Run `npm --prefix ui test`, `npm --prefix ui run build`, `cargo test -p perchbar`, `cargo clippy --all-targets -- -D warnings`. Expected: all PASS / clean. Live check (bounded, `PERCHBAR_DATA_DIR` temp dir, kill only your PID): click a widget, the window opens; close it, Perchbar keeps running; open and close 5 times, then the `msedgewebview2.exe` children of Perchbar are gone within 10 s.
- [ ] **Step 5:** Commit: `feat: settings window shell`.

---

### Task 5: Commands and validation

**Files:**
- Create: `app/src/validate.rs`, `app/src/commands.rs`
- Modify: `app/src/main.rs` (register commands, manage state)
- Test: `app/src/validate.rs`, `app/src/commands.rs`

**Interfaces:**
- Consumes: Tasks 1 to 4.
- Produces:
  - `validate::settings(m: &Manifest, input: &Map<String, Value>) -> Result<Map<String, Value>, Vec<FieldError>>` with `FieldError { key: String, message: String }`. Rules: unknown keys rejected; `text` strings at most **2,000** chars; `location` strings at most **200** chars; `number` finite; `select` value in `options`; `toggle` boolean; `list` array of at most **50** strings, each at most **200** chars, empty entries removed; `color` matches `^#[0-9a-fA-F]{6}$`; `required` fields non-empty (a required `secret` counts as present when the store has it); `secret` fields are never accepted here (they go through `set_secret`). The serialized result is at most **16 KB**.
  - `validate::max_width(v: Option<u32>) -> Result<Option<u32>, String>`: `None` or **40..=600**.
  - `validate::group_interval(v: u32) -> Result<u32, String>`: `MIN_GROUP_INTERVAL_SECS..=3600`.
  - `commands::AppState` (Tauri managed state) holding `SchedulerHandle`, `SecretStore`, `LogBook`, `Arc<dyn Autostart>`, prefs path, approvals (Task 8 fills it).
  - `commands::StateView` (serde, camelCase) returned by `get_state`:
    - `widgets: Vec<WidgetView { id, name, description, author, version, settings: Vec<SettingField>, permissions, local: bool, blocked: Option<String>, folder: Option<String> }>`
    - `instances: Vec<InstanceView { id, widget, settings /* no secrets */, secretsSet: Vec<String>, secretsUnreadable: bool, hidden, maxWidth, status: StatusView }>`
    - `groups`, `order`, `autostart: bool`, `version: String`, `logFolder: String`
    - `StatusView { state: "loading"|"ok"|"stale"|"error"|"hidden"|"blocked", text: Option<String> /* first text block of the shown frame */, message: Option<String> }`
  - Commands (all `Result<_, CommandError>` where `CommandError` serializes as `{ message, fields?: FieldError[] }`): `get_state`, `get_version`, `add_instance(widget) -> String`, `remove_instance(id)` (also deletes its storage file, secrets file and log book entry), `update_settings(id, settings)`, `set_secret(id, key, value)`, `clear_secret(id, key)`, `set_hidden(id, hidden)`, `set_max_width(id, max_width)`, `set_order(order)`, `create_group(members, interval) -> String`, `update_group(id, members, interval)`, `ungroup(id)`, `refresh_now(id)`, `get_logs(id) -> Vec<LogLine>`, `set_autostart(on)`, `open_log_folder()`.
  - Every mutating command builds a new `Instances` from `snapshot()`, applies the change, calls `normalize`, and passes it to `apply`. Ids from the UI are checked against the snapshot; unknown ids return an error.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn validate_rejects_unknown_key()
#[test] fn validate_number_must_be_finite()            // "12" (string) -> FieldError on that key
#[test] fn validate_select_must_be_an_option()
#[test] fn validate_text_length_capped()                // 2,001 chars -> error; 2,000 ok
#[test] fn validate_list_caps_entries_and_drops_empty() // 51 entries -> error; ["a",""," b "] -> ["a","b"]
#[test] fn validate_color_hex()
#[test] fn validate_required_secret_uses_store()
#[test] fn validate_rejects_secret_field_in_settings()
#[test] fn validate_total_size_capped()
#[test] fn max_width_bounds()                           // 39 err, 40 ok, 600 ok, 601 err, None ok
#[test] fn state_reports_secret_as_set_only()           // set_secret then get_state -> secretsSet contains key, no value anywhere in the JSON
#[test] fn remove_instance_deletes_its_files()          // storage + secrets files gone, log book empty
#[test] fn create_group_then_ungroup_restores_order()
#[test] fn unknown_instance_id_is_an_error()
#[test] fn status_view_from_slot_state()                // Ok(text block "BTC $1") -> {state:"ok", text:"BTC $1"}; Error -> message
```

Test the command bodies as plain functions over `AppState` (the `#[tauri::command]` wrappers only forward).

- [ ] **Step 2:** Run `cargo test -p perchbar validate commands`. Expected: FAIL.
- [ ] **Step 3:** Implement. Keep secret values out of every `Debug`, log and error path.
- [ ] **Step 4:** Run `cargo test -p perchbar`. Expected: all PASS.
- [ ] **Step 5:** Commit: `feat: settings commands with validation`.

---

### Task 6: Settings UI: widget list, forms, secrets, logs, app settings

**Files:**
- Create: `ui/src/api.ts`, `ui/src/theme.css`, `ui/src/lib/form.ts`, `ui/src/lib/form.test.ts`, `ui/src/lib/{WidgetList,WidgetEditor,SettingField,SecretField,ListField,AddWidgetDialog,LogView,AppSettings}.svelte`
- Modify: `ui/src/App.svelte`
- Test: `ui/src/lib/form.test.ts`

**Interfaces:**
- Consumes: Task 5 commands and `StateView`; events `perchbar://state-changed`, `perchbar://focus`.
- Produces:
  - `api.ts`: one typed wrapper per command, and `onStateChanged(cb)`, `onFocus(cb)`.
  - `form.ts` (pure): `toFormModel(fields: SettingField[], values: Record<string, unknown>) -> FormField[]` and `checkField(field: SettingField, value: unknown) -> string | null`, mirroring `validate.rs` limits so errors show while typing. The core stays the authority; its `fields` errors are shown next to the matching inputs.
  - Layout: left pane "On your taskbar": the instances and groups in strip order (drag to reorder, plus Move up/Move down buttons for keyboard users), each row with the widget name, a status line (`status.text`, or `message` for errors, "Hidden" for hidden ones), and a visibility toggle. "Add widget" opens a dialog listing installed widgets (name, description, author); picking one calls `add_instance` and selects the new row.
  - Right pane for the selected instance: the generated form (text, number, select, toggle, list with add/remove rows, color with a swatch and hex input, location as a text input labelled "City or place"), each secret as a password input showing "Saved" when set, with Replace and Remove actions, and "Re-enter this secret" when unreadable. Then "Maximum width" (empty = automatic), "Refresh now", "Remove from taskbar" (with confirmation), and a collapsible "Log" with the last lines from `get_logs` (refreshed on `state-changed` while open).
  - Saving: fields save on blur or Enter with a short "Saved" confirmation; a rejected value keeps the user's input and shows the message.
  - "Perchbar" section at the bottom of the left pane: Start with Windows toggle, version, "Open log folder".
  - `perchbar://focus` selects that instance. `state-changed` refetches `get_state` and keeps the selection and any unsaved input.
  - Styling: Segoe UI Variable, Windows 11 colors for light and dark, transparent body over Mica, 4 px radii on controls, visible focus rings, no external fonts or icons (inline SVG for the few icons needed).

- [ ] **Step 1: Write the failing tests**

```ts
test("number field rejects text", () => expect(checkField(num, "abc")).toMatch(/number/i));
test("select must be an option", () => expect(checkField(sel, "XYZ")).not.toBeNull());
test("list caps at 50 entries", () => expect(checkField(list, Array(51).fill("a"))).not.toBeNull());
test("color must be #rrggbb", () => { expect(checkField(color, "#12345")).not.toBeNull(); expect(checkField(color, "#a1b2c3")).toBeNull(); });
test("form model uses defaults for missing values", () => expect(toFormModel([textWithDefault], {})[0].value).toBe("Amsterdam"));
test("secret fields never carry a value", () => expect(toFormModel([secret], { apiKey: "x" })[0].value).toBeUndefined());
```

- [ ] **Step 2:** Run `npm --prefix ui test`. Expected: FAIL.
- [ ] **Step 3:** Implement the components. Render all widget-provided text with Svelte text bindings (never `{@html}`).
- [ ] **Step 4:** Run `npm --prefix ui test` and `npm --prefix ui run build`. Expected: PASS. Live check (bounded): add a second World clock, change its cities, see the strip update; hide and show; reorder; set a max width; save and remove a secret on a test manifest; the log shows lines.
- [ ] **Step 5:** Commit: `feat: settings UI for widgets and app preferences`.

---

### Task 7: Groups UI

**Files:**
- Create: `ui/src/lib/GroupEditor.svelte`
- Modify: `ui/src/lib/WidgetList.svelte`, `ui/src/lib/form.ts`, `ui/src/lib/form.test.ts`
- Test: `ui/src/lib/form.test.ts`

**Interfaces:**
- Consumes: Task 5 `create_group`, `update_group`, `ungroup`, `set_order`.
- Produces:
  - Each single row has "Group with..." listing other top-level single instances; picking one calls `create_group([this, other], 8)`. A group row shows its members indented and opens the group editor: interval in seconds (3 to 3600), member order (drag plus Move up/down), "Remove from group" per member (a group left with one member is ungrouped by `normalize` and the member returns to the list), and "Ungroup".
  - Dragging a single row onto a group row adds it to that group; onto another single row creates a group (spec 5.3).
  - `form.ts`: `moveItem<T>(list: T[], from: number, to: number) -> T[]` and `checkInterval(v: unknown) -> string | null`.

- [ ] **Step 1: Write the failing tests**

```ts
test("moveItem moves down and up", () => { expect(moveItem(["a","b","c"],0,2)).toEqual(["b","c","a"]); expect(moveItem(["a","b","c"],2,0)).toEqual(["c","a","b"]); });
test("interval bounds", () => { expect(checkInterval(2)).not.toBeNull(); expect(checkInterval(3)).toBeNull(); expect(checkInterval(3601)).not.toBeNull(); });
```

- [ ] **Step 2:** Run `npm --prefix ui test`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `npm --prefix ui test` and `npm --prefix ui run build`. Expected: PASS. Live check (bounded): group the Bitcoin price and the clock, change the interval to 4 s, see them alternate on the strip; ungroup.
- [ ] **Step 5:** Commit: `feat: rotation groups in the settings window`.

---

### Task 8: Load from folder, permission review, approvals

**Files:**
- Create: `app/src/approvals.rs`, `ui/src/lib/PermissionReview.svelte`
- Modify: `app/src/commands.rs`, `app/src/main.rs`, `app/src/setup.rs`, `app/src/paths.rs`, `ui/src/lib/AddWidgetDialog.svelte`, `ui/src/lib/WidgetEditor.svelte`
- Test: `app/src/approvals.rs`, `app/src/commands.rs`

**Interfaces:**
- Consumes: Task 1 `upsert_widget`, `remove_widget`, `WidgetEntry`; Task 5 `AppState`.
- Produces:
  - `paths::approvals_file()` = `data_dir()/approvals.json` (`{ "<widget-id>": Permissions }`), `paths::local_file()` = `data_dir()/local.json` (`{ "<widget-id>": "<absolute folder>" }`).
  - `approvals::is_within(asked: &Permissions, approved: &Permissions) -> bool` (every host, file and source in `asked` is in `approved`; host compare is ASCII case-insensitive).
  - `approvals::Approvals { load(path), save(path), get(id) -> Option<Permissions>, set(id, Permissions) }`. Bundled templates are written as approved on install.
  - Local widgets are used **in place** (not copied), so edits made by the user or their AI apply on Reload. At startup, `main.rs` loads bundled widgets from `widgets_dir()` and local ones from `local.json`. A local id that collides with an installed widget is skipped with a warning. A missing or broken local folder logs a warning and the widget is left out (its instances are kept, as Plan 1 already does).
  - Commands:
    - `inspect_folder(path) -> FolderReport { id, name, version, author, description, permissions, errors: Vec<String> }`: validates `widget.json` (Plan 1 `manifest::load`), checks `widget.js` exists, and reports an id collision. No code runs.
    - `install_local(path)`: requires a clean report; records `local.json` and approves the shown permissions; registers the widget with `upsert_widget`; returns the id.
    - `reload_widget(id)`: re-reads a local widget. If its permissions are within the approved set, it is upserted. If they grew, it is upserted with `blocked = Some("Waiting for permission review")` and `get_state` shows the review.
    - `approve_permissions(id)`: approves the widget's current permissions and unblocks it.
    - `remove_widget(id)`: local widgets only; removes all its instances (same cleanup as `remove_instance`), the `local.json` entry and the approval; never deletes the user's folder.
  - `WidgetView.local` is true and `folder` is set for local widgets; the UI shows "Local: not reviewed" on them (spec 5.4).
  - UI: "Add widget" dialog gains "Load from folder..." (Tauri dialog plugin folder picker), then `PermissionReview` shows "This widget will contact: <hosts>", "It will read: <files>", "It uses: <sources>", or "No network, file or data access" when all are empty, with Install and Cancel. A blocked widget's editor shows the same review with "Allow" for the new permissions, listing only what was added. Local widgets get "Reload" and "Remove widget" actions.
  - Add `tauri-plugin-dialog` (folder picker only) and grant `dialog:allow-open` to the `settings` capability.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn within_when_subset()                          // {network:[a]} within {network:[a,b]} -> true; {network:[c]} -> false; case-insensitive hosts
#[test] fn inspect_reports_bad_manifest()                // folder without widget.json -> errors non-empty, nothing registered
#[test] fn inspect_reports_id_collision()                // id "bitcoin-price" -> error mentions it
#[test] fn install_local_registers_and_approves()        // approvals.json has its permissions; get_state lists it with local=true
#[test] fn reload_with_new_host_blocks_until_approved()  // add a host to widget.json, reload -> blocked Some(..), approve -> unblocked, approvals updated
#[test] fn reload_with_fewer_permissions_does_not_block()
#[test] fn remove_widget_keeps_user_folder()             // folder still exists afterwards; instances of it gone
#[test] fn missing_local_folder_keeps_instances()         // local.json points at a deleted folder -> startup ok, instances kept, widget absent
#[test] fn bundled_templates_are_approved()
```

- [ ] **Step 2:** Run `cargo test -p perchbar approvals commands`. Expected: FAIL.
- [ ] **Step 3:** Implement the Rust side, then the UI.
- [ ] **Step 4:** Run `cargo test -p perchbar`, `npm --prefix ui test`, `npm --prefix ui run build`. Expected: PASS. Live check (bounded): load a copy of `widgets/world-clock` with a changed id from a temp folder, see the review, install, add it; add a host to its `widget.json`, Reload, see the block and the review; Allow.
- [ ] **Step 5:** Commit: `feat: load local widgets with a permission review`.

---

### Task 9: "+N" flyout

**Files:**
- Create: `app/src/strip/flyout.rs`
- Modify: `app/src/strip/window.rs`, `app/src/strip/mod.rs`, `app/src/strip/render.rs` (render one slot into a row), `app/src/strip/layout.rs` (pure flyout geometry)
- Test: `app/src/strip/layout.rs`, `app/src/strip/window.rs`

**Interfaces:**
- Consumes: Plan 1 renderer, layout, theme, `OverflowItem`; Task 4 `on_open_settings`.
- Produces:
  - `layout::flyout_geometry(rows: &[f32] /* row widths in DIP */, row_height_dip: f32, scale: f32, anchor: RECT /* "+N" item in screen px */, work: RECT, taskbar_edge: Edge) -> FlyoutGeometry { rect: RECT, rows: Vec<RECT> }`. Rows stack vertically with 8 DIP padding and 4 DIP spacing; width = widest row + padding, at least 160 DIP; the flyout sits above the anchor for a bottom taskbar (below for a top one), right-aligned to the anchor, clamped inside `work`. At most **12** rows; more scroll with the wheel.
  - `strip::flyout::Flyout`: a top-level `WS_POPUP` layered window owned by the strip's control window (never a child of Explorer), `WS_EX_TOOLWINDOW | WS_EX_TOPMOST`, drawn with the strip renderer and the current palette, with a 1 px border and an 8 DIP corner radius (DWM rounded corners). It is activated on open so it can close on `WM_ACTIVATE` inactive, on Esc, on a second click on "+N", or when the model no longer has hidden slots.
  - Rows behave like strip slots: hover highlight and tooltip, left click = the same action as on the strip (open settings for singles, advance for groups), right click = the same context menu. The rotation of group rows and multi-frame widgets keeps running while the flyout is open.
  - Plan 1's "+N" popup menu is removed, and with it the `names: HashMap<PathBuf, String>` parameter of `strip::spawn` (it went stale once widgets can be added at runtime).

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn flyout_above_bottom_taskbar()     // anchor at bottom -> rect.bottom == anchor.top - gap; right edge == anchor.right
#[test] fn flyout_below_top_taskbar()
#[test] fn flyout_clamped_to_work_area()     // anchor near the left edge -> rect.left >= work.left
#[test] fn flyout_width_is_widest_row()      // rows [80, 140] DIP at scale 1.5 -> width == (140 + 16) * 1.5 rounded
#[test] fn flyout_caps_visible_rows()        // 20 rows -> 12 row rects
#[test] fn plus_n_click_toggles_flyout()     // click "+N" -> open; click again -> closed (pure state fn)
```

- [ ] **Step 2:** Run `cargo test -p perchbar flyout`. Expected: FAIL.
- [ ] **Step 3:** Implement. Everything runs on the strip thread; no cross-process calls.
- [ ] **Step 4:** Run `cargo test -p perchbar` and clippy. Expected: PASS / clean. Live check (bounded): add enough instances that "+N" shows; open the flyout, hover, click a row (settings open on that widget), press Esc; switch the theme while open.
- [ ] **Step 5:** Commit: `feat: "+N" flyout`.

---

### Task 10: Integration, docs, release prep

**Files:**
- Modify: `README.md` (Status section, a settings window screenshot, Install notes), `app/Cargo.toml` and `app/tauri.conf.json` version **0.2.0**, `RELEASING.md` (UI build note)
- Create: `docs/images/settings-light.png`, `docs/images/settings-dark.png`
- Test: `app/tests/settings_flow.rs`

**Interfaces:**
- Consumes: everything above.

- [ ] **Step 1: Write the failing integration test** driving the command functions over a temp `PERCHBAR_DATA_DIR` with a fake runner:

```rust
#[tokio::test] async fn settings_flow_end_to_end()
// first start -> 2 default instances, autostart on (fake);
// add a world-clock instance, update its cities, set a secret on a test widget, create a group, hide one;
// restart (new AppState over the same folder) -> the same state comes back, the secret loads, instances.json holds no secret
```

- [ ] **Step 2:** Run `cargo test -p perchbar --test settings_flow`. Expected: FAIL until wiring gaps are closed.
- [ ] **Step 3:** Close the gaps. Update the README: the Status section (settings window done; next is Plan 3), the "Make your own" section gains "Load from folder" and "Reload", and screenshots of the settings window in light and dark. Bump the version to 0.2.0.
- [ ] **Step 4:** Run `npm --prefix ui test`, `npm --prefix ui run build`, `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `cargo deny check`, `cargo tauri build` (in `app/`). Expected: all PASS, and an NSIS installer in `target/release/bundle/nsis/`. Live check (bounded, temp data dir): first start shows two widgets; the window opens from a click; memory with the window closed after one open/close cycle is within 20 MB of the starting value; idle CPU about 0%.
- [ ] **Step 5:** Commit: `chore: Plan 2 integration and docs`. Push `plan-2-settings` and open a PR into `main` titled `Plan 2: settings window`, listing the manual checks below with their results:
  - Settings window: light/dark switch while open, 100/150/200% scaling, keyboard-only use (Tab, Enter, Esc, arrow keys in lists).
  - Start with Windows: sign out and back in; turning it off in Windows Settings > Apps > Startup shows it off in Perchbar.
  - Secrets: a secret saved on one Windows account cannot be read after copying `secrets\` to another account ("Re-enter this secret").
  - "+N" flyout: bottom and top taskbar, a second monitor, theme switch while open.
