//! zbus subscriber side of the GNOME Wayland backend.
//!
//! The Shell extension (see `extension.js`) broadcasts only a metadata
//! `CaptureReady(serial)` signal; the clipboard content itself is fetched
//! with a unicast `TakeCapture(serial)` call so it never rides the bus as a
//! broadcast any process could observe. Runs blocking on the listener thread.

use crate::capture::{build_event, html_is_meaningfully_rich, should_skip, Foreground};
use crate::config::Config;
use crate::event::{Captured, RichFormat};
use crate::gnome::installer::{self, InstallState};
use crate::CaptureHandler;

const BUS_NAME: &str = "org.gnome.Shell";
const OBJECT_PATH: &str = "/io/github/sincekmori/copycopy";
const INTERFACE: &str = "io.github.sincekmori.copycopy";

/// `TakeCapture` reply: (kind, mime, data, plain, app_name, wm_class, title, pid).
type TakeCaptureReply = (String, String, Vec<u8>, String, String, String, String, u32);

/// Blocking. Run this on a dedicated thread.
pub(crate) fn start_listener(
    config: Config,
    handler: CaptureHandler,
    status: crate::StatusHandler,
) {
    match installer::ensure_installed() {
        Ok(InstallState::Written) => {
            eprintln!(
                "[copycopy] installed the GNOME Shell extension ({})",
                installer::EXTENSION_UUID
            );
        }
        Ok(InstallState::UpToDate) => {}
        Err(e) => {
            eprintln!("[copycopy] failed to install the GNOME Shell extension: {e}");
            status(crate::TriggerStatus::Failed {
                message: format!("failed to install the GNOME Shell extension: {e}"),
            });
            return;
        }
    }

    if let Err(e) = run(&config, &handler, &status) {
        eprintln!("[copycopy] GNOME Wayland D-Bus listener failed: {e}");
        status(crate::TriggerStatus::Failed {
            message: format!("GNOME Wayland D-Bus listener failed: {e}"),
        });
    }
}

fn run(
    config: &Config,
    handler: &CaptureHandler,
    status: &crate::StatusHandler,
) -> zbus::Result<()> {
    let connection = zbus::blocking::Connection::session()?;
    let proxy = zbus::blocking::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE)?;

    // Probe whether the extension is actually loaded; a fresh install (or an
    // upgrade) only takes effect at the next login.
    match proxy.call::<_, _, u32>("GetVersion", &()) {
        Ok(loaded) if u64::from(loaded) < installer::embedded_version() => {
            eprintln!(
                "[copycopy] the GNOME Shell extension was updated (v{loaded} is loaded, \
                 v{} is installed); log out and back in to activate the new version.",
                installer::embedded_version()
            );
            // The old version keeps capturing, so this is "listening, but".
            status(crate::TriggerStatus::GnomeExtensionOutdated {
                loaded: u64::from(loaded),
                embedded: installer::embedded_version(),
            });
        }
        Ok(_) => status(crate::TriggerStatus::Listening),
        Err(_) => {
            eprintln!(
                "[copycopy] the GNOME Shell extension is installed and enabled but not \
                 loaded yet — GNOME Shell only loads new extensions at login. \
                 Log out and back in once, then restart this application."
            );
            status(crate::TriggerStatus::GnomeExtensionAwaitingLogin);
        }
    }

    let signals = proxy.receive_signal("CaptureReady")?;
    for message in signals {
        let serial: u32 = match message.body().deserialize() {
            Ok(s) => s,
            Err(_) => continue,
        };
        let reply: TakeCaptureReply = match proxy.call("TakeCapture", &serial) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[copycopy] TakeCapture({serial}) failed: {e}");
                continue;
            }
        };
        let (kind, mime, data, plain, app_name, wm_class, title, pid) = reply;
        if kind == "expired" {
            continue; // someone raced us to it, or we were too slow
        }

        let fg = Foreground {
            app_name,
            exec_name: wm_class,
            exec_path: String::new(),
            window_title: title,
            process_id: pid,
            url: None,
        };
        if should_skip(&fg, &config.denylist_exec_substrings) {
            continue;
        }
        let content = convert(&kind, &mime, data, plain, config.max_files);
        handler(build_event(fg, content));
    }
    Ok(())
}

/// Map an extension payload onto [`Captured`], mirroring the priority and
/// richness semantics of the other platforms' clipboard reader.
fn convert(kind: &str, mime: &str, data: Vec<u8>, plain: String, max_files: usize) -> Captured {
    match kind {
        "text" => {
            let text = String::from_utf8_lossy(&data).into_owned();
            if text.is_empty() {
                Captured::Empty
            } else {
                Captured::Text { text }
            }
        }
        "rich" => {
            let markup = String::from_utf8_lossy(&data).into_owned();
            if !markup.trim().is_empty() && html_is_meaningfully_rich(&markup) {
                Captured::RichText {
                    format: RichFormat::Html,
                    markup,
                    plain,
                }
            } else if !plain.is_empty() {
                // A styled wrapper around plain content: deliver the plain text.
                Captured::Text { text: plain }
            } else {
                Captured::Empty
            }
        }
        "image" => {
            if data.is_empty() {
                Captured::Empty
            } else {
                let (width, height) = png_dimensions(&data);
                Captured::Image {
                    width,
                    height,
                    png: data,
                }
            }
        }
        "files" => {
            let text = String::from_utf8_lossy(&data);
            let paths = parse_uri_list(&text, mime == "x-special/gnome-copied-files", max_files);
            if paths.is_empty() {
                Captured::Empty
            } else {
                Captured::Files { paths }
            }
        }
        _ => Captured::Empty,
    }
}

