//! Windows backend: one layered, click-through, topmost popup per monitor.
//!
//! All windows live on a dedicated thread with its own message loop; the
//! [`Overlay`] handle talks to it with thread messages. The window content is
//! a solid opaque colour and the layered-window constant alpha does the
//! blending, which gives exactly `src × (1 − α) + colour × α`.

use std::cell::{Cell, RefCell};
use std::ptr::{null, null_mut};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::{Color, Error, OverlayOptions, Result};

/// wParam = packed RGBA (see [`pack`]).
const MSG_SET_COLOR: u32 = WM_APP + 1;
/// Monitors were added, removed or resized.
const MSG_REBUILD: u32 = WM_APP + 2;

/// How often to re-assert topmost, in case another topmost window was raised.
const TOPMOST_INTERVAL_MS: u32 = 1000;

const CLASS_NAME: &[u16] = &utf16z::<17>("SpecialFxOverlay");

pub struct Overlay {
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
}

impl Overlay {
    pub fn new(options: &OverlayOptions) -> Result<Self> {
        let (ready_tx, ready_rx) = mpsc::channel();
        let options = options.clone();
        let thread = std::thread::Builder::new()
            .name("specialfx-overlay".into())
            .spawn(move || overlay_thread(options, ready_tx))
            .map_err(|e| Error::Os(format!("failed to spawn overlay thread: {e}")))?;

        match ready_rx.recv() {
            Ok(Ok(thread_id)) => Ok(Overlay { thread_id, thread: Some(thread) }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err(Error::Os("overlay thread exited during startup".into())),
        }
    }

    pub fn set_color(&mut self, color: Color) -> Result<()> {
        self.post(MSG_SET_COLOR, pack(color))
    }

    fn post(&self, msg: u32, wparam: WPARAM) -> Result<()> {
        if unsafe { PostThreadMessageW(self.thread_id, msg, wparam, 0) } == 0 {
            return Err(last_error("PostThreadMessageW"));
        }
        Ok(())
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        let _ = self.post(WM_QUIT, 0);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn set_background_app(_background: bool) -> Result<()> {
    Ok(())
}

pub fn run_until(mut should_stop: impl FnMut() -> bool, interval: Duration) {
    // The overlay thread pumps its own messages; nothing to do here.
    while !should_stop() {
        std::thread::sleep(interval);
    }
}

// ---- overlay thread ----------------------------------------------------------

struct ThreadState {
    color: Cell<Color>,
    exclude_from_capture: bool,
    windows: RefCell<Vec<HWND>>,
}

thread_local! {
    static STATE: RefCell<Option<ThreadState>> = const { RefCell::new(None) };
}

fn with_state<R>(f: impl FnOnce(&ThreadState) -> R) -> Option<R> {
    STATE.with(|s| s.borrow().as_ref().map(f))
}

fn overlay_thread(options: OverlayOptions, ready: mpsc::Sender<Result<u32>>) {
    unsafe {
        // Work in physical pixels so each window exactly covers its monitor,
        // even with mixed DPI. Failure (pre-1703) just means we get scaled.
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        // Make sure this thread has a message queue before anyone posts to it.
        let mut msg: MSG = std::mem::zeroed();
        PeekMessageW(&mut msg, null_mut(), WM_USER, WM_USER, PM_NOREMOVE);

        if let Err(e) = register_class() {
            let _ = ready.send(Err(e));
            return;
        }

        STATE.with(|s| {
            *s.borrow_mut() = Some(ThreadState {
                color: Cell::new(options.color),
                exclude_from_capture: options.exclude_from_capture,
                windows: RefCell::new(Vec::new()),
            })
        });

        if let Err(e) = rebuild_windows() {
            destroy_windows();
            let _ = ready.send(Err(e));
            return;
        }

        SetTimer(null_mut(), 0, TOPMOST_INTERVAL_MS, None);
        let _ = ready.send(Ok(GetCurrentThreadId()));

        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            if msg.hwnd.is_null() {
                match msg.message {
                    MSG_SET_COLOR => set_color(unpack(msg.wParam)),
                    MSG_REBUILD => {
                        // Best-effort: keep running with whatever we could create.
                        let _ = rebuild_windows();
                    }
                    WM_TIMER => raise_all(),
                    _ => {}
                }
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        destroy_windows();
        STATE.with(|s| s.borrow_mut().take());
    }
}

unsafe fn register_class() -> Result<()> {
    let class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wnd_proc),
        hInstance: GetModuleHandleW(null()),
        lpszClassName: CLASS_NAME.as_ptr(),
        hCursor: null_mut(),
        hbrBackground: null_mut(),
        ..std::mem::zeroed()
    };
    if RegisterClassW(&class) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
        return Err(last_error("RegisterClassW"));
    }
    Ok(())
}

unsafe fn rebuild_windows() -> Result<()> {
    destroy_windows();

    let mut rects: Vec<RECT> = Vec::new();
    if EnumDisplayMonitors(
        null_mut(),
        null(),
        Some(collect_monitor),
        &mut rects as *mut Vec<RECT> as LPARAM,
    ) == 0
    {
        return Err(last_error("EnumDisplayMonitors"));
    }

    let (color, exclude) =
        with_state(|s| (s.color.get(), s.exclude_from_capture)).expect("state initialised");

    let mut created = Vec::with_capacity(rects.len());
    for r in rects {
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            CLASS_NAME.as_ptr(),
            null(),
            WS_POPUP,
            r.left,
            r.top,
            r.right - r.left,
            r.bottom - r.top,
            null_mut(),
            null_mut(),
            GetModuleHandleW(null()),
            null(),
        );
        if hwnd.is_null() {
            let err = last_error("CreateWindowExW");
            with_state(|s| s.windows.borrow_mut().extend(created.iter().copied()));
            return Err(err);
        }
        apply_alpha(hwnd, color);
        if exclude {
            // Needs Windows 10 2004+; older versions just stay capturable.
            SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
        }
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        created.push(hwnd);
    }

