//! On a trigger: read the foreground window, wait for the clipboard to change
//! (= the fresh copy landed), read it, and hand a [`CaptureEvent`] to the handler.
//!
//! - Image is delivered as PNG-encoded bytes: the clipboard's own PNG as it
//!   is where it holds one, else what it holds instead, encoded as one.
//! - Files are delivered as normalized filesystem paths (read them downstream).
//! - Audio/Video are not on the clipboard as media — only as file references.
//! - A copy its source marked as a secret is not delivered at all.
//! - A clipboard with nothing in it yet, or one that offers what it will not
//!   hand over yet — its owner has emptied it and is about to fill it again,
//!   another program has it open, a promised format is still to be rendered —
//!   is read again until it has something and hands it over, for two seconds
//!   at most (Windows and Linux).
//!
//! On macOS the clipboard/window reads must run on the process main thread;
//! [`run_capture`] hops to the main thread via libdispatch while sleeping off it.

use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use clipboard_rs::common::RustImage;
use clipboard_rs::{Clipboard, ClipboardContext, ContentFormat};

use crate::CaptureHandler;
use crate::config::Config;
use crate::event::{CaptureEvent, Captured, RichFormat};
use crate::gate::CaptureGate;

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

impl Foreground {
    /// The snapshot of a window x-win read, with its browser URL where one
    /// can be had.
    fn of(info: x_win::WindowInfo) -> Self {
        let url = browser_url(&info);
        Self {
            app_name: info.info.name,
            exec_name: info.info.exec_name,
            exec_path: info.info.path,
            window_title: info.title,
            process_id: info.info.process_id,
            url,
        }
    }
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
fn clipboard_change_count() -> u64 {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetClipboardSequenceNumber() -> u32;
    }
    unsafe { GetClipboardSequenceNumber() as u64 }
}

#[cfg(target_os = "macos")]
fn clipboard_change_count() -> u64 {
    // [[NSPasteboard generalPasteboard] changeCount]
    objc2_app_kit::NSPasteboard::generalPasteboard().changeCount() as u64
}

