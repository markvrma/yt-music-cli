//! YouTube Music client. Search, album/playlist browse, and like go through
//! `ytmapi-rs` (typed, maintained). Home-feed recs and the history/watchtime
//! ping have no `ytmapi-rs` equivalent, so those two stay hand-rolled
//! InnerTube calls (ymc.py's get_home / record_history, ported).

use crate::auth::Auth;
use crate::{Album, Item, Track};
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use ytmapi_rs::auth::{noauth::NoAuthToken, BrowserToken};
use ytmapi_rs::common::{AlbumID, LikeStatus, PlaylistID, VideoID, YoutubeID};
use ytmapi_rs::parse::PlaylistItem;
use ytmapi_rs::YtMusic;

const YTM_DOMAIN: &str = "https://music.youtube.com";
const YTM_BASE_API: &str = "https://music.youtube.com/youtubei/v1/";
const YTM_PARAMS: &str = "?alt=json";
const YTM_PARAMS_KEY: &str = "&key=AIzaSyC9XL3ZjWddXya6X74dJoCTL-WEYFDNX30";
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:88.0) Gecko/20100101 Firefox/88.0";
// ytmusicapi uses 30s; 15s so a hung request can't freeze the UI for long
const TIMEOUT: Duration = Duration::from_secs(15);
const BODY_LIMIT: u64 = 64 * 1024 * 1024; // home pages run to a few MB
const CPNA: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_";
// content-type words get_home() prepends into the artists list as a stray token
const TYPEWORDS: &[&str] = &[
    "song", "video", "album", "single", "ep", "playlist", "artist", "episode", "podcast",
];
const MRLIR: &str = "musicResponsiveListItemRenderer";
const MTRIR: &str = "musicTwoRowItemRenderer";
const MMRIR: &str = "musicMultiRowListItemRenderer";
const THUMBNAILS: &str = "/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails";
const THUMBNAIL_RENDERER: &str = "/thumbnailRenderer/musicThumbnailRenderer/thumbnail/thumbnails";
const TITLE_TEXT: &str = "/title/runs/0/text";
const TITLE_BROWSE_ID: &str = "/title/runs/0/navigationEndpoint/browseEndpoint/browseId";

/// ytmapi-rs is async (tokio); the rest of msm is sync. One small
/// current-thread runtime, used only from this module.
fn block_on<F: std::future::Future>(f: F) -> F::Output {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
    })
    .block_on(f)
}

enum YtClient {
    Auth(YtMusic<BrowserToken>),
    Anon(YtMusic<NoAuthToken>),
}

async fn try_authed(cookie: &str) -> Option<YtClient> {
    let client = ytmapi_rs::Client::new().ok()?;
    let token = BrowserToken::from_str(cookie, &client).await.ok()?;
    let yt = ytmapi_rs::YtMusicBuilder::new_with_client(client)
        .with_auth_token(token)
        .build()
        .ok()?;
    Some(YtClient::Auth(yt))
}

async fn anon_client() -> YtClient {
    YtClient::Anon(
        YtMusic::new_unauthenticated()
            .await
            .expect("anonymous ytmusic client"),
    )
}

/// Dispatch a `ytmapi-rs` simplified-query method to whichever client variant
/// is live. Both `YtMusic<BrowserToken>` and `YtMusic<NoAuthToken>` expose the
/// same inherent methods (from `impl<A: AuthToken> YtMusic<A>`), so the two
/// match arms just monomorphize separately.
macro_rules! call {
    ($self:expr, $method:ident $(, $arg:expr)*) => {
        match $self.client() {
            YtClient::Auth(c) => block_on(c.$method($($arg),*)),
            YtClient::Anon(c) => block_on(c.$method($($arg),*)),
        }
    };
}

/// Client. `auth == None` -> anonymous (browse + play only). The real
/// ytmapi-rs client is built lazily on first use (and needs network even
/// anonymously, to fetch a visitor id), so a bare `Yt { auth: None, .. }` is
/// still free to construct for tests.
pub struct Yt {
    pub auth: Option<Auth>,
    client: OnceLock<YtClient>,
}

/// get_album() result, trimmed to what msm reads.
#[derive(Clone, Debug, Default, PartialEq)]
struct AlbumPage {
    title: String,
    artists: Vec<String>,
    thumb: String,
    tracks: Vec<Item>,
}

/// get_playlist() result, trimmed to what msm reads.
#[derive(Clone, Debug, Default, PartialEq)]
struct PlaylistPage {
    title: String,
    thumb: String,
    tracks: Vec<Item>,
}

/// Authed client if a browser cookie jar has a usable session, else
/// anonymous. Never fails — the ytmapi-rs client itself is built lazily.
pub fn get_yt() -> Yt {
    Yt {
        auth: Auth::load(),
        client: OnceLock::new(),
    }
}

impl Yt {
    /// Unauthenticated client with no auth loaded. Cheap to construct (the
    /// real ytmapi-rs client is built lazily on first network use). Test-only:
    /// production always goes through `get_yt()`.
    #[cfg(test)]
    pub fn anon() -> Yt {
        Yt {
            auth: None,
            client: OnceLock::new(),
        }
    }

    /// ymc.AUTHED equivalent.
    pub fn authed(&self) -> bool {
        self.auth.is_some()
    }

    fn client(&self) -> &YtClient {
        self.client.get_or_init(|| {
            block_on(async {
                if let Some(cookie) = self.auth.as_ref().and_then(Auth::cookie_string) {
                    if let Some(c) = try_authed(cookie).await {
                        return c;
                    }
                }
                anon_client().await
            })
        })
    }

    /// yt.search(query, filter=...) with filter in {"songs","albums","playlists"}.
    pub fn search(&self, query: &str, filter: &str) -> Result<Vec<Item>, String> {
        match filter {
            "songs" => call!(self, search_songs, query)
                .map(|v| v.into_iter().map(item_from_song).collect())
                .map_err(|e| e.to_string()),
            "albums" => call!(self, search_albums, query)
                .map(|v| v.into_iter().map(item_from_album).collect())
                .map_err(|e| e.to_string()),
            "playlists" => call!(self, search_playlists, query)
                .map(|v| v.into_iter().filter_map(item_from_playlist).collect())
                .map_err(|e| e.to_string()),
            _ => Err(format!("unsupported filter {filter:?}")),
        }
    }

