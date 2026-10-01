//! On a trigger: read the foreground window, wait for the clipboard to change
//! (= the fresh copy landed), read it, and hand a [`CaptureEvent`] to the handler.
//!
//! - Image is delivered as PNG-encoded bytes.
//! - Files are delivered as normalized filesystem paths (read them downstream).
//! - Audio/Video are not on the clipboard as media — only as file references.
//! - A copy its source marked as a secret is not delivered at all.
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

/// The browser's address for a window, where x-win can read one: Windows
/// (UI Automation) and macOS (AppleScript). On Linux it cannot, and says so
/// in its `Ok` value — a sentence, which is not a URL — so there the answer
/// is none without asking.
fn browser_url(info: &x_win::WindowInfo) -> Option<String> {
    if cfg!(target_os = "linux") {
        return None;
    }
    match x_win::get_browser_url(info) {
        Ok(url) if !url.trim().is_empty() => Some(url),
        _ => None,
    }
}

/// Windows/Linux: read window + browser URL together (any thread). macOS splits
/// this (see [`capture_macos`] / [`snapshot_with_url`]) to keep the slow URL
/// lookup off the main thread.
#[cfg(not(target_os = "macos"))]
pub(crate) fn read_active_window() -> Foreground {
    match x_win::get_active_window() {
        Ok(info) => {
            let url = browser_url(&info);
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

/// One clipboard read: `None` when the source app marked the content as a
/// secret (see [`marks_concealed`]), and nothing is delivered then.
///
/// Priority: files > text, when text is what was copied (see
/// [`text_outranks_image`]) > image > rich text > plain text.
fn read_clipboard(max_files: usize) -> Option<Captured> {
    let Ok(ctx) = ClipboardContext::new() else {
        return Some(Captured::Empty);
    };
    if is_concealed(&ctx) {
        return None;
    }

    if let Ok(files) = ctx.get_files() {
        let paths: Vec<String> = files
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .map(|s| normalize_file_path(&s))
            .take(max_files)
            .collect();
        if !paths.is_empty() {
            return Some(Captured::Files { paths });
        }
    }
    let plain = ctx.get_text().unwrap_or_default();
    let html = ctx.get_html().ok().filter(|html| !html.trim().is_empty());
    if !text_outranks_image(&plain, html.as_deref())
        && let Ok(img) = ctx.get_image()
        && !img.is_empty()
    {
        let (width, height) = img.get_size();
        let png = img
            .to_png()
            .ok()
            .map(|b| b.get_bytes().to_vec())
            .unwrap_or_default();
        return Some(Captured::Image { width, height, png });
    }
    if let Some(html) = html
        && html_is_meaningfully_rich(&html)
    {
        return Some(Captured::RichText {
            format: RichFormat::Html,
            markup: html,
            plain,
        });
    }
    if let Ok(rtf) = ctx.get_rich_text()
        && !rtf.trim().is_empty()
        && rtf_is_meaningfully_rich(&rtf)
    {
        return Some(Captured::RichText {
            format: RichFormat::Rtf,
            markup: rtf,
            plain,
        });
    }
    if !plain.is_empty() {
        return Some(Captured::Text { text: plain });
    }
    Some(Captured::Empty)
}

/// Whether the clipboard carries a marker that its content is a secret.
fn is_concealed(ctx: &ClipboardContext) -> bool {
    ctx.available_formats().is_ok_and(|formats| {
        formats
            .iter()
            .any(|format| marks_concealed(format, || ctx.get_buffer(format).ok()))
    })
}

/// Whether a clipboard format is an app's way of saying "this is a secret":
/// the markers password managers put next to what they copy, so that clipboard
/// tools leave it alone. `value` reads the format's data, for the markers
/// whose value carries the meaning; a marker whose value cannot be read counts
/// as set, since the app bothered to put it there.
///
/// - macOS: `org.nspasteboard.ConcealedType` (<http://nspasteboard.org>) and
///   1Password's older `com.agilebits.onepassword`.
/// - Windows: `ExcludeClipboardContentFromMonitorProcessing`, and
///   `CanIncludeInClipboardHistory` / `CanUploadToCloudClipboard` set to 0.
/// - Linux: `x-kde-passwordManagerHint` set to `secret`.
pub(crate) fn marks_concealed(format: &str, value: impl FnOnce() -> Option<Vec<u8>>) -> bool {
    match format {
        "org.nspasteboard.ConcealedType"
        | "com.agilebits.onepassword"
        | "ExcludeClipboardContentFromMonitorProcessing" => true,
        // A DWORD: 0 keeps the content out of the history / off other devices.
        "CanIncludeInClipboardHistory" | "CanUploadToCloudClipboard" => {
            value().is_none_or(|bytes| bytes.iter().take(4).all(|byte| *byte == 0))
        }
        "x-kde-passwordManagerHint" => value().is_none_or(|bytes| bytes.trim_ascii() == b"secret"),
        _ => false,
    }
}

/// Whether the text on the clipboard, rather than the image beside it, is what
/// the user copied. Spreadsheet, slide and note apps put a rendered bitmap
/// next to the text of a selection (cells copied in Excel arrive as their
/// text, an HTML table, and a picture of the cells), and there the text is the
/// content. A browser's "Copy image" does the opposite: the image is the
/// content, and what comes with it is its address or an `<img>` tag.
///
/// The markup tells the two apart when there is one — it describes the same
/// copy, so text showing in it means text was copied. Without markup, text
/// counts unless it is a lone address or path, which names the image.
pub(crate) fn text_outranks_image(plain: &str, html: Option<&str>) -> bool {
    let plain = plain.trim();
    if plain.is_empty() {
        return false;
    }
    match html {
        Some(html) => html_has_visible_text(html),
        None => !is_lone_locator(plain),
    }
}

/// Elements whose content a page does not show.
const UNSHOWN_ELEMENTS: [&str; 4] = ["head", "script", "style", "title"];

/// Whether HTML shows any text once its tags are gone: an `<img>` alone, or
/// markup wrapping nothing, shows none, and neither do comments or what sits
/// in the [`UNSHOWN_ELEMENTS`] — office apps hand over a whole document, its
/// stylesheet ahead of the body.
fn html_has_visible_text(html: &str) -> bool {
    // Lowercasing ASCII alone keeps every byte offset, and what shows.
    let html = html.to_ascii_lowercase();
    let mut rest = html.as_str();
    while let Some(open) = rest.find('<') {
        if shows_text(&rest[..open]) {
            return true;
        }
        rest = &rest[open..];
        if let Some(comment) = rest.strip_prefix("<!--") {
            rest = comment.split_once("-->").map_or("", |(_, after)| after);
            continue;
        }
        let Some(end) = tag_end(rest) else {
            return false;
        };
        let name = rest[1..end]
            .split(|c: char| !c.is_ascii_alphanumeric())
            .next()
            .unwrap_or_default();
        rest = &rest[end + 1..];
        if UNSHOWN_ELEMENTS.contains(&name) {
            // Up to the element's end tag; left open, it takes the rest.
            rest = rest
                .find(&format!("</{name}"))
                .map_or("", |close| &rest[close..]);
        }
    }
    shows_text(rest)
}

/// The index of the `>` that closes the tag `tag` starts with. A `>` inside a
/// quoted attribute value does not close it — unless the quote never ends,
/// where the first `>` has to do.
fn tag_end(tag: &str) -> Option<usize> {
    let mut quote = None;
    for (at, c) in tag.char_indices() {
        match (quote, c) {
            (Some(open), _) if c == open => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '>') => return Some(at),
            (None, _) => {}
        }
    }
    tag.find('>')
}

/// Whether the text between tags shows anything but space.
fn shows_text(text: &str) -> bool {
    ["&nbsp;", "&#160;", "&#xa0;"]
        .iter()
        .fold(text.to_string(), |text, space| text.replace(space, " "))
        .chars()
        .any(|c| !c.is_whitespace())
}

/// Whether text is one address or one absolute path, and nothing else.
fn is_lone_locator(text: &str) -> bool {
    let one_token = !text.contains(char::is_whitespace);
    let one_line = !text.contains(['\n', '\r']);
    let address = text.contains("://") || text.starts_with("data:");
    let path = text.starts_with('/')
        || text.starts_with("\\\\")
        || text.as_bytes().get(1..3) == Some(b":\\".as_slice());
    (one_token && address) || (one_line && path)
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

/// A clipboard file entry as a filesystem path. A `file://` URL (X11's
/// `text/uri-list`, and the GNOME backend) is stripped and percent-decoded;
/// anything else is already a path — Windows and macOS hand those back — and
/// is kept as it is: a `%20` in a real file name is part of the name.
pub(crate) fn normalize_file_path(raw: &str) -> String {
    match raw.strip_prefix("file://") {
        Some(path) => percent_decode(path),
        None => raw.to_string(),
    }
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
    // A secret is not delivered at all, like a denylisted app.
    if let Some(content) = wait_for_change_then_read(config, baseline) {
        handler(build_event(fg, content));
    }
}

#[cfg(not(target_os = "macos"))]
fn wait_for_change_then_read(config: &Config, baseline: u64) -> Option<Captured> {
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
    let mut read: Option<Option<Captured>> = None;
    for _ in 0..steps {
        thread::sleep(step);
        read = run_on_main(move || {
            (clipboard_change_count() != baseline).then(|| read_clipboard(max_files))
        });
        if read.is_some() {
            break;
        }
    }
    let content = read.unwrap_or_else(|| run_on_main(move || read_clipboard(max_files)));

    // A secret is not delivered at all, like a denylisted app.
    if let Some(content) = content {
        handler(build_event(fg, content));
    }
}

/// Build the foreground snapshot OFF the main thread: `get_browser_url` spawns
/// osascript, slow but not main-thread-bound.
#[cfg(target_os = "macos")]
fn snapshot_with_url(info: Option<x_win::WindowInfo>) -> Foreground {
    match info {
        Some(i) => {
            let url = browser_url(&i);
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
    fn normalize_keeps_a_percent_in_a_real_path() {
        // Not URLs: the `%20` is in the file's name on disk.
        assert_eq!(
            normalize_file_path("C:\\Users\\x\\Report%20Q3.pdf"),
            "C:\\Users\\x\\Report%20Q3.pdf"
        );
        assert_eq!(
            normalize_file_path("/Users/x/Report%20Q3.pdf"),
            "/Users/x/Report%20Q3.pdf"
        );
    }

    #[test]
    fn concealed_markers() {
        let unread = || None;
        assert!(marks_concealed("org.nspasteboard.ConcealedType", unread));
        assert!(marks_concealed("com.agilebits.onepassword", unread));
        assert!(marks_concealed(
            "ExcludeClipboardContentFromMonitorProcessing",
            unread
        ));
        assert!(!marks_concealed("public.utf8-plain-text", unread));
        assert!(!marks_concealed("org.nspasteboard.TransientType", unread));

        // Windows: a DWORD, where 0 means "keep it out".
        let dword = |n: u32| move || Some(n.to_le_bytes().to_vec());
        assert!(marks_concealed("CanIncludeInClipboardHistory", dword(0)));
        assert!(!marks_concealed("CanIncludeInClipboardHistory", dword(1)));
        assert!(marks_concealed("CanUploadToCloudClipboard", dword(0)));
        assert!(!marks_concealed("CanUploadToCloudClipboard", dword(1)));
        assert!(marks_concealed("CanIncludeInClipboardHistory", unread));

        // KDE: the value says what kind of hint it is.
        let hint = |s: &'static str| move || Some(s.as_bytes().to_vec());
        assert!(marks_concealed("x-kde-passwordManagerHint", hint("secret")));
        assert!(marks_concealed(
            "x-kde-passwordManagerHint",
            hint("secret\n")
        ));
        assert!(!marks_concealed(
            "x-kde-passwordManagerHint",
            hint("public")
        ));
        assert!(marks_concealed("x-kde-passwordManagerHint", unread));
    }

    #[test]
    fn text_next_to_an_image() {
        // Cells copied in a spreadsheet: text, a table, and a picture of them.
        let table = "<table><tr><td>Q3</td><td>1,200</td></tr></table>";
        assert!(text_outranks_image("Q3\t1,200", Some(table)));
        // A selection in a slide or note app: text and a bitmap, no markup.
        assert!(text_outranks_image("Launch plan for Q3", None));

        // A browser's "Copy image": the tag, with or without the address.
        let img = r#"<meta charset="utf-8"><img src="https://example.com/a.png" alt="a > b">"#;
        assert!(!text_outranks_image("https://example.com/a.png", Some(img)));
        assert!(!text_outranks_image("a cat", Some(img)));
        // An image with only its address or path as text.
        assert!(!text_outranks_image("https://example.com/a.png", None));
        assert!(!text_outranks_image("/Users/x/Screen Shot.png", None));
        assert!(!text_outranks_image("C:\\Users\\x\\shot.png", None));
        // No text at all.
        assert!(!text_outranks_image("  \n", Some(table)));
        assert!(!text_outranks_image("", None));
    }

    #[test]
    fn html_visible_text() {
        assert!(html_has_visible_text("<p>hi</p>"));
        assert!(html_has_visible_text("<!--StartFragment--><b>x</b>"));
        assert!(!html_has_visible_text("<img src='a.png'>"));
        assert!(!html_has_visible_text("<span>&nbsp; &#160;</span>\n<br>"));
        assert!(!html_has_visible_text(r#"<img alt="a > b" src="x">"#));
        assert!(html_has_visible_text("<a title='its>x</a>"));

        // An office app's document: the stylesheet and the title are not text.
        let document = |body: &str| {
            format!(
                "<html><HEAD><title>Sheet1</title><style><!-- td {{color:red}} --></style>\
                 </HEAD><body><!--[if gte mso 9]><xml>x</xml><![endif]-->{body}\
                 <script>let a = 1 > 0;</script></body></html>"
            )
        };
        assert!(html_has_visible_text(&document(
            "<table><td>Q3</td></table>"
        )));
        assert!(!html_has_visible_text(&document(
            "<![if !vml]><img src='a.png'><![endif]>"
        )));
        // Left open, an element or a comment takes the rest with it.
        assert!(!html_has_visible_text("<style>p {color:red}"));
        assert!(!html_has_visible_text("<!-- note"));
        assert!(html_has_visible_text("<header>News</header>"));
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
