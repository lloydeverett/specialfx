use std::fmt;
use std::str::FromStr;

/// An overlay colour in sRGB, with straight (non-premultiplied) alpha.
///
/// All channels are in `0.0..=1.0`. `a` is the overlay's opacity: `0.0` leaves
/// the screen untouched, `1.0` replaces it entirely with `(r, g, b)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const TRANSPARENT: Color = Color::rgba(0.0, 0.0, 0.0, 0.0);

    pub const fn rgba(r: f32, g: f32, b: f32, a: f32) -> Self {
        Color { r, g, b, a }
    }

    pub fn from_rgba8(r: u8, g: u8, b: u8, a: u8) -> Self {
        let f = |v: u8| v as f32 / 255.0;
        Color::rgba(f(r), f(g), f(b), f(a))
    }

    pub fn to_rgba8(self) -> [u8; 4] {
        let c = self.clamped();
        let q = |v: f32| (v * 255.0).round() as u8;
        [q(c.r), q(c.g), q(c.b), q(c.a)]
    }

    pub fn with_alpha(self, a: f32) -> Self {
        Color { a, ..self }
    }

    pub fn clamped(self) -> Self {
        let c = |v: f32| if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) };
        Color::rgba(c(self.r), c(self.g), c(self.b), c(self.a))
    }

    /// What the compositor will show for a screen pixel `src` (RGB, `0..=1`)
    /// underneath this overlay: `src × (1 − α) + colour × α`, per channel.
    ///
    /// This is a per-channel affine map, which is why an overlay can only ever
    /// lift blacks toward the tint and can't mix channels (no grayscale etc.).
    pub fn blend_over(self, src: [f32; 3]) -> [f32; 3] {
        let c = self.clamped();
        let k = 1.0 - c.a;
        [
            src[0] * k + c.r * c.a,
            src[1] * k + c.g * c.a,
            src[2] * k + c.b * c.a,
        ]
    }

    /// The equivalent gamma ramp for this overlay: `(scale, offset)` per
    /// channel such that `out = src × scale + offset`. Useful for comparing
    /// against (or falling back to) a gamma-LUT implementation.
    pub fn as_affine(self) -> [(f32, f32); 3] {
        let c = self.clamped();
        let k = 1.0 - c.a;
        [(k, c.r * c.a), (k, c.g * c.a), (k, c.b * c.a)]
    }

    /// Linear interpolation between two colours; `t` in `0..=1`.
    pub fn lerp(self, to: Color, t: f32) -> Color {
        let m = |a: f32, b: f32| a + (b - a) * t;
        Color::rgba(m(self.r, to.r), m(self.g, to.g), m(self.b, to.b), m(self.a, to.a))
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [r, g, b, a] = self.to_rgba8();
        write!(f, "#{r:02x}{g:02x}{b:02x}{a:02x}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseColorError(String);

impl fmt::Display for ParseColorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid colour {:?}: expected #rgb, #rrggbb or #rrggbbaa", self.0)
    }
}

impl std::error::Error for ParseColorError {}

impl FromStr for Color {
    type Err = ParseColorError;

    /// Parses `#rgb`, `#rrggbb` or `#rrggbbaa` (the `#` is optional).
    /// Colours without an alpha component are fully opaque.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ParseColorError(s.to_owned());
        let hex = s.strip_prefix('#').unwrap_or(s);
        if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(err());
        }
        let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| err());
        match hex.len() {
            3 => {
                let nib = |i: usize| u8::from_str_radix(&hex[i..i + 1], 16).map(|v| v * 17);
                let n = |i| nib(i).map_err(|_| err());
                Ok(Color::from_rgba8(n(0)?, n(1)?, n(2)?, 255))
            }
            6 => Ok(Color::from_rgba8(byte(0)?, byte(2)?, byte(4)?, 255)),
            8 => Ok(Color::from_rgba8(byte(0)?, byte(2)?, byte(4)?, byte(6)?)),
            _ => Err(err()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [f32; 3], b: [f32; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-6)
    }

    #[test]
    fn parses_hex_forms() {
        assert_eq!("#fff".parse::<Color>().unwrap().to_rgba8(), [255, 255, 255, 255]);
        assert_eq!("ff8000".parse::<Color>().unwrap().to_rgba8(), [255, 128, 0, 255]);
        assert_eq!("#ff800040".parse::<Color>().unwrap().to_rgba8(), [255, 128, 0, 64]);
        assert!("#ff80".parse::<Color>().is_err());
        assert!("#gggggg".parse::<Color>().is_err());
        assert!("#é12".parse::<Color>().is_err());
    }

    #[test]
    fn display_round_trips() {
        let c: Color = "#12345678".parse().unwrap();
        assert_eq!(c.to_string(), "#12345678");
    }

    #[test]
    fn transparent_overlay_is_identity() {
        assert!(close(Color::TRANSPARENT.blend_over([0.2, 0.5, 0.9]), [0.2, 0.5, 0.9]));
    }

    #[test]
    fn overlay_lifts_black_toward_tint() {
        let tint = Color::rgba(1.0, 0.5, 0.0, 0.4);
        assert!(close(tint.blend_over([0.0; 3]), [0.4, 0.2, 0.0]));
    }

    #[test]
    fn black_overlay_is_pure_dimming() {
        let dim = Color::rgba(0.0, 0.0, 0.0, 0.25);
        assert!(close(dim.blend_over([1.0, 0.5, 0.0]), [0.75, 0.375, 0.0]));
    }

    #[test]
    fn affine_form_matches_blend() {
        let c = Color::rgba(0.9, 0.3, 0.1, 0.35);
        let src = [0.6, 0.2, 0.8];
        let aff = c.as_affine();
        let via_affine = [0, 1, 2].map(|i| src[i] * aff[i].0 + aff[i].1);
        assert!(close(via_affine, c.blend_over(src)));
    }
}
