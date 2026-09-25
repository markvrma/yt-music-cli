//! Terminal UI: browse + search screens, bordered panes, purple/grey theme,
//! album art, progress bar. Port of tui.py onto plain crossterm.
//!
//! Screens:
//!   browse  left = tracklist of most-recent album; right = last 5 albums
//!           (enter expands, f plays from start); bottom-right = pixelated
//!           album art (square, capped); bottom = progress bar.
//!   search  '/' opens it: type query, enter runs it, jk pick a result,
//!           enter = load into browse (no play), f = load + play, Esc = back.
//! LOCAL pane: enter opens the selected album's tracklist in place (j/k move,
//!       enter plays that one track, f plays the album from there, a/A queue);
//!       Esc — or anything that leaves the pane, h/l or '/' — puts the album
//!       list back.
//! Keys: h/l switch pane, j/k move, space pause, n/p next/prev, a queue,
//!       A play-next, r repeat-all, e left-ear, [ ] volume, L like, q quit.
//!       Queue (a) = play after the whole queue; play-next (A) = play right
//!       after the current track, queue untouched. Repeat-all (r, browse screen
//!       only) restarts at track 1 after the last one; while it is on n/p wrap
//!       around the ends, and the next album played inherits the setting.
//!       ↻ in the progress bar = on.
//!
//! Rendering: every cycle draws into an in-memory cell buffer which is then
//! written out whole (see `render`), so the draw path is testable without a
//! terminal.

