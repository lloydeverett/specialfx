//! Shows a colour overlay until Ctrl-C (or `--duration` elapses).
//!
//!     specialfx                        # night preset
//!     specialfx dim --opacity 0.3
//!     specialfx '#0040ff' --opacity 0.2 --fade 2 --duration 10
//!     specialfx dim --hide-others --exempt com.apple.Terminal

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use specialfx::{Color, HideOthersOptions, Overlay, OverlayOptions};

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

    /// Keep every other app's windows hidden (macOS) or minimized (Windows)
    /// while running, and bring them back on exit.
    #[arg(long)]
    hide_others: bool,

    /// With --hide-others, leave this app alone: a bundle ID or executable name
    /// on macOS, an executable name on Windows. Repeatable.
    #[arg(long, value_name = "APP", requires = "hide_others")]
    exempt: Vec<String>,
}

/// Colours the CLI accepts by name in place of a hex colour.
const PRESETS: &[(&str, Color)] = &[
    // Warm orange tint that cuts blue light. Lifts blacks a little.
    ("night", Color::rgba(1.0, 0.55, 0.1, 0.30)),
    // Deeper red tint for late at night.
    ("red", Color::rgba(1.0, 0.1, 0.0, 0.35)),
    // Plain dimming: black at 50%. The only overlay that doesn't lift blacks.
    ("dim", Color::rgba(0.0, 0.0, 0.0, 0.50)),
];

fn parse_color(s: &str) -> Result<Color, String> {
    match PRESETS.iter().find(|(name, _)| name.eq_ignore_ascii_case(s)) {
        Some(&(_, color)) => Ok(color),
        None => s.parse().map_err(|e| {
            let names: Vec<_> = PRESETS.iter().map(|(name, _)| *name).collect();
            format!("{e}, or one of {names:?}")
        }),
    }
}

fn main() -> ExitCode {
    let args = Args::parse();
    let target = match args.opacity {
        Some(a) => args.color.with_alpha(a),
        None => args.color,
    }
    .clamped();
    let fade = Duration::from_secs_f32(args.fade.max(0.0));

    // No Dock icon, menu bar or Cmd-Tab entry on macOS.
    if let Err(e) = specialfx::set_background_app(true) {
        eprintln!("specialfx: couldn't run as a background app: {e}");
    }

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

    if args.hide_others {
        let options = HideOthersOptions { exempt: args.exempt.clone(), ..Default::default() };
        if let Err(e) = specialfx::hide_others(&options) {
            eprintln!("specialfx: couldn't hide other apps: {e}");
        }
    }

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

    if let Err(e) = specialfx::show_others() {
        eprintln!("specialfx: couldn't restore other apps: {e}");
    }

    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_color_accepts_names_case_insensitively() {
        assert_eq!(parse_color("night"), Ok(Color::rgba(1.0, 0.55, 0.1, 0.30)));
        assert_eq!(parse_color("DIM"), Ok(Color::rgba(0.0, 0.0, 0.0, 0.50)));
    }

    #[test]
    fn parse_color_accepts_hex() {
        assert_eq!(parse_color("#ff0000"), Ok(Color::rgba(1.0, 0.0, 0.0, 1.0)));
    }

    #[test]
    fn parse_color_error_lists_names() {
        let err = parse_color("nope").unwrap_err();
        assert!(err.contains(r#"["night", "red", "dim"]"#), "{err}");
    }
}
