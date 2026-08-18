#!/usr/bin/env python3
"""Self-check: cmusfm argv, album/result parse, history, album art.
No network, no mpv, no terminal."""
import curses
import os
import queue
import subprocess
import tempfile
from unittest import mock

from msm import tui, ymc

TRACK = {
    "url": "https://music.youtube.com/watch?v=abc",
    "artist": "Radiohead", "album": "In Rainbows",
    "title": "Nude", "duration": 256,
}


def test_cmusfm_argv():
    with mock.patch.object(subprocess, "run") as run:
        ymc.cmusfm("playing", TRACK)
        argv = run.call_args[0][0]
    # matches cmus status_display_program protocol: pairs after status
    assert argv[:3] == ["cmusfm", "status", "playing"], argv
    d = dict(zip(argv[3::2], argv[4::2]))
    assert d == {
        "file": TRACK["url"], "artist": "Radiohead", "album": "In Rainbows",
        "title": "Nude", "duration": "256",
    }, d


def test_cmusfm_stopped_has_no_track():
    with mock.patch.object(subprocess, "run") as run:
        ymc.cmusfm("stopped", None)
        assert run.call_args[0][0] == ["cmusfm", "status", "stopped"]


def test_album_parse_drops_unavailable():
    alb = {"title": "X", "artists": [{"name": "Band"}], "tracks": [
        {"title": "A", "videoId": "v1", "duration_seconds": 100, "artists": [{"name": "Band"}]},
        {"title": "Gone", "videoId": None},  # unavailable -> dropped
    ]}
    with mock.patch.object(ymc, "YTMusic"):
        yt = mock.Mock()
        yt.get_album.return_value = alb
        title, tracks, thumb = ymc.album_tracks(yt, "id")
    assert title == "X"
    assert len(tracks) == 1
    assert tracks[0]["url"].endswith("v1")


def test_resolve_song_is_single_track():
    r = {"resultType": "song", "title": "Nude", "videoId": "v9",
         "artists": [{"name": "Radiohead"}], "album": {"name": "In Rainbows"},
         "duration_seconds": 256}
    title, tracks, thumb = ymc.resolve_result(mock.Mock(), r)
    assert title == "Nude"
    assert len(tracks) == 1 and tracks[0]["url"].endswith("v9")
    assert tracks[0]["album"] == "In Rainbows"


def test_resolve_album_delegates():
    yt = mock.Mock()
    yt.get_album.return_value = {"title": "Al", "artists": [{"name": "B"}],
                                 "tracks": [{"title": "t", "videoId": "v", "artists": []}]}
    title, tracks, thumb = ymc.resolve_result(yt, {"resultType": "album", "browseId": "MPRE"})
    assert title == "Al" and len(tracks) == 1


def test_headers_from_input_formats():
    curl = ("curl 'https://music.youtube.com/youtubei/v1/browse' "
            "-H 'authorization: SAPISIDHASH 1_a' -H 'cookie: SID=x; SAPISID=y' "
            "-H 'x-goog-authuser: 0' --data-raw '{}'")
    h = ymc._headers_from_input(curl)
    assert "cookie: SID=x; SAPISID=y" in h and "x-goog-authuser: 0" in h
    # -b cookie form
    assert "cookie: SID=z" in ymc._headers_from_input("curl 'u' -H 'a: b' -b 'SID=z'")
    # alternating name/value (Chrome two-line paste)
    assert "cookie: SID=1" in ymc._headers_from_input("cookie\nSID=1\nx-goog-authuser\n0\n")
    # clean key: value passthrough
    assert "cookie: SID=1" in ymc._headers_from_input("cookie: SID=1\n")


def test_artists_str_drops_playcount_token():
    # get_home() song items lump a "<N> plays" token into artists -> must drop
    item = {"artists": [{"name": "The 1975", "id": "UC"}, {"name": "315M plays", "id": None}]}
    assert ymc.artists_str(item) == "The 1975"
    assert ymc.artists_str({"artists": [{"name": "Coldplay"}, {"name": "2.2B plays"}]}) == "Coldplay"
    assert ymc.artists_str({"artists": [{"name": "A"}, {"name": "1.2M views"}]}) == "A"
    # real multi-artist rows unaffected
    assert ymc.artists_str({"artists": [{"name": "A"}, {"name": "B"}]}) == "A, B"
    # type-word token ("Song") leaked by get_home is dropped
    assert ymc.artists_str({"artists": [{"name": "Song", "id": None},
                                        {"name": "Faerybabyy", "id": "UC"}]}) == "Faerybabyy"


