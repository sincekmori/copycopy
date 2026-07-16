//! Session detection and unprivileged installation of the embedded GNOME
//! Shell extension (`extension.js` / `metadata.json`, see this directory).
//!
//! Everything happens under `~/.local/share/gnome-shell/extensions/` — no
//! sudo, no system paths. The one thing we cannot do is make a running
//! gnome-shell load a *newly installed* extension: that requires the user to
//! log out and back in once (Wayland has no shell restart). The listener
//! detects that case and prints a clear message.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// UUID of the embedded extension (its install directory name).
pub(crate) const EXTENSION_UUID: &str = "copycopy@sincekmori.github.io";

const EXTENSION_JS: &str = include_str!("extension.js");
const METADATA_JSON: &str = include_str!("metadata.json");

/// Which listener backend the current session needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Session {
    /// X11 (including XWayland-free GNOME on Xorg): the rdev key listener works.
    X11,
    /// GNOME on Wayland: use the Shell-extension D-Bus backend.
    GnomeWayland,
    /// Some other Wayland compositor (KDE, wlroots, ...): unsupported for now.
    OtherWayland,
}

pub(crate) fn detect_session() -> Session {
    classify_session(
        env::var("WAYLAND_DISPLAY").ok().as_deref(),
        env::var("XDG_SESSION_TYPE").ok().as_deref(),
        env::var("XDG_CURRENT_DESKTOP").ok().as_deref(),
    )
}

fn classify_session(
    wayland_display: Option<&str>,
    session_type: Option<&str>,
    current_desktop: Option<&str>,
) -> Session {
    let wayland = wayland_display.is_some_and(|v| !v.trim().is_empty())
        || session_type.is_some_and(|v| v.trim().eq_ignore_ascii_case("wayland"));
    if !wayland {
        return Session::X11;
    }
    // XDG_CURRENT_DESKTOP is a colon-separated list, e.g. "ubuntu:GNOME".
    let gnome = current_desktop.is_some_and(|v| {
        v.split(':')
            .any(|part| part.trim().eq_ignore_ascii_case("gnome"))
    });
    if gnome {
        Session::GnomeWayland
    } else {
        Session::OtherWayland
    }
}

/// The `version` field of the embedded extension's metadata.json.
pub(crate) fn embedded_version() -> u64 {
    metadata_version(METADATA_JSON).unwrap_or(0)
}