use crate::player::Player;
use crate::ytm::Yt;
use crate::{Album, Item, Track};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{
    Attribute, Color, Print, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::{cursor, queue, terminal};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const ART_CAP: i64 = 18; // max album-art height in cells
const CELL_ASPECT: f64 = 2.4; // terminal cell height:width. iTerm2/Menlo ~2.4; tune per font
                              // so the cover reads as a square (art_w = art_h * CELL_ASPECT)

// ---- theme (curses init_pair on the default bg, use_default_colors) --------

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Style {
    fg: Option<u8>,
    bg: Option<u8>,
    bold: bool,
    reverse: bool,
}

const PLAIN: Style = Style {
    fg: None,
    bg: None,
    bold: false,
    reverse: false,
};
const BORDER: Style = Style {
    fg: Some(141),
    ..PLAIN
}; // purple border/title on default bg
const ACCENT: Style = Style {
    fg: Some(177),
    ..PLAIN
}; // bright purple: playing title, bar
const SELECT: Style = Style {
    fg: Some(16),
    bg: Some(141),
    ..PLAIN
}; // selected row: near-black on purple
const DIM: Style = Style {
    fg: Some(244),
    ..PLAIN
}; // grey

fn clamp(v: i64, lo: i64, hi: i64) -> i64 {
    lo.max(v.min(hi))
}

fn fmt_time(s: f64) -> String {
    let s = s as i64;
    format!("{}:{:02}", s.div_euclid(60), s.rem_euclid(60))
}

/// Python 3 round(): half to even.
fn py_round(v: f64) -> i64 {
    v.round_ties_even() as i64
}

// ---- cell buffer ------------------------------------------------------------

/// One screen cell: the base char plus any zero-width marks attached to it
/// (U+FE0F, combining accents). "" = right half of the wide char to its left.
type Cell = (String, Style);

/// The whole screen; `erase` = a fresh Buf.
struct Buf {
    h: i64,
    w: i64,
    cells: Vec<Cell>,
}

/// A derwin: a rectangle of the screen, coordinates relative to it.
#[derive(Clone, Copy)]
struct Win {
    y: i64,
    x: i64,
    h: i64,
    w: i64,
}

extern "C" {
    // the libc crate doesn't bind it; it's in libSystem / glibc.
    fn wcwidth(c: libc::wchar_t) -> libc::c_int;
}

/// Cells `c` takes, as ncurses measures it: wcwidth under LC_CTYPE (set in
/// run(), like Python does at startup). Controls come back -1.
fn char_width(c: char) -> i32 {
    // SAFETY: pure libc lookup on a code point.
    unsafe { wcwidth(c as libc::wchar_t) }
}

impl Buf {
    fn new(h: i64, w: i64) -> Buf {
        let n = (h.max(0) * w.max(0)) as usize;
        Buf {
            h: h.max(0),
            w: w.max(0),
            cells: vec![(" ".into(), PLAIN); n],
        }
    }

    fn idx(&self, y: i64, x: i64) -> Option<usize> {
        ((0..self.h).contains(&y) && (0..self.w).contains(&x)).then(|| (y * self.w + x) as usize)
    }

    /// Overwrite one cell. Like ncurses, clobbering either half of a wide char
    /// blanks its other half.
    fn set_cell(&mut self, y: i64, x: i64, s: String, st: Style) {
        let Some(i) = self.idx(y, x) else { return };
        if self.cells[i].0.is_empty() {
            if let Some(l) = self.idx(y, x - 1) {
                self.cells[l].0 = " ".into();
            }
        }
        if let Some(r) = self.idx(y, x + 1) {
            if self.cells[r].0.is_empty() {
                self.cells[r].0 = " ".into();
            }
        }
        self.cells[i] = (s, st);
    }

    fn set(&mut self, y: i64, x: i64, ch: char, st: Style) {
        self.set_cell(y, x, ch.to_string(), st);
    }

    #[cfg(test)]
    fn whole(&self) -> Win {
        Win {
            y: 0,
            x: 0,
            h: self.h,
            w: self.w,
        }
    }

    /// Frame as plain text, one line per row (tests / debugging).
    #[cfg(test)]
    fn text(&self) -> String {
        let mut s = String::new();
        for y in 0..self.h {
            let row: String = (0..self.w)
                .map(|x| self.cells[(y * self.w + x) as usize].0.as_str())
                .collect();
            s.push_str(row.trim_end());
            s.push('\n');
        }
        s
    }
}

/// addnstr semantics: at most `n` characters (chars, not cells — n < 0 = the
/// whole string), laid out by display width like ncurses: wide chars take 2
/// cells, zero-width ones attach to the cell before, and a char that doesn't
/// fit before the window edge ends the write. The buffer never scrolls, so the
/// last screen cell is safe to write.
fn put(buf: &mut Buf, win: Win, y: i64, x: i64, s: &str, n: i64, st: Style) {
    if !(0..win.h).contains(&y) || x < 0 {
        return;
    }
    let n = if n < 0 { usize::MAX } else { n as usize };
    let sy = win.y + y;
    let mut cx = x;
    for ch in s.chars().take(n) {
        let wd = char_width(ch);
        if wd == 0 {
            // combining mark / variation selector: joins the previous cell
            let mut px = win.x + cx - 1;
            while px > win.x && buf.idx(sy, px).is_some_and(|i| buf.cells[i].0.is_empty()) {
                px -= 1;
            }
            if px >= win.x {
                if let Some(i) = buf.idx(sy, px) {
                    buf.cells[i].0.push(ch);
                }
            }
            continue;
        }
        if wd < 0 {
            // ncurses shows C0 / DEL as ^X (unctrl); other unprintables are dropped
            if (ch as u32) < 0x20 || ch == '\x7f' {
                if cx + 2 > win.w {
                    break;
                }
                let caret = char::from_u32((ch as u32) ^ 0x40).unwrap_or('?');
                buf.set(sy, win.x + cx, '^', st);
                buf.set(sy, win.x + cx + 1, caret, st);
                cx += 2;
            }
            continue;
        }
        if cx + wd as i64 > win.w {
            break;
        }
        buf.set(sy, win.x + cx, ch, st);
        if wd == 2 {
            buf.set_cell(sy, win.x + cx + 1, String::new(), st);
        }
        cx += wd as i64;
    }
}

/// Bordered subwindow; title on the top edge. Returns win (or None).
fn draw_box(
    buf: &mut Buf,
    y: i64,
    x: i64,
    h: i64,
    w: i64,
    title: &str,
    focused: bool,
) -> Option<Win> {
    if h < 2 || w < 2 {
        return None;
    }
    let win = Win { y, x, h, w };
    let st = Style {
        bold: focused,
        ..BORDER
    };
    for cx in 1..w - 1 {
        buf.set(y, x + cx, '─', st);
        buf.set(y + h - 1, x + cx, '─', st);
    }
    for cy in 1..h - 1 {
        buf.set(y + cy, x, '│', st);
        buf.set(y + cy, x + w - 1, '│', st);
    }
    buf.set(y, x, '┌', st);
    buf.set(y, x + w - 1, '┐', st);
    buf.set(y + h - 1, x, '└', st);
    buf.set(y + h - 1, x + w - 1, '┘', st);
    if !title.is_empty() {
        let st = Style {
            bold: true,
            reverse: focused,
            ..BORDER
        };
        put(buf, win, 0, 2, &format!(" {title} "), w - 4, st);
    }
    Some(win)
}

/// Scroll offset of a list: keeps `sel` mid-box once the list overflows.
fn scroll_off(sel: i64, bh: i64, len: i64) -> i64 {
    clamp(sel - bh.div_euclid(2), 0, 0.max(len - bh))
}

/// Scrolling list inside a bordered box (inner region = h-2 x w-2).
fn draw_rows(buf: &mut Buf, win: Win, rows: &[String], sel: i64, focused: bool) {
    let (bh, bw) = (win.h - 2, win.w - 2);
    if bh <= 0 || bw <= 0 {
        return;
    }
    let off = scroll_off(sel, bh, rows.len() as i64);
    for i in 0..bh {
        let idx = off + i;
        if idx >= rows.len() as i64 {
            break;
        }
        let st = if idx == sel && focused { SELECT } else { PLAIN };
        let line = format!(" {}", rows[idx as usize]);
        let pad = (bw as usize).saturating_sub(line.chars().count());
        put(
            buf,
            win,
            1 + i,
            1,
            &format!("{line}{}", " ".repeat(pad)),
            bw,
            st,
        );
    }
}

/// Fill the box with a half-block rendering of the art (top px=fg, bot=bg).
fn draw_art(buf: &mut Buf, win: Win, path: Option<&PathBuf>) {
    let (h, w) = (win.h, win.w);
    let (rows, cols) = (h - 2, w - 2);
    if rows < 3 || cols < 6 {
        return;
    }
    let no_art = |buf: &mut Buf| {
        put(
            buf,
            win,
            h.div_euclid(2),
            (w - 6).div_euclid(2),
            "no art",
            6,
            DIM,
        )
    };
    let Some(path) = path else {
        return no_art(buf);
    };
    let Ok(grid) = crate::art::art_grid(path, cols as usize, rows as usize) else {
        return no_art(buf);
    };
    for (cy, row) in grid.iter().enumerate().take(rows as usize) {
        for (cx, &(fg, bg)) in row.iter().enumerate().take(cols as usize) {
            let st = Style {
                fg: Some(fg),
                bg: Some(bg),
                ..PLAIN
            };
            put(buf, win, 1 + cy as i64, 1 + cx as i64, "▀", 1, st);
        }
    }
}

struct Progress {
    pos: f64,
    dur: f64,
    paused: bool,
    title: String,
    looping: bool,
    left_ear: bool,
    vol: i64,
}

fn draw_progress(buf: &mut Buf, y: i64, w: i64, p: &Progress, note: &str) {
    let state = if p.paused { "‖" } else { "▶" };
    // loop glyph only on the real label -- never stapled onto a flash message
    let flags = format!(
        "{}{}{}",
        if p.looping { "↻ " } else { "" },
        if p.left_ear { "◐ " } else { "" },
        if p.vol == 100 {
            String::new()
        } else {
            format!("{}% ", p.vol)
        }
    );
    let label = if note.is_empty() {
        format!("{flags}{state} {}", p.title)
    } else {
        note.to_string()
    };
    let Some(win) = draw_box(buf, y, 0, 3, w, &label, false) else {
        return;
    };
    let bw = w - 2;
    let times = format!("{} / {}", fmt_time(p.pos), fmt_time(p.dur));
    let barw = 1.max(bw - times.chars().count() as i64 - 3);
    let filled = if p.dur != 0.0 {
        (barw as f64 * (p.pos / p.dur)) as i64
    } else {
        0
    };
    put(
        buf,
        win,
        1,
        1,
        &"█".repeat(filled.max(0) as usize),
        bw,
        ACCENT,
    );
    put(
        buf,
        win,
        1,
        1 + filled,
        &"░".repeat((barw - filled).max(0) as usize),
        bw - filled,
        DIM,
    );
    put(buf, win, 1, 1 + barw + 1, &times, bw - barw - 1, DIM);
}

/// Browse-screen geometry for an H x W terminal.
#[derive(Debug, PartialEq)]
struct Layout {
    main_h: i64,
    art_h: i64,
    art_bw: i64,
    art_bh: i64,
    rcw: i64,
    left_w: i64,
    npw: i64,
    lmw: i64,
    last5_h: i64,
}

fn layout(h: i64, w: i64) -> Layout {
    let prog_h = 3;
    let main_h = h - prog_h;
    // cover (visually square) defines the right-column width
    let mut art_h = ART_CAP.min(main_h.div_euclid(2) - 1);
    let mut art_bw = py_round(art_h as f64 * CELL_ASPECT) + 2;
    let cap = (w as f64 * 0.42) as i64;
    if art_bw > cap {
        // keep right column from dominating
        art_bw = cap;
        art_h = ((art_bw - 2) as f64 / CELL_ASPECT) as i64;
        art_bw = py_round(art_h as f64 * CELL_ASPECT) + 2;
    }
    let art_bh = art_h + 2;
    let rcw = if art_h >= 4 { art_bw } else { 0 };
    let left_w = w - rcw;
    let npw = left_w.div_euclid(2);
    Layout {
        main_h,
        art_h,
        art_bw,
        art_bh,
        rcw,
        left_w,
        npw,
        lmw: left_w - npw,
        last5_h: main_h - art_bh,
    }
}

// ---- side effects -------------------------------------------------------------

/// Everything the UI calls outside itself: player, YouTube, history/library.
/// ponytail: a trait with one real impl, kept only so the key handling and
/// draw path run in tests — Player/Yt are concrete and spawn mpv / hit the
/// network, and run()'s signature is fixed. Drop it if they grow fakes.
trait Env {
    fn authed(&self) -> bool;
    /// Fill `recs` off-thread (network) so it can't delay first paint.
    fn spawn_recs(&self, recs: Arc<Mutex<Vec<Album>>>);
    fn search_all(&self, q: &str) -> Vec<Item>;
    fn resolve_result(&self, r: &Item) -> Result<(String, Vec<Track>, String), String>;
    fn like_track(&self, t: &Track) -> bool;
    fn load_history(&self) -> Vec<Album>;
    fn scan_local(&self) -> Vec<Album>;
    fn record(&self, title: &str, tracks: &[Track], thumb: &str) -> Vec<Album>;
    fn load_local_album(&self, album: &mut Album);
    fn art_file(&self, title: &str, url: &str) -> Option<PathBuf>;
    fn progress(&self) -> Progress;
    fn current(&self) -> Option<Track>;
    fn play(&self, tracks: &[Track], start: usize);
    fn enqueue(&self, tracks: &[Track]);
    fn play_next(&self, tracks: &[Track]) -> Option<usize>;
    fn toggle_pause(&self);
    fn next(&self);
    fn prev(&self);
    fn toggle_loop(&self);
    fn toggle_left_ear(&self);
    fn volume(&self, delta: i64);
}

struct Real<'a> {
    yt: Arc<Yt>,
    player: &'a Player,
}

