//! Local ~/Music library, play history, album-art download cache.
//! Port of ymc.py art_file/scan_local/_probe/load_local_album/local_art/history.
#![allow(dead_code)]

use crate::{art_cache, hist_path, local_music, Album, LocalDir, Track, AUDIO_EXT};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// re.sub(r"[^a-zA-Z0-9]+", "_", s) — hand-rolled, no regex crate.
fn slug(s: &str) -> String {
    let mut out = String::new();
    let mut in_run = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            in_run = false;
        } else if !in_run {
            out.push('_');
            in_run = true;
        }
    }
    out
}

/// Slug is pure ASCII, so byte truncation == Python's code-point slice.
fn truncated(mut s: String, n: usize) -> String {
    s.truncate(n);
    s
}

/// Download art once into ~/.config/ymc/art/<slug>.jpg. None on failure / empty url.
pub fn art_file(title: &str, url: &str) -> Option<PathBuf> {
    if url.is_empty() {
        return None;
    }
    let cache = art_cache();
    let _ = std::fs::create_dir_all(&cache);
    let mut s = truncated(slug(title), 60);
    if s.is_empty() {
        s = "art".into();
    }
    let path = cache.join(s + ".jpg");
    if path.exists() {
        return Some(path);
    }
    // ureq treats non-2xx as Err by default (== raise_for_status)
    let mut resp = ureq::get(url)
        .config()
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .call()
        .ok()?;
    let bytes = resp.body_mut().read_to_vec().ok()?;
    std::fs::write(&path, bytes).ok()?;
    Some(path)
}

/// Immediate subdirs of ~/Music containing audio = albums (lazy, no tags).
pub fn scan_local() -> Vec<Album> {
    scan_dir(&local_music())
}

fn list_names(d: &Path) -> io::Result<Vec<String>> {
    Ok(std::fs::read_dir(d)?
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect())
}

fn scan_dir(root: &Path) -> Vec<Album> {
    let mut out = Vec::new();
    let Ok(mut names) = list_names(root) else {
        return out;
    };
    names.sort_by_key(|n| n.to_lowercase()); // sorted(key=str.lower), stable
    for name in names {
        let d = root.join(&name);
        if !d.is_dir() {
            continue;
        }
        let Ok(entries) = list_names(&d) else {
            continue;
        };
        let mut files: Vec<String> = entries
            .into_iter()
            .filter(|f| {
                let l = f.to_lowercase();
                AUDIO_EXT.iter().any(|e| l.ends_with(e))
            })
            .collect();
        files.sort();
        if !files.is_empty() {
            out.push(Album {
                title: name,
                local: Some(LocalDir { dir: d, files }),
                tracks: None,
                ..Default::default()
            });
        }
    }
    out
}

/// subprocess.run(..., timeout=secs): stdout captured, killed on timeout.
/// None on spawn failure / timeout. stdin is nulled so ffmpeg can't eat TUI keys.
fn run_timeout(cmd: &mut Command, secs: u64) -> Option<(bool, Vec<u8>)> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    // drain on a thread: big tag blobs (lyrics) could fill the pipe and stall the child
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + Duration::from_secs(secs);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    Some((status.success(), reader.join().unwrap_or_default()))
}

/// (lowercased tags, duration seconds) via ffprobe; failure -> ({}, 0.0).
pub fn probe(path: &Path) -> (BTreeMap<String, String>, f64) {
    let out = run_timeout(
        Command::new("ffprobe")
            .args(["-v", "quiet", "-print_format", "json", "-show_format"])
            .arg(path),
        15,
    );
    out.and_then(|(_, stdout)| parse_probe(&stdout)).unwrap_or_default()
}

fn parse_probe(stdout: &[u8]) -> Option<(BTreeMap<String, String>, f64)> {
    let v: Value = serde_json::from_slice(stdout).ok()?;
    let fmt = v.get("format").cloned().unwrap_or(Value::Null);
    let mut tags = BTreeMap::new();
    if let Some(t) = fmt.get("tags").and_then(Value::as_object) {
        for (k, val) in t {
            let s = val.as_str().map(str::to_owned).unwrap_or_else(|| val.to_string());
            tags.insert(k.to_lowercase(), s);
        }
    }
    // float(fmt.get("duration") or 0) — an unparsable duration fails the whole probe
    let dur = match fmt.get("duration") {
        Some(Value::String(s)) if !s.is_empty() => s.trim().parse::<f64>().ok()?,
        Some(Value::Number(n)) => n.as_f64()?,
        _ => 0.0,
    };
    Some((tags, dur))
}

