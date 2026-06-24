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

use crate::config::Config;
use crate::event::{CaptureEvent, Captured, RichFormat};
use crate::CaptureHandler;

/// Foreground application context captured alongside the clipboard. Defaults to
/// all-empty, which is used when the active window cannot be read.
#[derive(Default)]
pub(crate) struct Foreground {
    app_name: String,
    exec_name: String,
    exec_path: String,
    window_title: String,
    process_id: u32,
    url: Option<String>,
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
    extern "system" {
        fn GetClipboardSequenceNumber() -> u32;
    }
    unsafe { GetClipboardSequenceNumber() as u64 }
}

#[cfg(target_os = "macos")]
pub(crate) fn clipboard_change_count() -> u64 {
    use std::os::raw::{c_char, c_void};
    #[link(name = "AppKit", kind = "framework")]
    extern "C" {}
    #[link(name = "objc", kind = "dylib")]
    extern "C" {
        fn objc_getClass(name: *const c_char) -> *mut c_void;
        fn sel_registerName(name: *const c_char) -> *mut c_void;
        fn objc_msgSend();
    }
    // [[NSPasteboard generalPasteboard] changeCount]
    unsafe {
        let cls = objc_getClass(c"NSPasteboard".as_ptr());
        if cls.is_null() {
            return 0;
        }
        let send_obj: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let pb = send_obj(cls, sel_registerName(c"generalPasteboard".as_ptr()));
        if pb.is_null() {
            return 0;
        }
        let send_int: unsafe extern "C" fn(*mut c_void, *mut c_void) -> isize =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        send_int(pb, sel_registerName(c"changeCount".as_ptr())) as u64
    }
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
fn should_skip(fg: &Foreground, denylist: &[String]) -> bool {
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
    if let Ok(img) = ctx.get_image() {
        if !img.is_empty() {
            let (width, height) = img.get_size();
            let png = img
                .to_png()
                .ok()
                .map(|b| b.get_bytes().to_vec())
                .unwrap_or_default();
            return Captured::Image { width, height, png };
        }
    }
    if let Ok(html) = ctx.get_html() {
        if !html.trim().is_empty() && html_is_meaningfully_rich(&html) {
            let plain = ctx.get_text().unwrap_or_default();
            return Captured::RichText {
                format: RichFormat::Html,
                markup: html,
                plain,
            };
        }
    }
    if let Ok(rtf) = ctx.get_rich_text() {
        if !rtf.trim().is_empty() && rtf_is_meaningfully_rich(&rtf) {
            let plain = ctx.get_text().unwrap_or_default();
            return Captured::RichText {
                format: RichFormat::Rtf,
                markup: rtf,
                plain,
            };
        }
    }
    if let Ok(text) = ctx.get_text() {
        if !text.is_empty() {
            return Captured::Text { text };
        }
    }
    Captured::Empty
}

/// HTML is rich only with an actual formatting/structure tag (not just a styled
/// wrapper span browsers add to plain selections).
fn html_is_meaningfully_rich(html: &str) -> bool {
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

/// Strip a `file://` prefix (macOS may hand back URLs) and percent-decode.
fn normalize_file_path(raw: &str) -> String {
    let s = raw.strip_prefix("file://").unwrap_or(raw);
    percent_decode(s)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
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

fn build_event(fg: Foreground, content: Captured) -> CaptureEvent {
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
    let steps = (config.clipboard_change_timeout.as_millis() / step.as_millis().max(1)).max(1);
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
/// run loop (which drains the main queue). No framework dependency.
#[cfg(target_os = "macos")]
fn run_on_main<T, F>(f: F) -> T
where
    F: FnOnce() -> T + Send,
    T: Send,
{
    use std::os::raw::c_void;

    #[repr(C)]
    struct OpaqueQueue {
        _private: [u8; 0],
    }
    extern "C" {
        static _dispatch_main_q: OpaqueQueue;
        fn dispatch_sync_f(
            queue: *const OpaqueQueue,
            context: *mut c_void,
            work: extern "C" fn(*mut c_void),
        );
    }

    struct Ctx<T, F> {
        f: Option<F>,
        result: Option<T>,
    }
    extern "C" fn trampoline<T, F: FnOnce() -> T>(ctx: *mut c_void) {
        // Safety: `ctx` points to the `Ctx` on the caller's stack, kept alive for
        // the whole synchronous `dispatch_sync_f` call.
        let ctx = unsafe { &mut *(ctx as *mut Ctx<T, F>) };
        let f = ctx.f.take().expect("trampoline runs exactly once");
        ctx.result = Some(f());
    }

    let mut ctx: Ctx<T, F> = Ctx {
        f: Some(f),
        result: None,
    };
    unsafe {
        dispatch_sync_f(
            &_dispatch_main_q,
            &mut ctx as *mut _ as *mut c_void,
            trampoline::<T, F>,
        );
    }
    ctx.result
        .take()
        .expect("main-thread closure produced a result")
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
    let steps = (config.clipboard_change_timeout.as_millis() / step.as_millis().max(1)).max(1);
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
