//! OS-independent "modifier held + C tapped twice quickly" detector.
//! Pure logic (time injected as monotonic ms), unit-testable directly.

#[derive(Clone, Copy, PartialEq, Debug)]
enum CState {
    Idle,
    /// First C is down (waiting for its release). We stay here through OS
    /// auto-repeat key-down events, which is how auto-repeat is told apart
    /// from a genuine second tap.
    FirstDown,
    FirstReleased,
}

/// Detects `Press → Release → Press` of `C` within `window_ms`, while the trigger
/// modifier is held.
pub struct DoubleTap {
    window_ms: u64,
    modifier_down: bool,
    c_state: CState,
    first_press_at_ms: Option<u64>,
}

impl DoubleTap {
    pub fn new(window_ms: u64) -> Self {
        Self {
            window_ms,
            modifier_down: false,
            c_state: CState::Idle,
            first_press_at_ms: None,
        }
    }

    fn reset(&mut self) {
        self.c_state = CState::Idle;
        self.first_press_at_ms = None;
    }

    /// Update whether the trigger modifier is held. Releasing it resets the
    /// in-progress sequence. Windows/Linux call this from Ctrl key events;
    /// macOS calls it per `C` event from the event's Command flag.
    pub fn set_modifier(&mut self, down: bool) {
        self.modifier_down = down;
        if !down {
            self.reset();
        }
    }

    /// Feed a `C` key-down. Returns `true` exactly when a double-tap fires.
    pub fn on_c_down(&mut self, now_ms: u64, autorepeat: bool) -> bool {
        if !self.modifier_down {
            self.reset();
            return false;
        }
        if autorepeat {
            return false;
        }
        match self.c_state {
            CState::Idle => {
                self.c_state = CState::FirstDown;
                self.first_press_at_ms = Some(now_ms);
                false
            }
            CState::FirstDown => false, // press w/o release ⇒ auto-repeat: ignore
            CState::FirstReleased => {
                let within = self
                    .first_press_at_ms
                    .map(|t| now_ms.saturating_sub(t) <= self.window_ms)
                    .unwrap_or(false);
                if within {
                    self.reset();
                    true
                } else {
                    self.c_state = CState::FirstDown;
                    self.first_press_at_ms = Some(now_ms);
                    false
                }
            }
        }
    }

    /// Feed a `C` key-up.
    pub fn on_c_up(&mut self) {
        if self.c_state == CState::FirstDown {
            self.c_state = CState::FirstReleased;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const W: u64 = 400;
    fn armed() -> DoubleTap {
        let mut d = DoubleTap::new(W);
        d.set_modifier(true);
        d
    }

    #[test]
    fn double_tap_within_window_triggers() {
        let mut d = armed();
        assert!(!d.on_c_down(0, false));
        d.on_c_up();
        assert!(d.on_c_down(100, false));
    }

    #[test]
    fn single_tap_does_not_trigger() {
        let mut d = armed();
        assert!(!d.on_c_down(0, false));
        d.on_c_up();
    }

    #[test]
    fn repeated_down_without_release_does_not_trigger() {
        let mut d = armed();
        assert!(!d.on_c_down(0, false));
        assert!(!d.on_c_down(10, false));
        assert!(!d.on_c_down(20, false));
    }

    #[test]
    fn autorepeat_flag_is_ignored() {
        let mut d = armed();
        assert!(!d.on_c_down(0, false));
        d.on_c_up();
        assert!(!d.on_c_down(50, true));
    }

    #[test]
    fn second_tap_too_slow_does_not_trigger_but_rearms() {
        let mut d = armed();
        assert!(!d.on_c_down(0, false));
        d.on_c_up();
        assert!(!d.on_c_down(500, false));
        d.on_c_up();
        assert!(d.on_c_down(600, false));
    }

    #[test]
    fn plain_c_without_modifier_does_not_trigger() {
        let mut d = DoubleTap::new(W);
        assert!(!d.on_c_down(0, false));
        d.on_c_up();
        assert!(!d.on_c_down(50, false));
    }

    #[test]
    fn releasing_modifier_resets_the_sequence() {
        let mut d = armed();
        assert!(!d.on_c_down(0, false));
        d.on_c_up();
        d.set_modifier(false);
        d.set_modifier(true);
        assert!(!d.on_c_down(50, false));
    }

    #[test]
    fn window_boundary_is_inclusive() {
        let mut d = armed();
        assert!(!d.on_c_down(0, false));
        d.on_c_up();
        assert!(d.on_c_down(W, false));
    }

    #[test]
    fn just_outside_window_does_not_trigger() {
        let mut d = armed();
        assert!(!d.on_c_down(0, false));
        d.on_c_up();
        assert!(!d.on_c_down(W + 1, false));
    }

    #[test]
    fn triggers_again_after_a_trigger() {
        let mut d = armed();
        assert!(!d.on_c_down(0, false));
        d.on_c_up();
        assert!(d.on_c_down(100, false));
        assert!(!d.on_c_down(200, false));
        d.on_c_up();
        assert!(d.on_c_down(250, false));
    }
}
