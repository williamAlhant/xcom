//! X11 display lifecycle, window capture and input injection.
//!
//! Implementation lands after the x11rb request signatures are pinned against
//! the vendored source; this stub keeps the workspace building.

pub mod capture;
pub mod clipboard;
pub mod input;
pub mod keymap;
pub mod windows;
pub mod xvfb;