/// os.path.splitext(fn)[0]: strip the last extension; leading dots don't count.
fn splitext_root(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if !name[..i].chars().all(|c| c == '.') => &name[..i],
        _ => name,
    }
}

/// Fill a local album's tracks (ffprobe) + thumb (local_art). Cached on the album.
pub fn load_local_album(album: &mut Album) {
    if album.tracks.is_some() {
        return;
    }
    let Some(local) = album.local.clone() else {
        return;
    };
    let mut tracks = Vec::new();
    for f in &local.files {
        let p = local.dir.join(f);
        let (tags, dur) = probe(&p);
        let tag = |k: &str| tags.get(k).filter(|s| !s.is_empty()).cloned();
        tracks.push(Track {
            title: tag("title").unwrap_or_else(|| splitext_root(f).to_owned()),
            artist: tag("artist").or_else(|| tag("album_artist")).unwrap_or_default(),
            album: tag("album").unwrap_or_else(|| album.title.clone()),
            duration: dur as u64,
            url: p.to_string_lossy().into_owned(), // mpv + cmusfm take the local path
            thumb: String::new(),
        });
    }
    album.tracks = Some(tracks);
    // Python stores None when there's no art; "" is our "no art"
    album.thumb = local
        .files
        .first()
        .and_then(|f| local_art(&local.dir, &local.dir.join(f)))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
}

/// cover/folder .jpg/.png, else first *.jpg/*.png, else embedded art extracted once.
pub fn local_art(dir: &Path, first_file: &Path) -> Option<PathBuf> {
    if let Some(p) = folder_image(dir) {
        return Some(p);
    }
    let cache = art_cache();
    let _ = std::fs::create_dir_all(&cache);
    let base = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let out = cache.join(format!("local_{}.jpg", truncated(slug(&base), 50)));
    if out.exists() {
        return Some(out);
    }
    let ok = run_timeout(
        Command::new("ffmpeg")
            .args(["-y", "-v", "quiet", "-i"])
            .arg(first_file)
            .args(["-an", "-c:v", "copy", "-frames:v", "1"])
            .arg(&out),
        15,
    )
    .is_some_and(|(ok, _)| ok);
    let nonempty = std::fs::metadata(&out).map(|m| m.len() > 0).unwrap_or(false);
    (ok && nonempty).then_some(out)
}

fn folder_image(dir: &Path) -> Option<PathBuf> {
    for name in ["cover.jpg", "folder.jpg", "cover.png", "folder.png"] {
        let p = dir.join(name);
        if p.exists() {
            return Some(p);
        }
    }
    // glob("*.jpg") + glob("*.png"): case-sensitive suffix, dotfiles skipped.
    // No glob metachars to escape here — we list the dir literally.
    let mut imgs: Vec<String> = list_names(dir)
        .unwrap_or_default()
        .into_iter()
        .filter(|n| !n.starts_with('.') && (n.ends_with(".jpg") || n.ends_with(".png")))
        .collect();
    imgs.sort();
    imgs.first().map(|n| dir.join(n))
}

/// history.json, [] on any error.
pub fn load_history() -> Vec<Album> {
    std::fs::read(hist_path())
        .ok()
        .and_then(|b| parse_history(&b))
        .unwrap_or_default()
}

/// Python wrote thumb=None for art-less local albums and tolerates partial
/// track dicts; normalize so one such entry doesn't blank the whole history.
fn parse_history(bytes: &[u8]) -> Option<Vec<Album>> {
    let mut v: Value = serde_json::from_slice(bytes).ok()?;
    for a in v.as_array_mut()? {
        let Some(a) = a.as_object_mut() else { continue };
        if a.get("thumb").is_some_and(Value::is_null) {
            a.insert("thumb".into(), "".into());
        }
        let Some(ts) = a.get_mut("tracks").and_then(Value::as_array_mut) else {
            continue;
        };
        for t in ts.iter_mut().filter_map(Value::as_object_mut) {
            for k in ["title", "artist", "album", "url", "thumb"] {
                if !t.get(k).is_some_and(Value::is_string) {
                    t.insert(k.into(), "".into());
                }
            }
            t.entry("duration").or_insert(0.into());
        }
    }
    serde_json::from_value(v).ok()
}