impl Env for Real<'_> {
    fn authed(&self) -> bool {
        self.yt.authed()
    }
    fn spawn_recs(&self, recs: Arc<Mutex<Vec<Album>>>) {
        let yt = self.yt.clone();
        std::thread::spawn(move || {
            let got = yt.get_recs(5);
            recs.lock().unwrap_or_else(|e| e.into_inner()).extend(got);
        });
    }
    fn search_all(&self, q: &str) -> Vec<Item> {
        self.yt.search_all(q)
    }
    fn resolve_result(&self, r: &Item) -> Result<(String, Vec<Track>, String), String> {
        self.yt.resolve_result(r)
    }
    fn like_track(&self, t: &Track) -> bool {
        self.yt.like_track(t)
    }
    fn load_history(&self) -> Vec<Album> {
        crate::local::load_history()
    }
    fn scan_local(&self) -> Vec<Album> {
        crate::local::scan_local()
    }
    fn record(&self, title: &str, tracks: &[Track], thumb: &str) -> Vec<Album> {
        crate::local::record(title, tracks, thumb)
    }
    fn load_local_album(&self, album: &mut Album) {
        crate::local::load_local_album(album)
    }
    fn art_file(&self, title: &str, url: &str) -> Option<PathBuf> {
        crate::local::art_file(title, url)
    }
    fn progress(&self) -> Progress {
        let (pos, dur, paused, title) = self.player.progress();
        Progress {
            pos,
            dur,
            paused,
            title,
            looping: self.player.looping(),
            left_ear: self.player.left_ear(),
            vol: self.player.volume_pct(),
        }
    }
    fn current(&self) -> Option<Track> {
        self.player.current()
    }
    fn play(&self, tracks: &[Track], start: usize) {
        self.player.play(tracks, start)
    }
    fn enqueue(&self, tracks: &[Track]) {
        self.player.enqueue(tracks)
    }
    fn play_next(&self, tracks: &[Track]) -> Option<usize> {
        self.player.play_next(tracks)
    }
    fn toggle_pause(&self) {
        self.player.toggle_pause()
    }
    fn next(&self) {
        self.player.next()
    }
    fn prev(&self) {
        self.player.prev()
    }
    fn toggle_loop(&self) {
        self.player.toggle_loop()
    }
    fn toggle_left_ear(&self) {
        self.player.toggle_left_ear()
    }
    fn volume(&self, delta: i64) {
        self.player.volume(delta)
    }
}

/// Resolve an album's art to a local path. http thumb -> download; else a local path.
fn art_for(env: &dyn Env, title: &str, thumb: &str) -> Option<PathBuf> {
    if thumb.starts_with("http") {
        return env.art_file(title, thumb);
    }
    let p = PathBuf::from(thumb);
    (!thumb.is_empty() && p.exists()).then_some(p)
}

/// Fill an album's tracks lazily (local scan or YT rec resolve). -> playable.
fn ensure_tracks(env: &dyn Env, album: &mut Album) -> bool {
    if album.tracks.is_none() {
        if album.local.is_some() {
            env.load_local_album(album);
        } else if let Some(rec) = album.rec.clone() {
            let tracks = match env.resolve_result(&rec) {
                Ok((_, tracks, thumb)) => {
                    if !tracks.is_empty() {
                        album.thumb = thumb;
                    }
                    tracks
                }
                Err(_) => Vec::new(),
            };
            album.tracks = Some(tracks);
        }
    }
    album.tracks.as_ref().is_some_and(|t| !t.is_empty())
}

/// Tag each track with its album art so the cover can follow the
/// currently-playing track across queued albums (tracks carry no art
/// of their own).
fn stamp_art(tracks: &mut [Track], thumb: &str) {
    for t in tracks {
        if t.thumb.is_empty() {
            t.thumb = thumb.to_string();
        }
    }
}

// ---- state + keys ---------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
enum Key {
    Enter,
    Esc,
    Backspace,
    Up,
    Down,
    Char(char),
    /// Ctrl-C: curses runs cbreak, so it was SIGINT -> KeyboardInterrupt out of run.
    Interrupt,
    /// Ctrl-Z: under cbreak the tty sent SIGTSTP to the whole process group
    /// (mpv included, so the music stops) and `fg` resumed with a redraw.
    /// Raw mode turns that off, so run() does it by hand — see `suspend`.
    Suspend,
    /// Anything curses returned that no handler matches (left/right, Tab,
    /// Home, F-keys, other ^X): does nothing, but still closes the LOCAL
    /// drill-in like any other key there.
    Other,
}

