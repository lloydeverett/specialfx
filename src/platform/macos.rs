//! macOS backend: one borderless, click-through NSWindow per screen, at a
//! level above the menu bar and Dock, on every Space and over fullscreen apps.
//!
//! AppKit is main-thread only, so everything here requires a
//! [`MainThreadMarker`]. Windows render only while the main thread pumps
//! events, via [`run_until`] or an existing `NSApplication` run loop.
//!
//! Process-wide state such as the activation policy (Dock icon, menu bar) is
//! changed only when the caller asks, via [`set_background_app`].
//!
//! [`hide_others`] uses `-[NSRunningApplication hide]`, the public equivalent
//! of Cmd-H, and re-hides apps on workspace notifications and a timer.

use std::cell::RefCell;
use std::collections::HashSet;
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSColor, NSEventMask,
    NSRunningApplication, NSScreen, NSWindow, NSWindowCollectionBehavior, NSWindowSharingType,
    NSWindowStyleMask, NSWorkspace, NSWorkspaceDidActivateApplicationNotification,
    NSWorkspaceDidLaunchApplicationNotification, NSWorkspaceDidUnhideApplicationNotification,
};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSTimer};

use crate::{exemptions, Color, Error, HideOthersOptions, OverlayOptions, Result};

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

// ---- hiding other apps ------------------------------------------------------

/// Backstop for anything the workspace notifications miss, e.g. an app that
/// ignored a hide request because it was still launching.
const HIDE_SWEEP_INTERVAL: f64 = 0.5;

struct Hider {
    exempt: Vec<String>,
    builtin_exemptions: bool,
    /// Apps we hid, so `show_others` restores only those.
    hidden: HashSet<i32>,
    timer: Retained<NSTimer>,
    observers: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

thread_local! {
    // AppKit is main-thread only, so this only ever lives on the main thread.
    static HIDER: RefCell<Option<Hider>> = const { RefCell::new(None) };
}

pub fn hide_others(options: &HideOthersOptions) -> Result<()> {
    MainThreadMarker::new().ok_or(Error::NotMainThread)?;
    let exempt = options.exempt.clone();
    let updated = HIDER.with(|h| match h.borrow_mut().as_mut() {
        Some(hider) => {
            hider.exempt = exempt.clone();
            hider.builtin_exemptions = options.builtin_exemptions;
            true
        }
        None => false,
    });
    if !updated {
        let sweep_block = RcBlock::new(|_| sweep());
        // Safety: scheduled on this (main) thread's run loop, so it fires here.
        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_repeats_block(HIDE_SWEEP_INTERVAL, true, &sweep_block)
        };
        let center = NSWorkspace::sharedWorkspace().notificationCenter();
        let notify_block = RcBlock::new(|_| sweep());
        // Safety: NSWorkspace posts these on the main thread, and with no
        // queue the block runs on the posting thread.
        let observers = unsafe {
            [
                NSWorkspaceDidLaunchApplicationNotification,
                NSWorkspaceDidActivateApplicationNotification,
                NSWorkspaceDidUnhideApplicationNotification,
            ]
            .into_iter()
            .map(|name| center.addObserverForName_object_queue_usingBlock(Some(name), None, None, &notify_block))
            .collect()
        };
        let hider = Hider {
            exempt,
            builtin_exemptions: options.builtin_exemptions,
            hidden: HashSet::new(),
            timer,
            observers,
        };
        HIDER.with(|h| *h.borrow_mut() = Some(hider));
    }
    sweep();
    Ok(())
}

pub fn show_others() -> Result<()> {
    MainThreadMarker::new().ok_or(Error::NotMainThread)?;
    let Some(hider) = HIDER.with(|h| h.borrow_mut().take()) else {
        return Ok(());
    };
    hider.timer.invalidate();
    let center = NSWorkspace::sharedWorkspace().notificationCenter();
    for observer in &hider.observers {
        // Safety: these are the tokens addObserverForName returned.
        unsafe { center.removeObserver(observer.as_ref()) };
    }
    for app in NSWorkspace::sharedWorkspace().runningApplications() {
        if hider.hidden.contains(&app.processIdentifier()) && app.isHidden() && !app.isTerminated() {
            app.unhide();
        }
    }
    Ok(())
}

/// Hides every visible app that isn't us or exempt.
fn sweep() {
    HIDER.with(|h| {
        // Skip rather than panic if a hide somehow re-entered us.
        let Ok(mut guard) = h.try_borrow_mut() else { return };
        let Some(hider) = guard.as_mut() else { return };
        let own_pid = NSRunningApplication::currentApplication().processIdentifier();
        let apps = NSWorkspace::sharedWorkspace().runningApplications();
        // Forget apps that have quit, so a reused pid isn't unhidden later.
        hider.hidden.retain(|pid| apps.iter().any(|app| app.processIdentifier() == *pid));
        for app in apps {
            let pid = app.processIdentifier();
            if pid == own_pid
                || app.isHidden()
                || app.isTerminated()
                || app.activationPolicy() == NSApplicationActivationPolicy::Prohibited
                || is_exempt(hider, &app)
            {
                continue;
            }
            if app.hide() {
                hider.hidden.insert(pid);
            }
        }
    });
}

fn is_exempt(hider: &Hider, app: &NSRunningApplication) -> bool {
    let bundle_id = app.bundleIdentifier().map(|id| id.to_string());
    let exe = app
        .executableURL()
        .and_then(|url| url.lastPathComponent())
        .map(|name| name.to_string());
    let matches = |name: &str| {
        bundle_id.as_deref().is_some_and(|id| id.eq_ignore_ascii_case(name))
            || exe.as_deref().is_some_and(|exe| exe.eq_ignore_ascii_case(name))
    };
    if hider.exempt.iter().any(|name| matches(name)) {
        return true;
    }
    hider.builtin_exemptions
        && (exemptions::MACOS.iter().any(|name| matches(name))
            // Menu bar extras and system agents: Control Center, Spotlight, etc.
            || (app.activationPolicy() != NSApplicationActivationPolicy::Regular
                && bundle_id.as_deref().is_some_and(|id| id.starts_with("com.apple."))))
}

fn ns_color(color: Color) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(
        color.r as f64,
        color.g as f64,
        color.b as f64,
        color.a as f64,
    )
}