#[cfg(not(any(windows, target_os = "macos")))]
fn clipboard_change_count() -> u64 {
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
/// this (see its [`run_capture`]) to keep the slow URL lookup off the main
/// thread.
#[cfg(not(target_os = "macos"))]
fn read_active_window() -> Foreground {
    match x_win::get_active_window() {
        Ok(info) => Foreground::of(info),
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

/// What one read of the clipboard came back with.
#[derive(Debug)]
enum Read {
    /// The source app marked the content as a secret (see
    /// [`marks_concealed`]): nothing is delivered.
    Secret,
    /// The clipboard handed over all it offered.
    Whole(Captured),
    /// There may be more to come: the clipboard held nothing at all (an
    /// owner empties it before it fills it again), another program had it
    /// open, or it would not hand over a format it offered. The content is
    /// what could be read — another read may find the whole.
    Partial(Captured),
}

impl Read {
    /// What is delivered when this read is the last one: nothing for a
    /// secret, else its content.
    fn delivered(self) -> Option<Captured> {
        match self {
            Self::Secret => None,
            Self::Whole(content) | Self::Partial(content) => Some(content),
        }
    }
}

/// How long a capture goes back to a clipboard that has nothing yet, or that
/// offers what it will not hand over yet (see [`Read::Partial`]). A
/// spreadsheet's cells are the case, measured in Excel on Windows: the second
/// Ctrl+C first empties the clipboard — which is the change a capture wakes
/// up on — and puts its thirty formats back some fifty milliseconds later;
/// a little after that the system's clipboard history and other listeners
/// open it to fetch theirs. A single read at the change finds nothing at
/// all.
#[cfg(not(target_os = "macos"))]
const READ_PATIENCE: std::time::Duration = std::time::Duration::from_secs(2);

/// Read until the clipboard hands over all it has; once `patience` is spent,
/// what the last read found has to do. `None` for a secret.
#[cfg(any(not(target_os = "macos"), test))]
fn read_whole(
    mut read: impl FnMut() -> Read,
    patience: std::time::Duration,
    step: std::time::Duration,
) -> Option<Captured> {
    let started = std::time::Instant::now();
    loop {
        match read() {
            Read::Partial(_) if started.elapsed() < patience => {
                thread::sleep(step.max(std::time::Duration::from_millis(1)));
            }
            last => return last.delivered(),
        }
    }
}

/// Whether the clipboard holds nothing at all right now, told without opening
/// it: an owner that has just emptied it must not find it taken when it comes
/// back to fill it.
#[cfg(windows)]
fn clipboard_is_bare() -> bool {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn CountClipboardFormats() -> i32;
    }
    unsafe { CountClipboardFormats() == 0 }
}

#[cfg(not(windows))]
fn clipboard_is_bare() -> bool {
    false
}

/// Whether another program has the clipboard open right now. Windows lets
/// one window at a time open it, and a read tried meanwhile fails the way a
/// read of an empty clipboard does.
#[cfg(windows)]
fn clipboard_held_elsewhere() -> bool {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn OpenClipboard(owner: *mut core::ffi::c_void) -> i32;
        fn CloseClipboard() -> i32;
    }
    unsafe {
        if OpenClipboard(core::ptr::null_mut()) == 0 {
            return true;
        }
        CloseClipboard();
    }
    false
}

#[cfg(not(windows))]
fn clipboard_held_elsewhere() -> bool {
    false
}

/// The formats a capture reads, for asking the clipboard which it offers.
const READ_FORMATS: [ContentFormat; 5] = [
    ContentFormat::Files,
    ContentFormat::Text,
    ContentFormat::Html,
    ContentFormat::Image,
    ContentFormat::Rtf,
];

/// What a read returned — and, when it failed for a format the clipboard
/// offers, a note in `unread` that there is more to come back for.
fn fetched<T>(
    ctx: &ClipboardContext,
    format: ContentFormat,
    read: clipboard_rs::Result<T>,
    unread: &mut bool,
) -> Option<T> {
    if read.is_err() {
        *unread |= ctx.has(format);
    }
    read.ok()
}

/// The clipboard context every read goes through, made by the first read
/// and kept. On X11 clipboard-rs opens two connections to the server and
/// starts a thread for each context, and has nothing that gives them back, so
/// a context per read would leave those behind with every capture.
static CONTEXT: Mutex<Option<ClipboardContext>> = Mutex::new(None);

/// One clipboard read.
///
/// Priority: files > text, when text is what was copied (see
/// [`text_outranks_image`]) > image > rich text > plain text.
fn read_clipboard(max_files: usize) -> Read {
    if clipboard_is_bare() {
        return Read::Partial(Captured::Empty);
    }
    if clipboard_held_elsewhere() {
        return Read::Partial(Captured::Empty);
    }
    // A read that panicked with the lock held is no reason to refuse every
    // read after it, and a context that could not be made is not kept: the
    // next read tries again.
    let mut context = CONTEXT.lock().unwrap_or_else(PoisonError::into_inner);
    if context.is_none() {
        *context = ClipboardContext::new().ok();
    }
    let Some(ctx) = context.as_ref() else {
        return Read::Whole(Captured::Empty);
    };
    // The list of formats is where a secret is marked: content that offers
    // itself without one could not be listed, and is not read unchecked.
    let formats = ctx.available_formats().unwrap_or_default();
    if formats.is_empty() && READ_FORMATS.into_iter().any(|format| ctx.has(format)) {
        return Read::Partial(Captured::Empty);
    }
    if is_concealed(ctx, &formats) {
        return Read::Secret;
    }

    let mut unread = false;
    let content = read_content(ctx, &formats, max_files, &mut unread);
    if unread {
        Read::Partial(content)
    } else {
        Read::Whole(content)
    }
}

/// The clipboard's content by the priority of [`read_clipboard`], with a
/// note in `unread` of an offered format that would not be handed over.
/// `formats` is the clipboard's own list of what it offers.
fn read_content(
    ctx: &ClipboardContext,
    formats: &[String],
    max_files: usize,
    unread: &mut bool,
) -> Captured {
    if let Some(files) = fetched(ctx, ContentFormat::Files, ctx.get_files(), unread) {
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
    let plain = fetched(ctx, ContentFormat::Text, ctx.get_text(), unread).unwrap_or_default();
    let html = fetched(ctx, ContentFormat::Html, ctx.get_html(), unread)
        .filter(|html| !html.trim().is_empty());
    if !text_outranks_image(&plain, html.as_deref())
        && let Some(image) = read_image(ctx, formats, unread)
    {
        return image;
    }
    if let Some(html) = html
        && html_is_meaningfully_rich(&html)
    {
        return Captured::RichText {
            format: RichFormat::Html,
            markup: html,
            plain,
        };
    }
    if let Some(rtf) = fetched(ctx, ContentFormat::Rtf, ctx.get_rich_text(), unread)
        && !rtf.trim().is_empty()
        && rtf_is_meaningfully_rich(&rtf)
    {
        return Captured::RichText {
            format: RichFormat::Rtf,
            markup: rtf,
            plain,
        };
    }
    if !plain.is_empty() {
        return Captured::Text { text: plain };
    }
    Captured::Empty
}

/// The name a PNG goes by on the clipboard when an app put one there as it
/// is. It is the first place clipboard-rs looks for an image, to decode it.
#[cfg(target_os = "macos")]
const NATIVE_PNG: &str = "public.png";
#[cfg(windows)]
const NATIVE_PNG: &str = "PNG";
#[cfg(not(any(windows, target_os = "macos")))]
const NATIVE_PNG: &str = "image/png";

/// The clipboard's image as a PNG. A PNG it holds goes out as it is (see
/// [`untouched_png`]); what it holds instead — a TIFF on macOS, a bitmap on
/// Windows — is decoded and encoded as one.
fn read_image(ctx: &ClipboardContext, formats: &[String], unread: &mut bool) -> Option<Captured> {
    // Asked for only where the clipboard lists one, so that a clipboard
    // without one is read no more often than it was. The name is compared
    // the way Windows compares the names apps register: whatever the case.
    let lists_png = formats
        .iter()
        .any(|format| format.eq_ignore_ascii_case(NATIVE_PNG));
    if lists_png && let Some(image) = ctx.get_buffer(NATIVE_PNG).ok().and_then(untouched_png) {
        return Some(image);
    }
    let img = fetched(ctx, ContentFormat::Image, ctx.get_image(), unread)
        .filter(|img| !img.is_empty())?;
    let (width, height) = img.get_size();
    let png = img
        .to_png()
        .ok()
        .map(|b| b.get_bytes().to_vec())
        .unwrap_or_default();
    Some(Captured::Image { width, height, png })
}

/// A PNG as the image to deliver, its bytes left as the app that copied
/// wrote them: only the header is read, for the size. Decoding them to
/// encode them again delivers the same picture at the price of both — paid,
/// on macOS, on the host's main thread.
///
/// `None` leaves the bytes to the decoder: when they are not a PNG from its
/// signature to its end (see [`png_is_whole`]), or declare a size no picture
/// has.
fn untouched_png(png: Vec<u8>) -> Option<Captured> {
    let (width, height) = png_dimensions(&png)?;
    (width > 0 && height > 0 && png_is_whole(&png)).then_some(Captured::Image {
        width,
        height,
        png,
    })
}

/// The eight bytes every PNG starts with.
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// The width and height a PNG declares, read from its header without
/// decoding it: the `IHDR` chunk is mandatory and always first. `None` for
/// anything that does not start the way a PNG does.
pub(crate) fn png_dimensions(png: &[u8]) -> Option<(u32, u32)> {
    // After the signature: the chunk's length (4 bytes) and type (4), then
    // its data, which opens with the width (4) and the height (4).
    let (header, _) = png
        .strip_prefix(&PNG_SIGNATURE)?
        .split_first_chunk::<16>()?;
    if &header[4..8] != b"IHDR" {
        return None;
    }
    let number = |at: usize| {
        u32::from_be_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]])
    };
    Some((number(8), number(12)))
}

