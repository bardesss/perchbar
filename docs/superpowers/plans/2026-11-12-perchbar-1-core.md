# Perchbar Plan 1: Core (taskbar strip + widget runtime) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A running Perchbar that loads widget folders from `%APPDATA%\Perchbar\widgets\`, runs each widget's `widget.js` in a sandbox on its refresh schedule, and draws the results natively on the Windows 11 taskbar, including rotation groups, overflow ("+N"), tooltips, and a right-click menu. It ships with two templates: Bitcoin price and World clock.

**Architecture:** A Tauri 2 app with no windows yet; the settings window arrives in Plan 2. The Rust core runs a tokio runtime that schedules widget runs in rquickjs sandboxes. Results go over a channel to a dedicated **strip thread**. That thread owns a per-pixel-alpha layered window that is a `WS_CHILD` of `Shell_TrayWnd`, the approach proven by the reference app. It is drawn with Direct2D/DirectWrite into a DIB and presented with `UpdateLayeredWindow`. Pure logic (manifest, display model, permissions, layout, placement math) is separate from the Win32 code so it can be unit-tested.

**Tech Stack:** Rust (stable), Tauri 2, `rquickjs` (with the futures/async feature), `tokio`, `reqwest` (rustls), `serde`/`serde_json`, `chrono` + `chrono-tz`, the `windows` crate (Direct2D, DirectWrite, Win32 UI, UI Automation, HiDpi, Shell). Use the latest stable version of each crate and pin it in `Cargo.lock`.

**Spec:** `docs/superpowers/specs/2026-11-12-taskbar-widgets-design.md`

**Plan series:** Plan 1 is the core (this file). Plan 2 covers the settings window, instances UI, groups UI, secrets (DPAPI), autostart, and the permissions prompt. Plan 3 covers the catalog, updates, Share, AGENTS.md and the docs, the remaining templates, and the `battery`/`media`/`files` sources.

**Reference app:** read `src/main.rs` (`ensure_widget`, `present`, the `TaskbarCreated` handling) and `src/render.rs` of `github.com/AbdoSarmini/claude-usage-widget-for-windows-taskbar` before Tasks 8 to 10.

## Global Constraints

- Windows 11 only.
- App name: **Perchbar**. Data folder: `%APPDATA%\Perchbar\`. Widgets folder: `%APPDATA%\Perchbar\widgets\<widget-id>\`. Instances file: `%APPDATA%\Perchbar\instances.json`. Per-instance storage: `%APPDATA%\Perchbar\storage\<instance-id>.json`. Log: `%APPDATA%\Perchbar\perchbar.log`.
- Idle CPU ~0% (no busy loops, no polling faster than 1 s; the strip redraws only when the output, hover, theme, DPI, or position changes). Memory target: well under 100 MB total with no settings window open.
- Widget display is limited to the building blocks in this plan; widgets cannot use HTML or free drawing.
- Minimum widget `refresh` is **10 s**; lower values are raised to 10.
- Widget run limits: **10 s** wall time, **32 MB** JS heap. A widget that exceeds either is stopped and shown in the error state.
- Widgets reach the network only for hosts in `permissions.network`, including after every redirect hop.
- The strip never overlaps taskbar app buttons.
- License: MIT (designer's call).

## Review Focus

1. **Explorer restarts** (`taskkill /f /im explorer.exe` then `explorer`): the strip re-attaches within 2 s without a restart of Perchbar. Pinned by Task 9's manual check plus the `TaskbarCreated` handler test seam.
2. **A widget throws, hangs, or returns garbage:** only that widget shows the error state, others keep updating, and the next scheduled run retries. Pinned by `sandbox_timeout_is_error`, `invalid_output_is_error`, `one_failure_does_not_stop_others` (Tasks 4, 6).
3. **Network down or API rate-limits (HTTP 429/5xx):** the last good output stays visible and is marked stale; retries back off (refresh × 2, capped at 10 min), with no hot loop. Pinned by `failure_keeps_last_output_stale` and `backoff_doubles_and_caps` (Task 6).
4. **A redirect to an undeclared host**, a URL with a userinfo or IP trick (`https://api.coinbase.com@evil.com/`), or a non-https URL: all blocked. Pinned by tests in Task 5.
5. **Scaling of 125/150/200% and switching dark/light at runtime:** text stays crisp, height matches the taskbar, and colors flip without a restart. Pinned by `layout_scales_with_dpi` (Task 7) and Task 9's manual check.

---

## File Structure

```
Cargo.toml                      workspace: members = ["app"]
LICENSE                         MIT
.gitignore
app/
  Cargo.toml
  build.rs                      tauri_build::build()
  tauri.conf.json               productName "Perchbar", identifier "app.perchbar", no windows, frontendDist "../ui"
  src/
    main.rs                     startup: dirs, load widgets+instances, spawn runtime + strip thread, run Tauri
    paths.rs                    data folder paths
    manifest.rs                 widget.json parsing/validation
    display.rs                  widget output model + validation
    instances.rs                instances.json model, defaults, load/save
    sandbox/mod.rs              rquickjs runtime: run(widget, instance) -> Result<Output, RunError>
    sandbox/prelude.js          JS shim: fetch() Response wrapper, format helpers
    sandbox/host.rs             host functions exposed to JS (fetch, storage, log, time)
    sandbox/permissions.rs      URL allow-checks
    scheduler.rs                per-instance timers, backoff, stale state, publishes StripModel
    strip/mod.rs                strip thread entry, message loop, events
    strip/model.rs              StripModel / Slot (what the strip shows)
    strip/layout.rs             pure layout: sizes, rotation, overflow
    strip/placement.rs          pure placement math + Win32 queries (taskbar, tray, app buttons via UIA)
    strip/render.rs             Direct2D/DirectWrite rendering into a premultiplied BGRA bitmap
    strip/window.rs             layered child window, present, tooltip, menu, mouse
    strip/theme.rs              dark/light + accent detection, palette
    strip/icons.rs              icon name -> Segoe Fluent Icons codepoint
ui/index.html                   placeholder (Plan 2 replaces it)
widgets/
  bitcoin-price/widget.json, widget.js, README.md
  world-clock/widget.json, widget.js, README.md
schema/widget.schema.json       JSON Schema for widget.json (formatVersion 1)
```

