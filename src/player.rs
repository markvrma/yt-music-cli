//! One background mpv for the session, driven over its JSON IPC socket;
//! yt-dlp fetch cache; cmusfm scrobble bridge. Port of ymc.py's cmusfm /
//! cache_path / fetch / IPC / Player.
#![allow(dead_code)]

use crate::ytm::{self, Yt};
use crate::Track;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

// Both channels folded into the left one, right muted. Halved so the
// fold-down cannot clip. Two syntax traps, both verified live against mpv
// v0.41.0 + libavfilter 11: a bare pan=... trips mpv's own arg parser on
// the | separators (hence lavfi=[...]), and the mute has to be 0*c0, not
// 0 -- ffmpeg wants a channel name in every term. Neither shows up until
// the filter graph is built, which happens at playback, not at toggle.
const LEFT_EAR_AF: &str = "lavfi=[pan=stereo|c0=0.5*c0+0.5*c1|c1=0*c0]";

/// Lock that survives poisoning: quit() must still work from a panic hook.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Python truthiness of an mpv reply (None / False / "" / 0 / [] / {} -> false).
fn truthy(v: &Option<Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

// ---- cmusfm scrobble bridge ------------------------------------------------

/// Kill any stale cmusfm server; the next cmusfm call forks a fresh one.
///
/// cmusfm's daemon caches a Last.fm failure for 30 min (SERVICE_RETRY_DELAY) and
/// silently drops now-playing + scrobbles while in that state — after a
/// sleep/wake it can sit there for the rest of the session with no error
/// anywhere. Cheaper to start each msm session with a new daemon.
fn cmusfm_reset() {
    // SIGKILL, not SIGTERM: a TERM'd server unblocks its poll() but then hangs
    // in the curl teardown with the listening socket still open, so
    // cmusfm_server_check() keeps connecting to a corpse and every status
    // message is silently dropped. The leftover socket file is harmless — a
    // fresh server unlinks it before bind.
    // ponytail: blunt pkill. If you ever run cmus alongside msm, its in-flight
    // track loses its scrobble — narrow to a socket health-check if that bites.
    let _ = Command::new("pkill").args(["-9", "-x", "cmusfm"]).status();
}

/// argv for cmusfm as cmus's status_display_program.
pub fn cmusfm_argv(status: &str, track: Option<&Track>) -> Vec<String> {
    let mut args: Vec<String> = vec!["cmusfm".into(), "status".into(), status.into()];
    if let Some(t) = track {
        args.extend([
            "file".into(),
            t.url.clone(),
            "artist".into(),
            t.artist.clone(),
            "album".into(),
            t.album.clone(),
            "title".into(),
            t.title.clone(),
            "duration".into(),
            t.duration.to_string(),
        ]);
    }
    args
}

/// Fire cmusfm the way cmus does as status_display_program.
fn cmusfm(status: &str, track: Option<&Track>) {
    let argv = cmusfm_argv(status, track);
    // New process group (Python: start_new_session): the daemon is forked by
    // *this* client, so without it it lands in msm's process group and Ctrl+Z
    // on msm freezes it too. A stopped daemon is the worst failure mode there
    // is: connect() to its listening socket still succeeds, so
    // cmusfm_server_check() calls it healthy and every later status message
    // is written into a socket nobody reads — no error, exit 0, nothing
    // scrobbled. process_group(0) instead of setsid via pre_exec: std, no
    // unsafe, and leaving the terminal's foreground group is all that's
    // needed to dodge SIGTSTP.
    let _ = Command::new(&argv[0])
        .args(&argv[1..])
        .process_group(0)
        .status();
}

// ---- yt-dlp cache ------------------------------------------------------------

/// Local file mpv plays: ~/.cache/msm/<vid>.m4a for YT, the path itself for local.
///
/// googlevideo now 403s any open-ended `Range: bytes=0-`, which is the only
/// request ffmpeg knows how to make, so mpv cannot stream YouTube at all —
/// album art loaded but playback and duration never did. yt-dlp fetches in
/// bounded chunks, so it downloads and mpv plays the file.
pub fn cache_path(track: &Track) -> String {
    // ponytail: --extract-audio --audio-format m4a in fetch() forces the
    // extension regardless of source itag, so path is known before the
    // download finishes and mpv can be queued up front.
    // No query string -> no ?v= (urlparse semantics), so skip the parse.
    let vid = if track.url.contains('?') {
        ytm::video_id(&track.url)
    } else {
        None
    };
    match vid {
        Some(v) => crate::stream_cache()
            .join(format!("{v}.m4a"))
            .to_string_lossy()
            .into_owned(),
        None => track.url.clone(),
    }
}

/// Download into the cache if missing (yt-dlp argv exactly as ymc.py). -> path.
pub fn fetch(track: &Track) -> String {
    let path = cache_path(track);
    if path == track.url || Path::new(&path).exists() {
        return path;
    }
    let _ = std::fs::create_dir_all(crate::stream_cache());
    // no --no-part: a killed download leaves a .part file, not a truncated
    // cache hit that would play as a few seconds of silence forever after.
    // The web client is the only one still handing out a full-length URL, and
    // it needs both a signed-in cookie jar and a PO token provider — without
    // them googlevideo serves the first ~1MB and then 403s.
    // itag 140 now needs a PO token msm doesn't provide, so it 404s most of
    // the time — fall back to 18 (muxed mp4, no PO token needed) and strip
    // the video track back down to m4a so cache_path's extension still holds.
    let _ = Command::new("yt-dlp")
        .args(["-q", "--no-warnings", "-f", "140/18/bestaudio"])
        .args(["--extract-audio", "--audio-format", "m4a"])
        .args(["--cookies-from-browser", &crate::cookie_browser()])
        .args(["--extractor-args", "youtube:player_client=web"])
        .args(["-o", &path, &track.url])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    path
}

// ---- mpv IPC ---------------------------------------------------------------

/// One mpv IPC connection. Matches request_id and skips async events —
/// mpv interleaves event messages on the socket, so first-reply-wins is wrong.
struct Ipc {
    sock: UnixStream,
    buf: Vec<u8>,
    rid: u64,
}

impl Ipc {
    /// 50 x 100ms retries; gives up early once mpv has exited (`alive` false).
    fn connect(path: &str, mut alive: impl FnMut() -> bool) -> Result<Ipc, String> {
        for _ in 0..50 {
            if !alive() {
                return Err("mpv exited".into());
            }
            if let Ok(sock) = UnixStream::connect(path) {
                let _ = sock.set_read_timeout(Some(Duration::from_secs(2)));
                return Ok(Ipc { sock, buf: Vec::new(), rid: 0 });
            }
            thread::sleep(Duration::from_millis(100));
        }
        Err("mpv IPC socket never appeared".into())
    }

    /// Reply `data` iff error == "success"; None on any IO/JSON failure.
    fn cmd(&mut self, command: Value) -> Option<Value> {
        self.rid += 1;
        let rid = self.rid;
        let mut out = json!({"command": command, "request_id": rid}).to_string();
        out.push('\n');
        self.sock.write_all(out.as_bytes()).ok()?;
        loop {
            let nl = loop {
                if let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
                    break i;
                }
                let mut chunk = [0u8; 4096];
                let n = self.sock.read(&mut chunk).ok()?;
                if n == 0 {
                    return None;
                }
                self.buf.extend_from_slice(&chunk[..n]);
            };
            let line: Vec<u8> = self.buf.drain(..=nl).take(nl).collect();
            if line.is_empty() {
                continue;
            }
            let msg: Value = serde_json::from_slice(&line).ok()?;
            // ignore events + stale replies
            if msg.get("request_id").and_then(Value::as_u64) == Some(rid) {
                if msg.get("error").and_then(Value::as_str) != Some("success") {
                    return None;
                }
                return msg.get("data").filter(|d| !d.is_null()).cloned();
            }
        }
    }
}

