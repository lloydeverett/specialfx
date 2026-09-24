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

## Library

```rust
use specialfx::{presets, Overlay, OverlayOptions};

let mut overlay = Overlay::new(OverlayOptions { color: presets::NIGHT, ..Default::default() })?;
overlay.set_color(presets::DIM)?;
specialfx::run_until(|| false); // macOS: must pump events on the main thread
```

Build without the CLI's dependencies with `default-features = false`.

## Known gaps

- macOS doesn't rebuild windows when screens change (Windows does, on `WM_DISPLAYCHANGE`).
- Won't cover the Windows secure desktop (UAC, lock screen) or exclusive-fullscreen games,
  and some macOS system UI draws above it.
- Capture exclusion on macOS needs testing against ScreenCaptureKit.