Bundled templates in `widgets/` are copied into `%APPDATA%\Perchbar\widgets\` on first run if missing (Task 11).

---

### Task 0: Repository and GitHub setup

All settings below are applied with `gh` / `gh api` so they can be reproduced. Save the commands as `scripts/setup-github.sh` and commit that file.

- [ ] **Step 1: Local repo.** Run `git init -b main`. The first commit on `main` holds `docs/` (spec + plans), `LICENSE` (MIT), `.gitignore` (Rust + Node + `/target`), `SECURITY.md`, and `.github/dependabot.yml`. Commit message: `chore: initial docs and repo policy`.
  - `SECURITY.md`: report vulnerabilities privately through GitHub's "Report a vulnerability" (private vulnerability reporting), never in public issues. Supported version: the latest release. Expect a response within 7 days. In scope: sandbox escapes, permission bypasses (network, files, sources), and secret leaks.
  - `.github/dependabot.yml`: ecosystems `cargo` (directory `/`) and `github-actions` (directory `/`), checked weekly on Monday, at most 5 open PRs, minor and patch updates grouped into one PR per ecosystem, and commit-message prefix `deps`. Plan 2 adds `npm` for `ui/`.
- [ ] **Step 2: Create and push.** Run `gh repo create bardesss/perchbar --public --description "Customizable live widgets on the Windows 11 taskbar" --source . --push`. The visibility is **public**, as the user decided.
- [ ] **Step 3: Repo settings** (`gh api -X PATCH repos/bardesss/perchbar`):
  - `delete_branch_on_merge=true`, so merged branches are deleted automatically
  - `allow_squash_merge=true`, `allow_merge_commit=false`, `allow_rebase_merge=false`, `squash_merge_commit_title=PR_TITLE`, `squash_merge_commit_message=PR_BODY`
  - `allow_auto_merge=true`, so Dependabot PRs can merge themselves once CI passes, if the user enables that per PR
  - `has_wiki=false`, `has_projects=false`
- [ ] **Step 4: Security features.**
  - `PATCH repos/bardesss/perchbar` with `security_and_analysis`: `secret_scanning`, `secret_scanning_push_protection`, and `dependabot_security_updates` all `enabled`
  - `PUT repos/bardesss/perchbar/vulnerability-alerts` (Dependabot alerts)
  - `PUT repos/bardesss/perchbar/private-vulnerability-reporting`
  - Code scanning: `PATCH repos/bardesss/perchbar/code-scanning/default-setup` with `state=configured`, `query_suite=default`. CodeQL default setup covers Rust and GitHub Actions; if the API says Rust isn't supported, configure `actions` only and log the reason.
  - Actions: `PUT repos/bardesss/perchbar/actions/permissions/workflow` with `default_workflow_permissions=read`, `can_approve_pull_request_reviews=false`.
- [ ] **Step 5: Branch ruleset for `main`** (`POST repos/bardesss/perchbar/rulesets`, name `protect-main`, target `branch`, include `~DEFAULT_BRANCH`, enforcement `active`). Rules:
  - `deletion` and `non_fast_forward` (no deleting the branch, no force-pushes)
  - `pull_request` with `required_approving_review_count: 0`, `dismiss_stale_reviews_on_push: true`, and `required_review_thread_resolution: true`. Approvals are 0 because a solo owner can't approve their own PR. Raise it once there are maintainers.
  - `required_status_checks` with `CI / build`, `strict_required_status_checks_policy: true`
  - `required_linear_history`
  - Bypass: none. The owner goes through PRs as well.
- [ ] **Step 6: Verify.** `gh api repos/bardesss/perchbar --jq '{delete_branch_on_merge, allow_squash_merge, allow_merge_commit, sa: .security_and_analysis}'` shows the values above, `gh api repos/bardesss/perchbar/rulesets` lists `protect-main`, and `git push origin HEAD:main --force` from a scratch commit is **rejected**. Undo the scratch commit locally afterwards.
- [ ] **Step 7:** Create the branch `plan-1-core` from `main`. All later Plan 1 commits go on it.

---

### Task 1: Scaffold the workspace, CI, and manifest parsing

**Files:**
- Create: `.github/workflows/ci.yml`, `deny.toml`, `Cargo.toml`, `app/Cargo.toml`, `app/build.rs`, `app/tauri.conf.json`, `app/src/main.rs`, `app/src/paths.rs`, `app/src/manifest.rs`, `ui/index.html`, `schema/widget.schema.json`
- Test: `app/src/manifest.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Produces:
  - `paths::data_dir() -> PathBuf`, `paths::widgets_dir()`, `paths::instances_file()`, `paths::storage_file(instance_id: &str)`, `paths::log_file()`
  - `manifest::Manifest { id: String, name: String, description: String, author: String, version: String, format_version: u32, refresh: u32, settings: Vec<SettingField>, permissions: Permissions }`
  - `manifest::SettingField { key: String, label: String, kind: SettingKind, default: Option<serde_json::Value>, options: Vec<String> /* for Select */, required: bool }`
  - `manifest::SettingKind` enum, serde lowercase: `Text, Number, Select, Toggle, List, Secret, Location, Color`
  - `manifest::Permissions { network: Vec<String>, files: Vec<String>, sources: Vec<String> }` (all default empty)
  - `manifest::load(dir: &Path) -> Result<Manifest, ManifestError>`: reads `dir/widget.json` and requires `dir/widget.js` to exist
  - `ManifestError` variants: `Io`, `Json(String)`, `MissingWidgetJs`, `Invalid(String)`

- [ ] **Step 1:** Add `.github/workflows/ci.yml` (job name `build`, so the check is called **`CI / build`**). It runs on `pull_request` and on `push` to `main`, on `windows-latest`, with `permissions: contents: read`. Its steps are `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, and `cargo deny check` (add a `deny.toml` that allows the licenses MIT, Apache-2.0, BSD-2/3-Clause, ISC, Unicode-3.0, Zlib, and MPL-2.0, and that denies yanked crates and RustSec advisories). Pin every action to a full commit SHA with a `# vX` comment, and use Swatinem/rust-cache for caching. Then create the workspace, the Tauri 2 app crate (`app/`) with no windows in `tauri.conf.json` (`"app": {"windows": []}`), and `ui/index.html` containing just `<!doctype html><title>Perchbar</title>`. `main.rs` builds and runs Tauri with `.run(tauri::generate_context!())`, and `RunEvent::ExitRequested` must call `api.prevent_exit()` so that having no windows doesn't exit the app. Copy the spec and plans into the first commit.

- [ ] **Step 2: Write failing tests** in `manifest.rs`:

