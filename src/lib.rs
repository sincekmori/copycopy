#![doc = include_str!("../README.md")]
#![warn(missing_docs)]

mod capture;
mod config;
mod detector;
mod event;
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
    let handler: CaptureHandler = Arc::new(handler);

    #[cfg(not(target_os = "macos"))]
    {
        std::thread::spawn(move || listener::start_listener(config, handler));
        Ok(Capture { _private: () })
    }

    #[cfg(target_os = "macos")]
    {
        listener_macos::install(config, handler)?;
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
