//! YouTube Music browser auth: cookies pulled straight from an installed
//! browser's cookie jar via `rookie` (no more pasting a cURL by hand), plus
//! the SAPISIDHASH signing ytmusicapi's browser auth uses.
//!
//! Browser auth (not OAuth): YouTube's youtubei API rejects generic Google
//! Cloud OAuth tokens with HTTP 400, so history/recs need real website
//! session cookies.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

const YTM_DOMAIN: &str = "https://music.youtube.com";
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:88.0) Gecko/20100101 Firefox/88.0";

/// Saved request headers, built from the browser's cookie jar (lowercase keys).
#[derive(Clone, Debug, Default)]
pub struct Auth {
    pub headers: BTreeMap<String, String>,
}

impl Auth {
    /// Load cookies for youtube.com from `crate::cookie_browser()` (same
    /// browser yt-dlp's `--cookies-from-browser` reads) via `rookie`, and
    /// build the header set browser-auth requests need. None if the browser
    /// jar has no `__Secure-3PAPISID` (logged out / browser not found) or the
    /// visitor-id fetch fails at the network level.
    pub fn load() -> Option<Auth> {
        let cookies = load_cookies(&crate::cookie_browser())?;
        if cookies.is_empty() {
            return None;
        }
        let cookie_header = cookies
            .iter()
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ");
        sapisid_from_cookie(&cookie_header)?;
        let mut headers = BTreeMap::new();
        headers.insert("cookie".into(), cookie_header);
        headers.insert("origin".into(), YTM_DOMAIN.into());
        headers.insert("user-agent".into(), USER_AGENT.into());
        headers.insert("content-type".into(), "application/json".into());
        headers.insert("accept".into(), "*/*".into());
        headers.insert("x-goog-authuser".into(), "0".into());
        let vid = fetch_visitor_id()?;
        headers.insert("x-goog-visitor-id".into(), vid);
        Some(Auth { headers })
    }

    /// Headers to send on an authed InnerTube request to `origin`: the saved
    /// headers plus a fresh `authorization: SAPISIDHASH <ts>_<sha1>` computed
    /// from the cookie's `__Secure-3PAPISID` and the saved origin.
    pub fn request_headers(&self, origin: &str) -> Vec<(String, String)> {
        let hash_origin = self
            .headers
            .get("origin")
            .map(String::as_str)
            .unwrap_or(origin);
        let sapisid = self
            .headers
            .get("cookie")
            .and_then(|c| sapisid_from_cookie(c))
            .unwrap_or_default();
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut out: Vec<(String, String)> = self
            .headers
            .iter()
            .filter(|(k, _)| k.as_str() != "accept-encoding" && k.as_str() != "authorization")
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        out.push((
            "authorization".into(),
            get_authorization(&format!("{sapisid} {hash_origin}"), ts),
        ));
        out
    }
}

/// Cookies for youtube.com from the named browser (matches yt-dlp's
/// `--cookies-from-browser` names); unrecognized names fall back to scanning
/// every installed browser via `rookie::load`. None on any rookie error.
fn load_cookies(browser: &str) -> Option<Vec<rookie::enums::Cookie>> {
    let domains = Some(vec!["youtube.com".to_string()]);
    let result = match browser {
        "chrome" => rookie::chrome(domains),
        "chromium" => rookie::chromium(domains),
        "brave" => rookie::brave(domains),
        "edge" => rookie::edge(domains),
        "firefox" => rookie::firefox(domains),
        "librewolf" => rookie::librewolf(domains),
        "opera" => rookie::opera(domains),
        "opera_gx" | "operagx" => rookie::opera_gx(domains),
        "vivaldi" => rookie::vivaldi(domains),
        "arc" => rookie::arc(domains),
        "zen" => rookie::zen(domains),
        #[cfg(target_os = "macos")]
        "safari" => rookie::safari(domains),
        _ => rookie::load(domains),
    };
    result.ok()
}

/// helpers.sapisid_from_cookie: SimpleCookie over the cookie with quotes
/// stripped; last `__Secure-3PAPISID` wins.
fn sapisid_from_cookie(raw: &str) -> Option<String> {
    let raw = raw.replace('"', "");
    raw.split(';')
        .filter_map(|part| part.split_once('='))
        .filter(|(k, _)| k.trim() == "__Secure-3PAPISID")
        .map(|(_, v)| v.trim().to_string())
        .next_back()
}

/// helpers.get_authorization: "SAPISIDHASH <ts>_<sha1(ts + ' ' + auth)>",
/// auth = "<sapisid> <origin>".
fn get_authorization(auth: &str, ts: u64) -> String {
    let digest = sha1_smol::Sha1::from(format!("{ts} {auth}")).digest().to_string();
    format!("SAPISIDHASH {ts}_{digest}")
}

/// helpers.initialize_headers — the anonymous header set used for the
/// visitor-id fetch.
fn initialize_headers() -> [(&'static str, &'static str); 6] {
    [
        ("user-agent", USER_AGENT),
        ("accept", "*/*"),
        ("accept-encoding", "gzip, deflate"),
        ("content-type", "application/json"),
        ("content-encoding", "gzip"),
        ("origin", YTM_DOMAIN),
    ]
}

fn agent(timeout: u64) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(timeout)))
        // requests never raises on status; only transport errors count
        .http_status_as_error(false)
        .build()
        .into()
}