    /// songs + albums + playlists, first 5 of each, a failing category skipped.
    pub fn search_all(&self, query: &str) -> Vec<Item> {
        let mut out = Vec::new();
        for filt in ["songs", "albums", "playlists"] {
            if let Ok(r) = self.search(query, filt) {
                out.extend(r.into_iter().take(5));
            }
        }
        out
    }

    /// Search result OR home item -> (title, tracks, thumb); infers type when
    /// result_type is None (videoId -> song, MPRE browseId -> album, playlistId -> playlist).
    pub fn resolve_result(&self, r: &Item) -> Result<(String, Vec<Track>, String), String> {
        resolve_with(r, |id| self.get_album(id), |id| self.get_playlist(id))
    }

    /// Home rows flattened to `limit` playable items as lazy albums
    /// (tracks None, rec Some). Any failure -> empty.
    pub fn get_recs(&self, limit: usize) -> Vec<Album> {
        recs_from_rows(self.get_home(6), limit)
    }

    /// Thumbs-up; false if unauthed / not a YT track / request fails.
    pub fn like_track(&self, track: &Track) -> bool {
        if !self.authed() {
            return false; // avoids ever building a client for this call
        }
        let Some(vid) = video_id(&track.url) else {
            return false;
        };
        match self.client() {
            YtClient::Auth(c) => {
                block_on(c.rate_song(VideoID::from_raw(vid), LikeStatus::Liked)).is_ok()
            }
            YtClient::Anon(_) => false, // cookie didn't validate
        }
    }

    /// Register a play in YouTube Music history the way the web player does:
    /// a playback ping then a watchtime ping sharing one cpn. Watchtime is what
    /// makes the play stick. Requires an authed (browser) client + history not
    /// paused on the account. Errors on failure (caller swallows).
    ///
    /// Hand-rolled: this isn't a `ytmapi-rs` feature, just the raw InnerTube
    /// ping ymc.py made.
    pub fn record_history(&self, video_id: &str, watched: u64) -> Result<(), String> {
        let song = self.get_song(video_id)?;
        let pt = song.get("playbackTracking").ok_or("no playbackTracking")?;
        let (play, watch) = history_urls(pt, watched, &cpn())?;
        self.send_get(&play)?;
        self.send_get(&watch)
    }

    // ---- ytmapi-rs result -> msm Item/AlbumPage/PlaylistPage ----------------

    fn get_album(&self, browse_id: &str) -> Result<AlbumPage, String> {
        let alb =
            call!(self, get_album, AlbumID::from_raw(browse_id)).map_err(|e| e.to_string())?;
        Ok(AlbumPage {
            title: alb.title,
            artists: alb.artists.into_iter().map(|a| a.name).collect(),
            thumb: last_thumbnail(&alb.thumbnails),
            tracks: alb
                .tracks
                .into_iter()
                .map(|t| Item {
                    result_type: None,
                    title: t.title,
                    artists: Vec::new(), // album_from_page fills in the album artist
                    video_id: Some(t.video_id.get_raw().to_string()),
                    browse_id: None,
                    playlist_id: None,
                    album: None,
                    duration_seconds: parse_duration(&t.duration).unwrap_or(0),
                    thumb: String::new(),
                })
                .collect(),
        })
    }

    fn get_playlist(&self, playlist_id: &str) -> Result<PlaylistPage, String> {
        // resolve_with strips a leading "VL" (the browse-id form); ytmapi-rs
        // wants it back, same as ymc.py's get_playlist.
        let browse_id = if playlist_id.starts_with("VL") {
            playlist_id.to_string()
        } else {
            format!("VL{playlist_id}")
        };
        let details = call!(self, get_playlist_details, PlaylistID::from_raw(browse_id.as_str()))
            .map_err(|e| e.to_string())?;
        let items = call!(self, get_playlist_tracks, PlaylistID::from_raw(browse_id.as_str()))
            .map_err(|e| e.to_string())?;
        Ok(PlaylistPage {
            title: details.title,
            thumb: last_thumbnail(&details.thumbnails),
            tracks: items.into_iter().filter_map(playlist_item_to_item).collect(),
        })
    }

    fn get_home(&self, limit: usize) -> Result<Vec<HomeRow>, String> {
        let body = json!({ "browseId": "FEmusic_home" });
        let resp = self.send_request("browse", body.clone(), "")?;
        let sl = resp
            .pointer("/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer")
            .ok_or("home: no sectionListRenderer")?;
        let mut home = parse_mixed_content(
            sl.get("contents")
                .and_then(Value::as_array)
                .ok_or("home: no contents")?,
        );
        let mut token = next_continuation(sl);
        while let Some(t) = token.take() {
            if home.len() >= limit {
                break;
            }
            let resp = self.send_request(
                "browse",
                body.clone(),
                &format!("&ctoken={t}&continuation={t}"),
            )?;
            let Some(sl) = resp.pointer("/continuationContents/sectionListContinuation") else {
                break;
            };
            let rows = parse_mixed_content(
                sl.get("contents")
                    .and_then(Value::as_array)
                    .map_or(&[][..], |v| v),
            );
            if rows.is_empty() {
                break;
            }
            home.extend(rows);
            token = next_continuation(sl);
        }
        Ok(home)
    }

    fn get_song(&self, video_id: &str) -> Result<Value, String> {
        self.send_request("player", song_body(video_id, now_secs()), "")
    }

    // ---- transport for get_home / record_history (ytmusicapi YTMusicBase) ---

    /// Headers for a raw request: browser.json + fresh SAPISIDHASH when authed,
    /// ytmusicapi's defaults otherwise; plus x-goog-visitor-id if missing and
    /// the SOCS consent cookie when no cookie header is set.
    fn headers(&self) -> Result<Vec<(String, String)>, String> {
        let mut h = match &self.auth {
            Some(a) => a.request_headers(YTM_DOMAIN),
            None => initialize_headers(),
        };
        // ureq negotiates/decodes gzip itself; a forwarded "br" it can't decode
        h.retain(|(k, _)| {
            !k.eq_ignore_ascii_case("accept-encoding")
                && !k.eq_ignore_ascii_case("content-encoding")
        });
        if !has_header(&h, "x-goog-visitor-id") {
            h.push(("X-Goog-Visitor-Id".into(), visitor_id()?));
        }
        if !has_header(&h, "cookie") {
            h.push(("cookie".into(), "SOCS=CAI".into()));
        }
        Ok(h)
    }

