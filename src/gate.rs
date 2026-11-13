//! The capture gate of the key listeners: a trigger fires a capture unless
//! the last one fired too recently (the cooldown) or is still in flight, in
//! which case the trigger is dropped.
//!
//! The gate is claimed on the listener's thread and held by the capture's
//! worker thread, and it has to come back however the capture ends. A flag
//! cleared only after the capture returned would stay set for good once a
//! capture panicked — in the read path, or in the host's handler — and every
//! later trigger would be dropped without a word.

use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Whether a trigger may fire a capture now.
pub(crate) struct CaptureGate {
    cooldown: Duration,
    /// When the gate was last claimed. Only the listener's thread claims, so
    /// a `Cell` will do.
    last_claim: Cell<Option<Instant>>,
    in_flight: Arc<AtomicBool>,
}

impl CaptureGate {
    /// A gate that stays shut for `cooldown` after every capture it lets
    /// through.
    pub(crate) fn new(cooldown: Duration) -> Self {
        Self {
            cooldown,
            last_claim: Cell::new(None),
            in_flight: Arc::default(),
        }
    }

    /// Claims the gate for one capture; `None` during the cooldown and while
    /// another capture is in flight. Only a claim that succeeds starts the
    /// cooldown again.
    pub(crate) fn claim(&self) -> Option<InFlight> {
        if self
            .last_claim
            .get()
            .is_some_and(|at| at.elapsed() < self.cooldown)
        {
            return None;
        }
        if self.in_flight.swap(true, Ordering::AcqRel) {
            return None;
        }
        self.last_claim.set(Some(Instant::now()));
        Some(InFlight(Arc::clone(&self.in_flight)))
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

    /// A gate without a cooldown: only a capture in flight shuts it.
    fn gate() -> CaptureGate {
        CaptureGate::new(Duration::ZERO)
    }

    fn claimed(gate: &CaptureGate) -> InFlight {
        gate.claim().expect("an open gate can be claimed")
    }

    #[test]
    fn one_capture_at_a_time() {
        let gate = gate();
        let first = claimed(&gate);
        assert!(gate.claim().is_none());
        drop(first);
        assert!(gate.claim().is_some());
    }

    #[test]
    fn the_gate_is_held_while_the_capture_runs() {
        let gate = gate();
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
        let gate = gate();
        let worker = claimed(&gate).run(|| {
            // Unwinds the thread as a panicking handler does, without the
            // panic hook's report in the test output.
            std::panic::resume_unwind(Box::new("the handler panicked"));
        });
        assert!(worker.join().is_err());
        assert!(gate.claim().is_some());
    }

    #[test]
    fn the_gate_stays_shut_for_the_cooldown() {
        let gate = CaptureGate::new(Duration::from_secs(3600));
        drop(claimed(&gate));
        // The capture is over, the cooldown is not.
        assert!(gate.claim().is_none());
    }

    #[test]
    fn a_dropped_trigger_does_not_start_the_cooldown_again() {
        let cooldown = Duration::from_millis(20);
        let gate = CaptureGate::new(cooldown);
        let first = claimed(&gate);
        // The cooldown passes while the capture is still in flight, so the
        // next trigger is dropped for that reason alone.
        thread::sleep(cooldown);
        assert!(gate.claim().is_none());
        // Had that dropped trigger counted as a claim, the cooldown would be
        // running again now.
        drop(first);
        assert!(gate.claim().is_some());
    }
}
