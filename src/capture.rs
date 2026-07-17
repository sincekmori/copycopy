//! On a trigger: read the foreground window, wait for the clipboard to change
//! (= the fresh copy landed), read it, and hand a [`CaptureEvent`] to the handler.
//!
//! - Image is delivered as PNG-encoded bytes.
//! - Files are delivered as normalized filesystem paths (read them downstream).
//! - Audio/Video are not on the clipboard as media — only as file references.
//!
//! On macOS the clipboard/window reads must run on the process main thread;
//! [`capture_macos`] hops to the main thread via libdispatch while sleeping off it.

use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use clipboard_rs::common::RustImage;
use clipboard_rs::{Clipboard, ClipboardContext};

use crate::CaptureHandler;
use crate::config::Config;
use crate::event::{CaptureEvent, Captured, RichFormat};

/// Foreground application context captured alongside the clipboard. Defaults to
/// all-empty, which is used when the active window cannot be read. Fields are
/// crate-visible so alternative backends (GNOME Wayland) can build one.
#[derive(Default)]
pub(crate) struct Foreground {
    pub(crate) app_name: String,
    pub(crate) exec_name: String,
    pub(crate) exec_path: String,
    pub(crate) window_title: String,
    pub(crate) process_id: u32,
    pub(crate) url: Option<String>,
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ----------------------- clipboard change counter --------------------------

/// Monotonic counter the OS bumps on every clipboard write. On failure returns 0,
/// which only makes the wait fall back to its timeout (never incorrect).
#[cfg(windows)]
pub(crate) fn clipboard_change_count() -> u64 {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetClipboardSequenceNumber() -> u32;
    }
    unsafe { GetClipboardSequenceNumber() as u64 }
}

#[cfg(target_os = "macos")]
pub(crate) fn clipboard_change_count() -> u64 {
    // [[NSPasteboard generalPasteboard] changeCount]
    objc2_app_kit::NSPasteboard::generalPasteboard().changeCount() as u64
}

#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn clipboard_change_count() -> u64 {
    0
}

// ------------------------------- window ------------------------------------

/// Windows/Linux: read window + browser URL together (any thread). macOS splits
/// this (see [`capture_macos`] / [`snapshot_with_url`]) to keep the slow URL
/// lookup off the main thread.
#[cfg(not(target_os = "macos"))]
pub(crate) fn read_active_window() -> Foreground {
    match x_win::get_active_window() {
        Ok(info) => {
            let url = match x_win::get_browser_url(&info) {
                Ok(u) if !u.trim().is_empty() => Some(u),
                _ => None,
            };
            Foreground {
                app_name: info.info.name,
                exec_name: info.info.exec_name,
                exec_path: info.info.path,
                window_title: info.title,
                process_id: info.info.process_id,
                url,
            }
        }
        Err(e) => {
            eprintln!("[copycopy] get_active_window failed: {e:?}");
            Foreground::default()
        }
    }
}

/// Whether to skip capturing this foreground app: it's us, or it matches the
/// privacy denylist (substring of the executable name or full path).
pub(crate) fn should_skip(fg: &Foreground, denylist: &[String]) -> bool {
    if fg.process_id == std::process::id() {
        return true;
    }
    let name = fg.exec_name.to_ascii_lowercase();
    let path = fg.exec_path.to_ascii_lowercase();
    denylist.iter().any(|needle| {
        let needle = needle.to_ascii_lowercase();
        !needle.is_empty() && (name.contains(&needle) || path.contains(&needle))
    })
}

// ----------------------------- clipboard read ------------------------------

/// One clipboard read. Priority: files > image > rich text > plain text.
fn read_clipboard(max_files: usize) -> Captured {
    let ctx = match ClipboardContext::new() {
        Ok(c) => c,
        Err(_) => return Captured::Empty,
    };

    if let Ok(files) = ctx.get_files() {
        let paths: Vec<String> = files
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .map(|s| normalize_file_path(&s))
            .take(max_files)
            .collect();
        if !paths.is_empty() {
            return Captured::Files { paths };
        }
    }
    if let Ok(img) = ctx.get_image()
        && !img.is_empty()
    {
        let (width, height) = img.get_size();
        let png = img
            .to_png()
            .ok()
            .map(|b| b.get_bytes().to_vec())
            .unwrap_or_default();
        return Captured::Image { width, height, png };
    }
    if let Ok(html) = ctx.get_html()
        && !html.trim().is_empty()
        && html_is_meaningfully_rich(&html)
    {
        let plain = ctx.get_text().unwrap_or_default();
        return Captured::RichText {
            format: RichFormat::Html,
            markup: html,
            plain,
        };
    }
    if let Ok(rtf) = ctx.get_rich_text()
        && !rtf.trim().is_empty()
        && rtf_is_meaningfully_rich(&rtf)
    {
        let plain = ctx.get_text().unwrap_or_default();
        return Captured::RichText {
            format: RichFormat::Rtf,
            markup: rtf,
            plain,
        };
    }
    if let Ok(text) = ctx.get_text()
        && !text.is_empty()
    {
        return Captured::Text { text };
    }
    Captured::Empty
}

