//! macOS backend: one borderless, click-through NSWindow per screen, at a
//! level above the menu bar and Dock, on every Space and over fullscreen apps.
//! When screens are added, removed or rearranged, the windows are moved,
//! created or closed to match.
//!
//! AppKit is main-thread only, but everything here can be called from any
//! thread: off the main thread, [`Overlay::new`], `set_color`, drop and
//! [`set_background_app`] hand their work to the main dispatch queue. Windows
//! render (and queued work and screen changes are handled) only while the
//! main thread pumps events, via [`run_until`] or an existing `NSApplication`
//! run loop.
//!
//! Process-wide state such as the activation policy (Dock icon, menu bar) is
//! changed only when the caller asks, via [`set_background_app`].
//!
//! [`hide_others`] uses `-[NSRunningApplication hide]`, the public equivalent
//! of Cmd-H, and re-hides apps on workspace notifications and a timer. Like
//! `set_color`, it and [`show_others`] hand their work to the main dispatch
//! queue when called off the main thread.

use std::cell::RefCell;
use std::collections::HashSet;
use std::mem::ManuallyDrop;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::{DispatchQueue, MainThreadBound};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject};
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDidChangeScreenParametersNotification,
    NSBackingStoreType, NSColor, NSEventMask, NSRunningApplication, NSScreen, NSWindow,
    NSWindowCollectionBehavior, NSWindowSharingType, NSWindowStyleMask, NSWorkspace,
    NSWorkspaceDidActivateApplicationNotification, NSWorkspaceDidLaunchApplicationNotification,
    NSWorkspaceDidUnhideApplicationNotification,
};
use objc2_foundation::{
    NSDate, NSDefaultRunLoopMode, NSNotification, NSNotificationCenter, NSRect, NSTimer,
};

use crate::{exemptions, Color, Error, HideOthersOptions, OverlayOptions, Result};

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
    /// Set up on the main thread the first time it's needed, since the
    /// overlay may be created elsewhere. Use [`Self::state`].
    state: OnceLock<MainThreadBound<RefCell<WindowState>>>,
    exclude_from_capture: bool,
    /// The latest colour asked for. A repaint reads this when it runs rather
    /// than carrying a colour, so a queued repaint can't show a stale one.
    color: Mutex<Color>,
    /// A repaint is queued on the main thread and hasn't read `color` yet.
    repaint_queued: AtomicBool,
}

/// The windows themselves, and what keeps them in step with the screens.
#[derive(Default)]
struct WindowState {
    /// One per screen, in `NSScreen::screens` order.
    windows: Vec<Retained<NSWindow>>,
    /// Registration for screen-change notifications, removed on close.
    screen_observer: Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
    closed: bool,
}

impl OverlayWindows {
    /// The window state, set up on first use.
    fn state(&self, mtm: MainThreadMarker) -> &RefCell<WindowState> {
        self.state.get_or_init(|| MainThreadBound::new(RefCell::default(), mtm)).get(mtm)
    }

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

    /// Starts tracking the screens and creates a window on each. Does nothing
    /// if the overlay was closed first.
    fn open_now(self: &Arc<Self>, mtm: MainThreadMarker) {
        let mut state = self.state(mtm).borrow_mut();
        if state.closed {
            return;
        }
        // Weak, so the notification centre doesn't keep the windows alive.
        let weak = Arc::downgrade(self);
        let block = RcBlock::new(move |_: NonNull<NSNotification>| on_screens_changed(&weak));
        let observer = unsafe {
            NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
                Some(NSApplicationDidChangeScreenParametersNotification),
                None,
                // Run on the posting thread, which for AppKit is the main one.
                None,
                &block,
            )
        };
        state.screen_observer = Some(observer);
        drop(state);
        // With no screens yet, the windows arrive with the first one.
        self.fit_to_screens(mtm);
    }

    fn repaint(&self, mtm: MainThreadMarker) {
        let background = ns_color(*self.color.lock().unwrap());
        for window in &self.state(mtm).borrow().windows {
            window.setBackgroundColor(Some(&background));
        }
    }

    /// Makes the windows match the current screens: one per screen, each
    /// covering its screen exactly. Existing windows are reused.
    fn fit_to_screens(self: &Arc<Self>, mtm: MainThreadMarker) {
        let Ok(mut state) = self.state(mtm).try_borrow_mut() else {
            // AppKit called back in while we were changing the windows; a
            // panic here would abort the process. Try again once we're done.
            Arc::clone(self).queue_on_main(Self::fit_to_screens);
            return;
        };
        if state.closed {
            // A late notification mustn't bring the windows back.
            return;
        }
        let screens = NSScreen::screens(mtm);
        for (i, screen) in screens.iter().enumerate() {
            match state.windows.get(i) {
                Some(window) => {
                    window.setFrame_display(screen.frame(), true);
                    window.orderFrontRegardless();
                }
                None => state.windows.push(self.new_window(mtm, screen.frame())),
            }
        }
        // Any windows past one per screen are for screens that are gone.
        for window in state.windows.drain(screens.count()..) {
            window.orderOut(None);
            window.close();
        }
    }

    fn close_now(&self, mtm: MainThreadMarker) {
        let mut state = self.state(mtm).borrow_mut();
        state.closed = true;
        if let Some(observer) = state.screen_observer.take() {
            let observer: &AnyObject = observer.as_ref();
            unsafe { NSNotificationCenter::defaultCenter().removeObserver(observer) };
        }
        for window in state.windows.drain(..) {
            window.orderOut(None);
            window.close();
        }
    }

    fn new_window(&self, mtm: MainThreadMarker, frame: NSRect) -> Retained<NSWindow> {
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                frame,
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
        window.setBackgroundColor(Some(&ns_color(*self.color.lock().unwrap())));
        window.setLevel(OVERLAY_WINDOW_LEVEL);
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        if self.exclude_from_capture {
            // Honoured by CGWindowList captures; ScreenCaptureKit on
            // macOS 15+ may still include the window. Needs testing.
            window.setSharingType(NSWindowSharingType::None);
        }
        window.orderFrontRegardless();
        window
    }

    /// Runs `f` now on the main thread, or else via the main queue.
    fn on_main(self: Arc<Self>, f: fn(&Arc<Self>, MainThreadMarker)) {
        match MainThreadMarker::new() {
            Some(mtm) => f(&self, mtm),
            None => self.queue_on_main(f),
        }
    }

    /// Runs `f` on the main thread. The work takes `self` with it, so this
    /// reference is released there too.
    fn queue_on_main(self: Arc<Self>, f: fn(&Arc<Self>, MainThreadMarker)) {
        DispatchQueue::main().exec_async(move || {
            // The main queue only ever runs on the main thread.
            let mtm = MainThreadMarker::new().unwrap();
            f(&self, mtm);
        });
    }
}