// ---- Player ----------------------------------------------------------------

/// State the watcher writes and the main thread reads.
#[derive(Default)]
struct Shared {
    by_url: HashMap<String, Track>,
    current: Option<Track>,
}

/// Side effect the watch loop decided on; executed outside the Shared lock so
/// a slow cmusfm never stalls the TUI's progress() poll.
#[derive(Debug, PartialEq)]
enum Event {
    Cmusfm(&'static str, Option<Track>),
    Record(Track),
}

/// Watch-loop memory between one-second polls.
#[derive(Default)]
struct Watch {
    last_path: Option<String>,
    last_pause: Option<bool>,
    last_pos: f64,
    recorded: bool, // YT history logged for the current track yet?
}

impl Watch {
    fn step(
        &mut self,
        path: Option<String>,
        pause: Option<bool>,
        raw: Option<f64>,
        s: &mut Shared,
    ) -> Vec<Event> {
        let mut ev = Vec::new();
        let pos = raw.unwrap_or(0.0);
        // Repeat-all over a one-track playlist replays the same file, so
        // `path` never changes and cmusfm would never hear about the play
        // that just finished. A rewind is the only signal left. Nothing
        // seeks backwards in msm, so this cannot be a user scrub.
        let replayed = raw.is_some() && path == self.last_path && pos + 2.0 < self.last_pos;
        if path.is_some() && (path != self.last_path || replayed) {
            s.current = s.by_url.get(path.as_deref().unwrap_or_default()).cloned();
            if let Some(c) = &s.current {
                ev.push(Event::Cmusfm("playing", Some(c.clone())));
            }
            self.last_path = path;
            self.last_pause = Some(false);
            self.recorded = false;
        } else if self.last_path.is_some() && path.is_none() {
            // Playlist ran out: mpv idles instead of exiting, so without an
            // explicit stop cmusfm sits on the last track and never submits it.
            ev.push(Event::Cmusfm("stopped", s.current.take()));
            self.last_path = None;
        } else if s.current.is_some() && pause.is_some() && pause != self.last_pause {
            let status = if pause == Some(true) { "paused" } else { "playing" };
            ev.push(Event::Cmusfm(status, s.current.clone()));
            self.last_pause = pause;
        }
        self.last_pos = pos;
        if let Some(c) = &s.current {
            if !self.recorded && pos >= 30.0 {
                ev.push(Event::Record(c.clone()));
                self.recorded = true;
            }
        }
        ev
    }
}

/// Poll mpv once a second until it exits; fire cmusfm on track/pause change
/// and record to YouTube Music history once a track has played >=30s.
fn watch_loop(
    ipc: &mut Ipc,
    mut alive: impl FnMut() -> bool,
    shared: &Mutex<Shared>,
    tick: Duration,
    mut emit: impl FnMut(Event),
) {
    let mut w = Watch::default();
    while alive() {
        let path = ipc
            .cmd(json!(["get_property", "path"]))
            .and_then(|v| v.as_str().map(String::from))
            .filter(|p| !p.is_empty());
        let pause = ipc.cmd(json!(["get_property", "pause"])).and_then(|v| v.as_bool());
        let raw = ipc.cmd(json!(["get_property", "time-pos"])).and_then(|v| v.as_f64());
        let events = w.step(path, pause, raw, &mut lock(shared));
        events.into_iter().for_each(&mut emit);
        thread::sleep(tick);
    }
    let cur = lock(shared).current.clone();
    emit(Event::Cmusfm("stopped", cur));
}

/// Log a play to YouTube Music history (best-effort, off-thread so a
/// slow/hung request can't stall the watch loop). Local files are skipped.
///
/// Sends both the playback-start and watchtime pings with one shared cpn —
/// the watchtime ping is what actually registers the play (add_history_item
/// alone only fires playback, which YT often ignores).
fn record_yt(yt: &Arc<Yt>, track: &Track, watched: u64) {
    if !yt.authed() {
        return;
    }
    let Some(vid) = ytm::video_id(&track.url) else {
        return;
    };
    let yt = yt.clone();
    thread::spawn(move || {
        // region-locked / offline / token / history-paused -> skip
        let _ = yt.record_history(&vid, watched);
    });
}

fn child_alive(child: &Mutex<Child>) -> bool {
    matches!(lock(child).try_wait(), Ok(None))
}

pub struct Player {
    yt: Arc<Yt>,
    ipc: Mutex<Ipc>,
    shared: Arc<Mutex<Shared>>,
    fetchq: mpsc::Sender<Track>,
    proc: Option<Arc<Mutex<Child>>>,
    quit_done: AtomicBool,
}

impl Player {
    /// cmusfm_reset, remove stale socket, spawn mpv, connect IPC, start
    /// fetcher + watcher threads. Err(message) if mpv/IPC fails.
    pub fn new(yt: Arc<Yt>) -> Result<Player, String> {
        Player::spawn(yt, crate::MPV_SOCK, crate::MPV_LOG)
    }