```rust
#[test] fn loads_valid_manifest()        // fixture dir with widget.json {id:"btc",name:"BTC",formatVersion:1,refresh:60,permissions:{network:["api.coinbase.com"]}} + empty widget.js → Ok, refresh==60, network==["api.coinbase.com"]
#[test] fn refresh_below_minimum_is_raised() // refresh: 3 → manifest.refresh == 10
#[test] fn missing_widget_js_is_error()  // → Err(MissingWidgetJs)
#[test] fn id_must_be_kebab_case()       // id "My Widget" → Err(Invalid(_)); "my-widget-2" ok
#[test] fn unknown_format_version_is_error() // formatVersion: 2 → Err(Invalid(_))
#[test] fn network_entries_must_be_bare_hostnames() // "https://x.com/path" or "x.com:8080" → Err(Invalid(_)); "api.x.com" ok
#[test] fn select_requires_options()     // kind select with empty options → Err(Invalid(_))
```

Use `tempfile` (dev-dependency) for fixture directories.

- [ ] **Step 3:** Run `cargo test -p perchbar manifest`. Expected: FAIL (does not compile).
- [ ] **Step 4:** Implement `manifest.rs` and `paths.rs`. JSON field names are camelCase (`formatVersion`). The id must match `^[a-z0-9]+(-[a-z0-9]+)*$`. Write `schema/widget.schema.json` (draft 2020-12) to mirror exactly these rules.
- [ ] **Step 5:** Run `cargo test -p perchbar manifest`. Expected: PASS. Then run `cargo build`. Expected: builds.
- [ ] **Step 6:** Commit with the message `feat: scaffold Perchbar and widget manifest parsing`.

---

### Task 2: Display model (what a widget returns)

**Files:**
- Create: `app/src/display.rs`
- Test: same file

**Interfaces:**
- Produces (serde, camelCase, internally tagged by `"type"`):
  - `Output { frames: Vec<Frame>, rotate: Option<u32> /* seconds, min 2 */, click: Option<ClickAction> }`
  - `Frame { blocks: Vec<Block>, tooltip: Option<String> }`
  - `Block` enum:
    - `Text { value: String, color: Color, size: TextSize /* Normal|Small */ }`
    - `Icon { name: Option<String>, src: Option<String>, tint: bool /* default false */, color: Color }`: exactly one of `name` (built-in) or `src` (relative path to a `.svg`/`.png` inside the widget folder) is required. With `tint: true`, the image's alpha is kept and its color is replaced by `color`.
    - `Bar { value: f64 /* 0..=1 */, color: Color, width: Option<u32> /* DIPs, default 32 */ }`
    - `Ring { value: f64, color: Color }`
    - `Spark { values: Vec<f64>, color: Color }`
    - `Stack { top: Box<Block>, bottom: Box<Block> }` (two lines; neither may be a `Stack`)
  - `Color`: one of `"default" | "muted" | "up" | "down" | "warn" | "accent"` or `"#RRGGBB"`; default `default`
  - `ClickAction`: `"settings"` only in Plan 1 (`media.playPause` etc. arrive in Plan 3)
  - `display::parse(v: serde_json::Value) -> Result<Output, DisplayError>`, which accepts **either** a full `Output` **or** a single `Frame` (shorthand → `frames: [frame]`)
  - `DisplayError::Invalid(String)`

- [ ] **Step 1: Write failing tests:**

```rust
#[test] fn single_frame_shorthand()   // {"blocks":[{"type":"text","value":"BTC $1"}]} → frames.len()==1, color Default, size Normal
#[test] fn full_output_with_rotate()  // {"frames":[f,f],"rotate":5} → rotate Some(5)
#[test] fn rotate_below_two_is_raised() // rotate 1 → Some(2)
#[test] fn bar_value_is_clamped()     // value 1.7 → 1.0 ; -0.2 → 0.0 ; NaN → Err
#[test] fn hex_color_parses()         // "#1A2b3C" ok ; "#12345" Err ; "red" Err
#[test] fn nested_stack_rejected()    // Stack{top:Stack{..}} → Err
#[test] fn empty_frames_rejected()    // {"frames":[]} → Err
#[test] fn text_is_capped()           // value of 500 chars → truncated to 120 chars + "…"
#[test] fn spark_needs_two_points()   // values [1] → Err ; >60 points → keeps last 60
#[test] fn icon_needs_exactly_one_of_name_or_src() // both → Err ; neither → Err
#[test] fn icon_src_must_be_relative_svg_or_png()  // "logo.svg" ok ; "img/logo.png" ok ; "../x.svg", "C:\\x.png", "https://x/y.png", "logo.gif" → Err
```

- [ ] **Step 2:** Run `cargo test -p perchbar display`. Expected: FAIL.
- [ ] **Step 3:** Implement `display.rs`.
- [ ] **Step 4:** Run `cargo test -p perchbar display`. Expected: PASS.
- [ ] **Step 5:** Commit with the message `feat: widget display model`.

---

### Task 3: Instances file

**Files:**
- Create: `app/src/instances.rs`
- Test: same file

**Interfaces:**
- Produces:
  - `Instance { id: String /* "inst-" + 8 hex */, widget: String /* manifest id */, settings: serde_json::Map<String, Value>, hidden: bool, max_width: Option<u32> }`
  - `Group { id: String /* "grp-" + 8 hex */, members: Vec<String> /* instance ids */, interval: u32 /* seconds, default 8, min 3 */ }`
  - `Instances { instances: Vec<Instance>, groups: Vec<Group>, order: Vec<String> /* instance or group ids, left→right */ }`
  - `instances::load(path) -> Instances` (a missing or corrupt file gives an empty default; a corrupt file is first renamed to `instances.json.bak-<unix>`)
  - `instances::save(path, &Instances) -> io::Result<()>` (writes `instances.json.tmp`, then renames it, so the write is atomic)
  - `Instances::new_instance(&mut self, m: &Manifest) -> String`: fills settings from manifest defaults, appends the id to `order`, and returns the id
  - `Instances::normalize(&mut self, known_widgets: &HashSet<String>)`: drops instances of unknown widgets, drops group members that don't exist, drops empty groups, removes from `order` any instance that is in a group, and appends missing ids to `order`
  - `Instances::effective_settings(&self, inst: &Instance, m: &Manifest) -> Map`: manifest defaults overlaid with the instance's settings

- [ ] **Step 1: Write failing tests:**

```rust
#[test] fn two_instances_of_same_widget_have_independent_settings()
#[test] fn new_instance_uses_manifest_defaults()
#[test] fn normalize_drops_unknown_widget_and_empty_group()
#[test] fn grouped_instance_not_in_top_level_order()
#[test] fn corrupt_file_backed_up_and_defaulted()
#[test] fn save_then_load_roundtrip()
```