/// Map a crossterm key to what curses' getch would have returned (possibly
/// two codes: Alt+x arrives from curses as Esc then x). Release/Repeat -> none.
fn map_key(k: KeyEvent) -> Vec<Key> {
    if k.kind != KeyEventKind::Press {
        return vec![];
    }
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    match k.code {
        KeyCode::Enter => vec![Key::Enter],
        KeyCode::Esc => vec![Key::Esc],
        KeyCode::Backspace => vec![Key::Backspace],
        KeyCode::Up => vec![Key::Up],
        KeyCode::Down => vec![Key::Down],
        KeyCode::Char('c') if ctrl => vec![Key::Interrupt],
        KeyCode::Char('z') if ctrl => vec![Key::Suspend],
        KeyCode::Char('h') if ctrl => vec![Key::Backspace], // ^H = 8
        KeyCode::Char('j' | 'm') if ctrl => vec![Key::Enter], // ^J = 10, ^M = 13
        KeyCode::Char(_) if ctrl => vec![Key::Other],
        KeyCode::Char(c) if alt => vec![Key::Esc, Key::Char(c)],
        KeyCode::Char(c) => vec![Key::Char(c)], // 'A', 'L' carry SHIFT; match on the char
        _ => vec![Key::Other],
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Screen {
    Browse,
    Search,
}

/// Where an album being acted on lives, so in-place fills (ensure_tracks,
/// stamp_art) land on the shared list entry like the Python dict mutation did.
enum Src {
    Now,
    Local(usize),
    Pane2(usize),
    Temp(Box<Album>),
}

struct State {
    authed: bool,
    hist: Vec<Album>,
    local: Vec<Album>,
    recs: Arc<Mutex<Vec<Album>>>, // authed: YT Music recs, filled off-thread after first paint
    screen: Screen,
    focus: usize,  // 0=NowPlaying 1=Local 2=pane2  (search: 0=bar 1=results)
    sel: [i64; 3], // per-pane selection
    sel_s: i64,
    query: String,
    results: Vec<Item>,
    now: Option<Album>,
    now_art: Option<PathBuf>,
    drill: Option<usize>, // index into `local` of the album opened inside the LOCAL pane
    dsel: i64,            // selection inside that tracklist
    flash: String,        // transient status shown in the progress bar (e.g. "♥ liked")
    flash_ttl: i32,       // refresh cycles the flash stays visible
    art_memo: (Option<String>, Option<PathBuf>), // (url, path): cover art of the playing track
}

impl State {
    fn new(env: &dyn Env) -> State {
        let authed = env.authed();
        let recs = Arc::new(Mutex::new(Vec::new()));
        let mut st = State {
            authed,
            hist: env.load_history(),
            local: env.scan_local(),
            recs: recs.clone(),
            screen: Screen::Browse,
            focus: 0,
            sel: [0; 3],
            sel_s: 0,
            query: String::new(),
            results: Vec::new(),
            now: None,
            now_art: None,
            drill: None,
            dsel: 0,
            flash: String::new(),
            flash_ttl: 0,
            art_memo: (None, None),
        };
        if authed {
            env.spawn_recs(recs); // network call -> off-thread so it can't delay first paint
        }
        if let Some(h0) = st.hist.first().cloned() {
            st.set_now(env, Some(h0));
        }
        st
    }

    fn recs(&self) -> std::sync::MutexGuard<'_, Vec<Album>> {
        self.recs.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Active pane-2 list: YT recs when authed, else local last-5.
    fn pane2(&self) -> Vec<Album> {
        if self.authed {
            self.recs().clone()
        } else {
            self.hist.clone()
        }
    }

    fn pane2_len(&self) -> usize {
        if self.authed {
            self.recs().len()
        } else {
            self.hist.len()
        }
    }

    fn pane_len(&self, focus: usize) -> i64 {
        (match focus {
            0 => self
                .now
                .as_ref()
                .and_then(|a| a.tracks.as_ref())
                .map_or(0, Vec::len),
            1 => self.local.len(),
            _ => self.pane2_len(),
        }) as i64
    }

    fn now_tracks(&self) -> Option<&Vec<Track>> {
        self.now
            .as_ref()
            .and_then(|a| a.tracks.as_ref())
            .filter(|t| !t.is_empty())
    }

    // Own copy of the album and its track list: queued items append to the
    // NOW pane (see the 'a' handler) without mutating the source album object
    // shared by the LOCAL / FOR YOU / history lists.
    fn set_now(&mut self, env: &dyn Env, album: Option<Album>) {
        self.now_art = album
            .as_ref()
            .and_then(|a| art_for(env, &a.title, &a.thumb));
        self.now = album;
    }

    fn get(&self, src: &Src) -> Option<Album> {
        match src {
            Src::Now => self.now.clone(),
            Src::Local(i) => self.local.get(*i).cloned(),
            Src::Pane2(i) if self.authed => self.recs().get(*i).cloned(),
            Src::Pane2(i) => self.hist.get(*i).cloned(),
            Src::Temp(a) => Some((**a).clone()),
        }
    }

    fn put_back(&mut self, src: &Src, album: &Album) {
        let slot = match src {
            Src::Now => self.now.as_mut(),
            Src::Local(i) => self.local.get_mut(*i),
            Src::Pane2(i) if self.authed => {
                if let Some(a) = self.recs().get_mut(*i) {
                    *a = album.clone();
                }
                return;
            }
            Src::Pane2(i) => self.hist.get_mut(*i),
            Src::Temp(_) => None,
        };
        if let Some(a) = slot {
            *a = album.clone();
        }
    }

    /// ensure_tracks on the album where it lives. -> (album after fill, playable).
    fn ensure(&mut self, env: &dyn Env, src: &Src) -> Option<(Album, bool)> {
        let mut a = self.get(src)?;
        let ok = ensure_tracks(env, &mut a);
        self.put_back(src, &a);
        Some((a, ok))
    }

    fn do_play(&mut self, env: &dyn Env, src: Src, start: usize) {
        let Some((mut a, true)) = self.ensure(env, &src) else {
            return;
        };
        let thumb = a.thumb.clone();
        stamp_art(a.tracks.as_mut().unwrap(), &thumb);
        self.put_back(&src, &a);
        self.set_now(env, Some(a.clone()));
        let tracks = a.tracks.unwrap_or_default();
        self.hist = env.record(&a.title, &tracks, &a.thumb);
        env.play(&tracks, start);
    }

    /// Add tracks to the current playlist and mirror them into the NOW
    /// list. mode 'next' = insert right after the current track (rest of the
    /// queue untouched); else append to the tail. If nothing is playing, start
    /// playback. Reused by the browse and search a/A keybinds.
    fn add_tracks(
        &mut self,
        env: &dyn Env,
        mut tracks: Vec<Track>,
        label: &str,
        thumb: &str,
        next: bool,
    ) {
        stamp_art(&mut tracks, thumb);
        let temp = |tracks: Vec<Track>| {
            Src::Temp(Box::new(Album {
                title: label.to_string(),
                tracks: Some(tracks),
                thumb: thumb.to_string(),
                ..Default::default()
            }))
        };
        if self.now.as_ref().is_none_or(|a| a.tracks.is_none()) {
            return self.do_play(env, temp(tracks), 0);
        }
        if next {
            let Some(idx) = env.play_next(&tracks) else {
                // playlist idle -> just start
                return self.do_play(env, temp(tracks), 0);
            };
            let al = self.now.as_mut().and_then(|a| a.tracks.as_mut()).unwrap();
            let idx = idx.min(al.len());
            al.splice(idx..idx, tracks); // mirror insert into NOW list
            self.flash = format!("⏭ next: {label}");
        } else {
            env.enqueue(&tracks);
            self.now
                .as_mut()
                .and_then(|a| a.tracks.as_mut())
                .unwrap()
                .extend(tracks);
            self.flash = format!("⏭ queued: {label}");
        }
        self.flash_ttl = 6;
    }

    /// Album selected in the focused list pane (1=LOCAL, 2=FOR YOU/hist),
    /// or None. Pane 0 (NOW) has no separate album to open.
    fn cur_album(&self) -> Option<Src> {
        match self.focus {
            1 if !self.local.is_empty() => Some(Src::Local(self.sel[1] as usize)),
            2 if self.pane2_len() > 0 => Some(Src::Pane2(self.sel[2] as usize)),
            _ => None,
        }
    }

    /// Cover for the currently-playing track (follows it across queued
    /// albums); falls back to the opened album's art when nothing plays.
    fn cur_art(&mut self, env: &dyn Env) -> Option<PathBuf> {
        if let Some(ct) = env.current().filter(|t| !t.thumb.is_empty()) {
            if self.art_memo.0.as_deref() != Some(ct.url.as_str()) {
                let title = [&ct.album, &ct.title]
                    .into_iter()
                    .find(|s| !s.is_empty())
                    .map_or("art", |s| s.as_str());
                self.art_memo = (Some(ct.url.clone()), art_for(env, title, &ct.thumb));
            }
            return self.art_memo.1.clone();
        }
        self.now_art.clone()
    }

    // ponytail: blunt full repaint every cycle (render writes every cell). curses
    // only wrote cells it believed changed, and a subprocess printing to the
    // terminal desynced that belief, leaving panes stale until something in
    // them happened to change, which read as permanent blackout. Narrow to
    // "on demand" if the redraw traffic ever matters over ssh.
    fn draw(&mut self, env: &dyn Env, h: i64, w: i64) -> Buf {
        let mut buf = Buf::new(h, w);
        let lay = layout(h, w);
        let main_h = lay.main_h;

        if self.screen == Screen::Browse {
            let title = format!(
                "NOW: {}",
                self.now
                    .as_ref()
                    .map_or("(open an album)", |a| a.title.as_str())
            );
            if let Some(wl) = draw_box(&mut buf, 0, 0, main_h, lay.npw, &title, self.focus == 0) {
                let rows: Vec<String> = self
                    .now_tracks()
                    .map_or(vec![], |t| t.iter().map(|t| t.title.clone()).collect());
                draw_rows(&mut buf, wl, &rows, self.sel[0], self.focus == 0);
            }

            if let Some(d) = self.drill {
                let al = &self.local[d];
                let title = format!("{}  (esc=back enter=play a=queue)", al.title);
                if let Some(wm) = draw_box(
                    &mut buf,
                    0,
                    lay.npw,
                    main_h,
                    lay.lmw,
                    &title,
                    self.focus == 1,
                ) {
                    let rows: Vec<String> = al
                        .tracks
                        .iter()
                        .flatten()
                        .map(|t| t.title.clone())
                        .collect();
                    draw_rows(&mut buf, wm, &rows, self.dsel, self.focus == 1);
                }
            } else if let Some(wm) = draw_box(
                &mut buf,
                0,
                lay.npw,
                main_h,
                lay.lmw,
                "LOCAL ~/Music  (enter=open f=play)",
                self.focus == 1,
            ) {
                let rows: Vec<String> = self.local.iter().map(|a| a.title.clone()).collect();
                draw_rows(&mut buf, wm, &rows, self.sel[1], self.focus == 1);
            }

            if lay.rcw != 0 {
                let title2 = if self.authed {
                    "FOR YOU  (enter=open f=play)"
                } else {
                    "LAST 5  (enter=open f=play)"
                };
                if let Some(w5) = draw_box(
                    &mut buf,
                    0,
                    lay.left_w,
                    lay.last5_h,
                    lay.rcw,
                    title2,
                    self.focus == 2,
                ) {
                    let mut rows2: Vec<String> =
                        self.pane2().iter().map(|a| a.title.clone()).collect();
                    if rows2.is_empty() && self.authed {
                        rows2.push("loading…".into());
                    }
                    draw_rows(&mut buf, w5, &rows2, self.sel[2], self.focus == 2);
                }
                if let Some(wa) = draw_box(
                    &mut buf,
                    main_h - lay.art_bh,
                    lay.left_w,
                    lay.art_bh,
                    lay.art_bw,
                    "cover",
                    false,
                ) {
                    let art = self.cur_art(env);
                    draw_art(&mut buf, wa, art.as_ref());
                }
            }
        } else {
            if let Some(wb) = draw_box(&mut buf, 0, 0, 3, w, "search", self.focus == 0) {
                let q = format!(" {}{}", self.query, if self.focus == 0 { "█" } else { "" });
                put(&mut buf, wb, 1, 1, &q, w - 2, ACCENT);
            }
            let title = "RESULTS  (enter=load f=play a=queue A=next Esc=back)";
            if let Some(wr) = draw_box(&mut buf, 3, 0, main_h - 3, w, title, self.focus == 1) {
                let rows: Vec<String> = self
                    .results
                    .iter()
                    .map(|r| {
                        let rt = r.result_type.as_deref().unwrap_or("?");
                        format!(
                            "[{rt}] {} — {}",
                            r.title,
                            crate::ytm::artists_str(&r.artists)
                        )
                    })
                    .collect();
                draw_rows(&mut buf, wr, &rows, self.sel_s, self.focus == 1);
            }
        }

        let note = if self.flash_ttl > 0 {
            self.flash.clone()
        } else {
            String::new()
        };
        draw_progress(&mut buf, main_h, w, &env.progress(), &note);
        if self.flash_ttl > 0 {
            self.flash_ttl -= 1;
        }
        buf
    }

    /// One getch result. -> false = quit.
    fn handle_key(&mut self, env: &dyn Env, c: Key) -> bool {
        use Key::*;
        if c == Interrupt {
            return false;
        }

        // ----- search screen -----
        if self.screen == Screen::Search {
            if c == Esc {
                (self.screen, self.focus) = (Screen::Browse, 0);
            } else if self.focus == 0 {
                match c {
                    Enter => {
                        let q = self.query.trim().to_string();
                        if !q.is_empty() {
                            self.results = env.search_all(&q);
                            (self.sel_s, self.focus) = (0, 1);
                        }
                    }
                    Backspace => {
                        self.query.pop();
                    }
                    Char(ch @ ' '..='~') => self.query.push(ch),
                    _ => {}
                }
            } else {
                let n = self.results.len() as i64;
                match c {
                    Char('k') | Up => self.sel_s = clamp(self.sel_s - 1, 0, n - 1),
                    Char('j') | Down => self.sel_s = clamp(self.sel_s + 1, 0, n - 1),
                    Char('h') => self.focus = 0,
                    Enter | Char('f' | 'a' | 'A') if n > 0 => {
                        let Ok((title, tracks, thumb)) =
                            env.resolve_result(&self.results[self.sel_s as usize])
                        else {
                            return true;
                        };
                        if tracks.is_empty() {
                            return true;
                        }
                        let album = Album {
                            title: title.clone(),
                            tracks: Some(tracks.clone()),
                            thumb: thumb.clone(),
                            ..Default::default()
                        };
                        match c {
                            Char('a' | 'A') => {
                                self.add_tracks(env, tracks, &title, &thumb, c == Char('A'))
                            } // stays in search
                            Char('f') => {
                                self.do_play(env, Src::Temp(Box::new(album)), 0);
                                (self.screen, self.focus, self.sel[0]) = (Screen::Browse, 0, 0);
                            }
                            _ => {
                                self.hist = env.record(&title, &tracks, &thumb);
                                self.set_now(env, Some(album));
                                (self.screen, self.focus, self.sel[0]) = (Screen::Browse, 0, 0);
                            }
                        }
                    }
                    _ => {}
                }
            }
            return true;
        }

        // ----- browse screen -----
        if c == Char('q') {
            return false;
        }

        // LOCAL pane showing an album's tracklist: its own keys, and any key
        // that leaves the pane (h/l, /, Esc) closes it back to the album list.
        if let (Some(d), 1) = (self.drill, self.focus) {
            let dtracks = self.local[d].tracks.clone().unwrap_or_default();
            let n = dtracks.len() as i64;
            let dthumb = self.local[d].thumb.clone();
            match c {
                Char('j') | Down => {
                    self.dsel = clamp(self.dsel + 1, 0, n - 1);
                    return true;
                }
                Char('k') | Up => {
                    self.dsel = clamp(self.dsel - 1, 0, n - 1);
                    return true;
                }
                Enter | Char('f') => {
                    if c == Char('f') {
                        // f = the album, from here on
                        self.do_play(env, Src::Local(d), self.dsel as usize);
                    } else if let Some(t) = dtracks.get(self.dsel as usize) {
                        // enter = this track alone
                        let thumb = if t.thumb.is_empty() {
                            dthumb
                        } else {
                            t.thumb.clone()
                        };
                        let one = Album {
                            title: t.title.clone(),
                            tracks: Some(vec![t.clone()]),
                            thumb,
                            ..Default::default()
                        };
                        self.do_play(env, Src::Temp(Box::new(one)), 0);
                    }
                    (self.drill, self.focus, self.sel[0]) = (None, 0, 0);
                    return true;
                }
                Char('a' | 'A') => {
                    if let Some(t) = dtracks.get(self.dsel as usize) {
                        let thumb = if t.thumb.is_empty() {
                            dthumb
                        } else {
                            t.thumb.clone()
                        };
                        self.add_tracks(env, vec![t.clone()], &t.title, &thumb, c == Char('A'));
                    }
                    return true;
                }
                _ => {}
            }
            self.drill = None;
            if c == Esc {
                return true; // Esc: back to the album list, nothing else
            }
            // h/l, /, space, n/p ... fall through to the normal browse keys
        }
        match c {
            Char('/') => {
                (self.screen, self.focus) = (Screen::Search, 0);
                self.query.clear();
                self.results.clear();
            }
            Char(' ') => env.toggle_pause(),
            Char('n') => env.next(),
            Char('p') => env.prev(),
            Char('r') => env.toggle_loop(), // repeat-all; ↻ in the progress bar shows the state
            Char('e') => env.toggle_left_ear(), // left-ear-only; ◐ in the progress bar shows it
            Char('[') => env.volume(-5),    // msm's own volume, not the system's
            Char(']') => env.volume(5),
            Char('h') => self.focus = self.focus.saturating_sub(1),
            Char('l') => self.focus = (self.focus + 1).min(2),
            Char('j') | Down => {
                let n = self.pane_len(self.focus);
                self.sel[self.focus] = clamp(self.sel[self.focus] + 1, 0, n - 1);
            }
            Char('k') | Up => {
                let n = self.pane_len(self.focus);
                self.sel[self.focus] = clamp(self.sel[self.focus] - 1, 0, n - 1);
            }
            Enter => {
                if self.focus == 0 && self.now_tracks().is_some() {
                    self.do_play(env, Src::Now, self.sel[0] as usize);
                } else if self.focus == 1 {
                    if let Some(Src::Local(i)) = self.cur_album() {
                        if let Some((_, true)) = self.ensure(env, &Src::Local(i)) {
                            (self.drill, self.dsel) = (Some(i), 0);
                        }
                    }
                } else if let Some(src) = self.cur_album() {
                    // rec items resolve here; hist items no-op
                    if let Some((a, true)) = self.ensure(env, &src) {
                        self.set_now(env, Some(a));
                        (self.focus, self.sel[0]) = (0, 0);
                    }
                }
            }
            Char('f') => {
                if self.focus == 0 && self.now_tracks().is_some() {
                    self.do_play(env, Src::Now, 0);
                } else if let Some(src) = self.cur_album() {
                    self.do_play(env, src, 0);
                    (self.focus, self.sel[0]) = (0, 0);
                }
            }
            Char('a' | 'A') => {
                // a=queue at tail, A=play next
                let next = c == Char('A');
                if self.focus == 0 && self.now_tracks().is_some() {
                    let al = self.now.as_ref().unwrap();
                    if let Some(t) = al
                        .tracks
                        .as_ref()
                        .unwrap()
                        .get(self.sel[0] as usize)
                        .cloned()
                    {
                        let thumb = if t.thumb.is_empty() {
                            al.thumb.clone()
                        } else {
                            t.thumb.clone()
                        };
                        self.add_tracks(env, vec![t.clone()], &t.title, &thumb, next);
                    }
                } else if let Some(src) = self.cur_album() {
                    match self.ensure(env, &src) {
                        Some((a, true)) => self.add_tracks(
                            env,
                            a.tracks.clone().unwrap_or_default(),
                            &a.title,
                            &a.thumb,
                            next,
                        ),
                        _ => {
                            self.flash = "queue failed — run `msm auth`?".into();
                            self.flash_ttl = 8;
                        }
                    }
                }
            }
            Char('L') => {
                // shift+l: thumbs-up the highlighted (or playing) track
                let track = match (self.focus, self.now_tracks()) {
                    (0, Some(t)) => t.get(self.sel[0] as usize).cloned(),
                    _ => env.current(),
                };
                (self.flash, self.flash_ttl) = match track {
                    None => ("nothing to like".into(), 6),
                    Some(t) if env.like_track(&t) => (format!("♥ liked: {}", t.title), 6),
                    Some(_) => (
                        "♥ like failed — run `msm auth` (session expired?)".into(),
                        8,
                    ),
                };
            }
            _ => {}
        }
        true
    }
}

// ---- terminal -------------------------------------------------------------------

/// Write the whole buffer, one synchronized update, one flush.
fn render(buf: &Buf, out: &mut impl Write) -> io::Result<()> {
    queue!(out, terminal::BeginSynchronizedUpdate)?;
    for y in 0..buf.h {
        queue!(
            out,
            cursor::MoveTo(0, y as u16),
            SetAttribute(Attribute::Reset)
        )?;
        let mut cur = PLAIN;
        let mut run = String::new();
        for x in 0..buf.w {
            let (ch, st) = &buf.cells[(y * buf.w + x) as usize];
            let st = *st;
            if ch.is_empty() {
                continue; // right half of a wide char: the terminal already advanced
            }
            if st != cur {
                queue!(out, Print(std::mem::take(&mut run)))?;
                queue!(out, SetAttribute(Attribute::Reset))?;
                if let Some(fg) = st.fg {
                    queue!(out, SetForegroundColor(Color::AnsiValue(fg)))?;
                }
                if let Some(bg) = st.bg {
                    queue!(out, SetBackgroundColor(Color::AnsiValue(bg)))?;
                }
                if st.bold {
                    queue!(out, SetAttribute(Attribute::Bold))?;
                }
                if st.reverse {
                    queue!(out, SetAttribute(Attribute::Reverse))?;
                }
                cur = st;
            }
            run.push_str(ch);
        }
        queue!(out, Print(run))?;
    }
    queue!(
        out,
        SetAttribute(Attribute::Reset),
        terminal::EndSynchronizedUpdate
    )?;
    out.flush()
}

static TERM_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Leave raw mode / alt screen / hidden cursor. Idempotent.
fn restore_terminal() {
    if TERM_ACTIVE.swap(false, Ordering::SeqCst) {
        let mut out = io::stdout();
        let _ = queue!(
            out,
            SetAttribute(Attribute::Reset),
            cursor::Show,
            terminal::LeaveAlternateScreen
        );
        let _ = out.flush();
        let _ = terminal::disable_raw_mode();
    }
}

/// Take over the terminal: raw mode, alt screen, hidden cursor.
fn enter_terminal(out: &mut impl Write) -> io::Result<()> {
    terminal::enable_raw_mode()?;
    TERM_ACTIVE.store(true, Ordering::SeqCst);
    queue!(out, terminal::EnterAlternateScreen, cursor::Hide)?;
    out.flush()
}

/// Ctrl-Z the way curses did it: hand the terminal back, stop our whole
/// process group (mpv too — player.rs moves only cmusfm out of the group),
/// and take the terminal again once `fg` sends SIGCONT. The caller redraws.
fn suspend(out: &mut impl Write) {
    restore_terminal();
    // SAFETY: kill(2) with pid 0 = our process group; no memory involved.
    unsafe {
        libc::kill(0, libc::SIGTSTP);
    }
    let _ = enter_terminal(out);
}

/// Restores the terminal when run() returns. On a panic the hook (installed in
/// run) restores it first so the message lands on the normal screen; the
/// unwind then drops this guard, which also quits mpv — main's player.quit()
/// never runs on that path and mpv must not outlive us (quit is idempotent).
struct TermGuard<'a> {
    player: &'a Player,
}

impl Drop for TermGuard<'_> {
    fn drop(&mut self) {
        restore_terminal();
        if std::thread::panicking() {
            self.player.quit();
        }
    }
}

