//! YouTube Music browser auth: browser.json (ytmusicapi format), SAPISIDHASH,
//! and `msm auth`. Port of ymc.py get_yt/_headers_from_input/setup_auth/is_logged_in.
#![allow(dead_code)]

use std::collections::BTreeMap;

/// Saved request headers from browser.json (lowercase keys, ytmusicapi format).
#[derive(Clone, Debug, Default)]
pub struct Auth {
    pub headers: BTreeMap<String, String>,
}

impl Auth {
    /// Load ~/.config/ymc/browser.json. None if missing/corrupt/unusable
    /// (caller falls back to unauthenticated, never errors).
    pub fn load() -> Option<Auth> {
        todo!()
    }

    /// Headers to send on an authed InnerTube request to `origin`
    /// (e.g. "https://music.youtube.com"): saved headers + fresh
    /// `authorization: SAPISIDHASH <ts>_<sha1>` computed like ytmusicapi.
    pub fn request_headers(&self, origin: &str) -> Vec<(String, String)> {
        todo!()
    }
}

/// Normalize pasted request info into 'key: value\n' lines. Accepts a curl
/// command, clean 'key: value' lines, or Chrome's alternating name/value lines.
pub fn headers_from_input(raw: &str) -> String {
    todo!()
}

/// `msm auth [file]`: read file arg, else pbpaste, else stdin; write
/// browser.json exactly like ytmusicapi.setup(headers_raw=...), chmod 600,
/// verify with is_logged_in and print the same messages as ymc.py.
/// Missing cookie -> message to stderr, exit 1.
pub fn setup_auth(source: Option<&str>) {
    todo!()
}

/// GET https://music.youtube.com/ with saved cookie + user-agent; true iff
/// the body contains "\"LOGGED_IN\":true". Any failure -> false.
pub fn is_logged_in() -> bool {
    todo!()
}
