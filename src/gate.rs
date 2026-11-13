//! The single-in-flight gate: one capture runs at a time, and a trigger that
//! arrives while it does is dropped.
//!
//! The gate is claimed on the listener's thread and held by the capture's
//! worker thread, and it has to come back however the capture ends. A flag
//! cleared only after the capture returned would stay set for good once a
//! capture panicked — in the read path, or in the host's handler — and every
//! later trigger would be dropped without a word.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

/// Whether a capture is in flight.
#[derive(Default)]
pub(crate) struct CaptureGate(Arc<AtomicBool>);

impl CaptureGate {
    /// Claims the gate for one capture; `None` while another is in flight.
    pub(crate) fn claim(&self) -> Option<InFlight> {
        if self.0.swap(true, Ordering::AcqRel) {
            return None;
        }
        Some(InFlight(Arc::clone(&self.0)))
    }
}

/// The claimed gate. Dropping it gives the gate back, which also happens
/// while the thread holding it unwinds from a panic.
pub(crate) struct InFlight(Arc<AtomicBool>);

impl InFlight {
    /// Runs `capture` on a worker thread that holds the gate until the
    /// capture ends, by returning or by panicking.
    pub(crate) fn run(self, capture: impl FnOnce() + Send + 'static) -> JoinHandle<()> {
        thread::spawn(move || {
            let _in_flight = self;
            capture();
        })
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    fn claimed(gate: &CaptureGate) -> InFlight {
        gate.claim().expect("a free gate can be claimed")
    }

    #[test]
    fn one_capture_at_a_time() {
        let gate = CaptureGate::default();
        let first = claimed(&gate);
        assert!(gate.claim().is_none());
        drop(first);
        assert!(gate.claim().is_some());
    }

    #[test]
    fn the_gate_is_held_while_the_capture_runs() {
        let gate = CaptureGate::default();
        let (finish, finished) = mpsc::channel::<()>();
        let worker = claimed(&gate).run(move || {
            let _ = finished.recv();
        });
        assert!(gate.claim().is_none());
        finish.send(()).expect("the capture is waiting");
        worker.join().expect("the capture ran to its end");
        assert!(gate.claim().is_some());
    }

    #[test]
    fn the_gate_comes_back_when_the_capture_panics() {
        let gate = CaptureGate::default();
        let worker = claimed(&gate).run(|| {
            // Unwinds the thread as a panicking handler does, without the
            // panic hook's report in the test output.
            std::panic::resume_unwind(Box::new("the handler panicked"));
        });
        assert!(worker.join().is_err());
        assert!(gate.claim().is_some());
    }
}
