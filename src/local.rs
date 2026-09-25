//! Local ~/Music library, play history, album-art download cache.
//! Port of ymc.py art_file/scan_local/_probe/load_local_album/local_art/history.
#![allow(dead_code)]

use crate::{Album, Track};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Download art once into ~/.config/ymc/art/<slug>.jpg. None on failure / empty url.
pub fn art_file(title: &str, url: &str) -> Option<PathBuf> {
    todo!()
}

/// Immediate subdirs of ~/Music containing audio = albums (lazy, no tags).
pub fn scan_local() -> Vec<Album> {
    todo!()
}

/// (lowercased tags, duration seconds) via ffprobe; failure -> ({}, 0.0).
pub fn probe(path: &Path) -> (BTreeMap<String, String>, f64) {
    todo!()
}

/// Fill a local album's tracks (ffprobe) + thumb (local_art). Cached on the album.
pub fn load_local_album(album: &mut Album) {
    todo!()
}

/// cover/folder .jpg/.png, else first *.jpg/*.png, else embedded art extracted once.
pub fn local_art(dir: &Path, first_file: &Path) -> Option<PathBuf> {
    todo!()
}

/// history.json, [] on any error.
pub fn load_history() -> Vec<Album> {
    todo!()
}

/// Prepend album, drop older same-title, keep last 5, write, return it.
pub fn record(title: &str, tracks: &[Track], thumb: &str) -> Vec<Album> {
    todo!()
}
