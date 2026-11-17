//! Windows/Linux listener entry point.
//!
//! Default path: a global, *passive* keyboard listener (rdev `listen`, never
//! `grab`) that feeds key events to [`crate::detector`] and, on a trigger,
//! runs the capture (which calls the handler). macOS uses `listener_macos`.
//!
//! On Linux the session is inspected first: GNOME Wayland is routed to the
//! Shell-extension backend in [`crate::gnome`] (rdev cannot see keys under
//! Wayland), other Wayland compositors are reported as unsupported, and X11
//! falls through to the rdev path.

use std::time::Instant;

use rdev::{Event, EventType, Key, listen};

use crate::CaptureHandler;
use crate::capture::fire_if_allowed;
use crate::config::Config;
use crate::detector::DoubleTap;
use crate::gate::CaptureGate;

#[inline]
fn is_trigger_modifier(key: Key) -> bool {
    matches!(key, Key::ControlLeft | Key::ControlRight)
}

/// Blocking. Run this on a dedicated thread. Picks the backend for the session.
pub fn start_listener(config: Config, handler: CaptureHandler, status: crate::StatusHandler) {
    reporting_a_panic(&status, || run_backend(config, handler, status.clone()));
}

/// Runs a listener and reports the panic that ends it, if one does: the
/// trigger is gone with the listener's thread, and nothing else would tell the
/// host.
fn reporting_a_panic(status: &crate::StatusHandler, listener: impl FnOnce()) {
    if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(listener)) {
        let what = panic
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("no message");
        report_failed(status, format!("the listener panicked: {what}"));
    }
}

fn run_backend(config: Config, handler: CaptureHandler, status: crate::StatusHandler) {
    #[cfg(target_os = "linux")]
    {
        use crate::gnome::installer::{Session, detect_session};
        match detect_session() {
            Session::GnomeWayland => {
                return crate::gnome::listener::start_listener(config, handler, status);
            }
            Session::OtherWayland => {
                eprintln!(
                    "[copycopy] this Wayland session is not GNOME; only GNOME Wayland and \
                     X11 are supported on Linux (KDE/wlroots support may come later)."
                );
                status(crate::TriggerStatus::UnsupportedSession);
                return;
            }
            Session::X11 => {} // fall through to the rdev key listener
        }
    }

    start_rdev_listener(config, handler, status)
}

/// Blocking rdev key listener (Windows, and Linux on X11).
fn start_rdev_listener(config: Config, handler: CaptureHandler, status: crate::StatusHandler) {
    let mut detector = DoubleTap::new(config.double_tap_window.as_millis() as u64);
    let gate = CaptureGate::new(config.trigger_cooldown);
    let base = Instant::now();

    let callback = move |event: Event| {
        // Never let a panic unwind across rdev's C callback boundary (UB).
        let _ =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match &event.event_type {
                EventType::KeyPress(key) => {
                    let key = *key;
                    if is_trigger_modifier(key) {
                        detector.set_modifier(true);
                    } else if key == Key::KeyC {
                        let now_ms = base.elapsed().as_millis() as u64;
                        if detector.on_c_down(now_ms, false) {
                            fire_if_allowed(&config, &handler, &gate);
                        }
                    }
                }
                EventType::KeyRelease(key) => {
                    let key = *key;
                    if is_trigger_modifier(key) {
                        detector.set_modifier(false);
                    } else if key == Key::KeyC {
                        detector.on_c_up();
                    }
                }
                _ => {}
            }));
    };

    // rdev::listen blocks on success, so "armed" is reported just before —
    // an immediate error is then corrected by the Failed report (latest wins).
    status(crate::TriggerStatus::Listening);
    if let Err(e) = listen(callback) {
        report_failed(&status, format!("rdev::listen failed: {e:?}"));
    }
}

/// Reports that the listener could not start or died: on stderr, and as a
/// [`TriggerStatus::Failed`](crate::TriggerStatus::Failed) with the same
/// message.
pub(crate) fn report_failed(status: &crate::StatusHandler, message: String) {
    eprintln!("[copycopy] {message}");
    status(crate::TriggerStatus::Failed { message });
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::TriggerStatus;

    /// A status handler, and the reports it has been given.
    fn recording() -> (crate::StatusHandler, Arc<Mutex<Vec<TriggerStatus>>>) {
        let seen: Arc<Mutex<Vec<TriggerStatus>>> = Arc::default();
        let sink = Arc::clone(&seen);
        let status: crate::StatusHandler = Arc::new(move |report: TriggerStatus| {
            sink.lock().unwrap().push(report);
        });
        (status, seen)
    }

    #[test]
    fn a_listener_that_panics_is_reported() {
        let (status, seen) = recording();
        reporting_a_panic(&status, || {
            // Unwinds as a panic does, without the panic hook's report in the
            // test output.
            std::panic::resume_unwind(Box::new("boom"));
        });
        assert_eq!(
            *seen.lock().unwrap(),
            [TriggerStatus::Failed {
                message: "the listener panicked: boom".to_string()
            }]
        );
    }

    #[test]
    fn a_listener_that_returns_is_not_reported() {
        let (status, seen) = recording();
        reporting_a_panic(&status, || {});
        assert!(seen.lock().unwrap().is_empty());
    }
}