fn get_text(url: &str, headers: &[(&str, &str)], timeout: u64) -> Option<String> {
    let mut req = agent(timeout).get(url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let mut resp = req.call().ok()?;
    resp.body_mut()
        .with_config()
        .limit(64 * 1024 * 1024)
        .read_to_string()
        .ok()
}

/// helpers.get_visitor_id over the session's 30s timeout. None = request
/// failed or the ytcfg JSON didn't parse (both raise in ytmusicapi).
fn fetch_visitor_id() -> Option<String> {
    let mut hs: Vec<(&str, &str)> = initialize_headers()
        .into_iter()
        .filter(|(k, _)| *k != "accept-encoding") // ureq: gzip only
        .collect();
    hs.push(("cookie", "SOCS=CAI"));
    let html = get_text(YTM_DOMAIN, &hs, 30)?;
    visitor_data(&html)
}

/// First match of `ytcfg\.set\s*\(\s*({.+?})\s*\)\s*;` -> its VISITOR_DATA
/// ("" when there is no match or no key). None if the match isn't JSON.
fn visitor_data(html: &str) -> Option<String> {
    let Some(obj) = first_ytcfg(html) else {
        return Some(String::new());
    };
    let v: serde_json::Value = serde_json::from_str(obj).ok()?;
    let v = v.as_object()?;
    Some(v.get("VISITOR_DATA").and_then(|x| x.as_str()).unwrap_or("").to_string())
}

fn skip_ws(s: &str) -> &str {
    s.trim_start_matches(char::is_whitespace)
}

fn first_ytcfg(html: &str) -> Option<&str> {
    let mut from = 0;
    while let Some(i) = html[from..].find("ytcfg.set") {
        let at = from + i;
        from = at + 1;
        let rest = skip_ws(&html[at + "ytcfg.set".len()..]);
        let Some(rest) = rest.strip_prefix('(') else { continue };
        let rest = skip_ws(rest);
        if !rest.starts_with('{') {
            continue;
        }
        // `.+?` is at least one char and never crosses a newline
        for (j, c) in rest.char_indices().skip(1) {
            if c == '\n' {
                break;
            }
            if c == '}' && j >= 1 {
                let after = skip_ws(&rest[j + 1..]);
                if let Some(after) = after.strip_prefix(')') {
                    if skip_ws(after).starts_with(';') {
                        return Some(&rest[..=j]);
                    }
                }
            }
        }
    }
    None
}

/// `msm auth`: diagnostic that loads cookies via rookie and reports whether
/// YouTube treats the session as logged in. Auth itself is automatic now —
/// this just tells you if the browser jar has a usable session.
pub fn check_auth() {
    let browser = crate::cookie_browser();
    match Auth::load() {
        None => {
            eprintln!(
                "no usable YouTube session cookie found in {browser} \
                 (set MSM_COOKIE_BROWSER to pick a different browser, and \
                 make sure you're logged in to music.youtube.com there)."
            );
            std::process::exit(1);
        }
        Some(auth) => {
            let hs = auth.request_headers(YTM_DOMAIN);
            let cookie = hs.iter().find(|(k, _)| k == "cookie").map(|(_, v)| v.as_str()).unwrap_or("");
            let ua = hs.iter().find(|(k, _)| k == "user-agent").map(|(_, v)| v.as_str()).unwrap_or("");
            let logged_in = get_text(YTM_DOMAIN, &[("cookie", cookie), ("user-agent", ua)], 10)
                .is_some_and(|body| body.contains("\"LOGGED_IN\":true"));
            if logged_in {
                println!("found a logged-in {browser} session. run `msm` to play.");
            } else {
                eprintln!(
                    "found a {browser} cookie jar, but YouTube treats it as logged out. \
                     log in to music.youtube.com in {browser} and try again."
                );
                std::process::exit(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sapisidhash_matches_python() {
        let sid = sapisid_from_cookie("SID=a; __Secure-3PAPISID=p/q; Z=\"1\"").unwrap();
        assert_eq!(sid, "p/q");
        assert_eq!(
            get_authorization("p/q https://music.youtube.com", 1700000000),
            "SAPISIDHASH 1700000000_270d7c20af0359149878977e0a9a7ea85f0da7f0"
        );
        assert_eq!(sapisid_from_cookie("SID=a; SAPISID=b"), None);
    }

    #[test]
    fn test_visitor_data_extraction() {
        let html = "<script>ytcfg.set({\"A\":{\"b\":1},\"VISITOR_DATA\":\"Cgt4\"}) ;ytcfg.set({\"X\":2});</script>";
        assert_eq!(visitor_data(html).as_deref(), Some("Cgt4"));
        assert_eq!(visitor_data("no cfg here").as_deref(), Some(""));
        assert_eq!(visitor_data("ytcfg.set({\"A\":1});").as_deref(), Some(""));
    }

    /// `cargo test auth::tests::live -- --ignored`: read-only network + cookie
    /// jar checks against whatever's actually installed.
    #[test]
    #[ignore]
    fn live_load_and_visitor_id() {
        println!("Auth::load() = {:?}", Auth::load().is_some());
        let vid = fetch_visitor_id();
        println!("visitor id len = {:?}", vid.as_ref().map(|v| v.len()));
        assert!(vid.is_some_and(|v| !v.is_empty()));
    }
}
