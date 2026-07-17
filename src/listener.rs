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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use rdev::{Event, EventType, Key, listen};

use crate::CaptureHandler;
use crate::capture::{clipboard_change_count, run_capture};
use crate::config::Config;
use crate::detector::DoubleTap;

#[inline]
fn is_trigger_modifier(key: Key) -> bool {
    matches!(key, Key::ControlLeft | Key::ControlRight)
}

/// Blocking. Run this on a dedicated thread. Picks the backend for the session.
pub fn start_listener(config: Config, handler: CaptureHandler, status: crate::StatusHandler) {
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
    let mut last_trigger: Option<Instant> = None;
    let in_flight = Arc::new(AtomicBool::new(false));
    let base = Instant::now();
    let cooldown = config.trigger_cooldown;

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
                            fire_if_allowed(
                                &config,
                                &handler,
                                &in_flight,
                                &mut last_trigger,
                                cooldown,
                            );
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
        eprintln!("[copycopy] rdev::listen failed: {e:?}");
        status(crate::TriggerStatus::Failed {
            message: format!("rdev::listen failed: {e:?}"),
        });
    }
}

/// Cooldown + single-in-flight gate, then run the capture on a worker thread.
fn fire_if_allowed(
    config: &Config,
    handler: &CaptureHandler,
    in_flight: &Arc<AtomicBool>,
    last_trigger: &mut Option<Instant>,
    cooldown: Duration,
) {
    let cooled = last_trigger
        .map(|t| t.elapsed() >= cooldown)
        .unwrap_or(true);
    if !cooled {
        return;
    }
    if in_flight.swap(true, Ordering::AcqRel) {
        return;
    }
    *last_trigger = Some(Instant::now());

    let baseline = clipboard_change_count();
    let config = config.clone();
    let handler = handler.clone();
    let flag = in_flight.clone();
    thread::spawn(move || {
        run_capture(&config, &handler, baseline);
        flag.store(false, Ordering::Release);
    });
}
