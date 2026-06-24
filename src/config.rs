//! Tunables for the capture trigger.

use std::time::Duration;

/// Configuration for [`crate::start`]. Use [`Config::default`] for sensible values.
#[derive(Debug, Clone)]
pub struct Config {
    /// Maximum time between the first and second `C` press to count as a "double C".
    pub double_tap_window: Duration,
    /// After a trigger, poll up to this long for the clipboard change counter to
    /// advance (= a fresh copy landed) before reading.
    pub clipboard_change_timeout: Duration,
    /// Interval between clipboard change-counter polls.
    pub clipboard_poll_step: Duration,
    /// After a trigger fires, ignore further triggers for this long.
    pub trigger_cooldown: Duration,
    /// Foreground apps to skip capturing, matched case-insensitively as a substring
    /// of the executable name or full path (privacy — e.g. password managers).
    pub denylist_exec_substrings: Vec<String>,
    /// When the clipboard holds file references, the maximum number of paths kept.
    pub max_files: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            double_tap_window: Duration::from_millis(400),
            clipboard_change_timeout: Duration::from_millis(400),
            clipboard_poll_step: Duration::from_millis(20),
            trigger_cooldown: Duration::from_millis(350),
            denylist_exec_substrings: Vec::new(),
            max_files: 50,
        }
    }
}
