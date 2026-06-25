# copycopy

[![Crates.io](https://img.shields.io/crates/v/copycopy.svg)](https://crates.io/crates/copycopy)
[![Docs.rs](https://docs.rs/copycopy/badge.svg)](https://docs.rs/copycopy)
[![CI](https://github.com/sincekmori/copycopy/actions/workflows/ci.yml/badge.svg)](https://github.com/sincekmori/copycopy/actions/workflows/ci.yml)
[![License](https://img.shields.io/crates/l/copycopy.svg)](https://crates.io/crates/copycopy)

A small cross-platform (Windows + macOS) Rust library that turns a global **Ctrl/Cmd + C + C** gesture into a structured capture and hands it to your code.

Hold the platform modifier (Windows = **Ctrl**, macOS = **Cmd**) and press `C` **twice quickly**.
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
- **Reliable clipboard reads** — gated on the OS clipboard change counter, so we read the fresh copy rather than a stale one.
- **Multi-format reads** — text, image (PNG bytes), rich text (HTML/RTF, only when it carries real formatting), and files (normalized paths).

## Quickstart

```toml
[dependencies]
copycopy = "0.1"
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

## Platform support

| OS | Status |
|----|--------|
| Windows | ✅ supported |
| macOS | ✅ supported |
| Linux | ❌ not supported — rdev is X11-only, and Wayland blocks global key capture by design (a future evdev-based path would be a separate implementation) |

## Limitations

- Images embedded inside rich text are not captured, because no standalone bitmap is on the clipboard — copy the image by itself or as a file instead.
- Audio and video arrive as file references (the `Files` variant), not as raw media.
- The browser URL is chromium-only, and recent Chrome may need an accessibility helper.
- The listener runs for the process lifetime, since rdev has no stop API.
- Global key hooks are commonly flagged by EDR/AV as keyloggers, so code-sign for distribution; note the clipboard may hold secrets, which `denylist_exec_substrings` can exclude.
