# Perchbar: Design

Date: 2026-11-12
Status: approved 2026-11-12

## 1. Intent

A Windows 11 app that puts small, live, customizable widgets directly on the taskbar, the way Windows draws its own weather and stock widgets there. It is inspired by two single-purpose apps:

- `AbdoSarmini/claude-usage-widget-for-windows-taskbar`: Claude 5-hour/weekly usage drawn on the taskbar next to the clock.
- `HeyOkay/HaloBattery`: battery levels of wireless devices, shown as tray icons.

Those apps each do one thing. This one is a **platform**: anyone can install widgets from a catalog, tweak them through settings, or write their own.

Audience, in the user's words: tech-savvy people who are not developers (comfortable pasting an API key or changing a setting, should never have to write code), **and** developers who want to write their own widgets.

How people get widgets, in order of expected use:
1. Install a template from the catalog and adjust its settings (coin, city, key, colors).
2. Ask their own AI assistant (ChatGPT, Claude, Copilot, …) to write a widget, using the repo's documentation and templates as reference, then load it from a local folder.
3. Write one by hand (developers).

Good widgets get shared back to the catalog via a GitHub pull request.

## 2. Goals (v1)

- A taskbar host app that keeps a strip of widgets attached to the Windows 11 taskbar, looking native.
- A simple widget format: a folder with a manifest plus a small JavaScript file.
- A sandbox: widgets can only reach websites they declare and only the local resources the user approved.
- A settings window: installed widgets, per-widget settings forms (generated from the manifest), widget order and visibility, catalog browser, "load from folder", "Share".
- A catalog stored in this project's GitHub repo, which the app browses and installs from.
- A **Share** flow: validate a local widget, package it in catalog shape, and open a pre-filled GitHub pull request in the browser.
- AI-friendly documentation in the repo (`AGENTS.md` + widget guide) so any AI assistant can write working widgets.
- Rotation groups: several widgets sharing one spot on the taskbar and cycling (§5.3).
- Seven templates (see §7).

## 3. Non-goals (v1)

- **No built-in AI.** The app never asks for an AI API key and has no chat. People use their own AI against the repo docs.
- No vendor-specific 2.4 GHz receiver protocols for device battery (Halo Battery's approach). Only what Windows exposes. This is "not now," not "never."
- No free-form drawing or HTML in widgets. Display is limited to the building blocks in §5.3.
- No accounts, no backend server, no telemetry.
- No Windows 10 support, no macOS/Linux.
- No tray-icon mode. Widgets live on the taskbar itself.

## 4. Anti-goals (would be a failure even if it "works")

Confirmed by the user:
- **Looks bolted on.** Widgets must match the taskbar: system font, correct light/dark colors, correct DPI scaling, aligned to the taskbar height, no visible window frame or background box that differs from the taskbar.
- **Noticeable resource use.** Idle CPU should be ~0%; memory should be small (target well under 100 MB total for the app with the settings window closed). The settings window may use more while open.
- **Jumps around or disappears.** Must survive Explorer restarts, taskbar moves/resizes, DPI changes, monitor changes, tray icons being added/removed, and auto-hide taskbars, re-attaching quickly without flicker.

## 5. How it's built (builder's call, already made; user delegated tech choices)

### 5.1 App architecture

**Tauri (Rust core + WebView2 settings window).**

- **Rust core**: owns the taskbar strip, widget runtime, scheduling, sandbox, catalog fetch, settings storage, autostart.
- **Taskbar strip**: a single layered, non-activating tool window (`WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE`) positioned over the taskbar immediately left of the notification area (`Shell_TrayWnd` → `TrayNotifyWnd`), drawn with per-pixel alpha via `UpdateLayeredWindow`. This is the proven approach from the Claude usage widget. Read `src/main.rs` (`ensure_widget`, `present`) and `src/render.rs` of that repo before starting. All widgets render into this one strip, side by side, in the user's chosen order.
- **Rendering**: native (Direct2D/DirectWrite or equivalent) from the building blocks, *not* a WebView. That keeps memory low and the look native.
- **Settings window**: Tauri WebView2 window, opened from a click on the strip / a right-click menu. Closed by default; created on demand and destroyed on close to keep idle memory low.
- Frontend framework for the settings window: builder's choice.

### 5.2 Widget format

A widget is a folder:

```
bitcoin-price/
  widget.json     # manifest
  widget.js       # logic
  icon.svg|png    # optional, catalog/gallery icon
  README.md       # optional, shown in catalog
```

`widget.json` declares at least:
- `id`, `name`, `description`, `author`, `version`
- `refresh`: how often `widget.js` runs (seconds; host enforces a minimum, e.g. 10 s)
- `settings`: a list of user-facing fields (text, number, select, toggle, list-of-strings, secret/API key, city/location, color). The settings window renders a form from this automatically.
- `permissions`:
  - `network`: list of hostnames the widget may call
  - `files`: specific local file paths it may read (paths may use `%USERPROFILE%` etc.)
  - `sources`: built-in host data sources it uses (e.g. `battery`, `time`)
- `formatVersion` so the format can evolve.

**Instances (user decision):** a widget can be added more than once, each instance with its own settings (e.g. Weather for Amsterdam and Weather for Tokyo). One installed widget folder backs all its instances. Settings, secrets, storage, order, visibility, logs and schedules are keyed by an instance ID, not the widget `id`. The settings window has an "add another" action. Approved permissions belong to the widget, not the instance.

`widget.js` exports one function the host calls on each refresh. It receives the user's settings and the host API, and returns what to display (§5.3). Exact API shape is the builder's call, but it must be: tiny, documented with examples, easy for an AI to write correctly from the templates, and async (fetch returns a promise).

Host API offered to widgets (minimum):
- `fetch` (restricted to `permissions.network`)
- `readFile` (read-only, restricted to `permissions.files`)
- built-in sources (`battery` list, current time / time zones, `media` = current now-playing session with play/pause/next/previous actions)
- a widget may declare a click action (e.g. media play/pause). Without one, a left click opens that widget's settings (designer's call). Inside a rotation group, a click on a widget that has its own click action triggers that action. Skipping to the next widget is then done with the scroll wheel.
- small persistent per-widget storage (e.g. last value, for up/down coloring)
- `log` (visible in the settings window for debugging)