/// HTML is rich only with an actual formatting/structure tag (not just a styled
/// wrapper span browsers add to plain selections).
pub(crate) fn html_is_meaningfully_rich(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    const RICH: &[&str] = &[
        "<b>",
        "<b ",
        "<strong",
        "<i>",
        "<i ",
        "<em",
        "<u>",
        "<u ",
        "<s>",
        "<strike",
        "<del",
        "<mark",
        "<sub",
        "<sup",
        "<font",
        "<h1",
        "<h2",
        "<h3",
        "<h4",
        "<h5",
        "<h6",
        "<ul",
        "<ol",
        "<li",
        "<table",
        "<tr",
        "<td",
        "<th",
        "<blockquote",
        "<pre",
        "<code",
        "<a ",
        "<img",
        "<hr",
    ];
    RICH.iter().any(|tag| lower.contains(tag))
}

/// RTF is rich only with an explicit character-formatting toggle. Space-suffixed
/// checks avoid the "off" forms (e.g. `\ulnone`, `\b0`).
fn rtf_is_meaningfully_rich(rtf: &str) -> bool {
    const RICH: &[&str] = &[
        "\\b ",
        "\\i ",
        "\\ul ",
        "\\strike",
        "\\highlight",
        "\\pict",
        "\\sub ",
        "\\super ",
        "\\bullet",
    ];
    RICH.iter().any(|cw| rtf.contains(cw))
}

