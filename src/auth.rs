//! YouTube Music browser auth: browser.json (ytmusicapi format), SAPISIDHASH,
//! and `msm auth`. Port of ymc.py get_yt/_headers_from_input/setup_auth/is_logged_in.
//!
//! Browser auth (not OAuth): YouTube's youtubei API rejects generic Google
//! Cloud OAuth tokens with HTTP 400, so history/recs need real website
//! session headers, which is what ytmusicapi's browser auth provides.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const YTM_DOMAIN: &str = "https://music.youtube.com";
// ytmusicapi constants.USER_AGENT — setup() overwrites whatever the paste had
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:88.0) Gecko/20100101 Firefox/88.0";

/// Saved request headers from browser.json (lowercase keys, ytmusicapi format).
#[derive(Clone, Debug, Default)]
pub struct Auth {
    pub headers: BTreeMap<String, String>,
}

impl Auth {
    /// Load ~/.config/ymc/browser.json. None if missing/corrupt/unusable
    /// (caller falls back to unauthenticated, never errors).
    ///
    /// None exactly where `YTMusic(AUTH)` raises: not a JSON object, no
    /// `authorization` containing SAPISIDHASH (ytmusicapi then takes it for an
    /// OAuth file with no credentials), no `__Secure-3PAPISID` in the cookie,
    /// or — when the file has no x-goog-visitor-id — the visitor-id fetch
    /// from music.youtube.com fails at the network level.
    pub fn load() -> Option<Auth> {
        let path = crate::auth_path();
        if !path.is_file() {
            return None;
        }
        let text = std::fs::read_to_string(path).ok()?;
        let mut auth = parse_auth_json(&text)?;
        if !auth.headers.contains_key("x-goog-visitor-id") {
            // ytmusicapi base_headers: first use fetches the visitor id with
            // the anonymous init headers and stores it alongside (not on disk)
            let vid = fetch_visitor_id()?;
            auth.headers.insert("x-goog-visitor-id".into(), vid);
        }
        Some(auth)
    }

    /// Headers to send on an authed InnerTube request to `origin`
    /// (e.g. "https://music.youtube.com"): saved headers + fresh
    /// `authorization: SAPISIDHASH <ts>_<sha1>` computed like ytmusicapi.
    ///
    /// Returns every header ytmusicapi sends on a browser-auth request — the
    /// caller must NOT add any of these itself:
    /// - every key saved in browser.json, lowercased. For a file written by
    ///   `msm auth` that is at least `accept`, `content-encoding`,
    ///   `content-type`, `cookie`, `origin`, `user-agent`, `x-goog-authuser`,
    ///   plus whatever else was pasted (`x-origin`, `referer`,
    ///   `x-youtube-client-*`, `accept-language`, ...);
    /// - `x-goog-visitor-id` (from the file, or fetched once by `load`);
    /// - `authorization`, recomputed on every call from the cookie's
    ///   `__Secure-3PAPISID` and the saved `origin` (else `x-origin`, else the
    ///   `origin` argument) — matches ytmusicapi, which hashes the saved origin.
    ///
    /// Deliberately left out: `accept-encoding` (saved as "gzip, deflate");
    /// ureq sends its own `gzip` and cannot decode deflate. The SOCS=CAI
    /// session cookie ytmusicapi sets is never sent on authed requests either
    /// (an explicit cookie header wins in requests), so it is not added.
    pub fn request_headers(&self, origin: &str) -> Vec<(String, String)> {
        let hash_origin = self
            .headers
            .get("origin")
            .or_else(|| self.headers.get("x-origin"))
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

/// Validate browser.json text the way YTMusic(auth) does (minus the network
/// visitor-id step). Keys lowercased (ytmusicapi uses a CaseInsensitiveDict).
fn parse_auth_json(text: &str) -> Option<Auth> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let obj = v.as_object()?;
    let mut headers = BTreeMap::new();
    for (k, v) in obj {
        // non-string values make every requests call raise InvalidHeader
        headers.insert(k.to_lowercase(), v.as_str()?.to_string());
    }
    // determine_auth_type: SAPISIDHASH -> BROWSER; anything else is treated as
    // OAuth and raises without oauth_credentials. ("Bearer" full-OAuth files
    // load in ytmusicapi but msm never writes them.)
    if !headers.get("authorization")?.contains("SAPISIDHASH") {
        return None;
    }
    // "Your cookie is missing the required value __Secure-3PAPISID"
    sapisid_from_cookie(headers.get("cookie")?)?;
    // no origin/x-origin -> ytmusicapi fails on every request (None + str)
    if !headers.contains_key("origin") && !headers.contains_key("x-origin") {
        return None;
    }
    Some(Auth { headers })
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

/// helpers.initialize_headers — the anonymous header set ytmusicapi uses for
/// the visitor-id fetch (and that setup() stamps over the pasted headers).
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
        .timeout_global(Some(Duration::from_secs(timeout)))
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
            if c != '}' || j < 2 {
                continue;
            }
            let tail = skip_ws(&rest[j + 1..]);
            if let Some(t) = tail.strip_prefix(')') {
                if skip_ws(t).starts_with(';') {
                    return Some(&rest[..=j]);
                }
            }
        }
    }
    None
}

