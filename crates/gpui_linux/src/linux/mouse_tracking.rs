//! Linux pointer tracking setup.

/// Prepare mouse-move delivery for a new window.
///
/// TODO: Add backend-specific setup if a Linux backend needs explicit pointer tracking.
/// X11 and Wayland currently deliver pointer motion through their native event streams.
pub(crate) fn prepare_window() {}