### 5.3 Display building blocks

A widget returns a description of what to show, composed from:
- **text** (with optional emphasis/size variants appropriate to taskbar height)
- **icon**: a built-in icon (Windows 11 icon font) **or a custom image shipped in the widget's folder** (SVG or PNG, e.g. the Claude logo). An optional `tint` recolors a single-color logo to follow the text color, so it works in both dark and light mode. Full-color images are drawn as they are. Images must live inside the widget folder (no URLs, no `..`), max 256 KB each.
- **color** (for text/icon/bar; also semantic colors like up/down/warning that adapt to light/dark)
- **progress bar / ring** (0 to 100%)
- **mini chart / sparkline** (a short series of numbers)
- **rotate**: a list of frames the strip cycles through every N seconds (used by rotating prices and could be used by world clock)
- **tooltip** text (shown on hover with more detail)

Width is determined by content, with a max width per widget the user can cap. Layout within a widget: one or two lines, matching the taskbar's height.

**Rotation groups (user decision, v1):** besides a widget rotating its own frames, the user can put several widget instances (any widgets, e.g. Bitcoin + Weather + Claude limit) into one **group** that occupies a single spot on the strip and cycles between them. Per group: the interval (seconds) and the order. Hovering pauses the rotation, clicking/scrolling skips to the next one, and a subtle indicator shows that a spot rotates. Each widget in a group keeps its own refresh schedule. The group's width is the widest member's width so the strip doesn't jitter. Groups are created in the settings window (drag widgets onto each other or "group with…"). For overflow, a group counts as one item.

**When widgets don't fit (user decision):** the strip never overlaps app buttons. The host measures the free taskbar space between the app buttons and the notification area. Widgets that don't fit are hidden starting from the end of the user's order, and a small "+N" item appears in the strip. Clicking it opens a flyout showing the hidden widgets. The strip re-measures whenever app buttons, tray icons, or the taskbar change.

### 5.4 Sandbox