    fn spawn(yt: Arc<Yt>, sock: &str, log: &str) -> Result<Player, String> {
        cmusfm_reset();
        if Path::new(sock).exists() {
            std::fs::remove_file(sock).map_err(|e| format!("{sock}: {e}"))?;
        }
        let child = Command::new("mpv")
            .args(["--idle=yes", "--no-video", "--no-terminal"])
            .arg(format!("--log-file={log}"))
            .arg("--msg-level=all=v")
            .arg(format!("--input-ipc-server={sock}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("mpv: {e}"))?;
        let child = Arc::new(Mutex::new(child));
        let ipc = match Ipc::connect(sock, || child_alive(&child)) {
            Ok(ipc) => ipc,
            Err(e) => {
                let mut c = lock(&child);
                let _ = c.kill();
                let _ = c.wait();
                return Err(e);
            }
        };
        let (tx, rx) = mpsc::channel::<Track>();
        // Download queued tracks in playlist order, well ahead of playback.
        thread::spawn(move || rx.into_iter().for_each(|t| drop(fetch(&t))));
        let p = Player::with_ipc(yt, ipc, Some(child), tx);
        let (yt, shared, child, sock) =
            (p.yt.clone(), p.shared.clone(), p.proc.clone().unwrap(), sock.to_string());
        // Own IPC connection so the watcher never interleaves with the TUI's.
        thread::spawn(move || {
            let Ok(mut ipc) = Ipc::connect(&sock, || child_alive(&child)) else {
                return;
            };
            watch_loop(&mut ipc, || child_alive(&child), &shared, Duration::from_secs(1), |e| {
                match e {
                    Event::Cmusfm(s, t) => cmusfm(s, t.as_ref()),
                    Event::Record(t) => record_yt(&yt, &t, 30),
                }
            });
        });
        Ok(p)
    }

    fn with_ipc(
        yt: Arc<Yt>,
        ipc: Ipc,
        proc: Option<Arc<Mutex<Child>>>,
        fetchq: mpsc::Sender<Track>,
    ) -> Player {
        Player {
            yt,
            ipc: Mutex::new(ipc),
            shared: Arc::new(Mutex::new(Shared::default())),
            fetchq,
            proc,
            quit_done: AtomicBool::new(false),
        }
    }

    fn cmd(&self, command: Value) -> Option<Value> {
        lock(&self.ipc).cmd(command)
    }

    pub fn play(&self, tracks: &[Track], start: usize) {
        if tracks.is_empty() {
            return;
        }
        fetch(&tracks[0]); // mpv starts on it before playlist-pos is set
        if start != 0 {
            fetch(&tracks[start]); // the rest download in the background
        }
        let first = cache_path(&tracks[0]);
        lock(&self.shared).by_url = HashMap::from([(first.clone(), tracks[0].clone())]);
        self.cmd(json!(["loadfile", first, "replace"]));
        self.enqueue(&tracks[1..]);
        if start != 0 {
            self.cmd(json!(["set_property", "playlist-pos", start]));
        }
        self.cmd(json!(["set_property", "pause", false]));
    }

    /// Append tracks to the tail of the mpv playlist — they play after
    /// whatever is already queued. Merges into by_url so the watch loop can
    /// resolve queued tracks for scrobble/display.
    pub fn enqueue(&self, tracks: &[Track]) {
        lock(&self.shared)
            .by_url
            .extend(tracks.iter().map(|t| (cache_path(t), t.clone())));
        for t in tracks {
            let _ = self.fetchq.send(t.clone());
            self.cmd(json!(["loadfile", cache_path(t), "append"]));
        }
    }

    /// Insert after current; returns playlist index of first inserted, None if idle.
    /// The rest of the queue is left untouched; on None the caller should
    /// start playback instead.
    pub fn play_next(&self, tracks: &[Track]) -> Option<usize> {
        let pos = self.cmd(json!(["get_property", "playlist-pos"]))?.as_i64()?;
        if pos < 0 {
            return None;
        }
        let pos = pos as usize;
        lock(&self.shared)
            .by_url
            .extend(tracks.iter().map(|t| (cache_path(t), t.clone())));
        for (i, t) in tracks.iter().enumerate() {
            let _ = self.fetchq.send(t.clone());
            self.cmd(json!(["loadfile", cache_path(t), "insert-at", pos + 1 + i]));
        }
        Some(pos + 1)
    }

    pub fn toggle_pause(&self) {
        self.cmd(json!(["cycle", "pause"]));
    }

    /// Repeat-all: after the last track mpv restarts at track 1.
    ///
    /// The setting is a global mpv option, so it survives the `loadfile
    /// replace` in play() -- a new album inherits it -- and it makes
    /// playlist-next/prev wrap around the ends of the playlist.
    pub fn toggle_loop(&self) {
        // ponytail: mpv's own cycle-values, not a read-modify-write. Plain
        // `cycle` would walk loop-playlist's other choices (force/N too);
        // cycle-values pins the two we want.
        self.cmd(json!(["cycle-values", "loop-playlist", "inf", "no"]));
    }

    /// True while repeat-all is on. mpv answers false for off and the
    /// string "inf" for on (never "no"), so truthiness is enough. An IPC
    /// failure also reads false -- the indicator under-reports, never lies
    /// the other way.
    pub fn looping(&self) -> bool {
        truthy(&self.cmd(json!(["get_property", "loop-playlist"])))
    }

    /// Left-ear-only mode: music sits in the left bud, right stays free
    /// for everything else on the machine.
    ///
    /// ponytail: mpv's own `af toggle` -- it adds the filter if absent and
    /// drops it if present, so no mirrored flag here to drift out of sync.
    pub fn toggle_left_ear(&self) {
        self.cmd(json!(["af", "toggle", LEFT_EAR_AF]));
    }

    /// Nudge mpv's own software volume -- this player only, the system
    /// mixer and every other app are untouched. It is a global mpv option,
    /// so it survives the `loadfile replace` in play(). mpv clamps to
    /// --volume-max (130 by default) on its own.
    pub fn volume(&self, delta: i64) {
        self.cmd(json!(["add", "volume", delta]));
    }

    /// Current volume as a whole percent. An IPC failure reads 100 -- the
    /// indicator then just matches mpv's own default.
    pub fn volume_pct(&self) -> i64 {
        self.cmd(json!(["get_property", "volume"]))
            .and_then(|v| v.as_f64())
            .map_or(100, |v| v as i64)
    }

    /// True while left-ear-only is on. pan is the only filter we ever
    /// add, so a non-empty chain means it is on; an IPC failure reads false
    /// -- the indicator under-reports, never lies the other way.
    pub fn left_ear(&self) -> bool {
        truthy(&self.cmd(json!(["get_property", "af"])))
    }

    pub fn next(&self) {
        self.cmd(json!(["playlist-next"]));
    }

    pub fn prev(&self) {
        self.cmd(json!(["playlist-prev"]));
    }

    /// (pos_s, dur_s, paused, current title)
    pub fn progress(&self) -> (f64, f64, bool, String) {
        let num = |v: Option<Value>| v.and_then(|v| v.as_f64()).unwrap_or(0.0);
        let pos = num(self.cmd(json!(["get_property", "time-pos"])));
        let dur = num(self.cmd(json!(["get_property", "duration"])));
        let paused = truthy(&self.cmd(json!(["get_property", "pause"])));
        let title = lock(&self.shared)
            .current
            .as_ref()
            .map(|t| t.title.clone())
            .unwrap_or_default();
        (pos, dur, paused, title)
    }

    /// Track the watcher last saw start (what's playing now), if any.
    pub fn current(&self) -> Option<Track> {
        lock(&self.shared).current.clone()
    }

    /// mpv quit over IPC, wait 5s, else kill. Idempotent (safe from Drop + panic hook).
    pub fn quit(&self) {
        if self.quit_done.swap(true, Ordering::SeqCst) {
            return;
        }
        self.cmd(json!(["quit"]));
        let Some(child) = &self.proc else {
            return;
        };
        for _ in 0..50 {
            if !child_alive(child) {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
        let mut c = lock(child);
        let _ = c.kill();
        let _ = c.wait();
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.quit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;

    fn track() -> Track {
        Track {
            url: "https://music.youtube.com/watch?v=abc".into(),
            artist: "Radiohead".into(),
            album: "In Rainbows".into(),
            title: "Nude".into(),
            duration: 256,
            ..Default::default()
        }
    }

    fn t(url: &str, title: &str) -> Track {
        Track { url: url.into(), title: title.into(), ..Default::default() }
    }

    type Cmds = Arc<Mutex<Vec<Value>>>;

    /// Fake mpv: answers each request with `reply(command)` (None -> mpv's
    /// "property unavailable" error), preceded by an async event and a stale
    /// reply so request_id matching is exercised on every call.
    fn fake_mpv(mut reply: impl FnMut(&Value) -> Option<Value> + Send + 'static) -> (Ipc, Cmds) {
        static N: AtomicUsize = AtomicUsize::new(0);
        let sock: PathBuf = std::env::temp_dir().join(format!(
            "msm-test-{}-{}.sock",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        let cmds: Cmds = Arc::default();
        let log = cmds.clone();
        let path = sock.clone();
        thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let _ = std::fs::remove_file(&path);
            let mut w = conn.try_clone().unwrap();
            for line in BufReader::new(conn).lines() {
                let Ok(line) = line else { break };
                let req: Value = serde_json::from_str(&line).unwrap();
                let c = req["command"].clone();
                log.lock().unwrap().push(c.clone());
                let rid = &req["request_id"];
                let resp = match reply(&c) {
                    Some(d) => json!({"request_id": rid, "error": "success", "data": d}),
                    None => json!({"request_id": rid, "error": "property unavailable"}),
                };
                let out = format!(
                    "{}\n\n{}\n{}\n",
                    json!({"event": "property-change", "request_id": 0}),
                    json!({"request_id": 0, "error": "success", "data": "stale"}),
                    resp
                );
                if w.write_all(out.as_bytes()).is_err() {
                    break;
                }
            }
        });
        let ipc = Ipc::connect(sock.to_str().unwrap(), || true).unwrap();
        (ipc, cmds)
    }

    fn player(reply: impl FnMut(&Value) -> Option<Value> + Send + 'static) -> (Player, Cmds, mpsc::Receiver<Track>) {
        let (ipc, cmds) = fake_mpv(reply);
        let (tx, rx) = mpsc::channel();
        (Player::with_ipc(Arc::new(Yt { auth: None }), ipc, None, tx), cmds, rx)
    }

    fn by_url(p: &Player) -> HashMap<String, Track> {
        lock(&p.shared).by_url.clone()
    }

    #[test]
    fn test_cmusfm_argv() {
        let tr = track();
        let argv = cmusfm_argv("playing", Some(&tr));
        // matches cmus status_display_program protocol: pairs after status
        assert_eq!(argv[..3], ["cmusfm", "status", "playing"]);
        let d: HashMap<&str, &str> = argv[3..]
            .chunks(2)
            .map(|p| (p[0].as_str(), p[1].as_str()))
            .collect();
        assert_eq!(
            d,
            HashMap::from([
                ("file", tr.url.as_str()),
                ("artist", "Radiohead"),
                ("album", "In Rainbows"),
                ("title", "Nude"),
                ("duration", "256"),
            ])
        );
    }

    #[test]
    fn test_cmusfm_stopped_has_no_track() {
        assert_eq!(cmusfm_argv("stopped", None), ["cmusfm", "status", "stopped"]);
    }

    #[test]
    fn test_enqueue_appends_and_merges_by_url() {
        let (p, cmds, rx) = player(|_| None);
        let (t1, t2) = (t("u1", "A"), t("u2", "B"));
        p.enqueue(&[t1.clone(), t2.clone()]);
        assert_eq!(
            *cmds.lock().unwrap(),
            [json!(["loadfile", "u1", "append"]), json!(["loadfile", "u2", "append"])]
        );
        // queued tracks resolvable for scrobble
        assert_eq!(by_url(&p), HashMap::from([("u1".into(), t1.clone()), ("u2".into(), t2.clone())]));
        // download in order
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), [t1, t2]);
    }

    #[test]
    fn test_play_next_inserts_after_current_in_order() {
        // current track at playlist index 2
        let (p, cmds, _rx) =
            player(|c| (*c == json!(["get_property", "playlist-pos"])).then(|| json!(2)));
        let (t1, t2) = (t("u1", "A"), t("u2", "B"));
        let idx = p.play_next(&[t1.clone(), t2.clone()]);
        assert_eq!(idx, Some(3)); // first inserted right after current (2 -> 3)
        let loads: Vec<Value> =
            cmds.lock().unwrap().iter().filter(|c| c[0] == "loadfile").cloned().collect();
        assert_eq!(
            loads,
            [
                json!(["loadfile", "u1", "insert-at", 3]),
                json!(["loadfile", "u2", "insert-at", 4]), // order preserved, not reversed
            ]
        );
        assert_eq!(by_url(&p), HashMap::from([("u1".into(), t1), ("u2".into(), t2)]));
    }

    #[test]
    fn test_play_next_returns_none_when_idle() {
        let (p, _cmds, rx) = player(|_| Some(json!(-1))); // mpv reports no current entry
        assert_eq!(p.play_next(&[t("u1", "A")]), None);
        assert!(by_url(&p).is_empty()); // nothing queued when idle
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn test_toggle_loop_uses_cycle_values() {
        let (p, cmds, _rx) = player(|_| None);
        p.toggle_loop();
        // cycle-values pins the two states; plain `cycle` would walk force/N too
        assert_eq!(*cmds.lock().unwrap(), [json!(["cycle-values", "loop-playlist", "inf", "no"])]);
    }

    /// mpv answers false for off and the string "inf" for on; Ipc::cmd answers
    /// None on any failure. Verified live against mpv v0.41.0.
    #[test]
    fn test_looping_reads_mpvs_reply_shapes() {
        for (reply, want) in [(Some(json!(false)), false), (Some(json!("inf")), true), (None, false)] {
            let r = reply.clone();
            let (p, _c, _rx) = player(move |_| r.clone());
            assert_eq!(p.looping(), want, "{reply:?}");
        }
    }

    #[test]
    fn test_toggle_left_ear_uses_af_toggle() {
        let (p, cmds, _rx) = player(|_| None);
        p.toggle_left_ear();
        // `af toggle` is add-if-absent / drop-if-present, so no flag to keep in sync
        assert_eq!(
            *cmds.lock().unwrap(),
            [json!(["af", "toggle", "lavfi=[pan=stereo|c0=0.5*c0+0.5*c1|c1=0*c0]"])]
        );
    }

    #[test]
    fn test_volume_steps_and_reads_back() {
        let (p, cmds, _rx) = player(|_| None);
        p.volume(-5);
        p.volume(5);
        // relative `add`, so mpv owns the clamping against --volume-max
        assert_eq!(*cmds.lock().unwrap(), [json!(["add", "volume", -5]), json!(["add", "volume", 5])]);
        for (reply, want) in [(Some(json!(100.0)), 100), (Some(json!(85.0)), 85), (None, 100)] {
            let r = reply.clone();
            let (p, _c, _rx) = player(move |_| r.clone());
            assert_eq!(p.volume_pct(), want, "{reply:?}");
        }
    }

    /// The filter string only fails when the graph is built -- at playback,
    /// not at toggle -- so an invalid one looks like a dead key. Build it here.
    #[test]
    fn test_left_ear_filter_graph_is_valid_ffmpeg() {
        if !crate::on_path("ffmpeg") {
            return;
        }
        let af = &LEFT_EAR_AF["lavfi=[".len()..LEFT_EAR_AF.len() - 1];
        let r = Command::new("ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-i", "anullsrc=channel_layout=stereo"])
            .args(["-af", af, "-t", "0.1", "-f", "null", "-"])
            .output()
            .unwrap();
        assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    }

    /// mpv answers [] for an empty chain and a list of filter dicts when the
    /// pan filter is on; Ipc::cmd answers None on any failure.
    #[test]
    fn test_left_ear_reads_filter_chain() {
        for (reply, want) in [(Some(json!([])), false), (Some(json!([{"name": "pan"}])), true), (None, false)] {
            let r = reply.clone();
            let (p, _c, _rx) = player(move |_| r.clone());
            assert_eq!(p.left_ear(), want, "{reply:?}");
        }
    }

    /// repeat-all over one track keeps mpv's `path` constant, so the rewind is
    /// the only cue cmusfm gets; and an exhausted playlist leaves mpv idling, so
    /// the last track needs an explicit stop or it is never submitted.
    #[test]
    fn test_watch_rescrobbles_a_replayed_track_and_stops_at_playlist_end() {
        // (path, pause, time-pos) per one-second poll of the watch loop
        let frames: Vec<(Option<&str>, Option<bool>, Option<f64>)> = vec![
            (Some("u1"), Some(false), Some(5.0)),
            (Some("u1"), Some(false), Some(190.0)),
            (Some("u1"), Some(false), Some(1.0)),
            (None, None, None),
        ];
        let n = frames.len();
        let mut i: isize = -1;
        let (mut ipc, _cmds) = fake_mpv(move |c| {
            let prop = c[1].as_str().unwrap();
            if prop == "path" {
                i += 1;
            }
            let f = frames[(i.max(0) as usize).min(n - 1)];
            match prop {
                "path" => f.0.map(|p| json!(p)),
                "pause" => f.1.map(|p| json!(p)),
                _ => f.2.map(|p| json!(p)),
            }
        });
        let t1 = Track {
            url: "u1".into(),
            title: "A".into(),
            artist: "B".into(),
            album: "C".into(),
            duration: 198,
            ..Default::default()
        };
        let shared = Mutex::new(Shared {
            by_url: HashMap::from([("u1".into(), t1.clone())]),
            current: None,
        });
        let mut polls = 0;
        let mut calls = Vec::new();
        watch_loop(
            &mut ipc,
            || {
                polls += 1;
                polls <= n
            },
            &shared,
            Duration::ZERO,
            |e| calls.push(e),
        );
        let title = |e: &Event| match e {
            Event::Cmusfm(s, t) => (s.to_string(), t.as_ref().map(|t| t.title.clone())),
            Event::Record(t) => ("record".into(), Some(t.title.clone())),
        };
        let a = Some("A".to_string());
        assert_eq!(
            calls.iter().map(title).collect::<Vec<_>>(),
            [
                ("playing".into(), a.clone()), // first start
                ("record".into(), a.clone()),  // >=30s -> YT history
                ("playing".into(), a.clone()), // rewind == replay -> submits the play that finished
                ("stopped".into(), a.clone()), // playlist ran out while mpv stayed alive
                ("stopped".into(), None),      // mpv gone
            ]
        );
    }

    /// Port of check_playback.py: a YouTube track downloads and mpv actually
    /// decodes audio from it. Own mpv on a private socket so a live msm session
    /// is left alone, but it does play a few seconds of audio out loud.
    #[test]
    #[ignore]
    fn check_playback() {
        let tr = Track {
            title: "Marianne".into(),
            artist: "Fontaines D.C.".into(),
            album: "Marianne".into(),
            duration: 225,
            url: "https://music.youtube.com/watch?v=ikKBcZg9jUc".into(),
            ..Default::default()
        };
        let p = cache_path(&tr);
        assert!(p.ends_with("ikKBcZg9jUc.m4a"), "{p}");
        let _ = std::fs::remove_file(&p);
        assert_eq!(fetch(&tr), p);
        let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        assert!(size > 500_000, "download failed/short");
        println!("downloaded {size} bytes -> {p}");

        let (_tags, dur) = crate::local::probe(Path::new(&p));
        assert!(200.0 < dur && dur < 260.0, "bad duration {dur}"); // duration now loads
        println!("ffprobe duration: {dur:.1} s");

        // local file passes through untouched
        assert_eq!(cache_path(&t("/Users/x/Music/a.flac", "")), "/Users/x/Music/a.flac");

        // cache hit: second fetch must not re-download
        let mtime = || std::fs::metadata(&p).unwrap().modified().unwrap();
        let m = mtime();
        thread::sleep(Duration::from_secs(1));
        fetch(&tr);
        assert_eq!(mtime(), m, "re-downloaded a cached track");
        println!("cache hit OK");

        let pl = Player::spawn(Arc::new(Yt { auth: None }), "/tmp/ymc-check.sock", "/tmp/ymc-check.log")
            .unwrap();
        pl.play(&[tr], 0);
        let mut ok = false;
        for _ in 0..30 {
            thread::sleep(Duration::from_secs(1));
            let (pos, d, _paused, title) = pl.progress();
            if pos > 2.0 && d > 200.0 {
                println!("PLAYING pos={pos:.1}s dur={d:.1}s title={title:?}");
                ok = true;
                break;
            }
        }
        let last = pl.progress();
        pl.quit();
        assert!(ok, "mpv never played: {last:?}");
        println!("ALL OK");
    }
}