/// Every `<flag>\s+<q>([^<q>]+)<q>` match, left to right, non-overlapping
/// (re.findall with one of `flags` alternated at each position).
fn find_quoted(raw: &str, flags: &[&str], q: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        if !raw.is_char_boundary(i) {
            i += 1;
            continue;
        }
        let rest = &raw[i..];
        let mut end = None;
        for f in flags {
            let Some(after) = rest.strip_prefix(f) else { continue };
            let body = skip_ws(after);
            if body.len() == after.len() {
                continue; // \s+ needs at least one
            }
            let Some(body) = body.strip_prefix(q) else { continue };
            let Some(close) = body.find(q) else { continue };
            if close == 0 {
                continue;
            }
            out.push(body[..close].to_string());
            end = Some(raw.len() - body.len() + close + 1);
            break;
        }
        i = end.unwrap_or(i + 1);
    }
    out
}

/// Normalize pasted request info into 'key: value\n' lines. Accepts a curl
/// command, clean 'key: value' lines, or Chrome's alternating name/value lines.
pub fn headers_from_input(raw: &str) -> String {
    if raw.contains("curl ") && raw.contains(" -H ") {
        // Copy as cURL: pull every -H 'k: v' / -H "k: v", plus -b/--cookie.
        let mut pairs = find_quoted(raw, &["-H"], '\'');
        pairs.extend(find_quoted(raw, &["-H"], '"'));
        let mut cookie = find_quoted(raw, &["-b", "--cookie"], '\'');
        cookie.extend(find_quoted(raw, &["-b", "--cookie"], '"'));
        if !cookie.is_empty() && !pairs.iter().any(|p| p.to_lowercase().starts_with("cookie:")) {
            pairs.push(format!("cookie: {}", cookie[0]));
        }
        return pairs.join("\n");
    }
    let mut lines: Vec<String> = raw
        .split('\n')
        .filter(|l| !l.trim().is_empty())
        .map(String::from)
        .collect();
    if !lines.is_empty() && !lines.iter().take(4).any(|l| l.contains(": ")) {
        // alternating name / value lines -> pair them up
        lines = lines
            .chunks_exact(2)
            .map(|p| format!("{}: {}", p[0], p[1]))
            .collect();
    }
    lines.join("\n")
}

/// ytmusicapi auth.browser.setup_browser(headers_raw): parse, require
/// cookie + x-goog-authuser, drop sec-* / host / content-length /
/// accept-encoding, then stamp initialize_headers() over the result.
fn setup_browser(headers_raw: &str) -> Result<BTreeMap<String, String>, String> {
    let mut user: BTreeMap<String, String> = BTreeMap::new();
    let mut chrome_remembered_key = String::new();
    for content in headers_raw.split('\n') {
        let header: Vec<&str> = content.split(": ").collect();
        if header[0].starts_with(':') {
            continue; // nothing was split or chromium headers
        }
        if header[0].ends_with(':') {
            // weird new chrome "copy-paste in separate lines" format
            chrome_remembered_key = content.replace(':', "");
        }
        if header.len() == 1 {
            if !chrome_remembered_key.is_empty() {
                user.insert(chrome_remembered_key.clone(), header[0].to_string());
            }
            continue;
        }
        user.insert(header[0].to_lowercase(), header[1..].join(": "));
    }
    let missing: Vec<&str> = ["cookie", "x-goog-authuser"]
        .into_iter()
        .filter(|m| !user.keys().any(|k| k.to_lowercase() == *m))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "The following entries are missing in your headers: {}. Please try a different \
             request (such as /browse) and make sure you are logged in.",
            missing.join(", ")
        ));
    }
    user.retain(|k, _| {
        !(k.starts_with("sec") || matches!(k.as_str(), "host" | "content-length" | "accept-encoding"))
    });
    for (k, v) in initialize_headers() {
        user.insert(k.into(), v.into());
    }
    Ok(user)
}