Widget JS runs in an embedded JS engine inside the Rust core (e.g. QuickJS via `rquickjs`, or `boa`; builder's call), **not** in a browser or Node:
- No file system, process, or network access except through the host API above, and only within declared permissions.
- Time and memory limits per run; a misbehaving widget is stopped and shows an error state instead of affecting others.
- Each widget isolated from the others (separate context; no shared globals).
- Settings marked `secret` are stored encrypted with Windows DPAPI (user scope) and never shown in logs.

Permissions are shown to the user at install time ("This widget will contact: api.coinbase.com. It will read: ~/.claude/.credentials.json"). Widgets from the catalog have been reviewed through PR; widgets loaded from a local folder are marked "Local: not reviewed."

### 5.5 Catalog

- Lives in this project's GitHub repo, `widgets/<id>/`, one folder per widget.
- A generated `catalog.json` index at a stable URL (built by a GitHub Action on merge) lists all widgets with metadata and download paths. The app fetches it when the catalog view opens, plus a periodic check for updates of installed widgets.
- Install = download that widget folder into the user's widgets directory (`%APPDATA%\Perchbar\widgets\`).
- Contributions arrive as PRs; the repo owner reviews and merges. A CI check runs the same validation as the Share flow.
- Templates (§7) are the first catalog entries.
- **Updates (designer's call):** the app stores the permissions the user approved per widget. A catalog update whose permissions are identical or narrower installs automatically. An update that adds hosts, files or sources waits in the settings window as "update available, review permissions" until the user approves. The previous version is kept so an update can be rolled back. Reloading a local widget with changed permissions shows the same review prompt.

### 5.6 Local widgets and Share

- **Load from folder**: the user picks a folder; the app validates it, shows permissions, and installs it as a local widget. A "reload" action re-reads it so someone iterating with their AI sees changes immediately.
- **Share**: on any installed widget:
  1. Validate: required files present, manifest valid against the schema, a test run succeeds within limits, all network calls stay within declared hosts.
  2. Package into catalog shape.
  3. Open GitHub in the browser with a pre-filled "new file / pull request" flow (or, if not feasible via URL alone, open the repo's contribution page with the packaged folder revealed in Explorer and short instructions). The app never handles the user's GitHub login.

### 5.7 AI-friendly repo docs

- `AGENTS.md` at repo root: concise, self-contained spec of the widget format, host API, building blocks, permissions, and rules, plus "how to test locally." Written so a user can paste it (or the repo link) into any AI and get a working widget.
- `docs/widget-guide.md`: human-readable version with walkthroughs.
- A JSON Schema for `widget.json`.
- The templates double as examples; each is short and commented.

### 5.8 App behavior basics

- Light/dark follows Windows; updates live when the theme changes.
- Start with Windows: toggle, default on after first run.
- Right-click on a widget: refresh now, settings, hide, open settings window, quit.
- Primary monitor's taskbar only in v1 (builder may extend if cheap).
- App self-update: builder's call (Tauri updater or a "new version" notice).

## 6. Constraints

- Windows 11 only.
- Distribution: GitHub releases. Unsigned binaries are acceptable for v1 (SmartScreen warning, as in both reference apps).
- Open source; license: builder's call (both references are MIT).
- The repo is currently empty (`D:\Dev\toolbar-widgets`, not yet a git repository).

## 7. Templates (v1)

| Template | Data | Shows | Exercises |
|---|---|---|---|
| **Claude limit** | Reads `%USERPROFILE%\.claude\.credentials.json` (OAuth token from Claude Code), calls Anthropic's OAuth usage endpoint (see reference `src/api.rs`) | 5-hour and weekly usage as bars/percent, reset countdown in tooltip | `files` permission, network, progress bars |
| **Bitcoin price** | Public crypto price API, no key | Price, up/down color vs. previous value | Simplest network widget, storage |
| **Rotating prices** | Crypto: public API, no key. Stocks: free-key provider (e.g. Finnhub) via a `secret` setting | Cycles through a user-defined list of tickers with price, change color, optional sparkline | `rotate`, list setting, secret setting, sparkline |
| **Weather** | Free no-key weather API (e.g. Open-Meteo) for a chosen city | Temp + condition icon; details in tooltip | Location setting, icons |
| **World clock** | None (host time source) | Time in one or more chosen cities; may rotate | No network, time zones |
| **Now playing** | Host `media` source: current Windows media session via `GlobalSystemMediaTransportControlsSessionManager` (Spotify, browsers, most players). No sign-in, no network. | Title - artist (truncated), small artwork or play-state icon; hidden or dimmed when nothing plays. Click = play/pause (designer's call). | Built-in source, click action |
| **Device battery** | Host `battery` source: laptop battery + Bluetooth devices that report battery via Windows | Level per device (ring/bar + %), low-battery color | Built-in source permission |

Claude limit notes:
- The reference app refreshes the OAuth token itself and writes it back into Claude Code's credentials file (with lock files). **Builder's call made by the designer:** v1 is **read-only**. It never refreshes or writes the token. If the token is expired, show a stale state ("open Claude Code to refresh"). This keeps the `files` permission read-only and avoids corrupting Claude Code's login.
- The usage endpoint is undocumented and may change; the widget should fail gracefully.
- **No Claude logo in the template (user decision).** The catalog template uses a generic built-in icon (e.g. a gauge) or a text label, so we ship no Anthropic trademark. Users can still add their own logo locally: drop an SVG/PNG in the widget folder and point the icon's `src` at it. The template's README and the widget guide (Plan 3) show exactly how, with this widget as the example.

## 8. Backlog (after v1, in order)

1. **"Sign in with…" host feature (OAuth).** The app handles the sign-in flow and keeps the tokens (DPAPI). A widget declares the provider and scopes in `permissions` and receives only an authenticated `fetch` to that provider's API, never the token itself. Needs an app registration per provider. First uses:
   - **Outlook unread mail** (Microsoft Graph), as the first template. Covers Outlook.com and Microsoft 365. Classic-Outlook COM is not pursued, because the new Outlook doesn't support it.
   - Later: Gmail, Google Calendar, GitHub notifications, Spotify.
2. Vendor-specific battery protocols (Halo Battery-style 2.4 GHz receivers).

## 9. Left to the builder

- Settings window UI framework and visual design (must feel like Windows 11).
- Exact JS engine, exact `widget.js` function signature, and the exact return-value schema, within §5.2 to 5.4.
- Rendering library choice.
- Specific public APIs for crypto prices and weather.
- Catalog index format details and the CI validation implementation.
- How "Share" pre-fills the PR (URL-based flow vs. fallback).
- App name: **Perchbar** (user decision, 2026-11-12). Data folder `%APPDATA%\Perchbar\`.
- Testing strategy.
