#!/usr/bin/env python3
"""ymc TUI — curses browser: bordered panes, purple/grey theme, album art.

Screens:
  browse  left = tracklist of most-recent album; right = last 5 albums
          (enter expands, f plays from start); bottom-right = pixelated
          album art (square, capped); bottom = progress bar.
  search  '/' opens it: type query, enter runs it, jk pick a result,
          enter = load into browse (no play), f = load + play, Esc = back.
Keys: h/l switch pane, j/k move, space pause, n/p next/prev, L like, q quit.
"""
import curses
import os
import threading

from . import ymc

ART_CAP = 18       # max album-art height in cells
CELL_ASPECT = 2.4  # terminal cell height:width. iTerm2/Menlo ~2.4; tune per font
                   # so the cover reads as a square (art_w = art_h * CELL_ASPECT)

# theme color-pair ids (1-4 reserved); album-art pairs allocated from 10 up
BORDER, ACCENT, SELECT, DIM = 1, 2, 3, 4


def init_theme():
    try:
        curses.start_color()
        curses.use_default_colors()
    except curses.error:
        return
    curses.init_pair(BORDER, 141, -1)   # purple border/title on default bg
    curses.init_pair(ACCENT, 177, -1)   # bright purple: playing title, bar
    curses.init_pair(SELECT, 16, 141)   # selected row: near-black on purple
    curses.init_pair(DIM, 244, -1)      # grey


def clamp(v, lo, hi):
    return max(lo, min(v, hi))