/// Prepend album, drop older same-title, keep last 5, write, return it.
pub fn record(title: &str, tracks: &[Track], thumb: &str) -> Vec<Album> {
    let mut h: Vec<Album> = load_history().into_iter().filter(|a| a.title != title).collect();
    h.insert(
        0,
        Album {
            title: title.into(),
            tracks: Some(tracks.to_vec()),
            thumb: thumb.into(),
            ..Default::default()
        },
    );
    h.truncate(5);
    let p = hist_path();
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Ok(s) = to_python_json(&h) {
        let _ = std::fs::write(&p, s);
    }
    h
}

/// json.dump default style: ", " / ": " separators, ensure_ascii=True.
/// Keeps history.json byte-identical to what the Python wrote.
struct PyFormatter;

impl serde_json::ser::Formatter for PyFormatter {
    fn begin_array_value<W: ?Sized + Write>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        if first {
            Ok(())
        } else {
            w.write_all(b", ")
        }
    }
    fn begin_object_key<W: ?Sized + Write>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        if first {
            Ok(())
        } else {
            w.write_all(b", ")
        }
    }
    fn begin_object_value<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        w.write_all(b": ")
    }
    // serde already escapes " \ and < 0x20 exactly like Python (\b \f \n \r \t, else \u00XX);
    // we add Python's ensure_ascii: anything outside ' '..'~' -> \uXXXX (surrogate pairs past BMP).
    fn write_string_fragment<W: ?Sized + Write>(&mut self, w: &mut W, frag: &str) -> io::Result<()> {
        let mut units = [0u16; 2];
        for c in frag.chars() {
            if (' '..='~').contains(&c) {
                w.write_all(&[c as u8])?;
            } else {
                for u in c.encode_utf16(&mut units) {
                    write!(w, "\\u{:04x}", u)?;
                }
            }
        }
        Ok(())
    }
}

