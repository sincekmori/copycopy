//! macOS global key listener: a passive `CGEventTap` on the main run loop.
//! rdev can't be used (its callback hits main-thread-only Carbon APIs and SIGSEGVs
//! off the main thread), so we install our own ListenOnly tap on the host's main
//! `CFRunLoop` and decode the virtual key code ourselves.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use core_foundation::base::TCFType;
use core_foundation::mach_port::{CFMachPort, CFMachPortRef};
use core_foundation::runloop::CFRunLoop;
use core_foundation::string::CFStringRef;
use core_graphics::event::{
    CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType,
};

use crate::capture::{capture_macos, clipboard_change_count};
use crate::config::Config;
use crate::detector::DoubleTap;
use crate::{CaptureHandler, Error};

const KEY_C: i64 = 8; // kVK_ANSI_C
const FIELD_KEYBOARD_AUTOREPEAT: u32 = 8; // kCGKeyboardEventAutorepeat
const FIELD_KEYBOARD_KEYCODE: u32 = 9; // kCGKeyboardEventKeycode

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
}
// core-foundation 0.10 doesn't re-export this run-loop mode, so bind it directly.
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFRunLoopCommonModes: CFStringRef;
}

fn fire_if_allowed(
    config: &Config,
    handler: &CaptureHandler,
    in_flight: &Arc<AtomicBool>,
    last_trigger: &RefCell<Option<Instant>>,
) {
    let cooled = last_trigger
        .borrow()
        .map(|t| t.elapsed() >= config.trigger_cooldown)
        .unwrap_or(true);
    if !cooled {
        return;
    }
    if in_flight.swap(true, Ordering::AcqRel) {
        return;
    }
    *last_trigger.borrow_mut() = Some(Instant::now());

    // Keep the tap callback fast: snapshot only the cheap change counter here.
    let baseline = clipboard_change_count();
    let config = config.clone();
    let handler = handler.clone();
    let flag = in_flight.clone();
    thread::spawn(move || {
        capture_macos(config, handler, baseline);
        flag.store(false, Ordering::Release);
    });
}

/// Install the passive event tap on the current (main) run loop. MUST be called
/// on the main thread (e.g. from the host's startup hook / `main`).
pub fn install(config: Config, handler: CaptureHandler) -> Result<(), Error> {
    let detector = RefCell::new(DoubleTap::new(config.double_tap_window.as_millis() as u64));
    let last_trigger: RefCell<Option<Instant>> = RefCell::new(None);
    let in_flight = Arc::new(AtomicBool::new(false));
    let base = Instant::now();
    let port_holder: Rc<RefCell<Option<CFMachPort>>> = Rc::new(RefCell::new(None));
    let port_cb = port_holder.clone();

    let tap = CGEventTap::new(
        CGEventTapLocation::HID,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::ListenOnly,
        vec![CGEventType::KeyDown, CGEventType::KeyUp],
        move |_proxy, event_type, event| {
            // Never let a panic unwind across the C/Objective-C boundary (UB).
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                match event_type {
                    CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
                        if let Some(port) = port_cb.borrow().as_ref() {
                            unsafe { CGEventTapEnable(port.as_concrete_TypeRef(), true) };
                        }
                        return None;
                    }
                    CGEventType::KeyDown | CGEventType::KeyUp => {}
                    _ => return None,
                }

                let keycode = event.get_integer_value_field(FIELD_KEYBOARD_KEYCODE);
                if keycode != KEY_C {
                    return None;
                }
                let is_down = matches!(event_type, CGEventType::KeyDown);
                let cmd = event.get_flags().contains(CGEventFlags::CGEventFlagCommand);
                let autorepeat = event.get_integer_value_field(FIELD_KEYBOARD_AUTOREPEAT) != 0;

                if is_down {
                    let now_ms = base.elapsed().as_millis() as u64;
                    let triggered = {
                        let mut det = detector.borrow_mut();
                        det.set_modifier(cmd);
                        det.on_c_down(now_ms, autorepeat)
                    };
                    if triggered {
                        fire_if_allowed(&config, &handler, &in_flight, &last_trigger);
                    }
                } else {
                    detector.borrow_mut().on_c_up();
                }

                None
            }))
            .unwrap_or(None)
        },
    );

    let tap = tap.map_err(|_| {
        Error::ListenerInit(
            "could not create CGEventTap — grant Input Monitoring (and Accessibility) to the \
             host app (during development, that is your terminal)"
                .to_string(),
        )
    })?;

    *port_holder.borrow_mut() = Some(tap.mach_port.clone());

    let source = tap.mach_port.create_runloop_source(0).map_err(|_| {
        Error::ListenerInit("could not create run loop source for the tap".to_string())
    })?;

    let run_loop = CFRunLoop::get_current();
    unsafe { run_loop.add_source(&source, kCFRunLoopCommonModes) };
    tap.enable();

    // Keep the tap (and thus its callback) + source alive for the process lifetime.
    std::mem::forget(tap);
    std::mem::forget(source);
    Ok(())
}
