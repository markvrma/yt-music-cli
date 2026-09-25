//! Album art as 256-color half-blocks. Port of tui.py's art section; image
//! decode + BOX resize via ffmpeg (rawvideo rgb24) instead of Pillow.
#![allow(dead_code)]

use std::path::Path;

/// (fg, bg) xterm-256 index per cell; rows x cols. Each cell is a "▀":
/// fg = top pixel, bg = bottom pixel. Cached per (path, cols, rows).
pub type Grid = Vec<Vec<(u8, u8)>>;

/// Nearest xterm-256 index (cube + grey ramp gated by GREY_GATE).
pub fn xterm256(r: u8, g: u8, b: u8) -> u8 {
    todo!()
}

/// Resize to cols x 2*rows, saturation ART_SAT, gamma lift, FASTOCTREE
/// ART_COLORS palette, xterm256. Err = could not decode (show "no art").
pub fn art_grid(path: &Path, cols: usize, rows: usize) -> Result<Grid, String> {
    todo!()
}