/// Width/height from a PNG IHDR header (the IHDR chunk is mandatory and
/// always first). Returns (0, 0) for anything that is not a PNG.
fn png_dimensions(png: &[u8]) -> (u32, u32) {
    const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    if png.len() >= 24 && png[..8] == SIGNATURE && &png[12..16] == b"IHDR" {
        let width = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
        let height = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
        (width, height)
    } else {
        (0, 0)
    }
}

/// Parse `text/uri-list` (or `x-special/gnome-copied-files`, whose first line
/// is the "copy"/"cut" verb) into normalized filesystem paths.
fn parse_uri_list(text: &str, gnome_copied_files: bool, max_files: usize) -> Vec<String> {
    text.lines()
        .skip(usize::from(gnome_copied_files))
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(crate::capture::normalize_file_path)
        .take(max_files)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_with(width: u32, height: u32) -> Vec<u8> {
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        png.extend_from_slice(&13u32.to_be_bytes()); // IHDR length
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&width.to_be_bytes());
        png.extend_from_slice(&height.to_be_bytes());
        png
    }

    #[test]
    fn png_dimensions_from_ihdr() {
        assert_eq!(png_dimensions(&png_with(640, 480)), (640, 480));
        assert_eq!(png_dimensions(b"not a png at all, definitely"), (0, 0));
        assert_eq!(png_dimensions(&[]), (0, 0));
    }

    #[test]
    fn uri_list_is_normalized() {
        let text = "file:///home/u/a%20b.txt\r\nfile:///home/u/c.png\r\n";
        assert_eq!(
            parse_uri_list(text, false, 50),
            vec!["/home/u/a b.txt", "/home/u/c.png"]
        );
    }

    #[test]
    fn uri_list_skips_comments_and_blank_lines() {
        let text = "# comment\n\nfile:///x\n";
        assert_eq!(parse_uri_list(text, false, 50), vec!["/x"]);
    }

    #[test]
    fn gnome_copied_files_strips_the_verb_line() {
        let text = "copy\nfile:///home/u/doc.pdf";
        assert_eq!(parse_uri_list(text, true, 50), vec!["/home/u/doc.pdf"]);
        let cut = "cut\nfile:///y";
        assert_eq!(parse_uri_list(cut, true, 50), vec!["/y"]);
    }

    #[test]
    fn uri_list_respects_max_files() {
        let text = "file:///a\nfile:///b\nfile:///c\n";
        assert_eq!(parse_uri_list(text, false, 2), vec!["/a", "/b"]);
    }

    #[test]
    fn convert_text() {
        assert!(matches!(
            convert("text", "text/plain;charset=utf-8", b"hi".to_vec(), String::new(), 50),
            Captured::Text { text } if text == "hi"
        ));
        assert!(matches!(
            convert(
                "text",
                "text/plain;charset=utf-8",
                Vec::new(),
                String::new(),
                50
            ),
            Captured::Empty
        ));
    }

    #[test]
    fn convert_rich_keeps_meaningful_html() {
        let html = b"<p>hi <b>bold</b></p>".to_vec();
        match convert("rich", "text/html", html, "hi bold".into(), 50) {
            Captured::RichText {
                format,
                markup,
                plain,
            } => {
                assert_eq!(format, RichFormat::Html);
                assert!(markup.contains("<b>"));
                assert_eq!(plain, "hi bold");
            }
            other => panic!("expected RichText, got {other:?}"),
        }
    }

    #[test]
    fn convert_rich_downgrades_styled_plain_to_text() {
        let html = br#"<span style="color:#000">plain</span>"#.to_vec();
        assert!(matches!(
            convert("rich", "text/html", html, "plain".into(), 50),
            Captured::Text { text } if text == "plain"
        ));
    }

    #[test]
    fn convert_rich_without_plain_fallback_is_empty() {
        let html = b"<span>plain</span>".to_vec();
        assert!(matches!(
            convert("rich", "text/html", html, String::new(), 50),
            Captured::Empty
        ));
    }

    #[test]
    fn convert_image_parses_dimensions() {
        let png = png_with(2, 3);
        match convert("image", "image/png", png.clone(), String::new(), 50) {
            Captured::Image {
                width,
                height,
                png: bytes,
            } => {
                assert_eq!((width, height), (2, 3));
                assert_eq!(bytes, png);
            }
            other => panic!("expected Image, got {other:?}"),
        }
    }

    #[test]
    fn convert_files_and_empty_kinds() {
        assert!(matches!(
            convert("files", "text/uri-list", b"file:///a".to_vec(), String::new(), 50),
            Captured::Files { paths } if paths == ["/a"]
        ));
        assert!(matches!(
            convert(
                "files",
                "x-special/gnome-copied-files",
                b"copy\nfile:///a".to_vec(),
                String::new(),
                50
            ),
            Captured::Files { paths } if paths == ["/a"]
        ));
        assert!(matches!(
            convert("empty", "", Vec::new(), String::new(), 50),
            Captured::Empty
        ));
        assert!(matches!(
            convert("unknown-kind", "", b"x".to_vec(), String::new(), 50),
            Captured::Empty
        ));
    }
}
