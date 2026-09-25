//! macOS backend: one borderless, click-through NSWindow per screen, at a
//! level above the menu bar and Dock, on every Space and over fullscreen apps.
//!
//! AppKit is main-thread only, so everything here requires a
//! [`MainThreadMarker`]. Windows render only while the main thread pumps
//! events, via [`run_until`] or an existing `NSApplication` run loop.
//!
//! Process-wide state such as the activation policy (Dock icon, menu bar) is
//! changed only when the caller asks, via [`set_background_app`].

use std::time::Duration;

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSColor, NSEventMask,
    NSScreen, NSWindow, NSWindowCollectionBehavior, NSWindowSharingType, NSWindowStyleMask,
};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode};

use crate::{Color, Error, OverlayOptions, Result};

/// `kCGScreenSaverWindowLevel`: above the menu bar, Dock and pop-up menus.
/// (AppKit exposes it only as a macro, so it isn't in the bindings.)
const OVERLAY_WINDOW_LEVEL: isize = 1000;

pub struct Overlay {
    windows: Vec<Retained<NSWindow>>,
}

impl Overlay {
    pub fn new(options: &OverlayOptions) -> Result<Self> {
        let mtm = MainThreadMarker::new().ok_or(Error::NotMainThread)?;

        // Deliberately leaves the activation policy alone: whether the process
        // is a background app is the host app's decision, not the overlay's.
        let background = ns_color(options.color);
        let windows = NSScreen::screens(mtm)
            .iter()
            .map(|screen| {
                let window = unsafe {
                    NSWindow::initWithContentRect_styleMask_backing_defer(
                        NSWindow::alloc(mtm),
                        screen.frame(),
                        NSWindowStyleMask::Borderless,
                        NSBackingStoreType::Buffered,
                        false,
                    )
                };
                // We own it through `Retained`; don't let -close free it too.
                unsafe { window.setReleasedWhenClosed(false) };
                window.setOpaque(false);
                window.setHasShadow(false);
                window.setIgnoresMouseEvents(true);
                window.setBackgroundColor(Some(&background));
                window.setLevel(OVERLAY_WINDOW_LEVEL);
                window.setCollectionBehavior(
                    NSWindowCollectionBehavior::CanJoinAllSpaces
                        | NSWindowCollectionBehavior::Stationary
                        | NSWindowCollectionBehavior::FullScreenAuxiliary
                        | NSWindowCollectionBehavior::IgnoresCycle,
                );
                if options.exclude_from_capture {
                    // Honoured by CGWindowList captures; ScreenCaptureKit on
                    // macOS 15+ may still include the window. Needs testing.
                    window.setSharingType(NSWindowSharingType::None);
                }
                window.orderFrontRegardless();
                window
            })
            .collect::<Vec<_>>();

        if windows.is_empty() {
            return Err(Error::Os("no screens found".into()));
        }
        Ok(Overlay { windows })
    }

    pub fn set_color(&mut self, color: Color) -> Result<()> {
        let background = ns_color(color);
        for window in &self.windows {
            window.setBackgroundColor(Some(&background));
        }
        Ok(())
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        for window in &self.windows {
            window.orderOut(None);
            window.close();
        }
    }
}

pub fn set_background_app(background: bool) -> Result<()> {
    let mtm = MainThreadMarker::new().ok_or(Error::NotMainThread)?;
    let policy = if background {
        // No Dock icon, menu bar or Cmd-Tab entry, but windows still show.
        NSApplicationActivationPolicy::Accessory
    } else {
        NSApplicationActivationPolicy::Regular
    };
    if NSApplication::sharedApplication(mtm).setActivationPolicy(policy) {
        Ok(())
    } else {
        Err(Error::Os("couldn't change the activation policy".into()))
    }
}

pub fn run_until(mut should_stop: impl FnMut() -> bool, interval: Duration) {
    let Some(mtm) = MainThreadMarker::new() else {
        // Off the main thread we can't pump AppKit; assume someone else does.
        while !should_stop() {
            std::thread::sleep(interval);
        }
        return;
    };

    let app = NSApplication::sharedApplication(mtm);
    app.finishLaunching();

    while !should_stop() {
        let until = NSDate::dateWithTimeIntervalSinceNow(interval.as_secs_f64());
        // Drain everything that's queued, then wait up to `interval` for more.
        let mut deadline = Some(until);
        while let Some(event) = unsafe {
            app.nextEventMatchingMask_untilDate_inMode_dequeue(
                NSEventMask::Any,
                deadline.as_deref(),
                NSDefaultRunLoopMode,
                true,
            )
        } {
            app.sendEvent(&event);
            deadline = None;
        }
        app.updateWindows();
    }
}

fn ns_color(color: Color) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(
        color.r as f64,
        color.g as f64,
        color.b as f64,
        color.a as f64,
    )
}