    /// POST youtubei/v1/<endpoint> with the WEB_REMIX context merged into body.
    fn send_request(&self, endpoint: &str, body: Value, additional: &str) -> Result<Value, String> {
        let url = endpoint_url(endpoint, self.authed(), additional);
        let body = with_context(body, now_secs());
        let mut req = agent().post(&url);
        for (k, v) in self.headers()? {
            req = req.header(k.as_str(), v.as_str());
        }
        let mut resp = req.send(body.to_string()).map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let text = resp
            .body_mut()
            .with_config()
            .limit(BODY_LIMIT)
            .read_to_string()
            .map_err(|e| e.to_string())?;
        let v: Value =
            serde_json::from_str(&text).map_err(|e| format!("bad json from {endpoint}: {e}"))?;
        if status >= 400 {
            let msg = v
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("");
            return Err(format!("Server returned HTTP {status}.\n{msg}"));
        }
        Ok(v)
    }

    /// ytmusicapi _send_get_request: GET with the request headers; status ignored.
    fn send_get(&self, url: &str) -> Result<(), String> {
        let mut req = agent().get(url);
        for (k, v) in self.headers()? {
            req = req.header(k.as_str(), v.as_str());
        }
        req.call().map(|_| ()).map_err(|e| e.to_string())
    }
}

fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .http_status_as_error(false)
            .build()
            .new_agent()
    })
}

fn has_header(h: &[(String, String)], name: &str) -> bool {
    h.iter().any(|(k, _)| k.eq_ignore_ascii_case(name))
}

