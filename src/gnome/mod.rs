//! GNOME Wayland backend.
//!
//! GNOME's Wayland compositor (Mutter) exposes no unprivileged API for global
//! key listening or background clipboard reads, and does not implement the
//! wlr/ext data-control protocols. The only unprivileged vantage point is
//! inside the compositor itself, so this backend ships a small GNOME Shell
//! extension (embedded in the crate, installed into the user's home — no
//! sudo) that does the detection and clipboard reading in-process, and hands
//! each capture to us over the session bus:
//!
//! - [`installer`] — session detection plus writing/enabling the extension.
//! - [`listener`] — zbus subscriber that turns the extension's captures into
//!   [`crate::CaptureEvent`]s.
//!
//! The gesture here is "two explicit copies within the double-tap window"
//! (key events are unobservable), which is what Ctrl+C+C produces. A single
//! copy never fires — the crate's core philosophy is preserved.

pub(crate) mod installer;
pub(crate) mod listener;
