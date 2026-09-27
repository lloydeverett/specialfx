//! macOS backend: one borderless, click-through NSWindow per screen, at a
//! level above the menu bar and Dock, on every Space and over fullscreen apps.
//!
//! AppKit is main-thread only. [`Overlay::new`] must be called there, but the
//! handle it returns is `Send + Sync`: off the main thread, `set_color` and
//! drop hand their work to the main dispatch queue. Windows render (and
//! queued work runs) only while the main thread pumps events, via
//! [`run_until`] or an existing `NSApplication` run loop.
//!
//! Process-wide state such as the activation policy (Dock icon, menu bar) is
//! changed only when the caller asks, via [`set_background_app`].

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dispatch2::{DispatchQueue, MainThreadBound};
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
    // Taken by `Drop`, so it can hand the last reference to the main thread.
    windows: ManuallyDrop<Arc<OverlayWindows>>,
}

/// The windows, and the colour they should show. Every reference to this is
/// released on the main thread, which is where the windows must be freed.
struct OverlayWindows {
    windows: MainThreadBound<Vec<Retained<NSWindow>>>,
    /// The latest colour asked for. A repaint reads this when it runs rather
    /// than carrying a colour, so a queued repaint can't show a stale one.
    color: Mutex<Color>,
    /// A repaint is queued on the main thread and hasn't read `color` yet.
    repaint_queued: AtomicBool,
}

impl OverlayWindows {
    /// Shows the latest colour: now on the main thread, or else via the main
    /// queue. At most one repaint is queued at a time, so a burst of calls
    /// costs one repaint and a stalled main thread doesn't build a backlog.
    fn request_repaint(self: &Arc<Self>) {
        if let Some(mtm) = MainThreadMarker::new() {
            self.repaint(mtm);
        } else if !self.repaint_queued.swap(true, Ordering::AcqRel) {
            Arc::clone(self).queue_on_main(|this, mtm| {
                // Clear the flag before reading `color`, so a colour set after
                // the read queues another repaint. (A swap, not a store, so it
                // also sees the colour set by whoever queued this one.)
                this.repaint_queued.swap(false, Ordering::AcqRel);
                this.repaint(mtm);
            });
        }
    }

    /// Hides and closes the windows: now on the main thread, or else via the
    /// main queue.
    fn close(self: Arc<Self>) {
        match MainThreadMarker::new() {
            Some(mtm) => self.close_now(mtm),
            None => self.queue_on_main(Self::close_now),
        }
    }

    fn repaint(&self, mtm: MainThreadMarker) {
        let background = ns_color(*self.color.lock().unwrap());
        for window in self.windows.get(mtm) {
            window.setBackgroundColor(Some(&background));
        }
    }

    fn close_now(&self, mtm: MainThreadMarker) {
        for window in self.windows.get(mtm) {
            window.orderOut(None);
            window.close();
        }
    }

    /// Runs `f` on the main thread. The work takes `self` with it, so this
    /// reference is released there too.
    fn queue_on_main(self: Arc<Self>, f: fn(&Self, MainThreadMarker)) {
        DispatchQueue::main().exec_async(move || {
            // The main queue only ever runs on the main thread.
            let mtm = MainThreadMarker::new().unwrap();
            f(&self, mtm);
        });
    }
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
        let windows = OverlayWindows {
            windows: MainThreadBound::new(windows, mtm),
            color: Mutex::new(options.color),
            repaint_queued: AtomicBool::new(false),
        };
        Ok(Overlay { windows: ManuallyDrop::new(Arc::new(windows)) })
    }

    pub fn set_color(&mut self, color: Color) -> Result<()> {
        *self.windows.color.lock().unwrap() = color;
        self.windows.request_repaint();
        Ok(())
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        // SAFETY: `self.windows` is never used again.
        let windows = unsafe { ManuallyDrop::take(&mut self.windows) };
        windows.close();
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