/// Strip a `file://` prefix (macOS and GNOME hand back URLs) and percent-decode.
pub(crate) fn normalize_file_path(raw: &str) -> String {
    let s = raw.strip_prefix("file://").unwrap_or(raw);
    percent_decode(s)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub(crate) fn build_event(fg: Foreground, content: Captured) -> CaptureEvent {
    CaptureEvent {
        timestamp_ms: now_millis(),
        app_name: fg.app_name,
        exec_name: fg.exec_name,
        exec_path: fg.exec_path,
        window_title: fg.window_title,
        url: fg.url,
        process_id: fg.process_id,
        content,
    }
}

/// Number of poll iterations that cover `clipboard_change_timeout`, stepping by
/// `clipboard_poll_step`. Always >= 1 (so the clipboard is read at least once)
/// and divide-by-zero-safe when the step is zero.
fn poll_step_count(config: &Config) -> u128 {
    (config.clipboard_change_timeout.as_millis() / config.clipboard_poll_step.as_millis().max(1))
        .max(1)
}

// ----------------------------- Windows / Linux -----------------------------

/// Runs on a worker thread. Reads the window, waits for the clipboard to change
/// (vs `baseline`), reads it, and hands the event to the handler.
#[cfg(not(target_os = "macos"))]
pub(crate) fn run_capture(config: &Config, handler: &CaptureHandler, baseline: u64) {
    let fg = read_active_window();
    if should_skip(&fg, &config.denylist_exec_substrings) {
        return;
    }
    let content = wait_for_change_then_read(config, baseline);
    handler(build_event(fg, content));
}

#[cfg(not(target_os = "macos"))]
fn wait_for_change_then_read(config: &Config, baseline: u64) -> Captured {
    let step = config.clipboard_poll_step;
    let steps = poll_step_count(config);
    for _ in 0..steps {
        thread::sleep(step);
        if clipboard_change_count() != baseline {
            return read_clipboard(config.max_files);
        }
    }
    read_clipboard(config.max_files)
}

// --------------------------------- macOS -----------------------------------

/// Run `f` on the process main thread and return its result, via libdispatch's
/// main queue. Must be called from a non-main thread while the host runs the main
/// run loop (which drains the main queue).
#[cfg(target_os = "macos")]
fn run_on_main<T, F>(f: F) -> T
where
    F: FnOnce() -> T + Send,
    T: Send,
{
    let mut result = None;
    dispatch2::DispatchQueue::main().exec_sync(|| result = Some(f()));
    result.expect("main-thread closure produced a result")
}

/// Runs on a worker thread. Window via main hop (URL lookup off main), then poll
/// the clipboard change counter (reads on main, sleeps off it), then hand the
/// event to the handler.
#[cfg(target_os = "macos")]
pub(crate) fn capture_macos(config: Config, handler: CaptureHandler, baseline: u64) {
    let info = run_on_main(|| x_win::get_active_window().ok());
    let fg = snapshot_with_url(info);
    if should_skip(&fg, &config.denylist_exec_substrings) {
        return;
    }

    let step = config.clipboard_poll_step;
    let steps = poll_step_count(&config);
    let max_files = config.max_files;
    let mut content: Option<Captured> = None;
    for _ in 0..steps {
        thread::sleep(step);
        let changed = run_on_main(move || {
            if clipboard_change_count() != baseline {
                Some(read_clipboard(max_files))
            } else {
                None
            }
        });
        if let Some(c) = changed {
            content = Some(c);
            break;
        }
    }
    let content = match content {
        Some(c) => c,
        None => run_on_main(move || read_clipboard(max_files)),
    };

    handler(build_event(fg, content));
}

/// Build the foreground snapshot OFF the main thread: `get_browser_url` spawns
/// osascript, slow but not main-thread-bound.
#[cfg(target_os = "macos")]
fn snapshot_with_url(info: Option<x_win::WindowInfo>) -> Foreground {
    match info {
        Some(i) => {
            let url = match x_win::get_browser_url(&i) {
                Ok(u) if !u.trim().is_empty() => Some(u),
                _ => None,
            };
            Foreground {
                app_name: i.info.name,
                exec_name: i.info.exec_name,
                exec_path: i.info.path,
                window_title: i.title,
                process_id: i.info.process_id,
                url,
            }
        }
        None => Foreground::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_basics() {
        assert_eq!(percent_decode("/Users/x/a%20b.png"), "/Users/x/a b.png");
        assert_eq!(percent_decode("%E3%81%82"), "あ");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn normalize_strips_file_url() {
        assert_eq!(
            normalize_file_path("file:///Users/x/a%20b.png"),
            "/Users/x/a b.png"
        );
        assert_eq!(
            normalize_file_path("C:\\Users\\x\\a.png"),
            "C:\\Users\\x\\a.png"
        );
    }

    #[test]
    fn html_richness() {
        assert!(html_is_meaningfully_rich("<p>hi <b>x</b></p>"));
        assert!(!html_is_meaningfully_rich(
            "<html><body><span style=\"color:#000\">plain</span></body></html>"
        ));
    }

    #[test]
    fn rtf_richness() {
        assert!(rtf_is_meaningfully_rich(r"{\rtf1 \b bold\b0 }"));
        assert!(!rtf_is_meaningfully_rich(
            r"{\rtf1\ansi \f0\fs24 \cf0 plain}"
        ));
    }

    #[test]
    fn poll_step_count_covers_timeout_and_clamps() {
        use std::time::Duration;
        let cfg = |timeout_ms, step_ms| Config {
            clipboard_change_timeout: Duration::from_millis(timeout_ms),
            clipboard_poll_step: Duration::from_millis(step_ms),
            ..Default::default()
        };
        assert_eq!(poll_step_count(&cfg(400, 20)), 20); // covers the window
        assert_eq!(poll_step_count(&cfg(10, 20)), 1); // timeout < step => read once
        assert_eq!(poll_step_count(&cfg(0, 20)), 1); // zero timeout => read once
        assert_eq!(poll_step_count(&cfg(400, 0)), 400); // zero step => no divide-by-zero
    }

    #[test]
    fn skip_self_process() {
        let fg = Foreground {
            process_id: std::process::id(),
            ..Default::default()
        };
        assert!(should_skip(&fg, &[]));
    }

    #[test]
    fn denylist_matches_exec_substring_case_insensitively() {
        let fg = Foreground {
            process_id: 1, // not us
            exec_name: "1Password.exe".to_string(),
            ..Default::default()
        };
        assert!(should_skip(&fg, &["1password".to_string()]));
        assert!(!should_skip(&fg, &["keepass".to_string()]));
        assert!(!should_skip(&fg, &[]));
    }
}
