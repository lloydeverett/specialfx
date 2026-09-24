//! Fallback for platforms without a backend yet (e.g. Linux, where the
//! answer depends on X11 vs. the Wayland compositor in use).

use std::time::Duration;

use crate::{Color, Error, OverlayOptions, Result};

pub struct Overlay;

impl Overlay {
    pub fn new(_options: &OverlayOptions) -> Result<Self> {
        Err(Error::Unsupported)
    }

    pub fn set_color(&mut self, _color: Color) -> Result<()> {
        Err(Error::Unsupported)
    }
}

pub fn run_until(mut should_stop: impl FnMut() -> bool, interval: Duration) {
    while !should_stop() {
        std::thread::sleep(interval);
    }
}