/// json.dumps(s, ensure_ascii=True) for a string.
fn py_json_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            _ => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{u:04x}"));
                }
            }
        }
    }
    out.push('"');
}

/// json.dump(headers, ensure_ascii=True, indent=4, sort_keys=True) — byte
/// for byte what ytmusicapi writes (no trailing newline).
fn py_json_dump(map: &BTreeMap<String, String>) -> String {
    if map.is_empty() {
        return "{}".into();
    }
    let mut out = String::from("{");
    for (i, (k, v)) in map.iter().enumerate() {
        out.push_str(if i == 0 { "\n    " } else { ",\n    " });
        py_json_str(k, &mut out);
        out.push_str(": ");
        py_json_str(v, &mut out);
    }
    out.push_str("\n}");
    out
}

/// Parse + write browser.json at `path` (0600). Err = message for stderr.
fn write_browser_json(path: &Path, headers_raw: &str) -> Result<(), String> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let headers = setup_browser(headers_raw)?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    f.write_all(py_json_dump(&headers).as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Python text-mode reads translate \r\n and \r to \n (universal newlines).
fn universal_newlines(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

fn expand_user(p: &str) -> PathBuf {
    match p.strip_prefix('~') {
        Some("") => crate::home(),
        Some(rest) if rest.starts_with('/') => crate::home().join(&rest[1..]),
        _ => PathBuf::from(p),
    }
}

/// subprocess.run(["pbpaste"], capture_output=True, text=True, timeout=5).stdout,
/// "" on any failure.
fn pbpaste() -> String {
    let Ok(mut child) = Command::new("pbpaste")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return String::new();
    };
    // read on a thread: a big cURL overflows the pipe before pbpaste exits
    let mut out = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        out.read_to_end(&mut buf).map(|_| buf)
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return String::new();
            }
        }
    }
    match reader.join() {
        Ok(Ok(buf)) => String::from_utf8(buf).map(|s| universal_newlines(&s)).unwrap_or_default(),
        _ => String::new(),
    }
}

/// SystemExit(msg): message to stderr, exit status 1.
fn die(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}

/// `msm auth [file]`: read file arg, else pbpaste, else stdin; write
/// browser.json exactly like ytmusicapi.setup(headers_raw=...), chmod 600,
/// verify with is_logged_in and print the same messages as ymc.py.
/// Missing cookie -> message to stderr, exit 1.
///
/// Save YouTube Music browser-auth headers so plays record to history and
/// recommendations load (chmod 600 — they contain your cookies). Clipboard
/// avoids the fragile terminal paste of a huge multi-line cURL.
pub fn setup_auth(source: Option<&str>) {
    match run_setup(source, &crate::config_dir(), &crate::auth_path()) {
        Ok(msg) => println!("{msg}"),
        Err(msg) => die(&msg),
    }
}

/// setup_auth body with the paths injected (testable without touching HOME).
/// Ok = final stdout message, Err = SystemExit message.
fn run_setup(source: Option<&str>, dir: &Path, path: &Path) -> Result<String, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let raw = match source.filter(|s| !s.is_empty()) {
        Some(src) => {
            let p = expand_user(src);
            let s = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            universal_newlines(&s)
        }
        None => {
            // macOS: read the copied cURL straight from the clipboard
            let mut raw = pbpaste();
            if raw.trim().is_empty() {
                println!("Paste request headers / cURL, then press Ctrl-D:");
                raw.clear();
                std::io::stdin().read_to_string(&mut raw).map_err(|e| e.to_string())?;
                raw = universal_newlines(&raw);
            }
            raw
        }
    };

    let headers_raw = headers_from_input(&raw);
    if !headers_raw.to_lowercase().contains("cookie:") {
        return Err("No 'cookie' header found in the input.\n\
             In DevTools -> Network, right-click a /browse POST -> Copy -> \
             Copy as cURL, then run `msm auth` again (it reads your clipboard)."
            .into());
    }
    write_browser_json(path, &headers_raw)?;
    Ok(if is_logged_in_at(path) {
        format!("auth saved to {} and VERIFIED logged in. run `msm` to play.", path.display())
    } else {
        "\nWARNING: headers saved but YouTube treats them as LOGGED OUT.\n\
         Recopy the request headers from a fresh, logged-in (non-incognito)\n\
         music.youtube.com tab and run `msm auth` again promptly (session\n\
         tokens rotate). History recording won't work until this verifies."
            .into()
    })
}

