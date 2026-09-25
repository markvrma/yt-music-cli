//! One background mpv for the session, driven over its JSON IPC socket;
//! yt-dlp fetch cache; cmusfm scrobble bridge. Port of ymc.py's cmusfm /
//! cache_path / fetch / IPC / Player.
#![allow(dead_code)]

use crate::ytm::Yt;
use crate::Track;
use std::sync::Arc;

pub struct Player {
    // implementation-defined (Mutex'd IPC, child process, shared current track, fetch queue...)
}

impl Player {
    /// cmusfm_reset, remove stale socket, spawn mpv, connect IPC, start
    /// fetcher + watcher threads. Err(message) if mpv/IPC fails.
    pub fn new(yt: Arc<Yt>) -> Result<Player, String> {
        todo!()
    }
    pub fn play(&self, tracks: &[Track], start: usize) {
        todo!()
    }
    pub fn enqueue(&self, tracks: &[Track]) {
        todo!()
    }
    /// Insert after current; returns playlist index of first inserted, None if idle.
    pub fn play_next(&self, tracks: &[Track]) -> Option<usize> {
        todo!()
    }
    pub fn toggle_pause(&self) {
        todo!()
    }
    pub fn toggle_loop(&self) {
        todo!()
    }
    pub fn looping(&self) -> bool {
        todo!()
    }
    pub fn toggle_left_ear(&self) {
        todo!()
    }
    pub fn volume(&self, delta: i64) {
        todo!()
    }
    pub fn volume_pct(&self) -> i64 {
        todo!()
    }
    pub fn left_ear(&self) -> bool {
        todo!()
    }
    pub fn next(&self) {
        todo!()
    }
    pub fn prev(&self) {
        todo!()
    }
    /// (pos_s, dur_s, paused, current title)
    pub fn progress(&self) -> (f64, f64, bool, String) {
        todo!()
    }
    /// Track the watcher last saw start (what's playing now), if any.
    pub fn current(&self) -> Option<Track> {
        todo!()
    }
    /// mpv quit over IPC, wait 5s, else kill. Idempotent (safe from Drop + panic hook).
    pub fn quit(&self) {
        todo!()
    }
}

/// Local file mpv plays: ~/.cache/msm/<vid>.m4a for YT, the path itself for local.
pub fn cache_path(track: &Track) -> String {
    todo!()
}

/// Download into the cache if missing (yt-dlp argv exactly as ymc.py). -> path.
pub fn fetch(track: &Track) -> String {
    todo!()
}

/// argv for cmusfm as cmus's status_display_program.
pub fn cmusfm_argv(status: &str, track: Option<&Track>) -> Vec<String> {
    todo!()
}
