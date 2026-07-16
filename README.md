# copycopy

[![Crates.io](https://img.shields.io/crates/v/copycopy.svg)](https://crates.io/crates/copycopy)
[![Docs.rs](https://docs.rs/copycopy/badge.svg)](https://docs.rs/copycopy)
[![CI](https://github.com/sincekmori/copycopy/actions/workflows/ci.yml/badge.svg)](https://github.com/sincekmori/copycopy/actions/workflows/ci.yml)
[![License](https://img.shields.io/crates/l/copycopy.svg)](https://crates.io/crates/copycopy)

A small cross-platform (Windows + macOS + Linux GNOME Wayland) Rust library that turns a global **Ctrl/Cmd + C + C** gesture into a structured capture and hands it to your code.

Hold the platform modifier (Windows/Linux = **Ctrl**, macOS = **Cmd**) and press `C` **twice quickly**.
A normal single copy is never consumed.
On each trigger your handler receives a `CaptureEvent` — the clipboard content plus the foreground app (name, window title, browser URL, PID) — on a worker thread.

This crate is the **foundation** you build on.
It deliberately does not decide what happens next; you plug in the processing.
For example:

- **Translate on copy** — store an API key, translate the captured text, show it.
- **LLM on copy** — feed the content to a model, or pop up an action picker.
- **Snippet capture, quick-share, clip-and-send**, and so on.

Every one of those shares the same base — *Ctrl/Cmd+C+C → captured content → do something* — which is exactly what this crate provides.

## Why a library

The hard parts are the same for every such app, and they are fully encapsulated here.

- **Passive listening** — listen-only key hooks, so a plain single copy still works (no global hotkey registration that would swallow the keystroke).
- **An OS-independent double-tap state machine** — auto-repeat aware, and unit-tested.
- **The macOS minefield** — rdev crashes off the main thread, so we run our own `CGEventTap` on the main run loop and decode key codes directly, hop main-thread-only clipboard and window reads to the main thread via libdispatch, and keep the slow browser-URL lookup off it.
- **The GNOME Wayland wall** — Wayland lets no background process observe keys or the clipboard, so on GNOME the crate ships (and auto-installs, no sudo) a tiny GNOME Shell extension that detects the gesture inside the compositor and hands captures over via unicast D-Bus. See [Linux (GNOME Wayland)](#linux-gnome-wayland).
- **Reliable clipboard reads** — gated on the OS clipboard change counter, so we read the fresh copy rather than a stale one.
- **Multi-format reads** — text, image (PNG bytes), rich text (HTML/RTF, only when it carries real formatting), and files (normalized paths).

## Quickstart

```toml
[dependencies]
copycopy = "0.3"
```

```rust,no_run
use copycopy::{block_forever, start, Captured, Config};

fn main() {
    let _capture = start(Config::default(), |event| {
        match event.content {
            Captured::Text { text } => println!("text from {}: {text}", event.app_name),
            Captured::Image { width, height, .. } => println!("image {width}x{height}"),
            Captured::Files { paths } => println!("files: {paths:?}"),
            other => println!("{other:?}"),
        }
    })
    .expect("failed to start capture");

    block_forever(); // standalone: keep the process (and the listener) alive
}
```

`cargo run --example print_events` prints every capture.

## The captured event

```rust,ignore
pub struct CaptureEvent {
    pub timestamp_ms: u64,
    pub app_name: String,
    pub exec_name: String,
    pub exec_path: String,
    pub window_title: String,
    pub url: Option<String>,   // chromium browsers only
    pub process_id: u32,
    pub content: Captured,
}

pub enum Captured {
    Text { text: String },
    Image { width: u32, height: u32, png: Vec<u8> },     // PNG-encoded
    RichText { format: RichFormat, markup: String, plain: String },
    Files { paths: Vec<String> },                         // normalized paths
    Empty,
}
```

`CaptureEvent` and `Captured` derive `serde::Serialize`/`Deserialize` with an internal `"kind"` tag, so you can forward an event to a webview or serialize it for IPC in one line.

## Where the processing goes

The crate is agnostic — it only delivers the event.
Two common patterns:

- **In a GUI host (e.g. Tauri)** — call `start` from your main-thread startup hook and forward the event to the webview, then build the settings (API keys) and the result/action UI in the frontend:

  ```rust,ignore
  // Tauri's setup() runs on the main thread, satisfying the macOS requirement.
  let app = app_handle.clone();
  copycopy::start(Config::default(), move |event| {
      let _ = app.emit("capture", event);
  })?;
  // Do not call block_forever(); Tauri runs its own event loop.
  ```

- **In Rust** — run your processor directly in the handler and keep API keys native (e.g. an OS keychain); the handler runs on a worker thread, so blocking calls are fine.

## Trigger status

`start` has a sibling, `start_with_status`, that also reports how the trigger settled — the states a GUI host should surface to the user, because they are otherwise silent (the accompanying diagnostics go to stderr, which is invisible in a bundled app):

```rust,no_run
use copycopy::{start_with_status, Config, TriggerStatus};

let _capture = start_with_status(
    Config::default(),
    |event| println!("{event:?}"),
    |status| match status {
        // GNOME Wayland: installed, active after one logout/login.
        TriggerStatus::GnomeExtensionAwaitingLogin => { /* show "log out and back in once" */ }
        // Non-GNOME Wayland: no capture path exists.
        TriggerStatus::UnsupportedSession => { /* show "this desktop is not supported" */ }
        _ => {}
    },
)
.expect("failed to start capture");
```

Treat the latest report as current.
`TriggerStatus` serializes with a `"kind"` tag (like `CaptureEvent`), so it forwards to a webview or over IPC in one line.

## macOS permissions

`start` must be called on the thread that runs the app's main run loop.
A bare binary calls `block_forever` right after; a GUI host calls `start` from its main-thread startup hook.
Grant the following in System Settings → Privacy & Security:

| Permission | Needed for |
|------------|------------|
| **Input Monitoring** | detecting the keys at all |
| **Screen Recording** | reading the foreground window title |
| **Automation** | reading the browser URL |

During development the grant attaches to the launching process (your terminal), not the binary.
Missing permissions fail silently — no events, or an empty title/URL.

## Configuration

`Config` exposes public fields and a `Default`, so override only what you need with struct-update syntax.

```rust,no_run
use std::time::Duration;
use copycopy::Config;

let config = Config {
    trigger_cooldown: Duration::from_millis(500),
    denylist_exec_substrings: vec!["1password".into(), "keepass".into()],
    ..Default::default()
};
```

| Field | Default | Meaning |
|-------|---------|---------|
| `double_tap_window` | 400 ms | max gap between the two `C` presses |
| `clipboard_change_timeout` | 400 ms | how long to wait for a fresh copy |
| `clipboard_poll_step` | 20 ms | clipboard change-counter poll interval |
| `trigger_cooldown` | 350 ms | ignore further triggers for this long |
| `denylist_exec_substrings` | `[]` | skip capture for these apps (e.g. password managers) |
| `max_files` | 50 | cap on file paths captured |

On the GNOME Wayland backend the timing fields are fixed inside the Shell extension (same defaults); only `denylist_exec_substrings` (matched against the app name / `wm_class`) and `max_files` apply.

## Platform support

| OS | Status |
|----|--------|
| Windows | ✅ supported |
| macOS | ✅ supported |
| Linux — GNOME on Wayland | ✅ supported via an auto-installed GNOME Shell extension (see below) |
| Linux — X11 | ⚠️ best-effort — the rdev key-listener path compiles and runs, but is not regularly tested |
| Linux — other Wayland (KDE, wlroots, ...) | ❌ not supported yet — no unprivileged capture path; a future `data-control`-based backend may cover it |

## Linux (GNOME Wayland)

GNOME's Wayland compositor deliberately prevents background processes from observing keystrokes or reading the clipboard, so a userspace listener like rdev cannot work.
Instead, the crate embeds a small GNOME Shell extension and installs it automatically on first `start` — into `~/.local/share/gnome-shell/extensions/` (no sudo, nothing system-wide).

How it works:

- The extension runs inside the compositor (the same vantage point clipboard managers like GPaste use). It watches clipboard **owner changes**, so the trigger is **two explicit copies within 400 ms** — which is exactly what Ctrl+C+C produces. A single copy never fires anything.
- After the second copy it waits for the clipboard to settle, reads the content with the same priority as the other platforms (files > image > rich text > plain text), attaches the focused window's app name / `wm_class` / title / PID, and notifies the host app.
- **Privacy**: clipboard contents are never broadcast on the D-Bus session bus. The extension only broadcasts a serial number; the content itself is fetched with a unicast method call, is handed out once, and expires after a few seconds.

Things to know:

- **First run requires one logout/login.** GNOME Shell only loads newly installed extensions at login (Wayland has no shell restart). The listener prints a clear message on stderr when the extension is installed but not yet loaded, and reports it as `TriggerStatus::GnomeExtensionAwaitingLogin` via `start_with_status` (see [Trigger status](#trigger-status)) so a GUI host can tell the user. Subsequent runs need nothing.
- **Supported GNOME versions: 45–50 declared, verified on GNOME 46.** GNOME's `shell-version` metadata has no range syntax — each major must be listed explicitly, or the extension is disabled at login on that version. GNOME releases a new major every March and September; each release gets the new major appended and a patch release of this crate, and the auto-installer upgrades existing installs by version comparison, so staying current only takes a `cargo update`. (GNOME 44 and older are out: they use a different, pre-ESM extension entry point.)
- **Screenshots don't trigger.** GNOME's PrtSc writes the clipboard once, and one clipboard write is a single copy by definition. The macOS-style "screenshot, then Ctrl+C+C to grab it" flow is out of scope on this backend — copy an image from an app (two Ctrl+C presses) instead.
- `CaptureEvent.exec_path` is empty and `url` is `None` on this backend; `exec_name` carries the window's `wm_class`.

## Limitations

- Images embedded inside rich text are not captured, because no standalone bitmap is on the clipboard — copy the image by itself or as a file instead.
- Audio and video arrive as file references (the `Files` variant), not as raw media.
- The browser URL is chromium-only, and recent Chrome may need an accessibility helper.
- The listener runs for the process lifetime, since rdev has no stop API.
- Global key hooks are commonly flagged by EDR/AV as keyloggers, so code-sign for distribution; note the clipboard may hold secrets, which `denylist_exec_substrings` can exclude.