- [ ] **Step 2:** Run `cargo test -p perchbar instances`. Expected: FAIL.
- [ ] **Step 3:** Implement `instances.rs`. Ids use random hex from `rand` (or `getrandom`).
- [ ] **Step 4:** Run `cargo test -p perchbar instances`. Expected: PASS.
- [ ] **Step 5:** Commit with the message `feat: instances and groups model`.

---

### Task 4: Sandbox: run widget.js

**Files:**
- Create: `app/src/sandbox/mod.rs`, `app/src/sandbox/prelude.js`, `app/src/sandbox/host.rs` (storage, log, time only in this task)
- Test: `app/src/sandbox/mod.rs`

**Interfaces:**
- Consumes: `Manifest` (Task 1), `display::parse` (Task 2)
- Produces:
  - `sandbox::RunContext { manifest: Arc<Manifest>, widget_dir: PathBuf, instance_id: String, settings: Map<String, Value>, http: Arc<dyn HttpClient> }`
  - `sandbox::run(ctx: RunContext) -> impl Future<Output = Result<Output, RunError>>`
  - `RunError` variants: `Timeout`, `OutOfMemory`, `Script(String) /* message + line */`, `BadOutput(String)`, `Permission(String)`
  - `sandbox::HttpClient` trait (implemented in Task 5): `async fn send(&self, req: HttpRequest) -> Result<HttpResponse, String>`; `HttpRequest { method, url, headers: Vec<(String,String)>, body: Option<String> }`, `HttpResponse { status: u16, headers: Vec<(String,String)>, body: String }`

**What a widget author writes** (this is the contract AGENTS.md documents in Plan 3):

