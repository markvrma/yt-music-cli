#!/usr/bin/env python3
"""Self-check: cmusfm argv, album/result parse, history. No network, no mpv."""
import os
import subprocess
import tempfile
from unittest import mock

from msm import ymc

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


if __name__ == "__main__":
    for name, fn in sorted(globals().items()):
        if name.startswith("test_"):
            fn()
            print("ok", name)
    print("all passed")