impl Overlay {
    pub fn new(options: &OverlayOptions) -> Result<Self> {
        // Deliberately leaves the activation policy alone: whether the process
        // is a background app is the host app's decision, not the overlay's.
        let windows = Arc::new(OverlayWindows {
            state: OnceLock::new(),
            exclude_from_capture: options.exclude_from_capture,
            color: Mutex::new(options.color),
            repaint_queued: AtomicBool::new(false),
        });
        Arc::clone(&windows).on_main(OverlayWindows::open_now);
        Ok(Overlay { windows: ManuallyDrop::new(windows) })
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
        windows.on_main(|windows, mtm| windows.close_now(mtm));
    }
}

fn on_screens_changed(weak: &Weak<OverlayWindows>) {
    // Upgrading can't hand us the last reference off the main thread: every
    // other reference is released on the main thread (see `OverlayWindows`),
    // and we only upgrade there, or pass the reference straight to it.
    let Some(windows) = weak.upgrade() else { return };
    // AppKit posts this on the main thread, but just in case.
    windows.on_main(OverlayWindows::fit_to_screens);
}

/// The latest `set_background_app` request. Like the overlay's colour, work
/// queued from other threads reads this when it runs, so the last call wins.
/// Unlike it, each call queues its own update: it's rarely called, so a burst
/// isn't worth guarding against.
static WANTED_BACKGROUND: AtomicBool = AtomicBool::new(false);

pub fn set_background_app(background: bool) -> Result<()> {
    WANTED_BACKGROUND.store(background, Ordering::Release);
    match MainThreadMarker::new() {
        Some(mtm) => apply_wanted_background(mtm),
        None => {
            DispatchQueue::main().exec_async(|| {
                // The main queue only ever runs on the main thread. There's no
                // caller left to report a failure to.
                let _ = apply_wanted_background(MainThreadMarker::new().unwrap());
            });
            Ok(())
        }
    }
}

fn apply_wanted_background(mtm: MainThreadMarker) -> Result<()> {
    let policy = if WANTED_BACKGROUND.load(Ordering::Acquire) {
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

/// The latest `hide_others` options, or `None` after `show_others`. Like the
/// overlay's colour, work queued from other threads reads this when it runs,
/// so the last call wins whichever thread made it.
static WANTED_HIDING: Mutex<Option<HideOthersOptions>> = Mutex::new(None);
/// An `apply_wanted_hiding` is queued on the main thread and hasn't read
/// `WANTED_HIDING` yet.
static HIDING_QUEUED: AtomicBool = AtomicBool::new(false);

pub fn hide_others(options: &HideOthersOptions) -> Result<()> {
    *WANTED_HIDING.lock().unwrap() = Some(options.clone());
    request_hiding_update();
    Ok(())
}

pub fn show_others() -> Result<()> {
    *WANTED_HIDING.lock().unwrap() = None;
    request_hiding_update();
    Ok(())
}

/// Applies `WANTED_HIDING`: now on the main thread, or else via the main
/// queue, with at most one queued at a time (see `request_repaint`).
fn request_hiding_update() {
    if let Some(mtm) = MainThreadMarker::new() {
        apply_wanted_hiding(mtm);
    } else if !HIDING_QUEUED.swap(true, Ordering::AcqRel) {
        DispatchQueue::main().exec_async(|| {
            // The main queue only ever runs on the main thread.
            let mtm = MainThreadMarker::new().unwrap();
            // Clear the flag before reading `WANTED_HIDING`, so a call made
            // after the read queues another update. (A swap, as in
            // `request_repaint`.)
            HIDING_QUEUED.swap(false, Ordering::AcqRel);
            apply_wanted_hiding(mtm);
        });
    }
}

fn apply_wanted_hiding(mtm: MainThreadMarker) {
    // Cloned so the lock isn't held while AppKit runs.
    let wanted = WANTED_HIDING.lock().unwrap().clone();
    match wanted {
        Some(options) => hide_others_now(options, mtm),
        None => show_others_now(mtm),
    }
}

fn hide_others_now(options: HideOthersOptions, _mtm: MainThreadMarker) {
    let exempt = options.exempt;
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
}

fn show_others_now(_mtm: MainThreadMarker) {
    let Some(hider) = HIDER.with(|h| h.borrow_mut().take()) else {
        return;
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