```js
// widget.js
export default async function update({ settings, storage, log, time }) {
  const r = await fetch(`https://api.coinbase.com/v2/prices/${settings.coin}-${settings.currency}/spot`);
  const price = Number((await r.json()).data.amount);
  const prev = storage.get("last");
  storage.set("last", price);
  return { blocks: [
    { type: "icon", name: "bitcoin" },
    { type: "text", value: format.money(price, settings.currency), color: prev == null ? "default" : price >= prev ? "up" : "down" },
  ], tooltip: `Updated ${time.now().hhmm}` };
}
```

- Globals: `fetch(url, {method, headers, body})` returns a `Promise<{ok, status, headers, text(), json()}>`, plus `format` and `console.log` (an alias of `log`).
- The argument object: `settings` (plain object), `storage` (`get(key)`, `set(key, value)`, JSON values, persisted per instance to `paths::storage_file`, max 64 KB), `log(...args)` (writes to `perchbar.log` with the instance id prefix), and `time` (`now(tz?: string) -> {year, month, day, hour, minute, second, weekday /* "Mon" */, hhmm /* "14:05" */, offsetMinutes}` using `chrono-tz`; an unknown tz throws `TypeError`).
- `format` (implemented in `prelude.js`): `format.number(n, decimals = 0)` with `,` grouping, `format.money(n, currency)` (symbol prefix for USD `$`, EUR `€`, GBP `£`; otherwise an ISO code suffix; 2 decimals under 1000, 0 at or above), and `format.percent(fraction, decimals = 0)`.

- [ ] **Step 1: Write failing tests** using an `HttpClient` fake that returns canned responses:

```rust
#[tokio::test] async fn returns_parsed_output()           // widget.js returns {blocks:[{type:"text",value:"hi"}]} → Ok, text "hi"
#[tokio::test] async fn settings_are_passed()              // returns settings.city as text
#[tokio::test] async fn script_error_reports_message()     // throw new Error("boom") → Err(Script(m)) where m contains "boom"
#[tokio::test] async fn sandbox_timeout_is_error()         // while(true){} → Err(Timeout) within ~11 s (use a 200 ms limit via a test-only override)
#[tokio::test] async fn memory_limit_is_error()            // allocate huge array → Err(OutOfMemory)
#[tokio::test] async fn invalid_output_is_error()          // return 42 → Err(BadOutput(_))
#[tokio::test] async fn storage_persists_between_runs()    // run twice; 2nd run sees value set in 1st
#[tokio::test] async fn no_node_or_browser_globals()       // typeof require, process, window, XMLHttpRequest all "undefined"
#[tokio::test] async fn time_now_respects_timezone()       // time.now("Asia/Tokyo").offsetMinutes == 540
#[tokio::test] async fn format_money_usd()                 // format.money(67120.4,"USD") == "$67,120"; format.money(3.456,"EUR") == "€3.46"
```

- [ ] **Step 2:** Run `cargo test -p perchbar sandbox`. Expected: FAIL.
- [ ] **Step 3:** Implement. Create a fresh `rquickjs::AsyncRuntime` + `AsyncContext` per run, so widgets never share state. Use `set_memory_limit(32 MiB)` and an interrupt handler that checks a deadline (10 s, overridable in tests). Wrap the whole run in `tokio::time::timeout` as well. Load `widget.js` as an ES module and call its default export with the argument object. Evaluate `prelude.js` (via `include_str!`) before the module. Rust exposes a hidden `__host_fetch(reqJson) -> Promise<respJson>`; `prelude.js` wraps it into `fetch` and then deletes `globalThis.__host_fetch`, keeping a closure reference.
- [ ] **Step 4:** Run `cargo test -p perchbar sandbox`. Expected: PASS.
- [ ] **Step 5:** Commit with the message `feat: sandboxed widget runtime`.

---

### Task 5: Network permissions and the real HTTP client

**Files:**
- Create: `app/src/sandbox/permissions.rs`; Modify: `app/src/sandbox/host.rs` (real `ReqwestClient: HttpClient`)
- Test: `app/src/sandbox/permissions.rs`, `app/src/sandbox/mod.rs`

**Interfaces:**
- Produces:
  - `permissions::check_url(url: &str, allowed: &[String]) -> Result<url::Url, String>`
  - `permissions::redirect_allowed(chain: &[url::Url], allowed: &[String]) -> bool` (every URL passes `check_url`, and `chain.len() <= 6`, meaning the original request plus 5 hops)
  - `host::ReqwestClient::new() -> Self` (rustls, a 10 s per-request timeout, a 2 MB response cap, user agent `Perchbar/<version>`, and a custom redirect policy that calls `check_url` on every hop, with at most 5 hops)
  - `sandbox::run` calls `check_url` before every `send`. A violation rejects the JS promise with `TypeError("Perchbar: host not allowed: <host>")` and is logged, and the run result is `Err(Permission(_))` if uncaught.

- [ ] **Step 1: Write failing tests:**

```rust
#[test] fn allows_exact_host()                    // "https://api.coinbase.com/v2/x", ["api.coinbase.com"] → Ok
#[test] fn rejects_other_host()                   // "https://evil.com/" → Err
#[test] fn rejects_userinfo_trick()               // "https://api.coinbase.com@evil.com/" → Err
#[test] fn rejects_subdomain_not_declared()       // "https://x.api.coinbase.com/" with ["api.coinbase.com"] → Err
#[test] fn host_match_is_case_insensitive()       // "https://API.Coinbase.com/" → Ok
#[test] fn rejects_non_https()                    // "http://api.coinbase.com/" → Err
#[test] fn rejects_ip_literal_unless_declared()   // "https://127.0.0.1/" → Err
#[test] fn redirect_to_undeclared_host_blocked() // permissions::redirect_allowed(&[Url("https://api.coinbase.com/a"), Url("https://evil.com/b")], &["api.coinbase.com"]) == false; the ReqwestClient redirect policy calls redirect_allowed on the hop chain
#[test] fn more_than_five_redirects_blocked()
#[tokio::test] async fn undeclared_fetch_rejects_promise_in_js() // widget catches and returns e.message as text; contains "host not allowed"
```

- [ ] **Step 2:** Run `cargo test -p perchbar permissions`. Expected: FAIL.
- [ ] **Step 3:** Implement. Parse with `url::Url`. The scheme must be `https`, the `host_str()` (with userinfo ignored by the parser) must equal one of the allowed entries case-insensitively, and the port must be the default.
- [ ] **Step 4:** Run `cargo test -p perchbar`. Expected: PASS.
- [ ] **Step 5:** Commit with the message `feat: network allow-list and http client`.

---

### Task 6: Scheduler (refresh, backoff, stale, model publishing)

**Files:**
- Create: `app/src/scheduler.rs`, `app/src/strip/model.rs`
- Test: `app/src/scheduler.rs`

**Interfaces:**
- Consumes: `Instances` (Task 3), `sandbox::run` (Task 4), `Output` (Task 2)
- Produces:
  - `strip::model::SlotState { Loading, Ok(Output), Stale(Output), Error { message: String, last: Option<Output> } }`
  - `strip::model::Slot { id: String /* instance or group id */, kind: SlotKind }` with `SlotKind::Single { instance_id, state: SlotState, max_width: Option<u32> }` and `SlotKind::Group { interval: u32, members: Vec<(String /*instance_id*/, SlotState)> }`
  - `strip::model::StripModel { slots: Vec<Slot> /* in order */, generation: u64 }`
  - `scheduler::Scheduler::start(widgets: HashMap<String, (Arc<Manifest>, PathBuf)>, instances: Instances, http: Arc<dyn HttpClient>, publish: impl Fn(StripModel) + Send + Sync + 'static) -> SchedulerHandle`
  - `SchedulerHandle::refresh_now(&self, instance_id: &str)`, `SchedulerHandle::set_hidden(&self, instance_id: &str, hidden: bool)` (persists via `instances::save`), `SchedulerHandle::shutdown(self)`
  - `scheduler::next_delay(refresh: u32, consecutive_failures: u32) -> Duration`: `refresh` when there are 0 failures, otherwise `min(refresh * 2^failures, 600)` seconds

- [ ] **Step 1: Write failing tests** with a fake runner (make the run function injectable: `Scheduler::start_with_runner`):

```rust
#[test] fn backoff_doubles_and_caps()            // next_delay(60,0)=60s, (60,1)=120s, (60,3)=480s, (60,4)=600s, (10,10)=600s
#[tokio::test] async fn failure_keeps_last_output_stale() // ok then Err → state Error{last:Some(prev)} rendered as stale ; 2nd success → Ok
#[tokio::test] async fn one_failure_does_not_stop_others() // inst A always errors, inst B ok → B publishes Ok every tick
#[tokio::test] async fn hidden_instances_not_published_and_not_run()
#[tokio::test] async fn group_members_published_in_group_slot()
#[tokio::test] async fn publish_only_on_change()  // identical Output twice → publish called once for that change
```

Use `tokio::time::pause()` to advance time.

- [ ] **Step 2:** Run `cargo test -p perchbar scheduler`. Expected: FAIL.
- [ ] **Step 3:** Implement one tokio task per visible instance. Limit concurrent runs with a `Semaphore(4)`. The first run starts immediately with a 0 to 2 s random jitter. Hold the model behind a `Mutex`, bump `generation` on every change, and call `publish` with a clone.
- [ ] **Step 4:** Run `cargo test -p perchbar scheduler`. Expected: PASS.
- [ ] **Step 5:** Commit with the message `feat: scheduler with backoff and stale state`.

---

### Task 7: Strip layout (pure)

**Files:**
- Create: `app/src/strip/layout.rs`
- Test: same file

**Interfaces:**
- Consumes: `StripModel` (Task 6), `Block` (Task 2)
- Produces:
  - `trait Measure { fn text_width(&self, text: &str, size: TextSize) -> f32 /* DIPs */; }` (Task 8 implements it with DirectWrite; tests use a fake with 7 DIPs per char)
  - `layout::Metrics { height_dip: f32 /* taskbar height in DIPs */, scale: f32 /* dpi/96 */, available_dip: f32 /* free width */ }`
  - `layout::layout(model: &StripModel, rot: &RotationState, metrics: &Metrics, m: &dyn Measure) -> StripLayout`
  - `StripLayout { width_dip: f32, items: Vec<PlacedSlot>, overflow: Vec<String> /* slot ids hidden for space */ }`
  - `PlacedSlot { slot_id: String, x_dip: f32, width_dip: f32, frame: FrameRef /* which instance + frame index to draw */, rotating: bool, stale: bool, error: Option<String> }`
  - `RotationState { group_index: HashMap<String, usize>, frame_index: HashMap<String, usize>, paused_slot: Option<String> }` with `RotationState::tick(&mut self, model: &StripModel, now: Instant)` advancing groups by `interval` and multi-frame outputs by `rotate`, except the paused (hovered) slot
- Fixed values (DIPs): padding between slots 12, padding inside a slot 4 left/right, gap between blocks 4, icon size 16, ring size 16, default bar width 32 and height 4, spark size 32 × 14, normal text 12 pt, small text 9 pt, "+N" item width = text width + 8, and a minimum gap of 8 DIPs to app buttons.
- Width rules: a slot's width is the sum of its blocks plus padding, capped by `max_width` (text is ellipsized to fit). A **group's** width is the widest width of any member's frames, and a multi-frame output's width is its widest frame, so the strip doesn't jitter.
- Overflow: keep slots from the **left of `order`**, and once the next one doesn't fit within `available_dip` (counting the "+N" width), move it and every later slot into `overflow`.

- [ ] **Step 1: Write failing tests:**

```rust
#[test] fn single_text_slot_width()         // "BTC $67,120" (11 chars → 77) + 8 padding → 85
#[test] fn group_width_is_widest_member()
#[test] fn multi_frame_width_is_widest_frame()
#[test] fn overflow_hides_from_end_of_order() // available 150, three 85-wide slots → items [s1], overflow [s2,s3]
#[test] fn no_overflow_item_when_all_fit()
#[test] fn rotation_advances_and_pauses_on_hover()
#[test] fn layout_scales_with_dpi()          // same model at scale 1.5: width_dip identical, (pixel width = round(width_dip*1.5)) checked via helper px()
#[test] fn max_width_ellipsizes()            // max_width 40 → slot width 40
#[test] fn zero_available_space_shows_only_plus_n()
```

- [ ] **Step 2:** Run `cargo test -p perchbar layout`. Expected: FAIL.
- [ ] **Step 3:** Implement `layout.rs`.
- [ ] **Step 4:** Run `cargo test -p perchbar layout`. Expected: PASS.
- [ ] **Step 5:** Commit with the message `feat: strip layout with rotation and overflow`.

---

### Task 8: Renderer, theme, icons

**Files:**
- Create: `app/src/strip/render.rs`, `app/src/strip/theme.rs`, `app/src/strip/icons.rs`
- Test: `app/src/strip/icons.rs`, `app/src/strip/theme.rs`, and a render smoke test in `render.rs`

**Interfaces:**
- Consumes: `StripLayout` (Task 7)
- Produces:
  - `theme::Palette { text, muted, up, down, warn, accent, bar_track, hover_bg: [u8;4] /* premultiplied-ready RGBA */ }`
  - `theme::current() -> (bool /*dark*/, Palette)`: reads `HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize\SystemUsesLightTheme` (0 = dark) and the accent color from `DwmGetColorizationColor`. Colors: dark text `#FFFFFF`, muted `#FFFFFF` at 60% alpha, up `#6CCB5F`, down `#FF99A4`, warn `#FCE100`; light text `#1A1A1A`, muted `#1A1A1A` at 60% alpha, up `#0F7B0F`, down `#C42B1C`, warn `#9D5D00`.
  - `theme::resolve(color: &Color, p: &Palette) -> [u8;4]`
  - `icons::codepoint(name: &str) -> Option<char>`: a name → Segoe Fluent Icons glyph table. The first set is `bitcoin`→none (falls back to the text "₿"), `clock` U+E917, `globe` U+E774, `up` U+E70E, `down` U+E70D, `warning` U+E7BA, `battery` U+E83F, `music` U+EC4F, `mail` U+E715, plus weather glyphs `sun` U+E706, `cloud` U+E753, `rain` U+E9C4 (verify each glyph visually in Step 5 and correct any wrong codepoint). An unknown name gives `None` and renders a small dot.
  - `render::Renderer::new() -> Result<Self>` (D2D factory + DWrite factory, created once)
  - `render::Renderer::render(&mut self, layout: &StripLayout, model: &StripModel, metrics: &Metrics, palette: &Palette, hover: Option<&str>) -> Bitmap`, where `Bitmap { width: i32, height: i32, pixels: Vec<u8> /* premultiplied BGRA */ }`
  - `impl Measure for Renderer` (DirectWrite, font family "Segoe UI Variable Text" falling back to "Segoe UI")
  - **Custom icons:** `render::ImageCache::get(widget_dir: &Path, src: &str, size_px: u32, tint: Option<[u8;4]>) -> Option<Arc<Bitmap>>`. It resolves `src` against the widget folder, canonicalizes the path, and rejects it if the result is outside the folder (catching symlinks and junctions) or the file is over 256 KB. SVGs are rasterized with `resvg`/`usvg` (no external file or font references) and PNGs decoded with `png`, both scaled to `size_px` (16 DIPs × scale). Results are cached by (path, mtime, size_px, tint). A missing or bad image is logged once and drawn as the small-dot fallback.

- [ ] **Step 1: Write failing tests:**

```rust
#[test] fn known_icons_resolve()           // icons::codepoint("clock") == Some('\u{E917}')
#[test] fn unknown_icon_is_none()
#[test] fn resolve_hex_and_semantic_colors()
#[test] fn render_smoke_nonempty_pixels()  // render a one-text-slot layout at scale 1.0 and 1.5 → bitmap height == round(height_dip*scale), some pixel alpha > 0, no panic
#[test] fn measure_is_monotonic()          // text_width("WW") > text_width("W")
#[test] fn svg_icon_rasterizes_at_scale()  // fixture logo.svg at size 24 → 24×24 bitmap with alpha > 0
#[test] fn tint_replaces_color_keeps_alpha()
#[test] fn icon_outside_widget_dir_rejected() // a junction/symlink in the widget dir pointing outside → None
#[test] fn oversized_icon_rejected()      // 300 KB png → None
```

- [ ] **Step 2:** Run `cargo test -p perchbar render icons theme`. Expected: FAIL.
- [ ] **Step 3:** Implement. Use a DIB section (32-bit top-down) bound to an `ID2D1DCRenderTarget` with `D2D1_ALPHA_MODE_PREMULTIPLIED` and text antialias `GRAYSCALE` (ClearType doesn't work on per-pixel-alpha surfaces). Clear to fully transparent. A stale slot draws at 60% opacity. An error slot draws the `warning` icon in `warn` plus the last output at 60% opacity if there is one. A rotating slot shows a 2 px tall indicator of dots under its content, using `muted`. A hovered slot gets a rounded rect in `hover_bg` (white at 8% alpha in dark mode, black at 6% in light mode, radius 4). Text is vertically centered, and a `Stack` puts its two lines at 30% and 72% of the height.
- [ ] **Step 4:** Run `cargo test -p perchbar render icons theme`. Expected: PASS.
- [ ] **Step 5:** Write a temporary dev binary `app/examples/render_png.rs` that writes `render.png` for a sample layout in dark and light mode. Look at it and fix glyphs and alignment, then commit it as a dev tool.
- [ ] **Step 6:** Commit with the message `feat: Direct2D strip renderer with theme and icons`.

---

### Task 9: Placement and the strip window

**Files:**
- Create: `app/src/strip/placement.rs`, `app/src/strip/window.rs`, `app/src/strip/mod.rs`
- Test: `app/src/strip/placement.rs`

**Interfaces:**
- Consumes: `Renderer` (Task 8), `layout` (Task 7), `StripModel` (Task 6)
- Produces:
  - `placement::TaskbarInfo { taskbar: RECT, tray_left: i32, buttons_right: i32 /* rightmost edge of app buttons/Start/Search/Widgets */, dpi: u32, hidden: bool /* auto-hide currently hidden */ }`
  - `placement::query() -> Option<TaskbarInfo>`: `FindWindowW("Shell_TrayWnd")` → `GetWindowRect`, `TrayNotifyWnd` left edge (falling back to `taskbar.right`), and `GetDpiForWindow(taskbar)`. `buttons_right` uses UI Automation: from `ElementFromHandle(taskbar)`, `FindAll(TreeScope_Descendants)` on elements that are buttons, excluding descendants of the tray, taking the max `BoundingRectangle.right`; if UIA fails, use the taskbar's horizontal center plus 25% of its width as a conservative guess. `hidden` is true when less than 4 px of the taskbar rect is on its monitor's work area edge.
  - `placement::compute(info: &TaskbarInfo, strip_width_px: i32) -> Option<(i32, i32, i32, i32)>`, which returns the strip rect **relative to the taskbar client area** (x, y, w, h) with right edge = `tray_left - 2*scale`, or `None` if the info is hidden
  - `placement::available_px(info: &TaskbarInfo) -> i32 = tray_left - buttons_right - round(8*scale) - round(2*scale)`, clamped to ≥ 0
  - `strip::spawn(rx: std::sync::mpsc::Receiver<StripEvent>, sched: SchedulerHandle) -> JoinHandle<()>`
  - `StripEvent::{ Model(StripModel), Quit }`

- [ ] **Step 1: Write failing tests** for the pure functions:

```rust
#[test] fn strip_sits_left_of_tray()        // taskbar 0..1920, tray_left 1700, w 200 @96dpi → x = 1700-2-200 = 1498
#[test] fn available_excludes_buttons_and_gap() // buttons_right 1100, tray_left 1700, 96dpi → 1700-1100-8-2 = 590
#[test] fn available_never_negative()
#[test] fn hidden_taskbar_gives_none()
#[test] fn scales_gap_with_dpi()            // 144dpi → gap 3 px and 12 px
```

- [ ] **Step 2:** Run `cargo test -p perchbar placement`. Expected: FAIL.
- [ ] **Step 3:** Implement the pure functions and run the tests to make them PASS.
- [ ] **Step 4:** Implement `window.rs` and `strip/mod.rs`, following the reference `ensure_widget`/`present`:
  - The strip thread is per-monitor DPI aware v2, registers a window class, and creates the strip as `WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE` with `WS_CHILD | WS_CLIPSIBLINGS`, parented to `Shell_TrayWnd`.
  - `present()` uses `UpdateLayeredWindow` with `ULW_ALPHA` and the rendered bitmap.
  - After positioning, if `GetWindow(taskbar, GW_CHILD) != strip`, call `SetWindowPos(HWND_TOP, …, SWP_NOACTIVATE | SWP_SHOWWINDOW)`.
  - Re-attach: `RegisterWindowMessageW("TaskbarCreated")` on a hidden top-level message window recreates the strip when Explorer restarts. Also check on every placement tick: if `FindWindowW` returns a different taskbar HWND, recreate.
  - Placement ticks: a `SetWinEventHook` for `EVENT_OBJECT_LOCATIONCHANGE`, `EVENT_OBJECT_CREATE`, and `EVENT_OBJECT_DESTROY` scoped to the Explorer process id (`GetWindowThreadProcessId(taskbar)`), debounced to 250 ms, plus a 5 s fallback `SetTimer`. `WM_SETTINGCHANGE` ("ImmersiveColorSet") triggers a theme reload and redraw. `WM_DPICHANGED` (and a DPI change seen on a tick) triggers a re-layout.
  - The rotation timer is a `SetTimer` firing at the smallest active interval (≥ 1 s), and only while some slot rotates.
  - Redraw only when the key (model generation, rotation indices, hover, dark, dpi, height, available width) changes.
  - Spawn this from `main.rs`, and feed `Model` events from the scheduler's `publish` through the channel plus `PostMessageW(WM_APP)` to wake the loop.
- [ ] **Step 5: Manual check (run `cargo run -p perchbar`) with a test widget that returns static text.** Each must pass:
  - The text appears left of the tray, aligned, with no box or frame, in both dark and light mode. Switch the Windows theme while it runs and check that it flips within about 1 s.
  - Run `taskkill /f /im explorer.exe` then `start explorer`. The strip is back within about 2 s.
  - Pin and open many apps until the buttons approach the strip. Widgets go into the "+N" overflow and never cover a button.
  - At display scale 100%, 150% and 200% (Settings → Display), the text is sharp and the height matches the taskbar.
  - With auto-hide taskbar on, the strip hides and shows together with the taskbar.
  - Task Manager shows Perchbar at 0% CPU when idle.
- [ ] **Step 6:** Commit with the message `feat: taskbar strip window and placement`.

---

### Task 10: Interaction (hover, tooltip, click, scroll, right-click menu, "+N" flyout)

**Files:**
- Modify: `app/src/strip/window.rs`
- Test: `app/src/strip/layout.rs` (hit-testing)

**Interfaces:**
- Produces: `layout::hit_test(layout: &StripLayout, x_dip: f32) -> Option<Hit>`, where `Hit::{ Slot(String), Overflow }`

Behavior:
- **Hover:** highlights the slot, pauses its rotation (via `RotationState.paused_slot`), and shows a tooltip after 400 ms using a `TOOLTIPS_CLASSW` window (as in the reference). The tooltip shows the frame's `tooltip`, or the error message for an error slot, or `"<last tooltip>\nNot updated since hh:mm"` for a stale slot.
- **Left click** on a slot: Plan 1 opens nothing (the settings window comes in Plan 2). It calls `refresh_now` and briefly dims the slot. On a group, left click advances to the next member.
- **Mouse wheel** on a group or multi-frame slot: moves to the next or previous member/frame.
- **Right-click** on a slot: a popup menu (`TrackPopupMenuEx`) with Refresh now, Hide this widget, a separator, Quit Perchbar. On empty strip space or "+N", the menu has only Quit Perchbar.
- **Left click on "+N":** a popup menu listing the hidden widgets by name, with their current main text where there is one (e.g. "Bitcoin price: $67,120"). Clicking an entry calls `refresh_now`. A proper flyout window is a Plan 2 item.
- Mouse tracking uses `TrackMouseEvent(TME_LEAVE)`, and WM_MOUSELEAVE clears hover.

- [ ] **Step 1: Write failing tests:** `hit_test_finds_slot`, `hit_test_overflow_item`, `hit_test_gap_is_none`.
- [ ] **Step 2:** Run `cargo test -p perchbar layout`. Expected: FAIL.
- [ ] **Step 3:** Implement `hit_test`, then the window message handling.
- [ ] **Step 4:** Run `cargo test -p perchbar`. Expected: PASS. Then do a manual check of each behavior listed above.
- [ ] **Step 5:** Commit with the message `feat: strip interaction`.

---

### Task 11: First run, templates, logging, end-to-end

**Files:**
- Create: `widgets/bitcoin-price/{widget.json,widget.js,README.md}`, `widgets/world-clock/{widget.json,widget.js,README.md}`
- Modify: `app/src/main.rs`
- Test: `app/tests/templates.rs` (integration test: each bundled template runs in the sandbox with a fake HTTP client)

**Interfaces:**
- Consumes: everything above.
- `main.rs` startup order:
  1. Single-instance guard (named mutex `Local\Perchbar`). If it's already running, exit.
  2. Logging to `paths::log_file()` (rotate at 1 MB, keep 1).
  3. Create the data dirs, and copy bundled `widgets/*` (via `include_dir!` or Tauri resources) into the widgets dir for any id not already present.
  4. Load all widget folders. A widget that fails to load is logged and skipped.
  5. `instances::load` then `normalize`. If the file had no instances (first run), create one instance of `bitcoin-price` and one of `world-clock`, then save.
  6. Start the scheduler with `ReqwestClient`, spawn the strip, and run Tauri.

Templates:
- **bitcoin-price**: refresh 60, network `["api.coinbase.com"]`, settings `coin` (select: BTC, ETH, SOL, XRP, DOGE; default BTC), `currency` (select: USD, EUR, GBP; default USD). It calls `GET https://api.coinbase.com/v2/prices/{coin}-{currency}/spot`, which responds with `{data:{amount:"67120.41"}}`. Display: icon `bitcoin` (falls back to the text "₿") when the coin is BTC, otherwise the coin symbol as text; the price via `format.money`; color `up`/`down` against the stored last price; and a 24-point sparkline of stored history once there are at least 2 points. Tooltip: `"BTC-USD $67,120.41 · updated 14:05"`.
- **world-clock**: refresh 10, no network, settings `cities` (list; each entry is `"Label|IANA tz"`, default `["Amsterdam|Europe/Amsterdam", "Tokyo|Asia/Tokyo"]`) and `rotate` (number, seconds, default 6). With one city it shows one frame `"Tokyo 22:05"`. With more cities it returns one frame per city and `rotate`. Each frame's tooltip shows the weekday plus the UTC offset (e.g. `"Tokyo · Thu · UTC+9"`).
- Each README is 5 to 10 lines: what it shows, its settings, and where the data comes from.

- [ ] **Step 1: Write failing integration tests:**

```rust
#[tokio::test] async fn bitcoin_template_renders_price()      // fake http returns {"data":{"amount":"67120.41"}} → first block icon or text, a text block "$67,120"
#[tokio::test] async fn bitcoin_template_colors_down_on_drop()  // two runs 67120 then 66000 → second run text color Down
#[tokio::test] async fn bitcoin_template_only_calls_coinbase()  // fake http records URLs; all host == api.coinbase.com
#[tokio::test] async fn world_clock_two_cities_rotates()        // frames.len()==2, rotate==Some(6)
#[tokio::test] async fn world_clock_bad_timezone_is_error()     // "X|Mars/Base" → Err(Script(_)) mentioning the tz
#[test] fn bundled_templates_pass_manifest_validation()
```

- [ ] **Step 2:** Run `cargo test -p perchbar --test templates`. Expected: FAIL.
- [ ] **Step 3:** Write both templates, keeping each `widget.js` under 50 lines and commented (they are examples for AI and people), and wire up `main.rs`.
- [ ] **Step 4:** Run `cargo test -p perchbar`. Expected: all PASS.
- [ ] **Step 5: End-to-end manual check:** delete `%APPDATA%\Perchbar`, then run `cargo run -p perchbar --release`. The Bitcoin price and the clock appear within about 3 s. Edit `instances.json` by hand to put both in a group (`"groups":[{"id":"grp-00000001","members":[…],"interval":5}]`), then restart. The two now alternate in one spot. Disconnect from the network: the price dims (stale), shows "Not updated since …" in its tooltip, and comes back on reconnect. Check memory in Task Manager (< 100 MB) and that idle CPU is about 0%.
- [ ] **Step 6:** Commit with the message `feat: first-run setup and bitcoin/world-clock templates`.

---

### Task 12: Open the pull request

- [ ] **Step 1:** Run `cargo test -p perchbar`, `cargo clippy -- -D warnings`, and `cargo build --release`. Expected: all pass, with no warnings.
- [ ] **Step 2:** Push `plan-1-core` and open a PR into `main` titled `Plan 1: Perchbar core (taskbar strip + widget runtime)`. The body summarizes what works, lists the manual checks from Tasks 9 to 11 with their results, and links the spec and the plan.

---

## Self-review notes

- Spec coverage for Plan 1: §5.1 (strip, rendering, Tauri shell), §5.2 (format, network, storage, log, time), §5.3 (all building blocks, rotate, tooltip, max width, rotation groups, overflow), §5.4 (isolation, limits, network allow-list), §5.8 (light/dark, right-click menu; primary taskbar only), §4 anti-goals (manual checks in Tasks 9 and 11), and §7 (Bitcoin and World clock).
- Deliberately deferred: Plan 2 covers the settings window, setting forms, instances/groups UI, secrets (DPAPI), autostart, the permission prompt at install, the "+N" flyout window, and click → settings. Plan 3 covers the catalog, updates with permission diffs, Share, AGENTS.md, the docs, the `files`/`battery`/`media` sources, the media click actions, and the Claude limit, Rotating prices, Weather, Now playing, and Device battery templates.