fn to_python_json<T: Serialize>(v: &T) -> serde_json::Result<String> {
    let mut buf = Vec::new();
    v.serialize(&mut serde_json::Serializer::with_formatter(&mut buf, PyFormatter))?;
    Ok(String::from_utf8(buf).expect("ensure_ascii output is ASCII"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // crate::hist_path()/art_cache() read $HOME; serialize tests that repoint it
    static HOME_LOCK: Mutex<()> = Mutex::new(());

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("msm-local-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn with_home<R>(tag: &str, f: impl FnOnce(&Path) -> R) -> R {
        let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let d = tmpdir(tag);
        let old = std::env::var_os("HOME");
        std::env::set_var("HOME", &d);
        let r = f(&d);
        match old {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        let _ = std::fs::remove_dir_all(&d);
        r
    }

    fn touch(p: &Path) {
        std::fs::write(p, b"x").unwrap();
    }

    #[test]
    fn test_history_prepend_dedupe_cap() {
        let h = with_home("hist", |_| {
            let t = |s: &str| vec![Track { title: s.into(), ..Default::default() }];
            for i in 0..7 {
                record(&format!("Album {i}"), &t("x"), "");
            }
            record("Album 3", &t("again"), ""); // dedupe -> front
            load_history()
        });
        assert_eq!(h.len(), 5); // capped at 5
        assert_eq!(h[0].title, "Album 3"); // most recent first
        assert_eq!(h.iter().filter(|a| a.title == "Album 3").count(), 1); // no dup
        assert_eq!(h[0].tracks.as_ref().unwrap()[0].title, "again");
    }

    #[test]
    fn test_history_tolerates_python_nulls() {
        let raw = br#"[{"title": "A", "tracks": [{"title": "x"}], "thumb": null}]"#;
        let h = parse_history(raw).unwrap();
        assert_eq!(h[0].thumb, "");
        assert_eq!(h[0].tracks.as_ref().unwrap()[0].title, "x");
        assert!(parse_history(b"not json").is_none());
    }

    #[test]
    fn test_python_json_matches_json_dumps() {
        let tracks = vec![Track {
            title: "Nausicaä \"q\" \\ \n\t\u{7f} 😀 日本".into(),
            artist: "Cameron Winter".into(),
            album: "Heavy Metal".into(),
            duration: 251,
            url: "/Music/a.mp3".into(),
            thumb: "/art/x.jpg".into(),
        }];
        let h = vec![Album { title: "Heavy metal".into(), tracks: Some(tracks), ..Default::default() }];
        let ours = to_python_json(&h).unwrap();
        let rust_compact = serde_json::to_string(&h).unwrap();
        let py = Command::new("python3")
            .args(["-c", "import json,sys; sys.stdout.write(json.dumps(json.loads(sys.stdin.read())))"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn();
        let Ok(mut py) = py else { return }; // no python3: nothing to compare against
        py.stdin.take().unwrap().write_all(rust_compact.as_bytes()).unwrap();
        let out = py.wait_with_output().unwrap();
        assert_eq!(ours, String::from_utf8(out.stdout).unwrap());
    }

    #[test]
    fn test_real_history_reads() {
        // read-only: the real file must parse (never written by tests)
        let p = PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config/ymc/history.json");
        if let Ok(b) = std::fs::read(&p) {
            assert!(parse_history(&b).is_some(), "real history.json failed to parse");
        }
    }

    #[test]
    fn test_scan_local_order_and_filter() {
        let root = tmpdir("scan");
        for d in ["beta", "Alpha", "gamma", "Empty"] {
            std::fs::create_dir(root.join(d)).unwrap();
        }
        touch(&root.join("loose.mp3")); // not a dir -> skipped
        touch(&root.join("beta/b.FLAC"));
        touch(&root.join("beta/B.mp3"));
        touch(&root.join("beta/notes.txt"));
        touch(&root.join("Alpha/01.m4a"));
        touch(&root.join("gamma/cover.jpg"));
        touch(&root.join("Empty/readme"));
        let a = scan_dir(&root);
        let titles: Vec<_> = a.iter().map(|x| x.title.as_str()).collect();
        assert_eq!(titles, ["Alpha", "beta"]); // case-insensitive; audio-less dropped
        let l = a[1].local.as_ref().unwrap();
        assert_eq!(l.files, ["B.mp3", "b.FLAC"]); // plain code-point sort
        assert_eq!(l.dir, root.join("beta"));
        assert!(a[1].tracks.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_local_art_precedence() {
        let d = tmpdir("art");
        touch(&d.join("b.png"));
        touch(&d.join("a.PNG")); // glob is case-sensitive: skipped
        touch(&d.join(".hidden.jpg")); // glob skips dotfiles
        touch(&d.join("z.jpg"));
        assert_eq!(folder_image(&d), Some(d.join("b.png"))); // sorted jpg+png
        touch(&d.join("folder.png"));
        assert_eq!(folder_image(&d), Some(d.join("folder.png")));
        touch(&d.join("folder.jpg"));
        assert_eq!(folder_image(&d), Some(d.join("folder.jpg")));
        touch(&d.join("cover.jpg"));
        assert_eq!(folder_image(&d), Some(d.join("cover.jpg")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn test_local_art_embedded_cache_and_failure() {
        with_home("emb", |home| {
            let d = home.join("Music/Some [Album] (2020)");
            std::fs::create_dir_all(&d).unwrap();
            let f = d.join("01.mp3");
            touch(&f); // not real audio: ffmpeg fails -> None
            assert_eq!(local_art(&d, &f), None);
            let cached = art_cache().join("local_Some_Album_2020_.jpg");
            touch(&cached);
            assert_eq!(local_art(&d, &f), Some(cached));
        });
    }

    #[test]
    fn test_slug_and_splitext() {
        assert_eq!(slug("Heavy metal"), "Heavy_metal");
        assert_eq!(slug("a — b!!c"), "a_b_c");
        assert_eq!(slug("   "), "_");
        assert_eq!(truncated(slug(&"x".repeat(80)), 60).len(), 60);
        assert_eq!(splitext_root("01 - a.b.mp3"), "01 - a.b");
        assert_eq!(splitext_root(".mp3"), ".mp3");
        assert_eq!(splitext_root("..mp3"), "..mp3");
    }

    #[test]
    fn test_parse_probe() {
        let out = br#"{"format": {"duration": "198.73", "tags": {"TITLE": "T", "Artist": "A"}}}"#;
        let (tags, dur) = parse_probe(out).unwrap();
        assert_eq!(tags.get("title").unwrap(), "T");
        assert_eq!(tags.get("artist").unwrap(), "A");
        assert_eq!(dur, 198.73);
        assert_eq!(parse_probe(b"{}").unwrap().1, 0.0);
        assert!(parse_probe(b"").is_none());
        assert_eq!(probe(Path::new("/nonexistent/x.mp3")), (BTreeMap::new(), 0.0));
    }
}