def test_video_id_parses_yt_url_and_skips_local():
    assert ymc._video_id("https://music.youtube.com/watch?v=abc") == "abc"
    assert ymc._video_id("https://music.youtube.com/watch?v=abc&list=RDAMVMxyz") == "abc"
    assert ymc._video_id("/Users/mark/Music/Album/01 - Song.flac") is None


def test_resolve_home_song_without_resulttype():
    item = {"title": "Nude", "videoId": "v9", "artists": [{"name": "Radiohead"}],
            "playlistId": "RDAMVMv9"}  # radio playlistId present -> videoId must win
    title, tracks, _ = ymc.resolve_result(mock.Mock(), item)
    assert title == "Nude" and len(tracks) == 1 and tracks[0]["url"].endswith("v9")


def test_resolve_home_album_without_resulttype():
    yt = mock.Mock()
    yt.get_album.return_value = {"title": "Al", "artists": [{"name": "B"}],
                                 "tracks": [{"title": "t", "videoId": "v", "artists": []}]}
    title, tracks, _ = ymc.resolve_result(yt, {"title": "Al", "browseId": "MPREb_x"})
    assert title == "Al" and len(tracks) == 1
    yt.get_album.assert_called_once_with("MPREb_x")


def test_get_recs_flattens_and_drops_nonplayable():
    yt = mock.Mock()
    yt.get_home.return_value = [
        {"title": "Row1", "contents": [
            {"title": "Header only"},                      # no id -> dropped
            {"title": "Song", "videoId": "v1", "thumbnails": []},
        ]},
        {"title": "Row2", "contents": [
            {"title": "Album", "browseId": "MPREb", "thumbnails": []},
            {"videoId": "v2", "thumbnails": []},           # no title -> dropped
        ]},
    ]
    recs = ymc.get_recs(yt, limit=5)
    assert [r["title"] for r in recs] == ["Song", "Album"]
    assert all(r["tracks"] is None and "rec" in r for r in recs)  # lazy + raw kept


def test_get_recs_swallows_errors():
    yt = mock.Mock()
    yt.get_home.side_effect = RuntimeError("offline")
    assert ymc.get_recs(yt) == []


def test_history_prepend_dedupe_cap():
    with tempfile.TemporaryDirectory() as d:
        with mock.patch.object(ymc, "HIST", os.path.join(d, "h.json")):
            for i in range(7):
                ymc.record("Album %d" % i, [{"title": "x"}])
            ymc.record("Album 3", [{"title": "again"}])  # dedupe -> front
            h = ymc.load_history()
    assert len(h) == 5                       # capped at 5
    assert h[0]["title"] == "Album 3"        # most recent first
    assert [a["title"] for a in h].count("Album 3") == 1  # no dup


def test_enqueue_appends_and_merges_by_url():
    class StubIPC:
        def __init__(self):
            self.cmds = []

        def cmd(self, c):
            self.cmds.append(c)

    p = object.__new__(ymc.Player)  # bypass __init__ (spawns real mpv)
    p.ipc = StubIPC()
    p.by_url = {}
    p.fetchq = queue.Queue()
    t1 = {"url": "u1", "title": "A"}
    t2 = {"url": "u2", "title": "B"}
    p.enqueue([t1, t2])
    assert p.ipc.cmds == [["loadfile", "u1", "append"], ["loadfile", "u2", "append"]]
    assert p.by_url == {"u1": t1, "u2": t2}   # queued tracks resolvable for scrobble
    assert [p.fetchq.get_nowait() for _ in range(2)] == [t1, t2]  # download in order


def test_play_next_inserts_after_current_in_order():
    class StubIPC:
        def __init__(self, pos):
            self.pos, self.cmds = pos, []

        def cmd(self, c):
            self.cmds.append(c)
            return self.pos if c == ["get_property", "playlist-pos"] else None

    p = object.__new__(ymc.Player)
    p.ipc = StubIPC(2)          # current track at playlist index 2
    p.by_url = {}
    p.fetchq = queue.Queue()
    t1 = {"url": "u1", "title": "A"}
    t2 = {"url": "u2", "title": "B"}
    idx = p.play_next([t1, t2])
    assert idx == 3             # first inserted right after current (2 -> 3)
    assert [c for c in p.ipc.cmds if c[0] == "loadfile"] == [
        ["loadfile", "u1", "insert-at", 3],
        ["loadfile", "u2", "insert-at", 4],   # order preserved, not reversed
    ]
    assert p.by_url == {"u1": t1, "u2": t2}