    with_state(|s| *s.windows.borrow_mut() = created);
    Ok(())
}

unsafe extern "system" fn collect_monitor(
    _monitor: HMONITOR,
    _hdc: HDC,
    rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let rects = &mut *(data as *mut Vec<RECT>);
    rects.push(*rect);
    TRUE
}

unsafe fn destroy_windows() {
    let windows = with_state(|s| std::mem::take(&mut *s.windows.borrow_mut())).unwrap_or_default();
    for hwnd in windows {
        DestroyWindow(hwnd);
    }
}

unsafe fn set_color(color: Color) {
    let Some(windows) = with_state(|s| {
        s.color.set(color);
        s.windows.borrow().clone()
    }) else {
        return;
    };
    for hwnd in windows {
        apply_alpha(hwnd, color);
        InvalidateRect(hwnd, null(), FALSE);
    }
}

unsafe fn apply_alpha(hwnd: HWND, color: Color) {
    let [_, _, _, a] = color.to_rgba8();
    SetLayeredWindowAttributes(hwnd, 0, a, LWA_ALPHA);
}

unsafe fn raise_all() {
    let windows = with_state(|s| s.windows.borrow().clone()).unwrap_or_default();
    for hwnd in windows {
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
        );
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            let color = with_state(|s| s.color.get()).unwrap_or(Color::TRANSPARENT);
            let [r, g, b, _] = color.to_rgba8();
            let brush = CreateSolidBrush(r as u32 | (g as u32) << 8 | (b as u32) << 16);
            FillRect(hdc, &ps.rcPaint, brush);
            DeleteObject(brush);
            EndPaint(hwnd, &ps);
            0
        }
        // WM_PAINT covers everything; skipping the erase avoids flicker.
        WM_ERASEBKGND => 1,
        WM_NCHITTEST => HTTRANSPARENT as LRESULT,
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_DISPLAYCHANGE => {
            // Don't tear windows down from inside one of their own wndprocs.
            PostThreadMessageW(GetCurrentThreadId(), MSG_REBUILD, 0, 0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ---- helpers -----------------------------------------------------------------

fn pack(color: Color) -> WPARAM {
    u32::from_le_bytes(color.to_rgba8()) as WPARAM
}

fn unpack(wparam: WPARAM) -> Color {
    let [r, g, b, a] = (wparam as u32).to_le_bytes();
    Color::from_rgba8(r, g, b, a)
}

fn last_error(what: &str) -> Error {
    Error::Os(format!("{what} failed: {}", std::io::Error::last_os_error()))
}

const fn utf16z<const N: usize>(s: &str) -> [u16; N] {
    let bytes = s.as_bytes();
    assert!(bytes.len() < N, "no room for the nul terminator");
    let mut out = [0u16; N];
    let mut i = 0;
    while i < bytes.len() {
        out[i] = bytes[i] as u16;
        i += 1;
    }
    out
}
