//! Terminal UI: browse + search screens, bordered panes, purple/grey theme,
//! album art, progress bar. Port of tui.py onto plain crossterm.
#![allow(dead_code)]

use crate::player::Player;
use crate::ytm::Yt;
use std::sync::Arc;

/// Own the terminal until `q`. Restores the terminal on return AND on panic.
pub fn run(yt: Arc<Yt>, player: &Player) {
    todo!()
}