/// Whether the bytes are a PNG's chunks through to the last one, `IEND`,
/// with image data on the way — told by walking the chunks' lengths; nothing
/// is decoded. A PNG cut short, as by a transfer that stopped halfway, is
/// not.
fn png_is_whole(png: &[u8]) -> bool {
    let Some(mut chunks) = png.strip_prefix(&PNG_SIGNATURE) else {
        return false;
    };
    let mut has_data = false;
    // A chunk: the length of its data (4 bytes), its type (4), the data, and
    // a checksum (4).
    while let Some((head, rest)) = chunks.split_first_chunk::<8>() {
        let length = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let Some(next) = length.checked_add(4).and_then(|end| rest.get(end..)) else {
            return false;
        };
        match &head[4..] {
            b"IEND" => return has_data,
            b"IDAT" => has_data = true,
            _ => {}
        }
        chunks = next;
    }
    false
}

/// Whether the clipboard's formats carry a marker that its content is a secret.
fn is_concealed(ctx: &ClipboardContext, formats: &[String]) -> bool {
    formats
        .iter()
        .any(|format| marks_concealed(format, || ctx.get_buffer(format).ok()))
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

/// Fires a capture for a trigger, if the gate lets it through: the capture
/// runs on a worker thread and holds the gate until it ends. Called from the
/// key listener's callback, which has to stay fast, so the only thing done
/// here is the snapshot of the cheap change counter — taken now, before the
/// copy lands.
pub(crate) fn fire_if_allowed(config: &Config, handler: &CaptureHandler, gate: &CaptureGate) {
    let Some(in_flight) = gate.claim() else {
        return;
    };
    let baseline = clipboard_change_count();
    let config = config.clone();
    let handler = handler.clone();
    in_flight.run(move || run_capture(&config, &handler, baseline));
}

// ----------------------------- Windows / Linux -----------------------------

/// Runs on a worker thread. Reads the window, waits for the clipboard to change
/// (vs `baseline`), reads it, and hands the event to the handler.
#[cfg(not(target_os = "macos"))]
fn run_capture(config: &Config, handler: &CaptureHandler, baseline: u64) {
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
    for _ in 0..poll_step_count(config) {
        thread::sleep(step);
        if clipboard_change_count() != baseline {
            break;
        }
    }
    read_whole(|| read_clipboard(config.max_files), READ_PATIENCE, step)
}

// --------------------------------- macOS -----------------------------------

/// Run `f` on the process main thread and return its result, via libdispatch's
/// main queue, which the host's main run loop has to be draining.
///
/// A panic in `f` is caught on the main thread and carried on here, in the
/// calling thread: libdispatch calls `f` through a frame a panic cannot unwind
/// across, where it would abort the host process instead of ending one capture.
#[cfg(target_os = "macos")]
fn run_on_main<T, F>(f: F) -> T
where
    F: FnOnce() -> T + Send,
    T: Send,
{
    dispatch2::run_on_main(|_| std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)))
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// Runs on a worker thread. Window via main hop (URL lookup off main), then poll
/// the clipboard change counter (reads on main, sleeps off it), then hand the
/// event to the handler.
#[cfg(target_os = "macos")]
fn run_capture(config: &Config, handler: &CaptureHandler, baseline: u64) {
    let info = run_on_main(|| x_win::get_active_window().ok());
    // Built here, off the main thread: the URL lookup spawns osascript, which
    // is slow but not main-thread-bound.
    let fg = info.map(Foreground::of).unwrap_or_default();
    if should_skip(&fg, &config.denylist_exec_substrings) {
        return;
    }

    let step = config.clipboard_poll_step;
    let steps = poll_step_count(config);
    let max_files = config.max_files;
    let mut read: Option<Option<Captured>> = None;
    for _ in 0..steps {
        thread::sleep(step);
        read = run_on_main(move || {
            (clipboard_change_count() != baseline).then(|| read_clipboard(max_files).delivered())
        });
        if read.is_some() {
            break;
        }
    }
    let content =
        read.unwrap_or_else(|| run_on_main(move || read_clipboard(max_files).delivered()));

    // A secret is not delivered at all, like a denylisted app.
    if let Some(content) = content {
        handler(build_event(fg, content));
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

    /// A PNG of two pixels by three, as an encoder wrote it.
    const TINY_PNG: [u8; 81] = [
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x03, 0x08, 0x06, 0x00, 0x00, 0x00, 0xb9,
        0xea, 0xde, 0x81, 0x00, 0x00, 0x00, 0x18, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xf8,
        0xcf, 0xc0, 0xf0, 0x1f, 0x0c, 0x81, 0x34, 0x10, 0x30, 0x34, 0x80, 0x68, 0x07, 0x90, 0x00,
        0x00, 0xa7, 0xb9, 0x0c, 0xb5, 0xba, 0x25, 0x04, 0xdd, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
        0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    /// The bytes of a PNG as far as its chunks go: the signature, then each
    /// chunk with its length before its data and a checksum after it.
    fn png_of(chunks: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let mut png = PNG_SIGNATURE.to_vec();
        for (kind, data) in chunks {
            png.extend_from_slice(&(data.len() as u32).to_be_bytes());
            png.extend_from_slice(*kind);
            png.extend_from_slice(data);
            png.extend_from_slice(&[0; 4]); // the checksum, which nothing here reads
        }
        png
    }

    /// The data of an `IHDR` chunk for a picture of this size.
    fn ihdr(width: u32, height: u32) -> Vec<u8> {
        let mut data = [width.to_be_bytes(), height.to_be_bytes()].concat();
        data.extend_from_slice(&[8, 6, 0, 0, 0]); // eight bits a channel, RGBA
        data
    }

    #[test]
    fn png_dimensions_from_the_header() {
        assert_eq!(png_dimensions(&TINY_PNG), Some((2, 3)));
        let header = png_of(&[(b"IHDR", &ihdr(640, 480))]);
        assert_eq!(png_dimensions(&header), Some((640, 480)));
        // Cut one byte before the height ends.
        assert_eq!(png_dimensions(&header[..23]), None);
        // The first chunk is not the header.
        assert_eq!(png_dimensions(&png_of(&[(b"IDAT", &ihdr(640, 480))])), None);
        assert_eq!(png_dimensions(b"not a png at all, definitely"), None);
        assert_eq!(png_dimensions(&[]), None);
    }

    #[test]
    fn a_png_goes_out_untouched() {
        // The size it was delivered with, having gone out byte for byte.
        let untouched = |png: &[u8]| match untouched_png(png.to_vec()) {
            Some(Captured::Image {
                width,
                height,
                png: bytes,
            }) => {
                assert_eq!(bytes, png);
                Some((width, height))
            }
            None => None,
            other => panic!("expected an image, got {other:?}"),
        };
        assert_eq!(untouched(&TINY_PNG), Some((2, 3)));

        // With what else a PNG may carry: chunks the picture can do without,
        // its data in more than one chunk, bytes after its end.
        let mut png = png_of(&[
            (b"IHDR", &ihdr(640, 480)),
            (b"iCCP", b"a color profile"),
            (b"tEXt", b"Software\0an app"),
            (b"IDAT", b"some of the picture"),
            (b"IDAT", b"the rest of it"),
            (b"IEND", b""),
        ]);
        assert_eq!(untouched(&png), Some((640, 480)));
        png.extend_from_slice(b"trailing");
        assert_eq!(untouched(&png), Some((640, 480)));
    }

    #[test]
    fn what_is_not_a_whole_png_is_left_to_the_decoder() {
        // Cut short anywhere: a transfer that stopped halfway.
        for end in 0..TINY_PNG.len() {
            assert!(
                untouched_png(TINY_PNG[..end].to_vec()).is_none(),
                "cut at {end}"
            );
        }

        let refused = |chunks: &[(&[u8; 4], &[u8])]| untouched_png(png_of(chunks)).is_none();
        // No picture in it.
        assert!(refused(&[(b"IHDR", &ihdr(2, 3)), (b"IEND", b"")]));
        // A size no picture has.
        assert!(refused(&[
            (b"IHDR", &ihdr(0, 3)),
            (b"IDAT", b"x"),
            (b"IEND", b"")
        ]));
        assert!(refused(&[
            (b"IHDR", &ihdr(2, 0)),
            (b"IDAT", b"x"),
            (b"IEND", b"")
        ]));
        // Another kind of image under the PNG's name.
        let jpeg = b"\xff\xd8\xff\xe0\x00\x10JFIF, and so on for a while".to_vec();
        assert!(untouched_png(jpeg).is_none());

        // A chunk that claims more than is there — all a length can claim.
        let mut png = png_of(&[(b"IHDR", &ihdr(2, 3)), (b"IDAT", b"x"), (b"IEND", b"")]);
        assert!(untouched_png(png.clone()).is_some());
        let idat = 8 + 8 + 13 + 4; // past the signature and the whole of IHDR
        png[idat..idat + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(untouched_png(png).is_none());
    }

    #[test]
    fn the_decoder_and_the_untouched_path_agree() {
        // The PNG the tests go by is one a decoder takes, and what the
        // decoder's path encodes is in turn one the untouched path takes.
        let decoded = clipboard_rs::RustImageData::from_bytes(&TINY_PNG).expect("a valid PNG");
        assert_eq!(decoded.get_size(), (2, 3));
        let encoded = decoded.to_png().expect("encodes").get_bytes().to_vec();
        assert!(matches!(
            untouched_png(encoded),
            Some(Captured::Image {
                width: 2,
                height: 3,
                ..
            })
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_native_png_is_the_pasteboards_png_type() {
        // The type clipboard-rs reads its image from first.
        let png = unsafe { objc2_app_kit::NSPasteboardTypePNG };
        assert_eq!(png.to_string(), NATIVE_PNG);
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
    fn a_partial_read_is_gone_back_to() {
        use std::time::Duration;
        let text = |text: &str| Captured::Text {
            text: text.to_string(),
        };
        let says = |got: Option<Captured>, want: &str| matches!(got, Some(Captured::Text { text }) if text == want);
        let step = Duration::from_millis(1);
        let patient = Duration::from_secs(5);

        // The clipboard is bare, then busy, then hands over the whole.
        let mut reads = vec![
            Read::Whole(text("whole")),
            Read::Partial(text("part")),
            Read::Partial(Captured::Empty),
            Read::Partial(Captured::Empty),
        ];
        let got = read_whole(|| reads.pop().expect("a fifth read"), patient, step);
        assert!(says(got, "whole"));
        assert!(reads.is_empty());

        // A clipboard read whole and found empty is not gone back to.
        let mut reads = vec![Read::Whole(text("later")), Read::Whole(Captured::Empty)];
        let got = read_whole(|| reads.pop().expect("a third read"), patient, step);
        assert!(matches!(got, Some(Captured::Empty)));
        assert_eq!(reads.len(), 1);

        // Patience spent: what could be read has to do.
        let got = read_whole(|| Read::Partial(text("part")), Duration::ZERO, step);
        assert!(says(got, "part"));

        // A secret is not delivered, whatever came before it.
        let mut reads = vec![Read::Secret, Read::Partial(text("part"))];
        let got = read_whole(|| reads.pop().expect("a third read"), patient, step);
        assert!(got.is_none());
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