/// GET https://music.youtube.com/ with saved cookie + user-agent; true iff
/// the body contains "\"LOGGED_IN\":true". Any failure -> false.
///
/// The youtubei API silently downgrades unauthenticated requests, so the
/// LOGGED_IN flag on the home page is the reliable signal.
pub fn is_logged_in() -> bool {
    is_logged_in_at(&crate::auth_path())
}

fn is_logged_in_at(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(ck) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    let Some(cookie) = ck.get("cookie").and_then(|v| v.as_str()) else {
        return false;
    };
    let ua = match ck.get("user-agent") {
        None => "",
        Some(v) => match v.as_str() {
            Some(s) => s,
            None => return false,
        },
    };
    get_text(
        "https://music.youtube.com/",
        &[("cookie", cookie), ("user-agent", ua)],
        10,
    )
    .is_some_and(|body| body.contains("\"LOGGED_IN\":true"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_headers_from_input_formats() {
        let curl = "curl 'https://music.youtube.com/youtubei/v1/browse' \
                    -H 'authorization: SAPISIDHASH 1_a' -H 'cookie: SID=x; SAPISID=y' \
                    -H 'x-goog-authuser: 0' --data-raw '{}'";
        let h = headers_from_input(curl);
        assert!(h.contains("cookie: SID=x; SAPISID=y") && h.contains("x-goog-authuser: 0"));
        // -b cookie form
        assert!(headers_from_input("curl 'u' -H 'a: b' -b 'SID=z'").contains("cookie: SID=z"));
        // alternating name/value (Chrome two-line paste)
        assert!(headers_from_input("cookie\nSID=1\nx-goog-authuser\n0\n").contains("cookie: SID=1"));
        // clean key: value passthrough
        assert!(headers_from_input("cookie: SID=1\n").contains("cookie: SID=1"));
    }

    #[test]
    fn test_headers_from_input_curl_quote_order() {
        let h = headers_from_input("curl \"u\" -H \"x-a: 1\" --cookie \"SID=q\" -H 'x-b: 2'");
        // single-quoted matches first, then double-quoted, then cookie
        assert_eq!(h, "x-b: 2\nx-a: 1\ncookie: SID=q");
        // an explicit cookie header beats -b
        let h = headers_from_input("curl 'u' -H 'Cookie: A=1' -b 'B=2'");
        assert_eq!(h, "Cookie: A=1");
    }

    #[test]
    fn test_setup_browser_json_matches_ytmusicapi() {
        // expected bytes produced by ytmusicapi 1.10.3 setup_browser + json.dump
        let raw = ":authority: music.youtube.com\nHost: x\nCookie: SID=a; __Secure-3PAPISID=p/q; Z=\"1\"\n\
                   X-Goog-AuthUser: 0\nsec-ch-ua: \"Chromium\"\nContent-Length: 12\nAccept-Encoding: br\n\
                   User-Agent: Chrome\nx-weird: a: b\n\
                   accept-language: caf\u{e9} \u{1F600} \u{7f} tab\tq\"\\\nnocolon\n";
        let want = "{\n    \"accept\": \"*/*\",\n    \"accept-encoding\": \"gzip, deflate\",\n    \
                    \"accept-language\": \"caf\\u00e9 \\ud83d\\ude00 \\u007f tab\\tq\\\"\\\\\",\n    \
                    \"content-encoding\": \"gzip\",\n    \"content-type\": \"application/json\",\n    \
                    \"cookie\": \"SID=a; __Secure-3PAPISID=p/q; Z=\\\"1\\\"\",\n    \
                    \"origin\": \"https://music.youtube.com\",\n    \
                    \"user-agent\": \"Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:88.0) Gecko/20100101 Firefox/88.0\",\n    \
                    \"x-goog-authuser\": \"0\",\n    \"x-weird\": \"a: b\"\n}";
        assert_eq!(py_json_dump(&setup_browser(raw).unwrap()), want);
    }

    #[test]
    fn test_setup_browser_chrome_remembered_key_quirk() {
        // ytmusicapi keeps the remembered key's case and lets the trailing
        // empty line overwrite the last value — reproduced as-is
        let h = setup_browser("Cookie:\nSID=1\nx-goog-authuser:\n0\n").unwrap();
        assert_eq!(h["Cookie"], "SID=1");
        assert_eq!(h["x-goog-authuser"], "");
    }

    #[test]
    fn test_setup_browser_missing_authuser() {
        let e = setup_browser("cookie: SID=1").unwrap_err();
        assert!(e.starts_with("The following entries are missing in your headers: x-goog-authuser."));
    }

    #[test]
    fn test_write_browser_json_mode_600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("msm-auth-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("browser.json");
        write_browser_json(&p, "cookie: SID=1\nx-goog-authuser: 0").unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(std::fs::read_to_string(&p).unwrap().ends_with("\"x-goog-authuser\": \"0\"\n}"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

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
    fn test_parse_auth_json_validity() {
        let ok = r#"{"Authorization": "SAPISIDHASH 1_a", "cookie": "__Secure-3PAPISID=s",
                     "origin": "https://music.youtube.com", "x-goog-visitor-id": "V"}"#;
        let a = parse_auth_json(ok).unwrap();
        let hs = a.request_headers(YTM_DOMAIN);
        let auth = &hs.iter().find(|(k, _)| k == "authorization").unwrap().1;
        assert!(auth.starts_with("SAPISIDHASH ") && auth != "SAPISIDHASH 1_a");
        assert_eq!(hs.iter().filter(|(k, _)| k == "authorization").count(), 1);
        // no SAPISIDHASH -> ytmusicapi treats it as oauth and raises
        assert!(parse_auth_json(r#"{"cookie": "__Secure-3PAPISID=s", "origin": "o"}"#).is_none());
        // cookie without __Secure-3PAPISID
        assert!(parse_auth_json(
            r#"{"authorization": "SAPISIDHASH x", "cookie": "SID=1", "origin": "o"}"#
        )
        .is_none());
        assert!(parse_auth_json("[]").is_none());
        assert!(parse_auth_json("not json").is_none());
    }

    #[test]
    fn test_visitor_data_extraction() {
        let html = "<script>ytcfg.set({\"A\":{\"b\":1},\"VISITOR_DATA\":\"Cgt4\"}) ;ytcfg.set({\"X\":2});</script>";
        assert_eq!(visitor_data(html).as_deref(), Some("Cgt4"));
        assert_eq!(visitor_data("no cfg here").as_deref(), Some(""));
        assert_eq!(visitor_data("ytcfg.set({\"A\":1});").as_deref(), Some(""));
    }

    #[test]
    fn test_load_real_browser_json_readonly() {
        // read-only check against the real ~/.config/ymc/browser.json if present
        let p = crate::auth_path();
        if !p.is_file() {
            return;
        }
        let before = std::fs::read(&p).unwrap();
        let a = Auth::load().expect("real browser.json must load");
        let hs = a.request_headers(YTM_DOMAIN);
        assert!(hs.iter().any(|(k, v)| k == "authorization" && v.starts_with("SAPISIDHASH ")));
        assert!(!hs.iter().any(|(k, _)| k == "accept-encoding"));
        assert_eq!(std::fs::read(&p).unwrap(), before);
        // and it round-trips byte-for-byte through our writer
        let raw: BTreeMap<String, String> = serde_json::from_slice(&before).unwrap();
        assert_eq!(py_json_dump(&raw).as_bytes(), &before[..]);
    }

    #[test]
    fn test_run_setup_flow_temp_dir() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("msm-setup-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = dir.join(".config/ymc");
        let path = cfg.join("browser.json");
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("in.txt");
        // CRLF from a Windows-ish paste: Python text mode reads it as \n
        std::fs::write(
            &input,
            "curl 'u' -H 'x-goog-authuser: 0' -H 'authorization: SAPISIDHASH 1_a' \
             -b '__Secure-3PAPISID=z; SID=1'\r\n",
        )
        .unwrap();
        let src = input.to_str().unwrap();
        let msg = run_setup(Some(src), &cfg, &path).unwrap(); // fake cookie -> logged out
        assert!(msg.starts_with("\nWARNING: headers saved but YouTube treats them as LOGGED OUT."));
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("\"cookie\": \"__Secure-3PAPISID=z; SID=1\","));
        assert!(parse_auth_json(&saved).is_some());
        std::fs::write(&input, "a: b\n").unwrap();
        let err = run_setup(Some(src), &cfg, &path).unwrap_err();
        assert!(err.starts_with("No 'cookie' header found in the input.\n"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `cargo test auth::tests::live -- --ignored`: read-only network checks.
    #[test]
    #[ignore]
    fn live_is_logged_in_and_visitor_id() {
        println!("is_logged_in = {}", is_logged_in());
        let vid = fetch_visitor_id();
        println!("visitor id len = {:?}", vid.as_ref().map(|v| v.len()));
        assert!(vid.is_some_and(|v| !v.is_empty()));
    }
}
