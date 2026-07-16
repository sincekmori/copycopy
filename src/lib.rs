#![doc = include_str!("../README.md")]
#![warn(missing_docs)]

mod capture;
mod config;
mod detector;
mod event;
#[cfg(target_os = "linux")]
mod gnome;
#[cfg(not(target_os = "macos"))]
mod listener;
#[cfg(target_os = "macos")]
mod listener_macos;

use std::sync::Arc;

pub use config::Config;
pub use event::{CaptureEvent, Captured, RichFormat};

/// Handler invoked once per trigger, on a worker thread (off the main thread,
/// including on macOS), so it may perform slow work.
pub type CaptureHandler = Arc<dyn Fn(CaptureEvent) + Send + Sync>;

/// Handler invoked with [`TriggerStatus`] reports (see [`start_with_status`]).
pub type StatusHandler = Arc<dyn Fn(TriggerStatus) + Send + Sync>;

/// How the trigger settled, reported to the status handler of
/// [`start_with_status`] once the backend has probed its environment.
///
/// Treat the **latest** report as current. On most platforms exactly one
/// status arrives shortly after [`start_with_status`] returns; a [`Failed`]
/// report can follow later if the listener dies. GUI hosts should surface
/// [`GnomeExtensionAwaitingLogin`] and [`UnsupportedSession`] to the user —
/// in both states the trigger is silently inactive, and stderr (where the
/// accompanying diagnostics go) is invisible in a bundled app.
///
/// Serialized with a `"kind"` tag (`snake_case`), like [`Captured`], so a
/// report can be forwarded to a webview or over IPC in one line.
///
/// [`Failed`]: TriggerStatus::Failed
/// [`GnomeExtensionAwaitingLogin`]: TriggerStatus::GnomeExtensionAwaitingLogin
/// [`UnsupportedSession`]: TriggerStatus::UnsupportedSession
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggerStatus {
    /// The trigger is armed and watching for the gesture.
    Listening,
    /// Linux, GNOME Wayland: the Shell extension is installed and enabled but
    /// not loaded — GNOME Shell only loads new extensions at login. The
    /// trigger stays inactive until the user logs out and back in (and the
    /// host application starts again).
    GnomeExtensionAwaitingLogin,
    /// Linux, GNOME Wayland: an older extension version is loaded and the
    /// trigger **is** active; the updated version takes over at the next
    /// login.
    GnomeExtensionOutdated {
        /// The version currently loaded by GNOME Shell.
        loaded: u64,
        /// The version this crate installed on disk.
        embedded: u64,
    },
    /// Linux: this session offers no unprivileged capture path (a non-GNOME
    /// Wayland compositor). The trigger is inactive.
    UnsupportedSession,
    /// The listener could not start or died; the trigger is inactive.
    Failed {
        /// Human-readable diagnostic (also printed to stderr).
        message: String,
    },
}

/// Errors from [`start`].
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The OS key listener could not be installed (e.g. missing macOS Input
    /// Monitoring permission).
    ListenerInit(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::ListenerInit(msg) => write!(f, "failed to initialize the key listener: {msg}"),
        }
    }
}

impl std::error::Error for Error {}

/// Handle representing a running capture. The listener currently runs for the
/// process lifetime; there is no stop. Hold this (e.g. `let _capture = start(..)?;`)
/// to document intent and to keep the API stable if teardown is added later.
#[must_use = "dropping this does not stop capture, but holding it documents intent"]
pub struct Capture {
    _private: (),
}

/// Start global Ctrl/Cmd+C+C capture.
///
/// `handler` is called once per trigger on a worker thread. On macOS this **must**
/// be called on the thread running the app's main run loop (see the crate docs).
///
/// # Errors
///
/// Returns [`Error::ListenerInit`] if the OS key listener cannot be installed
/// (macOS). On Windows/Linux the listener runs on a spawned thread and any
/// initialization failure is logged to stderr instead.
pub fn start<F>(config: Config, handler: F) -> Result<Capture, Error>
where
    F: Fn(CaptureEvent) + Send + Sync + 'static,
{
    start_with_status(config, handler, |_| {})
}

/// [`start`], plus a `status` handler that reports how the trigger settled
/// (see [`TriggerStatus`]) — the states worth showing in a UI, like the
/// GNOME Wayland "log out and back in once" step, which is otherwise only a
/// stderr message.
///
/// `status` is called on the listener thread (macOS: on the calling thread
/// for the initial [`TriggerStatus::Listening`]); keep it quick and hop to
/// your UI thread yourself.
///
/// # Errors
///
/// Same as [`start`]: [`Error::ListenerInit`] when the OS key listener cannot
/// be installed synchronously (macOS). Everywhere else failures arrive as
/// [`TriggerStatus::Failed`] reports.
pub fn start_with_status<F, S>(config: Config, handler: F, status: S) -> Result<Capture, Error>
where
    F: Fn(CaptureEvent) + Send + Sync + 'static,
    S: Fn(TriggerStatus) + Send + Sync + 'static,
{
    let handler: CaptureHandler = Arc::new(handler);
    let status: StatusHandler = Arc::new(status);

    #[cfg(not(target_os = "macos"))]
    {
        std::thread::spawn(move || listener::start_listener(config, handler, status));
        Ok(Capture { _private: () })
    }

    #[cfg(target_os = "macos")]
    {
        listener_macos::install(config, handler)?;
        status(TriggerStatus::Listening);
        Ok(Capture { _private: () })
    }
}

/// Block the current thread forever so capture keeps running when used as a
/// standalone tool. On macOS this runs the main run loop (which services the
/// event tap and the libdispatch main queue); elsewhere it parks the thread.
///
/// A GUI host that already runs its own event loop should NOT call this — just
/// keep the [`Capture`] handle alive.
pub fn block_forever() -> ! {
    #[cfg(target_os = "macos")]
    {
        extern "C" {
            fn CFRunLoopRun();
        }
        // Returns only if the run loop has no sources/timers; fall through to park.
        unsafe { CFRunLoopRun() };
    }
    loop {
        std::thread::park();
    }
}

// The status enum is part of the host's IPC contract (forwarded to webviews
// keyed on the `kind` tag), so the serialized shape is load-bearing.
#[cfg(test)]
mod status_tests {
    use super::TriggerStatus;

    #[test]
    fn status_serializes_with_a_kind_tag() {
        let json = serde_json::to_string(&TriggerStatus::GnomeExtensionAwaitingLogin).unwrap();
        assert_eq!(json, r#"{"kind":"gnome_extension_awaiting_login"}"#);

        let json = serde_json::to_string(&TriggerStatus::GnomeExtensionOutdated {
            loaded: 1,
            embedded: 2,
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"kind":"gnome_extension_outdated","loaded":1,"embedded":2}"#
        );

        let json = serde_json::to_string(&TriggerStatus::Failed {
            message: "boom".into(),
        })
        .unwrap();
        assert_eq!(json, r#"{"kind":"failed","message":"boom"}"#);
    }
}
