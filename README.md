# specialfx

Translucent fullscreen colour overlays: night-mode tints, dimming and so on.
Each monitor gets a borderless, always-on-top, click-through window filled with
a semi-transparent colour, and the compositor blends it over everything:

    out = src × (1 − α) + colour × α

That's a per-channel affine map, so it's a strict subset of what a gamma ramp
can do (`Color::as_affine` gives the equivalent ramp). It lifts blacks toward
the tint and can't mix channels, so no grayscale or inversion.

| Platform | Backend | Status |
|----------|---------|--------|
| Windows  | Layered `WS_EX_LAYERED \| WS_EX_TRANSPARENT \| WS_EX_TOPMOST` popups on a dedicated thread, `WDA_EXCLUDEFROMCAPTURE` | prototype |
| macOS    | Borderless `NSWindow`s at screen-saver level, all Spaces + fullscreen, `sharingType = .none` | prototype |
| Linux    | none yet; `Overlay::new` returns `Error::Unsupported` | — |

## CLI

    cargo run --release -- night                      # warm tint until Ctrl-C
    cargo run --release -- dim --opacity 0.3
    cargo run --release -- '#0040ff' -o 0.2 --fade 2 --duration 10
    cargo run --release -- red --allow-capture        # show up in screenshots
    cargo run --release -- dim --hide-others --exempt com.apple.Terminal

## Library

```rust
use specialfx::{presets, Overlay, OverlayOptions};

let mut overlay = Overlay::new(OverlayOptions { color: presets::NIGHT, ..Default::default() })?;
overlay.set_color(presets::DIM)?;
specialfx::run_until(|| false); // macOS: must pump events on the main thread
```

Create the overlay on the main thread. The `Overlay` handle is `Send + Sync`,
so after that you can move it to any thread and update it from there. On macOS
those updates are queued to the main thread, which must be pumping events:
either your app's own loop or `run_until`.

The library never changes process-wide state like the macOS activation policy
unless you ask it to, so whether your app has a Dock icon and menu bar is up to
you. To run without them (the CLI does this), call
`specialfx::set_background_app(true)` on the main thread before creating the
overlay. It does nothing on other platforms.

### Hiding other apps

`specialfx::hide_others(&options)` hides every other app's windows and keeps
them hidden, re-hiding anything that reappears or launches, until
`specialfx::show_others()` brings back what it hid. Windows belonging to this
process are never touched. System UI and task managers are exempt by default
(`specialfx::exemptions`), and you can exempt more apps, named the way the
current platform names them (bundle IDs on macOS, executable names on Windows):

```rust
let terminal = if cfg!(windows) { "WindowsTerminal.exe" } else { "com.apple.Terminal" };
specialfx::hide_others(&specialfx::HideOthersOptions {
    exempt: vec![terminal.into()],
    ..Default::default()
})?;
```

Both backends use public APIs only:

- **macOS** hides whole apps with `NSRunningApplication.hide()`, as Cmd-H does,
  re-hiding on workspace launch/activate/unhide notifications and a 0.5 s
  timer. No permissions needed. Main thread only, and it needs events pumped.
- **Windows** minimizes top-level windows with `ShowWindowAsync`, re-minimizing
  on WinEvent hooks and a 0.5 s timer. It can't touch elevated apps unless
  it's elevated too.

Both are recoverable if the process dies: apps stay in the Dock or taskbar.

Build without the CLI's dependencies with `default-features = false`.

## Known gaps

- macOS doesn't rebuild windows when screens change (Windows does, on `WM_DISPLAYCHANGE`).
- Won't cover the Windows secure desktop (UAC, lock screen) or exclusive-fullscreen games,
  and some macOS system UI draws above it.
- Capture exclusion on macOS needs testing against ScreenCaptureKit.
