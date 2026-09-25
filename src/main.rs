//! msm — YouTube Music + local library terminal player with cmusfm scrobbling.
//!
//! mpv runs in the background driven over its JSON IPC socket, so the TUI owns
//! the terminal. cmusfm is fed the same way cmus feeds it as
//! status_display_program.

mod art;
mod auth;
mod local;
mod player;
mod tui;
mod ytm;

use serde::{Deserialize, Deserializer, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

pub const MPV_SOCK: &str = "/tmp/ymc-mpv.sock";
pub const MPV_LOG: &str = "/tmp/ymc-mpv.log"; // mpv verbose log — inspect on playback failures
pub const AUDIO_EXT: &[&str] = &[".mp3", ".flac", ".m4a", ".opus", ".ogg", ".wav", ".aac", ".wma"];

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}
/// ~/.config/ymc
pub fn config_dir() -> PathBuf {
    home().join(".config/ymc")
}
/// ~/.config/ymc/history.json
pub fn hist_path() -> PathBuf {
    config_dir().join("history.json")
}
/// ~/.config/ymc/browser.json — ytmusicapi browser-auth headers
pub fn auth_path() -> PathBuf {
    config_dir().join("browser.json")
}
/// ~/.config/ymc/art
pub fn art_cache() -> PathBuf {
    config_dir().join("art")
}
/// ~/Music
pub fn local_music() -> PathBuf {
    home().join("Music")
}
/// ~/.cache/msm
pub fn stream_cache() -> PathBuf {
    home().join(".cache/msm")
}
/// $MSM_COOKIE_BROWSER, default "chrome"
pub fn cookie_browser() -> String {
    std::env::var("MSM_COOKIE_BROWSER").unwrap_or_else(|_| "chrome".into())
}

/// One playable track. `url` is a music.youtube.com watch url or a local path.
/// `thumb` is the album art (http url or local path) stamped on so the cover
/// can follow the playing track across queued albums. Serialized shape matches
/// the Python history.json exactly.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub title: String,
    pub artist: String,
    pub album: String,
    #[serde(deserialize_with = "de_secs")]
    pub duration: u64,
    pub url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub thumb: String,
}

/// Accept ints or floats for durations (old history files, ffprobe output).
fn de_secs<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(v.as_f64().map(|f| f as u64).unwrap_or(0))
}

/// Local album folder: ~/Music/<name>/ with its audio files (sorted).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LocalDir {
    pub dir: PathBuf,
    pub files: Vec<String>,
}

/// An album-like list: a history entry, a local folder, a YT rec, or an
/// ad-hoc list built by the TUI. `tracks == None` means lazy / not loaded yet.
/// Only title/tracks/thumb are serialized (history.json shape).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Album {
    pub title: String,
    #[serde(default)]
    pub tracks: Option<Vec<Track>>,
    #[serde(default)]
    pub thumb: String,
    #[serde(skip)]
    pub local: Option<LocalDir>,
    #[serde(skip)]
    pub rec: Option<Item>,
}

/// A YouTube Music search result or home-feed item — the subset of
/// ytmusicapi's parsed dict that msm reads.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Item {
    /// "song" / "album" / "playlist" / "video"...; None for home items.
    pub result_type: Option<String>,
    pub title: String,
    /// raw artist names in order (artists_str filters type words / play counts)
    pub artists: Vec<String>,
    pub video_id: Option<String>,
    pub browse_id: Option<String>,
    pub playlist_id: Option<String>,
    /// album name for song results
    pub album: Option<String>,
    pub duration_seconds: u64,
    /// largest thumbnail url, "" if none
    pub thumb: String,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("auth") {
        auth::setup_auth(args.get(2).map(String::as_str));
        return;
    }
    for tool in ["mpv", "cmusfm"] {
        if !on_path(tool) {
            eprintln!("missing required tool: {tool}");
            std::process::exit(1);
        }
    }
    let yt = Arc::new(ytm::get_yt());
    let player = match player::Player::new(yt.clone()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    tui::run(yt, &player);
    player.quit();
}

/// crate::*_path() read $HOME; tests that repoint it or read the real files
/// serialize on this.
#[cfg(test)]
pub static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// shutil.which equivalent: a regular file with an exec bit somewhere on PATH.
pub fn on_path(tool: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH")
        .map(|p| {
            std::env::split_paths(&p).any(|d| {
                std::fs::metadata(d.join(tool))
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
        })
        .unwrap_or(false)
}
