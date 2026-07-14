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