/// Own the terminal until `q`. Restores the terminal on return AND on panic.
pub fn run(yt: Arc<Yt>, player: &Player) {
    let ui = std::thread::current().id();
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // only the UI thread owns the screen; a background panic leaves it alone
        if std::thread::current().id() == ui {
            restore_terminal();
        }
        prev(info);
    }));

    // Python sets LC_CTYPE from the env at startup and curses measures text
    // with it; wcwidth needs the same or every non-ASCII char is unprintable.
    // SAFETY: called before any other thread reads the locale.
    unsafe {
        libc::setlocale(libc::LC_CTYPE, c"".as_ptr());
    }
    let mut out = io::stdout();
    if terminal::enable_raw_mode().is_err() {
        eprintln!("msm: not a terminal");
        return;
    }
    let _guard = TermGuard { player };
    let _ = enter_terminal(&mut out);

    let env = Real { yt, player };
    let mut st = State::new(&env);
    loop {
        let (w, h) = terminal::size().unwrap_or((80, 24));
        let buf = st.draw(&env, h as i64, w as i64);
        let _ = render(&buf, &mut out);

        // 500ms timeout: refresh progress even with no keypress. Resize and
        // other events just fall through to the next repaint.
        if !event::poll(Duration::from_millis(500)).unwrap_or(false) {
            continue;
        }
        let Ok(Event::Key(k)) = event::read() else {
            continue;
        };
        for key in map_key(k) {
            if key == Key::Suspend {
                suspend(&mut out);
                break; // back from fg: repaint
            }
            if !st.handle_key(&env, key) {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Fake {
        local: Vec<Album>,
        played: RefCell<Vec<(usize, usize)>>,
    }

    impl Env for Fake {
        fn authed(&self) -> bool {
            false
        }
        fn spawn_recs(&self, _: Arc<Mutex<Vec<Album>>>) {}
        fn search_all(&self, _: &str) -> Vec<Item> {
            vec![]
        }
        fn resolve_result(&self, _: &Item) -> Result<(String, Vec<Track>, String), String> {
            Err("fake".into())
        }
        fn like_track(&self, _: &Track) -> bool {
            false
        }
        fn load_history(&self) -> Vec<Album> {
            vec![]
        }
        fn scan_local(&self) -> Vec<Album> {
            self.local.clone()
        }
        fn record(&self, _: &str, _: &[Track], _: &str) -> Vec<Album> {
            vec![]
        }
        fn load_local_album(&self, _: &mut Album) {}
        fn art_file(&self, _: &str, _: &str) -> Option<PathBuf> {
            None
        }
        fn progress(&self) -> Progress {
            Progress {
                pos: 0.0,
                dur: 0.0,
                paused: false,
                title: String::new(),
                looping: false,
                left_ear: false,
                vol: 100,
            }
        }
        fn current(&self) -> Option<Track> {
            None
        }
        fn play(&self, tracks: &[Track], start: usize) {
            self.played.borrow_mut().push((tracks.len(), start));
        }
        fn enqueue(&self, _: &[Track]) {}
        fn play_next(&self, _: &[Track]) -> Option<usize> {
            None
        }
        fn toggle_pause(&self) {}
        fn next(&self) {}
        fn prev(&self) {}
        fn toggle_loop(&self) {}
        fn toggle_left_ear(&self) {}
        fn volume(&self, _: i64) {}
    }

    /// Drive the loop like _Scr: one frame per repaint, then one key; keys
    /// exhausted -> 'q'.
    fn drive(env: &Fake, keys: &[Key], h: i64, w: i64) -> Vec<String> {
        let mut st = State::new(env);
        let mut keys = keys.iter().copied();
        let mut frames = vec![];
        loop {
            frames.push(st.draw(env, h, w).text());
            if !st.handle_key(env, keys.next().unwrap_or(Key::Char('q'))) {
                return frames;
            }
        }
    }

    fn album_x() -> Album {
        let tracks = (0..3)
            .map(|i| Track {
                url: format!("u{i}"),
                title: format!("T{i}"),
                ..Default::default()
            })
            .collect();
        Album {
            title: "Album X".into(),
            tracks: Some(tracks),
            local: Some(Default::default()),
            ..Default::default()
        }
    }

    #[test]
    fn test_local_pane_opens_an_album_tracklist_and_esc_puts_the_list_back() {
        utf8_locale();
        // enter on a LOCAL album swaps the pane to its tracks; j/k move inside it,
        // enter plays the highlighted track alone, f the album from there, Esc
        // restores the album list.
        use Key::*;
        let env = Fake {
            local: vec![album_x()],
            ..Default::default()
        };
        let keys = [
            Char('l'),
            Enter,
            Char('j'),
            Esc,
            Enter,
            Char('j'),
            Enter,
            Char('l'),
            Enter,
            Char('j'),
            Char('f'),
        ];
        let frames = drive(&env, &keys, 40, 120);
        assert!(
            frames[2].contains("Album X") && frames[2].contains("T2"),
            "{}",
            frames[2]
        );
        assert!(
            !frames[2].contains("LOCAL ~/Music"),
            "album list still there"
        );
        assert!(frames[4].contains("LOCAL ~/Music"), "{}", frames[4]); // Esc -> album list back
                                                                       // enter = the highlighted track alone; f = the whole album from there
        assert_eq!(*env.played.borrow(), vec![(1, 0), (3, 1)]);
    }

    #[test]
    fn ctrl_keys_map_like_curses() {
        let k = |c, m| map_key(KeyEvent::new(KeyCode::Char(c), m));
        assert_eq!(k('z', KeyModifiers::CONTROL), [Key::Suspend]);
        assert_eq!(k('c', KeyModifiers::CONTROL), [Key::Interrupt]);
        assert_eq!(k('h', KeyModifiers::CONTROL), [Key::Backspace]);
        assert_eq!(k('A', KeyModifiers::SHIFT), [Key::Char('A')]);
        assert_eq!(k('x', KeyModifiers::ALT), [Key::Esc, Key::Char('x')]);
    }

    /// Rust starts in the "C" locale, where wcwidth calls every non-ASCII char
    /// unprintable; the locale is process-wide, so set it once for all tests.
    fn utf8_locale() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        // SAFETY: once, before this test measures; any UTF-8 locale gives the same widths.
        ONCE.call_once(|| unsafe {
            libc::setlocale(libc::LC_CTYPE, c"en_US.UTF-8".as_ptr());
        });
    }

    #[test]
    fn put_lays_out_wide_and_zero_width_chars_like_ncurses() {
        utf8_locale();
        let mut buf = Buf::new(3, 10);
        let win = buf.whole();
        // U+2B50 is 2 cells, U+FE0F joins it: 8 chars -> "Album " + 2 cells
        put(&mut buf, win, 0, 0, "Album \u{2B50}\u{FE0F}!", -1, PLAIN);
        assert_eq!(buf.cells[6].0, "\u{2B50}\u{FE0F}");
        assert_eq!(buf.cells[7].0, ""); // right half
        assert_eq!(buf.cells[8].0, "!");
        // CJK: 2 cells each; the one that would straddle the edge isn't drawn
        put(&mut buf, win, 1, 0, "漢字漢字漢字", -1, PLAIN);
        assert_eq!(buf.text().lines().nth(1).unwrap(), "漢字漢字漢");
        put(&mut buf, win, 1, 1, "漢字漢字漢字", -1, PLAIN);
        assert_eq!(buf.text().lines().nth(1).unwrap(), " 漢字漢字"); // clobbered half blanked
                                                                     // NFD é: n counts chars, so n=2 keeps "e" + accent in one cell
        put(&mut buf, win, 2, 0, "e\u{301}xyz", 2, PLAIN);
        assert_eq!(buf.text().lines().nth(2).unwrap(), "e\u{301}");
        let mut out = Vec::new();
        render(&buf, &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("Album \u{2B50}\u{FE0F}!"), "{out:?}"); // continuation not printed
    }

    #[test]
    fn draw_rows_pads_by_chars_then_clips_by_cells() {
        utf8_locale();
        let mut buf = Buf::new(3, 10);
        let win = draw_box(&mut buf, 0, 0, 3, 10, "", false).unwrap();
        draw_rows(&mut buf, win, &["漢字漢字漢字".into()], 0, false);
        // 7 chars + 1 pad = bw, but 13 cells: like curses it runs over the right
        // border (the derwin includes it) and stops at the window edge
        assert_eq!(buf.text().lines().nth(1).unwrap(), "│ 漢字漢字");
    }

    #[test]
    fn other_keys_close_the_drill_in() {
        utf8_locale();
        use Key::*;
        let env = Fake {
            local: vec![album_x()],
            ..Default::default()
        };
        let frames = drive(&env, &[Char('l'), Enter, Other], 40, 120);
        assert!(!frames[2].contains("LOCAL ~/Music"), "{}", frames[2]);
        assert!(frames[3].contains("LOCAL ~/Music"), "{}", frames[3]);
        assert_eq!(
            map_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            [Other]
        );
        assert_eq!(
            map_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            [Other]
        );
        assert_eq!(
            map_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)),
            [Other]
        );
    }

    #[test]
    fn layout_matches_python() {
        // expected values computed by the Python layout code
        let cases = [
            ((40, 120), (17, 43, 19, 43, 38, 39, 18)),
            ((24, 80), (9, 24, 11, 24, 28, 28, 10)),
            ((10, 40), (2, 7, 4, 0, 20, 20, 3)),
            ((60, 200), (18, 45, 20, 45, 77, 78, 37)),
            ((30, 60), (9, 24, 11, 24, 18, 18, 16)),
            ((5, 20), (0, 2, 2, 0, 10, 10, 0)),
        ];
        for ((h, w), want) in cases {
            let l = layout(h, w);
            assert_eq!(
                (l.art_h, l.art_bw, l.art_bh, l.rcw, l.npw, l.lmw, l.last5_h),
                want,
                "{h}x{w}"
            );
        }
    }

    #[test]
    fn py_round_is_bankers() {
        assert_eq!(
            [
                py_round(0.5),
                py_round(1.5),
                py_round(2.5),
                py_round(-0.5),
                py_round(2.6)
            ],
            [0, 2, 2, 0, 3]
        );
    }

    #[test]
    fn draw_rows_scroll_offset() {
        utf8_locale();
        assert_eq!(scroll_off(0, 5, 20), 0);
        assert_eq!(scroll_off(10, 5, 20), 8); // sel mid-box
        assert_eq!(scroll_off(19, 5, 20), 15); // clamped to the end
        assert_eq!(scroll_off(3, 10, 4), 0); // list fits
        let mut buf = Buf::new(4, 8);
        let win = draw_box(&mut buf, 0, 0, 4, 8, "", true).unwrap();
        let rows: Vec<String> = (0..6).map(|i| format!("r{i}")).collect();
        draw_rows(&mut buf, win, &rows, 4, true);
        let text = buf.text();
        assert!(
            text.contains("│ r3   │") && text.contains("│ r4   │"),
            "{text}"
        );
        assert_eq!(buf.cells[2 * 8 + 1].1, SELECT); // r4 row highlighted
    }

    #[test]
    fn put_truncates_by_chars_and_clips() {
        utf8_locale();
        let mut buf = Buf::new(2, 6);
        let win = buf.whole();
        put(&mut buf, win, 0, 0, "héllo world", 3, PLAIN);
        assert_eq!(buf.text().lines().next().unwrap(), "hél");
        put(&mut buf, win, 1, 2, "abcdefgh", -1, PLAIN); // n<0 = whole string, clipped
        assert_eq!(buf.text().lines().nth(1).unwrap(), "  abcd");
        put(&mut buf, win, 5, 0, "x", 1, PLAIN); // off-window: nothing
        let mut out = Vec::new();
        render(&buf, &mut out).unwrap(); // last cell written, no newline -> no scroll
        assert!(!out.contains(&b'\n'));
    }

    #[test]
    fn box_title_and_progress() {
        utf8_locale();
        let mut buf = Buf::new(3, 20);
        draw_progress(
            &mut buf,
            0,
            20,
            &Progress {
                pos: 30.0,
                dur: 120.0,
                paused: true,
                title: "Song".into(),
                looping: true,
                left_ear: false,
                vol: 80,
            },
            "",
        );
        let t = buf.text();
        assert!(t.starts_with("┌─ ↻ 80% ‖ So"), "{t}"); // title truncated to w-4
        assert!(t.contains("│█░░░ 0:30 / 2:00  │"), "{t}");
    }

    #[test]
    fn dump_frame() {
        utf8_locale();
        let env = Fake {
            local: vec![album_x()],
            ..Default::default()
        };
        let mut st = State::new(&env);
        st.handle_key(&env, Key::Char('l'));
        let f = st.draw(&env, 24, 80).text();
        println!("{f}");
        assert_eq!(f.lines().count(), 24);
        assert!(
            f.contains("LAST 5") && f.contains("cover") && f.contains("no art"),
            "{f}"
        );
    }
}
