//! Translucent fullscreen colour overlays.
//!
//! An [`Overlay`] is a set of borderless, always-on-top, click-through windows
//! (one per monitor) filled with a semi-transparent [`Color`]. The system
//! compositor blends them over everything else, so every pixel becomes
//! `src × (1 − α) + colour × α` (see [`Color::blend_over`]).
//!
//! ```no_run
//! use specialfx::{Color, Overlay, OverlayOptions};
//!
//! let overlay = Overlay::new(OverlayOptions {
//!     color: Color::rgba(1.0, 0.55, 0.1, 0.3),
//!     ..Default::default()
//! })?;
//! // On macOS the overlay only renders while events are pumped on the main thread.
//! specialfx::run_until(|| false);
//! # Ok::<(), specialfx::Error>(())
//! ```
//!
//! ## Threading
//!
//! - **macOS**: AppKit requires [`Overlay::new`] and [`run_until`] to be called
//!   on the main thread. If your app already runs an `NSApplication` loop you
//!   don't need [`run_until`].
//! - **Windows**: the overlay owns a background thread with its own message
//!   loop, so it works from any thread and needs no pumping by the caller.
//!   [`run_until`] just sleeps.

mod color;
mod platform;

use std::fmt;
use std::time::Duration;

pub use color::{Color, ParseColorError};

/// Built-in overlay colours.
pub mod presets {
    use super::Color;

    /// Warm orange tint that cuts blue light. Lifts blacks a little.
    pub const NIGHT: Color = Color::rgba(1.0, 0.55, 0.1, 0.30);
    /// Deeper red tint for late at night.
    pub const RED: Color = Color::rgba(1.0, 0.1, 0.0, 0.35);
    /// Plain dimming: black at 50%. The only overlay that doesn't lift blacks.
    pub const DIM: Color = Color::rgba(0.0, 0.0, 0.0, 0.50);

    pub fn by_name(name: &str) -> Option<Color> {
        match name.to_ascii_lowercase().as_str() {
            "night" => Some(NIGHT),
            "red" => Some(RED),
            "dim" => Some(DIM),
            _ => None,
        }
    }

    pub const NAMES: &[&str] = &["night", "red", "dim"];
}

#[derive(Debug, Clone)]
pub struct OverlayOptions {
    pub color: Color,
    /// Hide the overlay from screenshots and screen recordings, so captures
    /// show the real screen contents. Windows 10 2004+ / macOS; best-effort.
    pub exclude_from_capture: bool,
}

impl Default for OverlayOptions {
    fn default() -> Self {
        OverlayOptions { color: presets::NIGHT, exclude_from_capture: true }
    }
}

#[derive(Debug)]
pub enum Error {
    /// This platform has no overlay backend.
    Unsupported,
    /// macOS: called off the main thread.
    NotMainThread,
    /// The OS refused to create or update the overlay.
    Os(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unsupported => write!(f, "colour overlays are not supported on this platform"),
            Error::NotMainThread => write!(f, "overlays must be created on the main thread"),
            Error::Os(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A live overlay covering every monitor. Dropping it removes the overlay.
pub struct Overlay {
    inner: platform::Overlay,
    color: Color,
}

impl Overlay {
    pub fn new(options: OverlayOptions) -> Result<Self> {
        let color = options.color.clamped();
        let inner = platform::Overlay::new(&OverlayOptions { color, ..options })?;
        Ok(Overlay { inner, color })
    }

    pub fn color(&self) -> Color {
        self.color
    }

    pub fn set_color(&mut self, color: Color) -> Result<()> {
        let color = color.clamped();
        self.inner.set_color(color)?;
        self.color = color;
        Ok(())
    }
}

/// Pumps platform events until `should_stop` returns true. It is polled
/// roughly every [`POLL_INTERVAL`], so it's a good place to animate colours.
pub fn run_until(should_stop: impl FnMut() -> bool) {
    platform::run_until(should_stop, POLL_INTERVAL)
}

pub const POLL_INTERVAL: Duration = Duration::from_millis(16);
