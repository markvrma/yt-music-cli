"""Self-check: a YouTube track downloads and mpv actually decodes audio from it.

Runs its own mpv on a private IPC socket so a live msm session is left alone,
but it does play a few seconds of audio out loud.
"""
import os, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from msm import ymc

ymc.MPV_SOCK = "/tmp/ymc-check.sock"   # do not hijack a running msm
ymc.MPV_LOG = "/tmp/ymc-check.log"

t = {"title": "Marianne", "artist": "Fontaines D.C.", "album": "Marianne",
     "duration": 225, "url": "https://music.youtube.com/watch?v=ikKBcZg9jUc"}

p = ymc.cache_path(t)
assert p.endswith("ikKBcZg9jUc.m4a"), p
if os.path.exists(p):
    os.remove(p)

assert ymc.fetch(t) == p
assert os.path.exists(p) and os.path.getsize(p) > 500_000, "download failed/short"
print("downloaded", os.path.getsize(p), "bytes ->", p)

tags, dur = ymc._probe(p)
assert 200 < dur < 260, f"bad duration {dur}"       # duration now loads
print("ffprobe duration:", round(dur, 1), "s")

# local file passes through untouched
assert ymc.cache_path({"url": "/Users/x/Music/a.flac"}) == "/Users/x/Music/a.flac"

# cache hit: second fetch must not re-download
m = os.path.getmtime(p); time.sleep(1); ymc.fetch(t)
assert os.path.getmtime(p) == m, "re-downloaded a cached track"
print("cache hit OK")

pl = ymc.Player()
try:
    pl.play([t])
    for _ in range(30):
        time.sleep(1)
        pos, d, paused, title = pl.progress()
        if pos > 2 and d > 200:
            print(f"PLAYING pos={pos:.1f}s dur={d:.1f}s title={title!r}")
            break
    else:
        raise AssertionError("mpv never played: " + repr(pl.progress()))
finally:
    pl.quit()
print("ALL OK")
