//! YouTube Music InnerTube client — the slice of ytmusicapi msm uses:
//! search, album, playlist, home recs, song playback tracking, rate.
//! Port of the YouTube Music section of ymc.py (+ ytmusicapi's parsers).
#![allow(dead_code)]

use crate::auth::Auth;
use crate::{Album, Item, Track};

/// Client. `auth == None` -> anonymous (browse + play only).
pub struct Yt {
    pub auth: Option<Auth>,
}

/// Authed client if browser.json loads, else anonymous. Never fails.
pub fn get_yt() -> Yt {
    todo!()
}

impl Yt {
    /// ymc.AUTHED equivalent.
    pub fn authed(&self) -> bool {
        self.auth.is_some()
    }

    /// yt.search(query, filter=...) with filter in {"songs","albums","playlists"}.
    pub fn search(&self, query: &str, filter: &str) -> Result<Vec<Item>, String> {
        todo!()
    }

    /// songs + albums + playlists, first 5 of each, a failing category skipped.
    pub fn search_all(&self, query: &str) -> Vec<Item> {
        todo!()
    }

    /// -> (title, tracks, thumb). Unavailable tracks (no videoId) dropped.
    pub fn album_tracks(&self, browse_id: &str) -> Result<(String, Vec<Track>, String), String> {
        todo!()
    }

    /// Search result OR home item -> (title, tracks, thumb); infers type when
    /// result_type is None (videoId -> song, MPRE browseId -> album, playlistId -> playlist).
    pub fn resolve_result(&self, r: &Item) -> Result<(String, Vec<Track>, String), String> {
        todo!()
    }

    /// Home rows flattened to `limit` playable items as lazy albums
    /// (tracks None, rec Some). Any failure -> empty.
    pub fn get_recs(&self, limit: usize) -> Vec<Album> {
        todo!()
    }

    /// Thumbs-up; false if unauthed / not a YT track / request fails.
    pub fn like_track(&self, track: &Track) -> bool {
        todo!()
    }

    /// Playback + watchtime pings with one shared 16-char cpn (see ymc.py).
    pub fn record_history(&self, video_id: &str, watched: u64) -> Result<(), String> {
        todo!()
    }
}

/// Artist names joined by ", ", dropping type words and "<N> plays" tokens.
pub fn artists_str(artists: &[String]) -> String {
    todo!()
}

/// videoId from a YT watch url; None for local paths (no ?v=).
pub fn video_id(url: &str) -> Option<String> {
    todo!()
}
