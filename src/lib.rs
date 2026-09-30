//! Translucent fullscreen colour overlays.
//!
//! An [`Overlay`] is a set of borderless, always-on-top, click-through windows
//! (one per monitor) filled with a semi-transparent [`Color`]. The system
//! compositor blends them over everything else, so every pixel becomes
//! `src × (1 − α) + colour × α` (see [`Color::blend_over`]). The windows follow
//! monitors being plugged in, unplugged, resized or rearranged, so the whole
//! desktop stays covered for as long as the overlay lives.
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
//! [`Overlay`] is `Send + Sync` on every platform: once created, it can be
//! moved to and updated from any thread.
//!
//! - **macOS**: AppKit requires [`Overlay::new`], [`set_background_app`] and
//!   [`run_until`] to be called on the main thread. Calls to
//!   [`Overlay::set_color`] from other threads are queued to the main thread,
//!   which must be pumping events for them to show: either an existing
//!   `NSApplication` loop, or [`run_until`]. [`hide_others`] and
//!   [`show_others`] work the same way. The overlay never changes the
//!   activation policy on its own, so whether your app has a Dock icon and
//!   menu bar is up to you: see [`set_background_app`], or set `LSUIElement`
//!   in your `Info.plist`.
//! - **Windows**: the overlay owns a background thread with its own message
//!   loop, so it needs no pumping by the caller.
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
    /// This platform has no backend for the feature.
    Unsupported,
    /// macOS: [`Overlay::new`] or [`set_background_app`] called off the main thread.
    NotMainThread,
    /// The OS refused to create or update the overlay.
    Os(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unsupported => write!(f, "not supported on this platform"),
            Error::NotMainThread => write!(f, "overlays must be created on the main thread"),
            Error::Os(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A live overlay covering every monitor. Dropping it removes the overlay.
///
/// Create it on the main thread; after that it can be used from any thread
/// (see [Threading](crate#threading)).
pub struct Overlay {
    inner: platform::Overlay,
    color: Color,
}

// Every backend must keep the handle thread-safe, so code that builds on one
// platform builds on the others.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Overlay>();
};

impl Overlay {
    pub fn new(options: OverlayOptions) -> Result<Self> {
        let color = options.color.clamped();
        let inner = platform::Overlay::new(&OverlayOptions { color, ..options })?;
        Ok(Overlay { inner, color })
    }

    /// The colour last passed to [`set_color`](Self::set_color) (or `new`),
    /// clamped. Updates from other threads may not be on screen yet.
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

/// Makes this process a background app (`true`) or a regular one (`false`).
///
/// On macOS a background app has no Dock icon, menu bar or Cmd-Tab entry, but
/// its windows, overlays included, still show. This is the `Accessory`
/// activation policy; `false` restores `Regular`. It applies to the whole
/// process, so it's the app's call, not the overlay's. A background tool
/// typically calls `set_background_app(true)` once, before creating an overlay.
///
/// Must be called on the main thread on macOS. Does nothing elsewhere.
pub fn set_background_app(background: bool) -> Result<()> {
    platform::set_background_app(background)
}

/// Which apps [`hide_others`] leaves alone. This process's own windows are
/// never hidden, whether or not they belong to an [`Overlay`].
#[derive(Debug, Clone)]
pub struct HideOthersOptions {
    /// Leave system UI and task managers alone (see [`exemptions`]). You
    /// almost certainly want this.
    pub builtin_exemptions: bool,
    /// More apps to leave alone, case-insensitive. What counts as an app
    /// depends on the platform:
    ///
    /// - **macOS**: a bundle identifier (`com.apple.Terminal`), or the
    ///   executable name for apps without one.
    /// - **Windows**: an executable file name (`WindowsTerminal.exe`).
    pub exempt: Vec<String>,
}

impl Default for HideOthersOptions {
    fn default() -> Self {
        HideOthersOptions { builtin_exemptions: true, exempt: Vec::new() }
    }
}

/// Apps that [`hide_others`] leaves alone when
/// [`HideOthersOptions::builtin_exemptions`] is set.
pub mod exemptions {
    /// Bundle identifiers. On top of these, any `com.apple.*` app that isn't a
    /// regular Dock app (menu bar extras, system agents) is exempt.
    pub const MACOS: &[&str] = &[
        "com.apple.dock",
        "com.apple.loginwindow",
        "com.apple.SecurityAgent",
        "com.apple.coreservices.uiagent",
        "com.apple.UserNotificationCenter",
        "com.apple.screencaptureui",
        "com.apple.ActivityMonitor",
    ];

    /// Executable names. Shell windows (taskbar, desktop, task view) are also
    /// exempt by window class, since `explorer.exe` owns them as well as
    /// ordinary File Explorer windows.
    pub const WINDOWS: &[&str] = &[
        "ShellExperienceHost.exe",
        "StartMenuExperienceHost.exe",
        "ShellHost.exe",
        "SearchHost.exe",
        "SearchApp.exe",
        "SearchUI.exe",
        "TextInputHost.exe",
        "LockApp.exe",
        "LogonUI.exe",
        "consent.exe",
        "CredentialUIBroker.exe",
        "ScreenClippingHost.exe",
        "SnippingTool.exe",
        "Taskmgr.exe",
    ];
}

/// Hides every other app's windows and keeps them hidden, re-hiding any that
/// reappear or launch, until [`show_others`]. Calling it again while active
/// just updates the exemptions.
///
/// - **macOS**: hides whole apps, as Cmd-H does (they stay in the Dock and
///   Cmd-Tab). Callable from any thread: off the main thread it's queued
///   there. Either way it only takes effect while the main thread pumps
///   events.
/// - **Windows**: minimizes top-level windows to the taskbar, from a
///   background thread. Windows of elevated processes can't be touched unless
///   this process is elevated too.
///
/// If the process dies without calling [`show_others`], the other windows
/// just stay hidden or minimized, which the user can undo as usual.
pub fn hide_others(options: &HideOthersOptions) -> Result<()> {
    platform::hide_others(options)
}

/// Stops [`hide_others`] and brings back the windows it hid. Windows the user
/// had hidden or minimized themselves stay that way. Does nothing if
/// [`hide_others`] isn't active. On macOS, off the main thread it's queued
/// there, like [`hide_others`].
pub fn show_others() -> Result<()> {
    platform::show_others()
}

/// Pumps platform events until `should_stop` returns true. It is polled
/// roughly every [`POLL_INTERVAL`], so it's a good place to animate colours.
pub fn run_until(should_stop: impl FnMut() -> bool) {
    platform::run_until(should_stop, POLL_INTERVAL)
}

pub const POLL_INTERVAL: Duration = Duration::from_millis(16);
