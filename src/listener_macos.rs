//! macOS global key listener: a passive `CGEventTap` on the main run loop.
//! rdev can't be used (its callback hits main-thread-only Carbon APIs and SIGSEGVs
//! off the main thread), so we install our own ListenOnly tap on the host's main
//! `CFRunLoop` and decode the virtual key code ourselves.

use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Instant;

use objc2_core_foundation::{CFMachPort, CFRunLoop, kCFRunLoopCommonModes};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType,
};

use crate::capture::{capture_macos, clipboard_change_count};
use crate::config::Config;
use crate::detector::DoubleTap;
use crate::{CaptureHandler, Error};

const KEY_C: i64 = 8; // kVK_ANSI_C

/// Everything the tap callback needs, leaked once in [`install`]. The callback
/// only ever runs on the main run loop, so the `RefCell`s are never contended.
struct TapState {
    config: Config,
    handler: CaptureHandler,
    detector: RefCell<DoubleTap>,
    last_trigger: RefCell<Option<Instant>>,
    in_flight: Arc<AtomicBool>,
    base: Instant,
    /// The tap's mach port (as a raw pointer value) so the callback can
    /// re-enable the tap if macOS disables it. Valid for the program's life
    /// because [`install`] leaks the port.
    port: OnceLock<usize>,
}

fn fire_if_allowed(state: &TapState) {
    let cooled = state
        .last_trigger
        .borrow()
        .map(|t| t.elapsed() >= state.config.trigger_cooldown)
        .unwrap_or(true);
    if !cooled {
        return;
    }
    if state.in_flight.swap(true, Ordering::AcqRel) {
        return;
    }
    *state.last_trigger.borrow_mut() = Some(Instant::now());

    // Keep the tap callback fast: snapshot only the cheap change counter here.
    let baseline = clipboard_change_count();
    let config = state.config.clone();
    let handler = state.handler.clone();
    let flag = state.in_flight.clone();
    thread::spawn(move || {
        capture_macos(config, handler, baseline);
        flag.store(false, Ordering::Release);
    });
}

unsafe extern "C-unwind" fn tap_callback(
    _proxy: CGEventTapProxy,
    event_type: CGEventType,
    event: NonNull<CGEvent>,
    user_info: *mut c_void,
) -> *mut CGEvent {
    // Never let a panic unwind into the CoreGraphics caller.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Safety: `user_info` is the `TapState` leaked by `install`.
        let state = unsafe { &*(user_info as *const TapState) };
        match event_type {
            CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
                if let Some(&p) = state.port.get() {
                    // Safety: the port is leaked in `install`, alive forever.
                    CGEvent::tap_enable(unsafe { &*(p as *const CFMachPort) }, true);
                }
                return;
            }
            CGEventType::KeyDown | CGEventType::KeyUp => {}
            _ => return,
        }

        // Safety: CoreGraphics hands the callback a valid event.
        let ev = unsafe { event.as_ref() };
        if CGEvent::integer_value_field(Some(ev), CGEventField::KeyboardEventKeycode) != KEY_C {
            return;
        }
        let is_down = event_type == CGEventType::KeyDown;
        let cmd = CGEvent::flags(Some(ev)).contains(CGEventFlags::MaskCommand);
        let autorepeat =
            CGEvent::integer_value_field(Some(ev), CGEventField::KeyboardEventAutorepeat) != 0;

        if is_down {
            let now_ms = state.base.elapsed().as_millis() as u64;
            let triggered = {
                let mut det = state.detector.borrow_mut();
                det.set_modifier(cmd);
                det.on_c_down(now_ms, autorepeat)
            };
            if triggered {
                fire_if_allowed(state);
            }
        } else {
            state.detector.borrow_mut().on_c_up();
        }
    }));
    // ListenOnly tap: the return value is ignored; pass the event through.
    event.as_ptr()
}

/// Install the passive event tap on the current (main) run loop. MUST be called
/// on the main thread (e.g. from the host's startup hook / `main`).
pub fn install(config: Config, handler: CaptureHandler) -> Result<(), Error> {
    let double_tap_window = config.double_tap_window.as_millis() as u64;
    let state: &'static TapState = Box::leak(Box::new(TapState {
        config,
        handler,
        detector: RefCell::new(DoubleTap::new(double_tap_window)),
        last_trigger: RefCell::new(None),
        in_flight: Arc::new(AtomicBool::new(false)),
        base: Instant::now(),
        port: OnceLock::new(),
    }));

    let mask = (1u64 << CGEventType::KeyDown.0) | (1u64 << CGEventType::KeyUp.0);
    // Safety: the callback matches the tap contract, and `state` outlives the
    // tap (both are leaked for the process lifetime).
    let tap = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::HIDEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::ListenOnly,
            mask,
            Some(tap_callback),
            state as *const TapState as *mut c_void,
        )
    }
    .ok_or_else(|| {
        Error::ListenerInit(
            "could not create CGEventTap — grant Input Monitoring to the host app (during \
             development, that is your terminal)"
                .to_string(),
        )
    })?;

    let source = CFMachPort::new_run_loop_source(None, Some(&tap), 0).ok_or_else(|| {
        Error::ListenerInit("could not create run loop source for the tap".to_string())
    })?;

    // Remember the port (as a raw pointer value) so the callback can re-enable
    // the tap after a timeout; valid forever because the tap is leaked below.
    let _ = state.port.set(&*tap as *const CFMachPort as usize);

    let run_loop = CFRunLoop::current()
        .ok_or_else(|| Error::ListenerInit("could not get the current run loop".to_string()))?;
    // Safety: `kCFRunLoopCommonModes` is a CoreFoundation constant, always valid.
    run_loop.add_source(Some(&source), unsafe { kCFRunLoopCommonModes });
    CGEvent::tap_enable(&tap, true);

    // Keep the tap (and thus its callback) + source alive for the process lifetime.
    std::mem::forget(tap);
    std::mem::forget(source);
    Ok(())
}