/// ytmusicapi initialize_headers() (minus the encoding pair ureq handles).
fn initialize_headers() -> Vec<(String, String)> {
    [
        ("user-agent", USER_AGENT),
        ("accept", "*/*"),
        ("content-type", "application/json"),
        ("origin", YTM_DOMAIN),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// Visitor id scraped once per process from the music.youtube.com page, like
/// ytmusicapi's cached base_headers (a failed fetch is retried next request).
fn visitor_id() -> Result<String, String> {
    static VISITOR: Mutex<Option<String>> = Mutex::new(None);
    if let Some(v) = VISITOR.lock().map_err(|e| e.to_string())?.clone() {
        return Ok(v);
    }
    let mut req = agent().get(YTM_DOMAIN).header("cookie", "SOCS=CAI");
    for (k, v) in initialize_headers() {
        req = req.header(k.as_str(), v.as_str());
    }
    let mut resp = req.call().map_err(|e| e.to_string())?;
    let html = resp
        .body_mut()
        .with_config()
        .limit(BODY_LIMIT)
        .read_to_string()
        .map_err(|e| e.to_string())?;
    let v = extract_visitor_id(&html);
    *VISITOR.lock().map_err(|e| e.to_string())? = Some(v.clone());
    Ok(v)
}

/// VISITOR_DATA from the first `ytcfg.set({...});` (ytmusicapi's
/// `ytcfg\.set\s*\(\s*({.+?})\s*\)\s*;`, hand-matched). "" if absent.
fn extract_visitor_id(html: &str) -> String {
    let mut from = 0;
    while let Some(p) = html[from..].find("ytcfg.set") {
        let start = from + p + "ytcfg.set".len();
        if let Some(obj) = match_cfg(&html[start..]) {
            return serde_json::from_str::<Value>(obj)
                .ok()
                .and_then(|v| {
                    v.get("VISITOR_DATA")
                        .and_then(Value::as_str)
                        .map(String::from)
                })
                .unwrap_or_default();
        }
        from = start;
    }
    String::new()
}

/// `\s*\(\s*({.+?})\s*\)\s*;` anchored at s; returns the `{...}` group.
fn match_cfg(s: &str) -> Option<&str> {
    let s = s.trim_start().strip_prefix('(')?.trim_start();
    if !s.starts_with('{') {
        return None;
    }
    for (i, c) in s.char_indices().skip(1) {
        if c == '\n' {
            return None; // `.` doesn't cross lines
        }
        if c == '}' && i >= 2 {
            let rest = s[i + 1..].trim_start();
            if let Some(r) = rest.strip_prefix(')') {
                if r.trim_start().starts_with(';') {
                    return Some(&s[..=i]);
                }
            }
        }
    }
    None
}

fn endpoint_url(endpoint: &str, authed: bool, additional: &str) -> String {
    let key = if authed { YTM_PARAMS_KEY } else { "" };
    format!("{YTM_BASE_API}{endpoint}{YTM_PARAMS}{key}{additional}")
}

/// body.update(context): WEB_REMIX client, clientVersion dated today (UTC), hl=en.
fn with_context(mut body: Value, now: u64) -> Value {
    let (y, m, d) = civil_from_days((now / 86400) as i64);
    body["context"] = json!({
        "client": {
            "clientName": "WEB_REMIX",
            "clientVersion": format!("1.{y:04}{m:02}{d:02}.01.00"),
            "hl": "en",
        },
        "user": {},
    });
    body
}

fn song_body(video_id: &str, now: u64) -> Value {
    // get_datestamp() - 1: days since epoch, minus one
    let ts = (now / 86400) as i64 - 1;
    json!({
        "playbackContext": { "contentPlaybackContext": { "signatureTimestamp": ts } },
        "video_id": video_id,
    })
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Days since 1970-01-01 -> (y, m, d) (Howard Hinnant's civil_from_days).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// 16-char client playback nonce. No rand crate: time + pid + a counter hashed.
fn cpn() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    (0..16u32)
        .map(|i| {
            let mut h = DefaultHasher::new();
            (nanos, std::process::id(), n, i).hash(&mut h);
            CPNA[(h.finish() % CPNA.len() as u64) as usize] as char
        })
        .collect()
}

/// (playback ping url, watchtime ping url) from playbackTracking.
/// Length comes from the playback url so watchtime end never exceeds track length.
fn history_urls(pt: &Value, watched: u64, cpn: &str) -> Result<(String, String), String> {
    let play = pt
        .pointer("/videostatsPlaybackUrl/baseUrl")
        .and_then(Value::as_str)
        .ok_or("no playback url")?;
    let watch = pt
        .pointer("/videostatsWatchtimeUrl/baseUrl")
        .and_then(Value::as_str)
        .ok_or("no watchtime url")?;
    let length = match query_param(play, "len") {
        Some(l) => l
            .parse::<u64>()
            .map_err(|e| format!("bad len {l:?}: {e}"))?,
        None => watched,
    };
    let et = if length > 0 {
        watched.min(length)
    } else {
        watched
    }
    .to_string();
    let base = [("ver", "2"), ("c", "WEB_REMIX"), ("cpn", cpn)];
    let play = add_query(play, &base);
    let mut wp = base.to_vec();
    wp.extend([("st", "0"), ("et", et.as_str()), ("cmt", et.as_str())]);
    Ok((play, add_query(watch, &wp)))
}

/// requests-style params: appended to any existing query with '&'.
fn add_query(url: &str, params: &[(&str, &str)]) -> String {
    let enc: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{}={}", quote_plus(k), quote_plus(v)))
        .collect();
    let (base, frag) = url
        .split_once('#')
        .map_or((url, None), |(b, f)| (b, Some(f)));
    let sep = match base.split_once('?') {
        Some((_, q)) if !q.is_empty() => "&",
        Some(_) => "",
        None => "?",
    };
    let mut out = format!("{base}{sep}{}", enc.join("&"));
    if let Some(f) = frag {
        out.push('#');
        out.push_str(f);
    }
    out
}

fn quote_plus(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn unquote_plus(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() && hex(b[i + 1]).is_some() && hex(b[i + 2]).is_some() => {
                out.push(hex(b[i + 1]).unwrap_or(0) * 16 + hex(b[i + 2]).unwrap_or(0));
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// parse_qs(urlparse(url).query)[key][0]; blank values count as absent.
fn query_param(url: &str, key: &str) -> Option<String> {
    let q = url.split_once('?')?.1;
    let q = q.split('#').next().unwrap_or("");
    q.split('&')
        .filter_map(|p| p.split_once('='))
        .find(|(k, v)| !v.is_empty() && unquote_plus(k) == key)
        .map(|(_, v)| unquote_plus(v))
}

// ---- ymc.py logic -----------------------------------------------------------

fn real_artist(name: &str) -> bool {
    !name.is_empty() && !TYPEWORDS.contains(&name.to_lowercase().as_str()) && !is_playcount(name)
}

/// `^[\d.,]+\s*[KMB]?\s*(plays|views)$` (case-insensitive), hand-parsed.
fn is_playcount(s: &str) -> bool {
    let rest = s.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c == ',');
    if rest.len() == s.len() {
        return false; // needs at least one digit/.,
    }
    let rest = rest.trim_start();
    let rest = rest
        .strip_prefix(['K', 'M', 'B', 'k', 'm', 'b'])
        .unwrap_or(rest)
        .trim_start();
    rest.eq_ignore_ascii_case("plays") || rest.eq_ignore_ascii_case("views")
}

/// Artist names joined by ", ", dropping type words and "<N> plays" tokens.
/// get_home() song items pad the artists list with a type word ("Song") and a
/// "<N> plays" token; keep only genuine artist names.
pub fn artists_str(artists: &[String]) -> String {
    artists
        .iter()
        .filter(|a| real_artist(a))
        .cloned()
        .collect::<Vec<_>>()
        .join(", ")
}

/// videoId from a YT watch url; None for local paths (no ?v=).
pub fn video_id(url: &str) -> Option<String> {
    query_param(url, "v")
}

/// ymc._track: needs a videoId (KeyError in Python otherwise).
fn track(item: &Item, album: &str, fallback_artist: &str) -> Result<Track, String> {
    let vid = item
        .video_id
        .as_deref()
        .ok_or_else(|| format!("{:?}: no videoId", item.title))?;
    let artist = artists_str(&item.artists);
    Ok(Track {
        title: item.title.clone(),
        artist: if artist.is_empty() {
            fallback_artist.to_string()
        } else {
            artist
        },
        album: album.to_string(),
        duration: item.duration_seconds,
        url: format!("https://music.youtube.com/watch?v={vid}"),
        thumb: String::new(),
    })
}

fn album_from_page(alb: &AlbumPage) -> (String, Vec<Track>, String) {
    let fb = artists_str(&alb.artists);
    let tracks = alb
        .tracks
        .iter()
        .filter_map(|t| track(t, &alb.title, &fb).ok())
        .collect();
    (alb.title.clone(), tracks, alb.thumb.clone())
}

/// resolve_result over injectable fetchers (album browseId / playlist id).
/// album/playlist expand; song = 1. Home items carry no resultType, so infer
/// it from which id field is present (videoId before playlistId: song 'radio'
/// items carry both).
fn resolve_with(
    r: &Item,
    get_album: impl FnOnce(&str) -> Result<AlbumPage, String>,
    get_playlist: impl FnOnce(&str) -> Result<PlaylistPage, String>,
) -> Result<(String, Vec<Track>, String), String> {
    let rt = match r.result_type.as_deref() {
        Some(t) => Some(t),
        None if r.video_id.is_some() => Some("song"),
        None if r
            .browse_id
            .as_deref()
            .is_some_and(|b| b.starts_with("MPRE")) =>
        {
            Some("album")
        }
        None if r.playlist_id.is_some() => Some("playlist"),
        None => None,
    };
    match rt {
        Some("album") => Ok(album_from_page(&get_album(
            r.browse_id.as_deref().ok_or("album without browseId")?,
        )?)),
        Some("playlist") => {
            let pid = r
                .playlist_id
                .as_deref()
                .or(r.browse_id.as_deref())
                .ok_or("playlist without id")?;
            let pl = get_playlist(pid.strip_prefix("VL").unwrap_or(pid))?;
            let tracks = pl
                .tracks
                .iter()
                .filter_map(|t| track(t, &pl.title, &artists_str(&t.artists)).ok())
                .collect();
            Ok((pl.title, tracks, pl.thumb))
        }
        // song / video -> single track
        _ => {
            let album = r.album.clone().unwrap_or_default();
            Ok((
                r.title.clone(),
                vec![track(r, &album, &artists_str(&r.artists))?],
                r.thumb.clone(),
            ))
        }
    }
}

/// One get_home() row: carousel title + parsed items (unparseable ones skipped).
#[derive(Clone, Debug, Default, PartialEq)]
struct HomeRow {
    title: String,
    contents: Vec<Item>,
}

/// Personalized YouTube Music home rows, flattened to `limit` playable
/// items. Each is a lazy album (tracks None) resolved on open via the stored
/// raw item. Best-effort: any failure -> [].
fn recs_from_rows(rows: Result<Vec<HomeRow>, String>, limit: usize) -> Vec<Album> {
    let Ok(rows) = rows else { return Vec::new() };
    let mut out = Vec::new();
    for item in rows.into_iter().flat_map(|r| r.contents) {
        if item.video_id.is_none() && item.browse_id.is_none() && item.playlist_id.is_none() {
            continue; // header/shelf/artist -> not playable here
        }
        if item.title.is_empty() {
            continue;
        }
        out.push(Album {
            title: item.title.clone(),
            thumb: item.thumb.clone(),
            tracks: None,
            rec: Some(item),
            local: None,
        });
        if out.len() >= limit {
            break;
        }
    }
    out
}

// ---- ytmapi-rs result adapters -> Item --------------------------------------

fn last_thumbnail(thumbs: &[ytmapi_rs::common::Thumbnail]) -> String {
    thumbs
        .iter()
        .max_by_key(|t| t.width * t.height)
        .map(|t| t.url.clone())
        .unwrap_or_default()
}

fn item_from_song(s: ytmapi_rs::parse::SearchResultSong) -> Item {
    Item {
        result_type: Some("song".into()),
        title: s.title,
        artists: vec![s.artist],
        video_id: Some(s.video_id.get_raw().to_string()),
        browse_id: None,
        playlist_id: None,
        album: s.album.map(|a| a.name),
        duration_seconds: parse_duration(&s.duration).unwrap_or(0),
        thumb: last_thumbnail(&s.thumbnails),
    }
}

fn item_from_album(a: ytmapi_rs::parse::SearchResultAlbum) -> Item {
    Item {
        result_type: Some("album".into()),
        title: a.title,
        artists: vec![a.artist],
        video_id: None,
        browse_id: Some(a.album_id.get_raw().to_string()),
        playlist_id: None,
        album: None,
        duration_seconds: 0,
        thumb: last_thumbnail(&a.thumbnails),
    }
}

fn item_from_playlist(p: ytmapi_rs::parse::SearchResultPlaylist) -> Option<Item> {
    use ytmapi_rs::parse::SearchResultPlaylist as P;
    let (title, playlist_id, thumbs) = match p {
        P::Featured(f) => (f.title, f.playlist_id, f.thumbnails),
        P::Community(c) => (c.title, c.playlist_id, c.thumbnails),
        P::Podcast(_) => return None, // not a playable playlist here
        _ => return None,             // non_exhaustive: future variants
    };
    Some(Item {
        result_type: Some("playlist".into()),
        title,
        artists: Vec::new(),
        video_id: None,
        browse_id: None,
        playlist_id: Some(playlist_id.get_raw().to_string()),
        album: None,
        duration_seconds: 0,
        thumb: last_thumbnail(&thumbs),
    })
}

fn playlist_item_to_item(it: PlaylistItem) -> Option<Item> {
    match it {
        PlaylistItem::Song(s) => Some(Item {
            result_type: None,
            title: s.title,
            artists: s.artists.into_iter().map(|a| a.name).collect(),
            video_id: Some(s.video_id.get_raw().to_string()),
            browse_id: None,
            playlist_id: None,
            album: Some(s.album.name),
            duration_seconds: parse_duration(&s.duration).unwrap_or(0),
            thumb: last_thumbnail(&s.thumbnails),
        }),
        PlaylistItem::Video(v) => Some(Item {
            result_type: None,
            title: v.title,
            artists: vec![v.channel_name],
            video_id: Some(v.video_id.get_raw().to_string()),
            browse_id: None,
            playlist_id: None,
            album: None,
            duration_seconds: parse_duration(&v.duration).unwrap_or(0),
            thumb: last_thumbnail(&v.thumbnails),
        }),
        // episodes / uploaded-library tracks / future variants: not surfaced here
        _ => None,
    }
}

// ---- ytmusicapi parsers for get_home (no ytmapi-rs equivalent) --------------

fn text(v: &Value, ptr: &str) -> Option<String> {
    v.pointer(ptr).and_then(Value::as_str).map(String::from)
}

/// Largest thumbnail URL (ytmusicapi lists them smallest-first).
fn last_thumb(v: &Value, ptr: &str) -> String {
    v.pointer(ptr)
        .and_then(Value::as_array)
        .and_then(|a| a.last())
        .and_then(|t| t.get("url"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn arr<'a>(v: &'a Value, ptr: &str) -> &'a [Value] {
    v.pointer(ptr)
        .and_then(Value::as_array)
        .map_or(&[], |a| a.as_slice())
}

/// get_flex_column_item: the column renderer iff it has text.runs.
fn flex_col(data: &Value, i: usize) -> Option<&Value> {
    let c = data
        .get("flexColumns")?
        .get(i)?
        .get("musicResponsiveListItemFlexColumnRenderer")?;
    c.pointer("/text/runs")?;
    Some(c)
}

fn is_duration(s: &str) -> bool {
    let parts: Vec<&str> = s.split(':').collect();
    parts.len() >= 2
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// `^\d([^ ])* [^ ]*$`: digit first, exactly one plain space.
fn is_views(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_digit()) && s.matches(' ').count() == 1
}

fn is_year(s: &str) -> bool {
    s.len() == 4 && s.bytes().all(|b| b.is_ascii_digit())
}

/// parse_duration: "h:m:s" -> seconds; None if any part isn't digits.
fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if s.split(':')
        .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None; // e.g. "2,343"
    }
    let mut total = 0u64;
    for (mult, part) in [1u64, 60, 3600].iter().zip(s.split(':').rev()) {
        total += mult * part.parse::<u64>().ok()?;
    }
    Some(total)
}

#[derive(Default)]
struct SongRuns {
    artists: Vec<String>,
    album: Option<String>,
    duration_seconds: Option<u64>,
}

/// parse_song_runs: even runs are artist/album/views/duration/year; odd are separators.
fn parse_song_runs(runs: &[Value]) -> SongRuns {
    let mut out = SongRuns::default();
    for (i, run) in runs.iter().enumerate().step_by(2) {
        let t = run
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if run.get("navigationEndpoint").is_some() {
            let id = text(run, "/navigationEndpoint/browseEndpoint/browseId").unwrap_or_default();
            if id.starts_with("MPRE") || id.contains("release_detail") {
                out.album = Some(t);
            } else {
                out.artists.push(t);
            }
        } else if is_views(&t) && i > 0 {
            // views
        } else if is_duration(&t) {
            out.duration_seconds = parse_duration(&t);
        } else if is_year(&t) {
            // year
        } else {
            out.artists.push(t); // artist without id
        }
    }
    out
}

/// parse_song_artists_runs: every even run is an artist name.
fn artists_runs(runs: &[Value]) -> Vec<String> {
    runs.iter()
        .step_by(2)
        .map(|r| {
            r.get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        })
        .collect()
}

fn song_artists(data: &Value, i: usize) -> Option<Vec<String>> {
    flex_col(data, i).map(|c| artists_runs(arr(c, "/text/runs")))
}

fn next_continuation(section_list: &Value) -> Option<String> {
    text(
        section_list,
        "/continuations/0/nextContinuationData/continuation",
    )
}

/// parse_mixed_content: carousel rows of two-row / list items.
fn parse_mixed_content(rows: &[Value]) -> Vec<HomeRow> {
    let mut out = Vec::new();
    for row in rows {
        if let Some(d) = row.get("musicDescriptionShelfRenderer") {
            // contents is a text blob, nothing playable
            out.push(HomeRow {
                title: text(d, "/header/runs/0/text").unwrap_or_default(),
                contents: Vec::new(),
            });
            continue;
        }
        let Some(results) = row.as_object().and_then(|o| o.values().next()) else {
            continue;
        };
        let Some(contents) = results.get("contents").and_then(Value::as_array) else {
            continue;
        };
        out.push(HomeRow {
            title: text(
                results,
                "/header/musicCarouselShelfBasicHeaderRenderer/title/runs/0/text",
            )
            .unwrap_or_default(),
            contents: contents.iter().filter_map(parse_home_item).collect(),
        });
    }
    out
}

fn parse_home_item(result: &Value) -> Option<Item> {
    if let Some(d) = result.get(MTRIR) {
        let mut it = Item {
            title: text(d, TITLE_TEXT).unwrap_or_default(),
            thumb: last_thumb(d, THUMBNAIL_RENDERER),
            ..Default::default()
        };
        let page_type = text(
            d,
            "/title/runs/0/navigationEndpoint/browseEndpoint/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType",
        );
        match page_type.as_deref() {
            None => {
                if let Some(pid) = text(d, "/navigationEndpoint/watchPlaylistEndpoint/playlistId") {
                    it.playlist_id = Some(pid); // watch playlist
                } else {
                    // song
                    it.video_id = Some(text(d, "/navigationEndpoint/watchEndpoint/videoId")?);
                    it.playlist_id = text(d, "/navigationEndpoint/watchEndpoint/playlistId");
                    let sr = parse_song_runs(arr(d, "/subtitle/runs"));
                    it.artists = sr.artists;
                    it.album = sr.album;
                    it.duration_seconds = sr.duration_seconds.unwrap_or(0);
                }
            }
            Some("MUSIC_PAGE_TYPE_ALBUM") => {
                it.artists = arr(d, "/subtitle/runs")
                    .iter()
                    .filter(|r| r.get("navigationEndpoint").is_some())
                    .map(|r| {
                        r.get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string()
                    })
                    .collect();
                it.browse_id = text(d, TITLE_BROWSE_ID);
            }
            Some("MUSIC_PAGE_TYPE_ARTIST" | "MUSIC_PAGE_TYPE_PODCAST_SHOW_DETAIL_PAGE") => {
                it.browse_id = text(d, TITLE_BROWSE_ID);
            }
            Some("MUSIC_PAGE_TYPE_PLAYLIST") => {
                it.playlist_id =
                    text(d, TITLE_BROWSE_ID).map(|b| b.get(2..).unwrap_or("").to_string());
            }
            Some(_) => return None,
        }
        return Some(it);
    }
    if let Some(d) = result.get(MRLIR) {
        // parse_song_flat
        let c0 = flex_col(d, 0);
        let mut it = Item {
            title: c0
                .and_then(|c| text(c, "/text/runs/0/text"))
                .unwrap_or_default(),
            video_id: c0
                .and_then(|c| text(c, "/text/runs/0/navigationEndpoint/watchEndpoint/videoId")),
            artists: song_artists(d, 1).unwrap_or_default(),
            thumb: last_thumb(d, THUMBNAILS),
            ..Default::default()
        };
        if let Some(c2) = flex_col(d, 2) {
            if c2.pointer("/text/runs/0/navigationEndpoint").is_some() {
                it.album = text(c2, "/text/runs/0/text");
            }
        }
        return Some(it);
    }
    // parse_episode
    let d = result.get(MMRIR)?;
    Some(Item {
        title: text(d, TITLE_TEXT).unwrap_or_default(),
        video_id: text(d, "/onTap/watchEndpoint/videoId"),
        browse_id: text(d, TITLE_BROWSE_ID),
        thumb: last_thumb(d, THUMBNAILS),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn item(title: &str, vid: Option<&str>, artists: &[&str]) -> Item {
        Item {
            title: title.into(),
            video_id: vid.map(String::from),
            artists: artists.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn no_playlist(_: &str) -> Result<PlaylistPage, String> {
        panic!("get_playlist called")
    }

    fn no_album(_: &str) -> Result<AlbumPage, String> {
        panic!("get_album called")
    }

    // ---- get_home fixture: parser reproduces ytmusicapi's parsed fields ----

    #[test]
    fn home_fixture_matches_ytmusicapi() {
        let fx = fixture("home");
        let call = &fx["calls"][0];
        assert_eq!(call["body"]["browseId"], "FEmusic_home");
        let sl = call["resp"]
            .pointer("/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer")
            .unwrap();
        let rows = parse_mixed_content(sl["contents"].as_array().unwrap());
        let e = fx["expected"].as_array().unwrap();
        assert_eq!(rows.len(), e.len());
        for (row, e) in rows.iter().zip(e) {
            assert_eq!(row.title, e["title"].as_str().unwrap());
            let items = &row.contents;
            let ee = e["contents"].as_array().unwrap();
            assert_eq!(items.len(), ee.len());
            for (it, e) in items.iter().zip(ee) {
                assert_eq!(it.title, e["title"].as_str().unwrap_or(""));
                assert_eq!(it.video_id, e["videoId"].as_str().map(String::from));
                assert_eq!(it.browse_id, e["browseId"].as_str().map(String::from));
            }
        }
        // an album rec and a "Song"-padded rec both resolve the right way
        let recs = recs_from_rows(Ok(rows), 50);
        assert!(recs.iter().any(|a| a
            .rec
            .as_ref()
            .unwrap()
            .browse_id
            .as_deref()
            .is_some_and(|b| b.starts_with("MPRE"))));
        let padded = recs
            .iter()
            .filter_map(|a| a.rec.as_ref())
            .find(|r| r.artists.first().map(String::as_str) == Some("Song"));
        assert_eq!(artists_str(&padded.unwrap().artists), "Nirvana");
    }

    #[test]
    fn song_fixture_playback_tracking_to_history_urls() {
        let fx = fixture("song");
        let call = &fx["calls"][0];
        let vid = call["body"]["video_id"].as_str().unwrap();
        let body = song_body(vid, 1_700_000_000);
        assert_eq!(body["video_id"], call["body"]["video_id"]);
        assert_eq!(
            body.pointer("/playbackContext/contentPlaybackContext/signatureTimestamp")
                .unwrap(),
            19674
        ); // days since epoch - 1
        let pt = &call["resp"]["playbackTracking"];
        let e = &fx["expected"];
        let (play, watch) = history_urls(pt, 30, "abcdefghijklmnop").unwrap();
        assert_eq!(
            play,
            format!(
                "{}&ver=2&c=WEB_REMIX&cpn=abcdefghijklmnop",
                e["videostatsPlaybackUrl"].as_str().unwrap()
            )
        );
        assert_eq!(
            watch,
            format!(
                "{}&ver=2&c=WEB_REMIX&cpn=abcdefghijklmnop&st=0&et=30&cmt=30",
                e["videostatsWatchtimeUrl"].as_str().unwrap()
            )
        );
        // len=256 in the fixture caps et
        let (_, watch) = history_urls(pt, 1000, "x").unwrap();
        assert!(watch.ends_with("&st=0&et=256&cmt=256"));
    }

    // ---- request building ---------------------------------------------------

    #[test]
    fn history_len_missing_falls_back_to_watched() {
        let pt = json!({"videostatsPlaybackUrl": {"baseUrl": "https://s.youtube.com/api/stats/playback?docid=x"},
                        "videostatsWatchtimeUrl": {"baseUrl": "https://s.youtube.com/api/stats/watchtime?docid=x"}});
        let (play, watch) = history_urls(&pt, 42, "c").unwrap();
        assert_eq!(
            play,
            "https://s.youtube.com/api/stats/playback?docid=x&ver=2&c=WEB_REMIX&cpn=c"
        );
        assert!(watch.ends_with("&et=42&cmt=42"));
        assert!(history_urls(&json!({}), 1, "c").is_err());
    }

    #[test]
    fn endpoint_url_shapes_match_ytmusicapi() {
        assert_eq!(
            endpoint_url("like/like", true, ""),
            "https://music.youtube.com/youtubei/v1/like/like?alt=json&key=AIzaSyC9XL3ZjWddXya6X74dJoCTL-WEYFDNX30"
        );
        assert_eq!(
            endpoint_url("search", false, "&ctoken=a"),
            "https://music.youtube.com/youtubei/v1/search?alt=json&ctoken=a"
        );
    }

    #[test]
    fn like_track_refuses_when_unauthed_or_local() {
        // never touches the network: authed() is false, so .client() is never built
        let yt = Yt::anon();
        let t = Track {
            url: "https://music.youtube.com/watch?v=abc".into(),
            ..Default::default()
        };
        assert!(!yt.like_track(&t));
    }

    #[test]
    fn context_is_web_remix_dated_utc() {
        let b = with_context(json!({"browseId": "x"}), 1_700_000_000); // 2023-11-14
        assert_eq!(b["browseId"], "x");
        assert_eq!(b["context"]["client"]["clientName"], "WEB_REMIX");
        assert_eq!(b["context"]["client"]["clientVersion"], "1.20231114.01.00");
        assert_eq!(b["context"]["client"]["hl"], "en");
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11016), (2000, 2, 29));
    }

    #[test]
    fn cpn_is_16_url_safe_chars_and_varies() {
        let a = cpn();
        assert_eq!(a.len(), 16);
        assert!(a.bytes().all(|b| CPNA.contains(&b)));
        assert_ne!(a, cpn());
    }

    #[test]
    fn visitor_id_from_ytcfg() {
        let html = "<script>ytcfg.set = f;</script><script>ytcfg.set({\"A\":{\"b\":1},\"VISITOR_DATA\":\"Cgt%3D\"}) ;</script>";
        assert_eq!(extract_visitor_id(html), "Cgt%3D");
        assert_eq!(extract_visitor_id("<html>nothing</html>"), "");
    }

    #[test]
    fn run_classifiers_match_ytmusicapi_regexes() {
        assert_eq!(parse_duration("4:16"), Some(256));
        assert_eq!(parse_duration("1:00:01"), Some(3601));
        assert_eq!(parse_duration("2,343"), None);
        assert!(is_duration("3:05") && !is_duration("305") && !is_duration("3:"));
        assert!(is_views("1.2M views") && !is_views("Radiohead") && !is_views("12 a b"));
        assert!(is_year("2007") && !is_year("20077"));
        let sr = parse_song_runs(&[
            json!({"text": "Radiohead", "navigationEndpoint": {"browseEndpoint": {"browseId": "UC1"}}}),
            json!({"text": " • "}),
            json!({"text": "In Rainbows", "navigationEndpoint": {"browseEndpoint": {"browseId": "MPREb_1"}}}),
            json!({"text": " • "}),
            json!({"text": "4:16"}),
        ]);
        assert_eq!(sr.artists, vec!["Radiohead"]);
        assert_eq!(sr.album.as_deref(), Some("In Rainbows"));
        assert_eq!(sr.duration_seconds, Some(256));
    }

    // ---- ported from test_ymc.py ------------------------------------------

    #[test]
    fn test_album_parse_drops_unavailable() {
        let alb = AlbumPage {
            title: "X".into(),
            artists: vec!["Band".into()],
            thumb: String::new(),
            tracks: vec![
                Item {
                    duration_seconds: 100,
                    ..item("A", Some("v1"), &["Band"])
                },
                item("Gone", None, &[]), // unavailable -> dropped
            ],
        };
        let (title, tracks, _thumb) = album_from_page(&alb);
        assert_eq!(title, "X");
        assert_eq!(tracks.len(), 1);
        assert!(tracks[0].url.ends_with("v1"));
    }

    #[test]
    fn test_resolve_song_is_single_track() {
        let r = Item {
            result_type: Some("song".into()),
            album: Some("In Rainbows".into()),
            duration_seconds: 256,
            ..item("Nude", Some("v9"), &["Radiohead"])
        };
        let (title, tracks, _thumb) = resolve_with(&r, no_album, no_playlist).unwrap();
        assert_eq!(title, "Nude");
        assert!(tracks.len() == 1 && tracks[0].url.ends_with("v9"));
        assert_eq!(tracks[0].album, "In Rainbows");
    }

    fn al_page() -> AlbumPage {
        AlbumPage {
            title: "Al".into(),
            artists: vec!["B".into()],
            tracks: vec![item("t", Some("v"), &[])],
            ..Default::default()
        }
    }

    #[test]
    fn test_resolve_album_delegates() {
        let r = Item {
            result_type: Some("album".into()),
            browse_id: Some("MPRE".into()),
            ..Default::default()
        };
        let (title, tracks, _) = resolve_with(&r, |_| Ok(al_page()), no_playlist).unwrap();
        assert!(title == "Al" && tracks.len() == 1);
        assert_eq!(tracks[0].artist, "B"); // falls back to the album artist
    }

    #[test]
    fn test_artists_str_drops_playcount_token() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // get_home() song items lump a "<N> plays" token into artists -> must drop
        assert_eq!(artists_str(&a(&["The 1975", "315M plays"])), "The 1975");
        assert_eq!(artists_str(&a(&["Coldplay", "2.2B plays"])), "Coldplay");
        assert_eq!(artists_str(&a(&["A", "1.2M views"])), "A");
        // real multi-artist rows unaffected
        assert_eq!(artists_str(&a(&["A", "B"])), "A, B");
        // type-word token ("Song") leaked by get_home is dropped
        assert_eq!(artists_str(&a(&["Song", "Faerybabyy"])), "Faerybabyy");
        // YT puts a no-break space between number and magnitude
        assert_eq!(artists_str(&a(&["A", "140K\u{a0}views", "7 plays"])), "A");
    }

    #[test]
    fn test_video_id_parses_yt_url_and_skips_local() {
        assert_eq!(
            video_id("https://music.youtube.com/watch?v=abc").as_deref(),
            Some("abc")
        );
        assert_eq!(
            video_id("https://music.youtube.com/watch?v=abc&list=RDAMVMxyz").as_deref(),
            Some("abc")
        );
        assert_eq!(video_id("/Users/mark/Music/Album/01 - Song.flac"), None);
        assert_eq!(video_id("https://music.youtube.com/watch?v="), None);
    }

    #[test]
    fn test_resolve_home_song_without_resulttype() {
        // radio playlistId present -> videoId must win
        let r = Item {
            playlist_id: Some("RDAMVMv9".into()),
            ..item("Nude", Some("v9"), &["Radiohead"])
        };
        let (title, tracks, _) = resolve_with(&r, no_album, no_playlist).unwrap();
        assert!(title == "Nude" && tracks.len() == 1 && tracks[0].url.ends_with("v9"));
    }

    #[test]
    fn test_resolve_home_album_without_resulttype() {
        let r = Item {
            browse_id: Some("MPREb_x".into()),
            ..item("Al", None, &[])
        };
        let mut called = Vec::new();
        let (title, tracks, _) = resolve_with(
            &r,
            |id| {
                called.push(id.to_string());
                Ok(al_page())
            },
            no_playlist,
        )
        .unwrap();
        assert!(title == "Al" && tracks.len() == 1);
        assert_eq!(called, vec!["MPREb_x"]);
    }

    #[test]
    fn test_get_recs_flattens_and_drops_nonplayable() {
        let rows = vec![
            HomeRow {
                title: "Row1".into(),
                contents: vec![
                    item("Header only", None, &[]), // no id -> dropped
                    item("Song", Some("v1"), &[]),
                ],
            },
            HomeRow {
                title: "Row2".into(),
                contents: vec![
                    Item {
                        browse_id: Some("MPREb".into()),
                        ..item("Album", None, &[])
                    },
                    item("", Some("v2"), &[]), // no title -> dropped
                ],
            },
        ];
        let recs = recs_from_rows(Ok(rows), 5);
        assert_eq!(
            recs.iter().map(|r| r.title.as_str()).collect::<Vec<_>>(),
            vec!["Song", "Album"]
        );
        assert!(recs.iter().all(|r| r.tracks.is_none() && r.rec.is_some())); // lazy + raw kept
    }

    #[test]
    fn test_get_recs_swallows_errors() {
        assert!(recs_from_rows(Err("offline".into()), 5).is_empty());
    }

    /// Live anonymous search against music.youtube.com (read-only).
    /// `cargo test ytm::tests::live -- --ignored`
    #[test]
    #[ignore]
    fn live_anonymous_search() {
        let yt = Yt::anon();
        let r = yt.search_all("radiohead nude");
        assert!(
            r.iter()
                .any(|i| i.result_type.as_deref() == Some("song") && i.video_id.is_some()),
            "{r:?}"
        );
        assert!(r.iter().any(|i| i.result_type.as_deref() == Some("album")));
        assert!(r
            .iter()
            .any(|i| i.result_type.as_deref() == Some("playlist")));
        let alb = r
            .iter()
            .find(|i| i.result_type.as_deref() == Some("album"))
            .unwrap();
        let (_, tracks, _) = yt.resolve_result(alb).unwrap();
        assert!(!tracks.is_empty());
        let recs = yt.get_recs(5);
        assert!(!recs.is_empty());
        let _ = yt.resolve_result(recs[0].rec.as_ref().unwrap()).unwrap();
    }

    /// Live authed read-only smoke (search, home, album/playlist browse).
    /// Never rates or pings history. `cargo test ytm::tests::live -- --ignored`
    #[test]
    #[ignore]
    fn live_authed_read_only() {
        let yt = get_yt();
        assert!(yt.authed(), "no usable browser session");
        let r = yt.search_all("radiohead in rainbows");
        assert!(
            r.iter().any(|i| i.result_type.as_deref() == Some("song")),
            "{r:?}"
        );
        let pl = r
            .iter()
            .find(|i| i.result_type.as_deref() == Some("playlist"))
            .unwrap();
        let (_, tracks, _) = yt.resolve_result(pl).unwrap();
        assert!(!tracks.is_empty());
        let recs = yt.get_recs(5);
        assert!(!recs.is_empty());
        for a in &recs {
            let (t, tracks, _) = yt.resolve_result(a.rec.as_ref().unwrap()).unwrap();
            eprintln!("REC {t:?}: {} tracks", tracks.len());
        }
    }
}
