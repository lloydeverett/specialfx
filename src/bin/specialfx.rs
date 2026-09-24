//! Shows a colour overlay until Ctrl-C (or `--duration` elapses).
//!
//!     specialfx                        # night preset
//!     specialfx dim --opacity 0.3
//!     specialfx '#0040ff' --opacity 0.2 --fade 2 --duration 10

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use specialfx::{presets, Color, Overlay, OverlayOptions};

#[derive(Parser)]
#[command(version, about = "Tint or dim the whole screen with a translucent colour overlay")]
struct Args {
    /// A preset (night, red, dim) or a hex colour: #rgb, #rrggbb or #rrggbbaa.
    #[arg(default_value = "night", value_parser = parse_color)]
    color: Color,

    /// Overlay opacity from 0 to 1. Overrides the preset's or hex colour's alpha.
    #[arg(short, long)]
    opacity: Option<f32>,

    /// Seconds to fade in from transparent.
    #[arg(short, long, default_value_t = 0.0)]
    fade: f32,

    /// Remove the overlay and exit after this many seconds.
    #[arg(short, long)]
    duration: Option<f32>,

    /// Let screenshots and screen recordings see the overlay.
    #[arg(long)]
    allow_capture: bool,
}

fn parse_color(s: &str) -> Result<Color, String> {
    presets::by_name(s)
        .map(Ok)
        .unwrap_or_else(|| s.parse().map_err(|e| format!("{e}, or one of {:?}", presets::NAMES)))
}

fn main() -> ExitCode {
    let args = Args::parse();
    let target = match args.opacity {
        Some(a) => args.color.with_alpha(a),
        None => args.color,
    }
    .clamped();
    let fade = Duration::from_secs_f32(args.fade.max(0.0));

    let mut overlay = match Overlay::new(OverlayOptions {
        color: if fade.is_zero() { target } else { target.with_alpha(0.0) },
        exclude_from_capture: !args.allow_capture,
    }) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("specialfx: {e}");
            return ExitCode::FAILURE;
        }
    };

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        if let Err(e) = ctrlc::set_handler(move || stop.store(true, Ordering::SeqCst)) {
            eprintln!("specialfx: couldn't install Ctrl-C handler: {e}");
        }
    }

    eprintln!("specialfx: showing {target}; Ctrl-C to stop");
    let start = Instant::now();
    let deadline = args.duration.map(|s| start + Duration::from_secs_f32(s.max(0.0)));
    let mut fading = !fade.is_zero();

    specialfx::run_until(|| {
        if fading {
            let t = (start.elapsed().as_secs_f32() / fade.as_secs_f32()).min(1.0);
            if let Err(e) = overlay.set_color(target.with_alpha(target.a * t)) {
                eprintln!("specialfx: {e}");
            }
            fading = t < 1.0;
        }
        stop.load(Ordering::SeqCst) || deadline.is_some_and(|d| Instant::now() >= d)
    });

    ExitCode::SUCCESS
}