/// Parse the integer `"version"` field out of a metadata.json without pulling
/// in a JSON dependency. (The quoted-key search cannot match inside the
/// `"shell-version"` key.)
fn metadata_version(metadata_json: &str) -> Option<u64> {
    let key = "\"version\"";
    let rest = &metadata_json[metadata_json.find(key)? + key.len()..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// `~/.local/share/gnome-shell/extensions/<UUID>`, honoring `XDG_DATA_HOME`.
fn extension_dir(xdg_data_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    let data_dir = match xdg_data_home {
        Some(d) if !d.trim().is_empty() => PathBuf::from(d),
        _ => PathBuf::from(home?).join(".local").join("share"),
    };
    Some(
        data_dir
            .join("gnome-shell")
            .join("extensions")
            .join(EXTENSION_UUID),
    )
}

/// Whether the extension files must be (re)written.
fn needs_write(installed_version: Option<u64>, embedded: u64, extension_js_present: bool) -> bool {
    !extension_js_present || installed_version.is_none_or(|v| v < embedded)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InstallState {
    /// The installed copy is already at (or beyond) the embedded version.
    UpToDate,
    /// The extension files were written (fresh install or upgrade). A running
    /// gnome-shell will not pick this up until the user logs out and back in.
    Written,
}

/// Write the extension if missing/outdated and (idempotently) enable it.
///
/// This is the only entry point that touches the real environment (the user's
/// data dir and gsettings), and it is reached exclusively from the runtime
/// listener path (i.e. an application actually calling [`crate::start`]) —
/// never from tests.
pub(crate) fn ensure_installed() -> Result<InstallState, String> {
    let dir = extension_dir(
        env::var("XDG_DATA_HOME").ok().as_deref(),
        env::var("HOME").ok().as_deref(),
    )
    .ok_or_else(|| "cannot determine the home directory (HOME is unset)".to_string())?;

    let state = install_into(&dir)?;

    if let Err(e) = enable_extension() {
        eprintln!("[copycopy] could not enable the GNOME Shell extension: {e}");
    }

    Ok(state)
}

/// Write the extension files into `dir` when missing or outdated. Pure
/// filesystem against the given directory (no env lookups, no gsettings), so
/// it is unit-tested against a temp directory.
fn install_into(dir: &Path) -> Result<InstallState, String> {
    let installed_version = fs::read_to_string(dir.join("metadata.json"))
        .ok()
        .and_then(|s| metadata_version(&s));
    let js_present = dir.join("extension.js").is_file();

    if !needs_write(installed_version, embedded_version(), js_present) {
        return Ok(InstallState::UpToDate);
    }
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    fs::write(dir.join("extension.js"), EXTENSION_JS)
        .map_err(|e| format!("cannot write extension.js: {e}"))?;
    fs::write(dir.join("metadata.json"), METADATA_JSON)
        .map_err(|e| format!("cannot write metadata.json: {e}"))?;
    Ok(InstallState::Written)
}

/// Enable the extension, preferring the running shell over raw gsettings.
///
/// Path 1: `org.gnome.Shell.Extensions.EnableExtension` over the session bus
/// (the same call `gnome-extensions enable` makes). When the shell already
/// knows the extension — installed at a previous login but sitting dormant —
/// this clears any stale `disabled-extensions` entry and activates it
/// immediately, no logout required. It cannot enable a freshly written
/// extension: gnome-shell only scans for new ones at login.
///
/// Path 2 (fresh install): write the gsettings keys directly — that is what
/// GNOME reads at the next login. Adding to `enabled-extensions` alone is
/// not enough: `disabled-extensions` outranks it (GNOME 45+), so a stale
/// entry there keeps the extension loaded-but-INITIALIZED through every
/// future login, with nothing in the UI to explain why. Remove it too.
fn enable_extension() -> Result<(), String> {
    if enable_via_shell() {
        return Ok(());
    }
    update_extension_list("enabled-extensions", |uuids| {
        append_uuid(uuids, EXTENSION_UUID)
    })?;
    update_extension_list("disabled-extensions", |uuids| {
        remove_uuid(uuids, EXTENSION_UUID)
    })
}

/// Ask the running gnome-shell to enable the extension. `false` covers every
/// failure — no session bus, no shell on it, extension not scanned yet — and
/// sends the caller down the gsettings path.
fn enable_via_shell() -> bool {
    let Ok(connection) = zbus::blocking::Connection::session() else {
        return false;
    };
    let Ok(proxy) = zbus::blocking::Proxy::new(
        &connection,
        "org.gnome.Shell.Extensions",
        "/org/gnome/Shell/Extensions",
        "org.gnome.Shell.Extensions",
    ) else {
        return false;
    };
    proxy
        .call::<_, _, bool>("EnableExtension", &(EXTENSION_UUID,))
        .unwrap_or(false)
}

/// The list with `uuid` appended, or `None` when it is already present.
fn append_uuid(mut uuids: Vec<String>, uuid: &str) -> Option<Vec<String>> {
    if uuids.iter().any(|u| u == uuid) {
        return None;
    }
    uuids.push(uuid.to_string());
    Some(uuids)
}

/// The list with `uuid` removed, or `None` when it was absent.
fn remove_uuid(uuids: Vec<String>, uuid: &str) -> Option<Vec<String>> {
    if !uuids.iter().any(|u| u == uuid) {
        return None;
    }
    Some(uuids.into_iter().filter(|u| u != uuid).collect())
}

/// Read-modify-write one `org.gnome.shell` string-array key via the
/// `gsettings` CLI. `update` returns `None` when the list is already in the
/// desired state, which skips the write.
fn update_extension_list(
    key: &str,
    update: impl FnOnce(Vec<String>) -> Option<Vec<String>>,
) -> Result<(), String> {
    const SCHEMA: &str = "org.gnome.shell";

    let out = Command::new("gsettings")
        .args(["get", SCHEMA, key])
        .output()
        .map_err(|e| format!("cannot run gsettings: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`gsettings get {SCHEMA} {key}` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    let current = String::from_utf8_lossy(&out.stdout);
    let Some(next) = update(parse_gvariant_string_array(&current)) else {
        return Ok(());
    };

    let out = Command::new("gsettings")
        .args(["set", SCHEMA, key, &format_gvariant_string_array(&next)])
        .output()
        .map_err(|e| format!("cannot run gsettings: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`gsettings set {SCHEMA} {key}` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Parse gsettings' text form of an `as` value: `['a', 'b']`, `[]`, `@as []`.
/// Extension UUIDs contain no quotes, so simple quote-delimited scanning is
/// enough; anything unparsable yields the entries we could read.
fn parse_gvariant_string_array(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text.trim();
    while let Some(start) = rest.find(['\'', '"']) {
        let quote = rest.as_bytes()[start] as char;
        let after = &rest[start + 1..];
        let Some(len) = after.find(quote) else { break };
        out.push(after[..len].to_string());
        rest = &after[len + 1..];
    }
    out
}

fn format_gvariant_string_array(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|s| format!("'{s}'")).collect();
    format!("[{}]", quoted.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Self-cleaning scratch directory under the system temp dir. Tests must
    /// never touch the real `$HOME` / gsettings / session bus — only pure
    /// helpers and `install_into` (which takes an explicit directory) are
    /// exercised here; `ensure_installed`, `enable_extension`,
    /// `enable_via_shell`, and `update_extension_list` are runtime-only.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = env::temp_dir().join(format!(
                "copycopy-installer-test-{}-{name}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn install_into_writes_then_reports_up_to_date() {
        let tmp = TempDir::new("fresh");
        let dir = tmp.0.join(EXTENSION_UUID);

        assert_eq!(install_into(&dir), Ok(InstallState::Written));
        assert_eq!(
            fs::read_to_string(dir.join("extension.js")).unwrap(),
            EXTENSION_JS
        );
        assert_eq!(
            fs::read_to_string(dir.join("metadata.json")).unwrap(),
            METADATA_JSON
        );

        assert_eq!(install_into(&dir), Ok(InstallState::UpToDate));
    }

    #[test]
    fn install_into_upgrades_an_older_version() {
        let tmp = TempDir::new("upgrade");
        let dir = tmp.0.join(EXTENSION_UUID);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("extension.js"), "// old").unwrap();
        fs::write(dir.join("metadata.json"), r#"{"version": 0}"#).unwrap();

        assert_eq!(install_into(&dir), Ok(InstallState::Written));
        assert_eq!(
            fs::read_to_string(dir.join("extension.js")).unwrap(),
            EXTENSION_JS
        );
    }

    #[test]
    fn install_into_repairs_a_missing_extension_js() {
        let tmp = TempDir::new("repair");
        let dir = tmp.0.join(EXTENSION_UUID);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("metadata.json"), METADATA_JSON).unwrap();

        assert_eq!(install_into(&dir), Ok(InstallState::Written));
        assert!(dir.join("extension.js").is_file());
    }

    #[test]
    fn install_into_keeps_a_newer_installed_version() {
        let tmp = TempDir::new("newer");
        let dir = tmp.0.join(EXTENSION_UUID);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("extension.js"), "// from the future").unwrap();
        fs::write(dir.join("metadata.json"), r#"{"version": 999999}"#).unwrap();

        assert_eq!(install_into(&dir), Ok(InstallState::UpToDate));
        assert_eq!(
            fs::read_to_string(dir.join("extension.js")).unwrap(),
            "// from the future"
        );
    }

    #[test]
    fn classify_x11_without_wayland_display() {
        assert_eq!(
            classify_session(None, Some("x11"), Some("ubuntu:GNOME")),
            Session::X11
        );
        assert_eq!(classify_session(Some(""), None, None), Session::X11);
    }

    #[test]
    fn classify_gnome_wayland() {
        assert_eq!(
            classify_session(Some("wayland-0"), Some("wayland"), Some("ubuntu:GNOME")),
            Session::GnomeWayland
        );
        assert_eq!(
            classify_session(Some("wayland-0"), None, Some("GNOME")),
            Session::GnomeWayland
        );
        // Session type alone is enough when WAYLAND_DISPLAY is not exported.
        assert_eq!(
            classify_session(None, Some("wayland"), Some("gnome")),
            Session::GnomeWayland
        );
    }

    #[test]
    fn classify_other_wayland() {
        assert_eq!(
            classify_session(Some("wayland-0"), Some("wayland"), Some("KDE")),
            Session::OtherWayland
        );
        assert_eq!(
            classify_session(Some("wayland-0"), Some("wayland"), None),
            Session::OtherWayland
        );
        // "GNOME-Classic" is not plain GNOME unless GNOME is also listed.
        assert_eq!(
            classify_session(Some("wayland-0"), None, Some("GNOME-Classic")),
            Session::OtherWayland
        );
        assert_eq!(
            classify_session(Some("wayland-0"), None, Some("GNOME-Classic:GNOME")),
            Session::GnomeWayland
        );
    }

    #[test]
    fn extension_dir_prefers_xdg_data_home() {
        assert_eq!(
            extension_dir(Some("/xdg/data"), Some("/home/u")).unwrap(),
            PathBuf::from(format!("/xdg/data/gnome-shell/extensions/{EXTENSION_UUID}"))
        );
        assert_eq!(
            extension_dir(None, Some("/home/u")).unwrap(),
            PathBuf::from(format!(
                "/home/u/.local/share/gnome-shell/extensions/{EXTENSION_UUID}"
            ))
        );
        assert_eq!(
            extension_dir(Some("  "), Some("/home/u")).unwrap(),
            PathBuf::from(format!(
                "/home/u/.local/share/gnome-shell/extensions/{EXTENSION_UUID}"
            ))
        );
        assert!(extension_dir(None, None).is_none());
    }

    #[test]
    fn metadata_version_parses_and_skips_shell_version() {
        assert_eq!(
            metadata_version(r#"{"shell-version": ["45", "46"], "version": 3}"#),
            Some(3)
        );
        assert_eq!(metadata_version(r#"{"version" : 12 }"#), Some(12));
        assert_eq!(metadata_version(r#"{"shell-version": ["45"]}"#), None);
        assert_eq!(metadata_version("not json"), None);
    }

    #[test]
    fn embedded_metadata_has_a_version() {
        assert!(embedded_version() >= 1);
    }

    #[test]
    fn gvariant_string_array_roundtrip() {
        assert_eq!(
            parse_gvariant_string_array("['a@b', 'c@d']"),
            vec!["a@b", "c@d"]
        );
        assert_eq!(parse_gvariant_string_array("@as []"), Vec::<String>::new());
        assert_eq!(parse_gvariant_string_array("[]"), Vec::<String>::new());
        assert_eq!(parse_gvariant_string_array("[\"x\"]"), vec!["x"]);
        assert_eq!(
            format_gvariant_string_array(&["a".to_string(), "b".to_string()]),
            "['a', 'b']"
        );
        assert_eq!(format_gvariant_string_array(&[]), "[]");
    }

    #[test]
    fn append_uuid_adds_once() {
        assert_eq!(append_uuid(vec![], "a@b"), Some(vec!["a@b".to_string()]));
        assert_eq!(
            append_uuid(vec!["x@y".to_string()], "a@b"),
            Some(vec!["x@y".to_string(), "a@b".to_string()])
        );
        assert_eq!(append_uuid(vec!["a@b".to_string()], "a@b"), None);
    }

    #[test]
    fn remove_uuid_removes_every_occurrence() {
        assert_eq!(remove_uuid(vec![], "a@b"), None);
        assert_eq!(remove_uuid(vec!["x@y".to_string()], "a@b"), None);
        assert_eq!(
            remove_uuid(
                vec!["a@b".to_string(), "x@y".to_string(), "a@b".to_string()],
                "a@b"
            ),
            Some(vec!["x@y".to_string()])
        );
    }

    #[test]
    fn needs_write_matrix() {
        assert!(needs_write(None, 1, false)); // fresh install
        assert!(needs_write(None, 1, true)); // metadata missing/corrupt
        assert!(needs_write(Some(1), 2, true)); // upgrade
        assert!(needs_write(Some(1), 1, false)); // extension.js deleted
        assert!(!needs_write(Some(1), 1, true)); // current
        assert!(!needs_write(Some(2), 1, true)); // newer than embedded: keep
    }
}
