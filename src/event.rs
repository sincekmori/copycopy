//! The payload handed to your capture handler.
//!
//! All types derive `serde::Serialize`/`Deserialize`, so a consumer can forward a
//! [`CaptureEvent`] straight to a webview (e.g. Tauri `emit`) or serialize it for
//! IPC, while Rust consumers can match on it directly.

use serde::{Deserialize, Serialize};

/// Rich-text markup flavor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RichFormat {
    /// HTML markup.
    Html,
    /// Rich Text Format (RTF) markup.
    Rtf,
}

/// The clipboard content captured on a trigger.
///
/// Serialized with an internal `"kind"` tag, e.g. `{ "kind": "text", "text": "..." }`.
///
/// - [`Captured::Image`] carries PNG-encoded bytes.
/// - [`Captured::Files`] carries normalized filesystem paths — read them in your
///   own code. Audio and video are not on the clipboard as media; they arrive here
///   as file references.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Captured {
    /// Plain UTF-8 text.
    Text {
        /// The copied text.
        text: String,
    },
    /// A bitmap image, PNG-encoded.
    Image {
        /// Image width in pixels.
        width: u32,
        /// Image height in pixels.
        height: u32,
        /// PNG-encoded image bytes.
        png: Vec<u8>,
    },
    /// Formatted text carrying meaningful styling.
    RichText {
        /// Which markup language `markup` is written in.
        format: RichFormat,
        /// The rich markup (HTML or RTF).
        markup: String,
        /// The plain-text fallback, when the clipboard also offered one.
        plain: String,
    },
    /// One or more copied file references.
    Files {
        /// Normalized filesystem paths.
        paths: Vec<String>,
    },
    /// The clipboard was empty or could not be read.
    Empty,
}

/// One capture: the foreground app context plus the clipboard [`Captured`] content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureEvent {
    /// Unix epoch milliseconds when the event was built.
    pub timestamp_ms: u64,
    /// Human-readable application name (may be empty if unavailable).
    pub app_name: String,
    /// Executable file name.
    pub exec_name: String,
    /// Full executable path.
    pub exec_path: String,
    /// Foreground window title (empty if unavailable / lacking permission).
    pub window_title: String,
    /// Browser URL when the foreground app is a supported chromium browser.
    pub url: Option<String>,
    /// Foreground process id.
    pub process_id: u32,
    /// The captured clipboard content.
    pub content: Captured,
}