def test_play_next_returns_none_when_idle():
    class StubIPC:
        def cmd(self, c):
            return -1           # mpv reports no current entry
    p = object.__new__(ymc.Player)
    p.ipc = StubIPC()
    p.by_url = {}
    assert p.play_next([{"url": "u1", "title": "A"}]) is None
    assert p.by_url == {}       # nothing queued when idle


def _cover(path, kind):
    """Two shapes of cover: a busy one with no separable background, and a
    clean black background with one bright object."""
    from PIL import Image
    img = Image.new("RGB", (300, 300))
    px = img.load()
    for y in range(300):
        for x in range(300):
            if kind == "busy":   # dense color everywhere, low contrast
                px[x, y] = ((x * 7 + y * 3) % 256, (x * 3 + y * 11) % 256, (x * 5 + y) % 256)
            else:                # pure black field, one saturated blob
                d = (x - 150) ** 2 + (y - 150) ** 2
                px[x, y] = (240, 180, 20) if d < 80 ** 2 else (0, 0, 0)
    img.save(path)
    return path


class _Win:
    """Minimal curses window: records the attr of every cell written."""

    def __init__(self, h, w):
        self.h, self.w, self.attrs = h, w, []

    def getmaxyx(self):
        return self.h, self.w

    def addnstr(self, y, x, s, n, attr=0):
        self.attrs.append(attr)


def test_art_never_falls_back_to_uncolored_pairs():
    """Every art cell must get a real color pair. Pair 0 renders as a default
    fg/bg half-block — the black and white bars bug — and it used to happen once
    a few covers' worth of pairs had leaked, whatever the image looked like.
    """
    budget = 256
    with tempfile.TemporaryDirectory() as d:
        covers = [_cover(os.path.join(d, f"{k}{i}.png"), k)
                  for i in range(3) for k in ("busy", "clean")]
        with mock.patch.object(curses, "COLOR_PAIRS", budget, create=True), \
                mock.patch.object(curses, "color_pair", lambda p: p), \
                mock.patch.object(curses, "init_pair") as init_pair:
            for path in covers:                    # same session, one cover at a time
                win = _Win(20, 45)
                tui.draw_art(win, path)
                assert win.attrs, "nothing drawn"
                assert 0 not in win.attrs, f"uncolored cells for {os.path.basename(path)}"
                assert tui._art_next[0] <= budget, tui._art_next[0]
            assert init_pair.call_count > 0
            for call in init_pair.call_args_list:  # pair id in range, colors valid
                pair, fg, bg = call[0]
                assert tui.ART_PAIR0 <= pair < budget, pair
                assert 16 <= fg <= 255 and 16 <= bg <= 255, (fg, bg)


def test_dark_hues_keep_their_color():
    """Dark blue and dark green must not snap to the grey ramp. The ramp is the
    only fine gradation in xterm-256, so it wins on distance for any muted dark
    color and covers rendered grey/black. Neutrals must still take the ramp."""
    from PIL import Image
    with tempfile.TemporaryDirectory() as d:
        for name, col, grey in (("navy", (25, 25, 60), False),
                                ("green", (28, 52, 33), False),
                                ("grey", (64, 64, 64), True)):
            path = os.path.join(d, f"{name}.png")
            Image.new("RGB", (90, 72), col).save(path)
            tui._grid_cache.clear()
            fg = tui._art_grid(path, 45, 18)[0][0][0]
            assert (fg >= 232) == grey, (name, fg)


def test_art_grid_fits_pair_budget_on_a_tiny_table():
    """A terminal with barely any pairs must still get a colored cover: the
    palette steps down until the (fg,bg) combos fit."""
    with tempfile.TemporaryDirectory() as d:
        path = _cover(os.path.join(d, "busy.png"), "busy")
        with mock.patch.object(curses, "COLOR_PAIRS", 80, create=True):
            grid = tui._art_grid(path, 43, 18)
    pairs = {p for row in grid for p in row}
    assert len(pairs) <= 80 - tui.ART_PAIR0, len(pairs)
    assert len({fg for fg, _ in pairs}) > 1, "art collapsed to a single color"


if __name__ == "__main__":
    for name, fn in sorted(globals().items()):
        if name.startswith("test_"):
            fn()
            print("ok", name)
    print("all passed")