def fmt_time(s):
    s = int(s)
    return "%d:%02d" % (s // 60, s % 60)


def _put(win, y, x, s, w, attr=0):
    try:
        win.addnstr(y, x, s, w, attr)
    except curses.error:
        pass  # curses errors writing the last cell; harmless


def box(stdscr, y, x, h, w, title, focused):
    """Bordered subwindow; title on the top edge. Returns win (or None)."""
    if h < 2 or w < 2:
        return None
    win = stdscr.derwin(h, w, y, x)
    win.attrset(curses.color_pair(BORDER) | (curses.A_BOLD if focused else 0))
    win.box()
    win.attrset(0)
    if title:
        attr = curses.color_pair(BORDER) | curses.A_BOLD
        if focused:
            attr |= curses.A_REVERSE
        _put(win, 0, 2, " %s " % title, w - 4, attr)
    return win


def draw_rows(win, rows, sel, focused):
    """Scrolling list inside a bordered box (inner region = h-2 x w-2)."""
    h, w = win.getmaxyx()
    bh, bw = h - 2, w - 2
    if bh <= 0 or bw <= 0:
        return
    off = clamp(sel - bh // 2, 0, max(0, len(rows) - bh))
    for i in range(bh):
        idx = off + i
        if idx >= len(rows):
            break
        attr = curses.color_pair(SELECT) if (idx == sel and focused) else 0
        _put(win, 1 + i, 1, (" " + rows[idx]).ljust(bw), bw, attr)


# ---- album art (256-color half-block) --------------------------------------

_art_pairs = {}
_art_next = [10]
_grid_cache = {}


def _xterm256(r, g, b):
    return 16 + 36 * (r // 51) + 6 * (g // 51) + (b // 51)


def _pair(fg, bg):
    key = (fg, bg)
    if key not in _art_pairs:
        if _art_next[0] >= min(curses.COLOR_PAIRS, 32000):
            return 0  # out of pairs -> uncolored
        try:
            curses.init_pair(_art_next[0], fg, bg)
        except curses.error:
            return 0
        _art_pairs[key] = _art_next[0]
        _art_next[0] += 1
    return _art_pairs[key]


def _art_grid(path, cols, rows):
    """Resize art to cols x 2*rows px (NEAREST = pixelated); (fg,bg) per cell.
    Each cell is a half-block: 1px wide, 2px tall -> square when cols == 2*rows.
    """
    key = (path, cols, rows)
    if key in _grid_cache:
        return _grid_cache[key]
    from PIL import Image
    img = Image.open(path).convert("RGB").resize((cols, rows * 2), Image.NEAREST)
    px = img.load()
    grid = [[(_xterm256(*px[cx, cy * 2]), _xterm256(*px[cx, cy * 2 + 1]))
             for cx in range(cols)] for cy in range(rows)]
    _grid_cache[key] = grid
    return grid


def draw_art(win, path):
    """Fill the box with a half-block rendering of the art (top px=fg, bot=bg)."""
    h, w = win.getmaxyx()
    rows, cols = h - 2, w - 2
    if rows < 3 or cols < 6:
        return
    if not path:
        _put(win, h // 2, (w - 6) // 2, "no art", 6, curses.color_pair(DIM))
        return
    try:
        grid = _art_grid(path, cols, rows)
    except Exception:
        _put(win, h // 2, (w - 6) // 2, "no art", 6, curses.color_pair(DIM))
        return
    for cy in range(rows):
        for cx in range(cols):
            fg, bg = grid[cy][cx]
            p = _pair(fg, bg)
            _put(win, 1 + cy, 1 + cx, "▀", 1, curses.color_pair(p) if p else 0)


def draw_progress(stdscr, y, w, player, note=""):
    pos, dur, paused, title = player.progress()
    state = "‖" if paused else "▶"
    label = note if note else "%s %s" % (state, title)
    win = box(stdscr, y, 0, 3, w, label, False)
    if not win:
        return
    bw = w - 2
    times = "%s / %s" % (fmt_time(pos), fmt_time(dur))
    barw = max(1, bw - len(times) - 3)
    filled = int(barw * (pos / dur)) if dur else 0
    _put(win, 1, 1, "█" * filled, bw, curses.color_pair(ACCENT))
    _put(win, 1, 1 + filled, "░" * (barw - filled), bw - filled, curses.color_pair(DIM))
    _put(win, 1, 1 + barw + 1, times, bw - barw - 1, curses.color_pair(DIM))


def _pane_len(focus, now, local, hist):
    if focus == 0:
        al = now["album"]
        return len(al["tracks"]) if al and al.get("tracks") else 0
    if focus == 1:
        return len(local)
    return len(hist)


def art_for(album):
    """Resolve an album's art to a local path. http thumb -> download; else a local path."""
    if not album:
        return None
    t = album.get("thumb") or ""
    if t.startswith("http"):
        return ymc.art_file(album["title"], t)
    return t if (t and os.path.exists(t)) else None


def run(stdscr, yt, player):
    curses.curs_set(0)
    init_theme()
    stdscr.timeout(500)  # ms; refresh progress even with no keypress

    hist = ymc.load_history()
    local = ymc.scan_local()
    recs = []            # authed: YT Music recs, filled off-thread after first paint
    screen = "browse"
    focus = 0            # 0=NowPlaying 1=Local 2=pane2  (search: 0=bar 1=results)
    sel = [0, 0, 0]      # per-pane selection
    sel_s = 0
    query = ""
    results = []
    now = {"album": None, "art": None}

    if ymc.AUTHED:  # network call -> off-thread so it can't delay first paint
        threading.Thread(target=lambda: recs.extend(ymc.get_recs(yt)),
                          daemon=True).start()

    def pane2():
        """Active pane-2 list: YT recs when authed, else local last-5."""
        return recs if ymc.AUTHED else hist

    def set_now(album):
        now["album"] = album
        now["art"] = art_for(album)

    def ensure_tracks(album):
        """Fill an album's tracks lazily (local scan or YT rec resolve). -> bool playable."""
        if album.get("tracks") is None:
            if album.get("local"):
                ymc.load_local_album(album)
            elif album.get("rec"):
                try:
                    _, tracks, thumb = ymc.resolve_result(yt, album["rec"])
                except Exception:
                    tracks = []
                album["tracks"] = tracks
                if tracks:
                    album["thumb"] = thumb
        return bool(album.get("tracks"))

    def do_play(album, start_idx=0):
        nonlocal hist
        if not ensure_tracks(album):
            return
        set_now(album)
        hist = ymc.record(album["title"], album["tracks"], album.get("thumb") or "")
        player.play(album["tracks"], start_idx)

    if hist:
        set_now(hist[0])

    flash = ""       # transient status shown in the progress bar (e.g. "♥ liked")
    flash_ttl = 0    # refresh cycles the flash stays visible

    while True:
        stdscr.erase()
        H, W = stdscr.getmaxyx()
        prog_h = 3
        main_h = H - prog_h

        if screen == "browse":
            # cover (visually square) defines the right-column width
            art_h = min(ART_CAP, main_h // 2 - 1)
            art_bw = round(art_h * CELL_ASPECT) + 2
            if art_bw > int(W * 0.42):          # keep right column from dominating
                art_bw = int(W * 0.42)
                art_h = int((art_bw - 2) / CELL_ASPECT)
                art_bw = round(art_h * CELL_ASPECT) + 2
            art_bh = art_h + 2
            rcw = art_bw if art_h >= 4 else 0
            left_w = W - rcw
            npw = left_w // 2
            lmw = left_w - npw
            last5_h = main_h - art_bh

            al = now["album"]
            wl = box(stdscr, 0, 0, main_h, npw,
                     "NOW: " + (al["title"] if al else "(open an album)"), focus == 0)
            if wl:
                draw_rows(wl, [t["title"] for t in al["tracks"]] if al and al.get("tracks") else [],
                          sel[0], focus == 0)

            wm = box(stdscr, 0, npw, main_h, lmw, "LOCAL ~/Music  (enter=open f=play)", focus == 1)
            if wm:
                draw_rows(wm, [a["title"] for a in local], sel[1], focus == 1)

            if rcw:
                p2 = pane2()
                title2 = "FOR YOU  (enter=open f=play)" if ymc.AUTHED else "LAST 5  (enter=open f=play)"
                w5 = box(stdscr, 0, left_w, last5_h, rcw, title2, focus == 2)
                if w5:
                    rows2 = [a["title"] for a in p2] or (["loading…"] if ymc.AUTHED else [])
                    draw_rows(w5, rows2, sel[2], focus == 2)
                wa = box(stdscr, main_h - art_bh, left_w, art_bh, art_bw, "cover", False)
                if wa:
                    draw_art(wa, now["art"])
        else:  # search
            wb = box(stdscr, 0, 0, 3, W, "search", focus == 0)
            if wb:
                _put(wb, 1, 1, " " + query + ("█" if focus == 0 else ""), W - 2,
                     curses.color_pair(ACCENT))
            wr = box(stdscr, 3, 0, main_h - 3, W, "RESULTS  (enter=load f=play Esc=back)", focus == 1)
            if wr:
                draw_rows(wr, ["[%s] %s — %s" % (r.get("resultType", "?"), r.get("title", "?"),
                               ymc.artists_str(r)) for r in results], sel_s, focus == 1)

        draw_progress(stdscr, main_h, W, player, flash if flash_ttl > 0 else "")
        if flash_ttl > 0:
            flash_ttl -= 1
        stdscr.refresh()

        c = stdscr.getch()
        if c == -1:
            continue

        # ----- search screen -----
        if screen == "search":
            if c == 27:  # Esc
                screen, focus = "browse", 0
            elif focus == 0:
                if c in (curses.KEY_ENTER, 10, 13):
                    if query.strip():
                        results = ymc.search_all(yt, query.strip())
                        sel_s, focus = 0, 1
                elif c in (curses.KEY_BACKSPACE, 127, 8):
                    query = query[:-1]
                elif 32 <= c < 127:
                    query += chr(c)
            else:
                if c in (ord("k"), curses.KEY_UP):
                    sel_s = clamp(sel_s - 1, 0, len(results) - 1)
                elif c in (ord("j"), curses.KEY_DOWN):
                    sel_s = clamp(sel_s + 1, 0, len(results) - 1)
                elif c == ord("h"):
                    focus = 0
                elif c in (curses.KEY_ENTER, 10, 13, ord("f")) and results:
                    try:
                        title, tracks, thumb = ymc.resolve_result(yt, results[sel_s])
                    except Exception:
                        tracks = []
                    if tracks:
                        album = {"title": title, "tracks": tracks, "thumb": thumb}
                        if c == ord("f"):
                            do_play(album, 0)
                        else:
                            hist = ymc.record(title, tracks, thumb)
                            set_now(album)
                        screen, focus, sel[0] = "browse", 0, 0
            continue

        # ----- browse screen -----
        if c == ord("q"):
            return
        if c == ord("/"):
            screen, focus, query, results = "search", 0, "", []
        elif c == ord(" "):
            player.toggle_pause()
        elif c == ord("n"):
            player.next()
        elif c == ord("p"):
            player.prev()
        elif c == ord("h"):
            focus = max(0, focus - 1)
        elif c == ord("l"):
            focus = min(2, focus + 1)
        elif c in (ord("j"), curses.KEY_DOWN):
            n = _pane_len(focus, now, local, pane2())
            sel[focus] = clamp(sel[focus] + 1, 0, n - 1)
        elif c in (ord("k"), curses.KEY_UP):
            n = _pane_len(focus, now, local, pane2())
            sel[focus] = clamp(sel[focus] - 1, 0, n - 1)
        elif c in (curses.KEY_ENTER, 10, 13):
            if focus == 0 and now["album"] and now["album"].get("tracks"):
                do_play(now["album"], sel[0])
            elif focus == 1 and local:
                a = local[sel[1]]
                if a.get("tracks") is None:
                    ymc.load_local_album(a)
                set_now(a)
                focus, sel[0] = 0, 0
            elif focus == 2 and pane2():
                a = pane2()[sel[2]]
                if ensure_tracks(a):  # rec items resolve here; hist items no-op
                    set_now(a)
                    focus, sel[0] = 0, 0
        elif c == ord("f"):
            if focus == 0 and now["album"] and now["album"].get("tracks"):
                do_play(now["album"], 0)
            elif focus == 1 and local:
                do_play(local[sel[1]], 0)
                focus, sel[0] = 0, 0
            elif focus == 2 and pane2():
                do_play(pane2()[sel[2]], 0)
                focus, sel[0] = 0, 0
        elif c == ord("L"):  # shift+l: thumbs-up the highlighted (or playing) track
            al = now["album"]
            track = (al["tracks"][sel[0]] if focus == 0 and al and al.get("tracks")
                     else player.current)
            if track and ymc.like_track(yt, track):
                flash, flash_ttl = "♥ liked: " + track["title"], 6
            else:
                flash, flash_ttl = "♥ like failed (needs YT sign-in + a YT track)", 6


def main():
    import shutil
    import sys
    if len(sys.argv) > 1 and sys.argv[1] == "auth":
        ymc.setup_auth(sys.argv[2] if len(sys.argv) > 2 else None)
        return
    for tool in ("mpv", "cmusfm"):
        if not shutil.which(tool):
            sys.exit("missing required tool: " + tool)
    yt = ymc.get_yt()
    player = ymc.Player(yt)
    try:
        curses.wrapper(run, yt, player)
    finally:
        player.quit()


if __name__ == "__main__":
    main()
