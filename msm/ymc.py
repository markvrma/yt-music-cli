#!/usr/bin/env python3
"""ymc — YouTube Music core: browse, play (mpv over IPC), cmusfm scrobble.

mpv runs in the background driven over its JSON IPC socket, so a TUI can own
the terminal. cmusfm is fed the same way cmus feeds it as status_display_program.
"""
import json
import os
import socket
import subprocess
import threading
import time

from ytmusicapi import YTMusic

MPV_SOCK = "/tmp/ymc-mpv.sock"
HIST = os.path.expanduser("~/.config/ymc/history.json")
LOCAL_MUSIC = os.path.expanduser("~/Music")
ART_CACHE = os.path.expanduser("~/.config/ymc/art")
AUDIO_EXT = (".mp3", ".flac", ".m4a", ".opus", ".ogg", ".wav", ".aac", ".wma")


# ---- YouTube Music ---------------------------------------------------------

def artists_str(item):
    return ", ".join(a["name"] for a in (item.get("artists") or []) if a.get("name"))


def thumb_url(item):
    """Largest thumbnail URL (ytmusicapi lists them smallest-first)."""
    th = item.get("thumbnails") or []
    return th[-1]["url"] if th else ""


def _track(item, album_title, fallback_artist):
    return {
        "title": item["title"],
        "artist": artists_str(item) or fallback_artist,
        "album": album_title,
        "duration": item.get("duration_seconds") or 0,
        "url": "https://music.youtube.com/watch?v=" + item["videoId"],
    }


def search_albums(yt, query):
    return yt.search(query, filter="albums")


def search_all(yt, query):
    """Songs + albums + playlists, resultType kept. Filtered queries are used
    instead of one unfiltered search: the unfiltered call also returns artist
    rows that ytmusicapi can fail to parse, and artists aren't playable here.
    """
    out = []
    for filt in ("songs", "albums", "playlists"):
        try:
            out += yt.search(query, filter=filt)[:5]
        except Exception:  # one bad category shouldn't sink the whole search
            pass
    return out


def album_tracks(yt, browse_id):
    alb = yt.get_album(browse_id)
    fb = artists_str(alb)
    tracks = [_track(t, alb["title"], fb) for t in alb["tracks"] if t.get("videoId")]
    return alb["title"], tracks, thumb_url(alb)


def resolve_result(yt, r):
    """Search result -> (title, [tracks], thumb). album/playlist expand; song = 1."""
    rt = r.get("resultType")
    if rt == "album":
        return album_tracks(yt, r["browseId"])
    if rt == "playlist":
        pid = r.get("playlistId") or r["browseId"]
        if pid.startswith("VL"):
            pid = pid[2:]
        pl = yt.get_playlist(pid)
        tracks = [_track(t, pl["title"], artists_str(t)) for t in pl["tracks"] if t.get("videoId")]
        return pl["title"], tracks, thumb_url(pl)
    # song / video -> single track
    album = r.get("album", {}).get("name", "") if isinstance(r.get("album"), dict) else ""
    return r["title"], [_track(r, album, artists_str(r))], thumb_url(r)


def art_file(title, url):
    """Download album art once, cached under ~/.config/ymc/art/. Returns path or None."""
    if not url:
        return None
    import re
    import requests  # ytmusicapi dep; bundles certifi (urllib fails SSL on stock py)
    os.makedirs(ART_CACHE, exist_ok=True)
    slug = re.sub(r"[^a-zA-Z0-9]+", "_", title)[:60] or "art"
    path = os.path.join(ART_CACHE, slug + ".jpg")
    if os.path.exists(path):
        return path
    try:
        resp = requests.get(url, timeout=10)
        resp.raise_for_status()
        with open(path, "wb") as f:
            f.write(resp.content)
        return path
    except Exception:
        return None


# ---- local music (~/Music, one album per subfolder) ------------------------

def scan_local():
    """Immediate subdirs of ~/Music containing audio = albums. Lazy: no tags yet."""
    out = []
    try:
        names = sorted(os.listdir(LOCAL_MUSIC), key=str.lower)
    except OSError:
        return out
    for name in names:
        d = os.path.join(LOCAL_MUSIC, name)
        if not os.path.isdir(d):
            continue
        try:
            files = sorted(f for f in os.listdir(d) if f.lower().endswith(AUDIO_EXT))
        except OSError:
            continue
        if files:
            out.append({"title": name, "dir": d, "files": files, "local": True, "tracks": None})
    return out


def _probe(path):
    """(tags dict lowercased, duration seconds) via ffprobe."""
    try:
        r = subprocess.run(
            ["ffprobe", "-v", "quiet", "-print_format", "json", "-show_format", path],
            capture_output=True, text=True, timeout=15)
        fmt = json.loads(r.stdout).get("format", {})
        tags = {k.lower(): v for k, v in (fmt.get("tags") or {}).items()}
        return tags, float(fmt.get("duration") or 0)
    except Exception:
        return {}, 0.0


def load_local_album(album):
    """Fill tracks (ffprobe tags) + art. Cached on the album dict. -> (title, tracks, art)."""
    if album.get("tracks") is not None:
        return album["title"], album["tracks"], album.get("thumb")
    tracks = []
    for fn in album["files"]:
        p = os.path.join(album["dir"], fn)
        tags, dur = _probe(p)
        tracks.append({
            "title": tags.get("title") or os.path.splitext(fn)[0],
            "artist": tags.get("artist") or tags.get("album_artist") or "",
            "album": tags.get("album") or album["title"],
            "duration": int(dur),
            "url": p,  # mpv + cmusfm take the local path
        })
    album["tracks"] = tracks
    album["thumb"] = local_art(album["dir"], os.path.join(album["dir"], album["files"][0]))
    return album["title"], tracks, album["thumb"]


def local_art(d, first_file):
    """Folder image (cover/folder/*.jpg|png) else embedded art extracted once. Path or None."""
    import glob
    import re
    esc = glob.escape(d)  # album folders contain []()  -> escape glob metachars
    for name in ("cover.jpg", "folder.jpg", "cover.png", "folder.png"):
        p = os.path.join(d, name)
        if os.path.exists(p):
            return p
    imgs = sorted(glob.glob(os.path.join(esc, "*.jpg")) + glob.glob(os.path.join(esc, "*.png")))
    if imgs:
        return imgs[0]
    os.makedirs(ART_CACHE, exist_ok=True)
    out = os.path.join(ART_CACHE, "local_" + re.sub(r"[^a-zA-Z0-9]+", "_", os.path.basename(d))[:50] + ".jpg")
    if os.path.exists(out):
        return out
    try:
        subprocess.run(["ffmpeg", "-y", "-v", "quiet", "-i", first_file,
                        "-an", "-c:v", "copy", "-frames:v", "1", out],
                       timeout=15, check=True)
        if os.path.exists(out) and os.path.getsize(out) > 0:
            return out
    except Exception:
        pass
    return None


# ---- history ---------------------------------------------------------------

def load_history():
    try:
        with open(HIST) as f:
            return json.load(f)
    except (OSError, json.JSONDecodeError):
        return []


def record(title, tracks, thumb=""):
    """Prepend album, drop older same-title, keep last 5."""
    h = [a for a in load_history() if a["title"] != title]
    h.insert(0, {"title": title, "tracks": tracks, "thumb": thumb})
    h = h[:5]
    os.makedirs(os.path.dirname(HIST), exist_ok=True)
    with open(HIST, "w") as f:
        json.dump(h, f)
    return h


# ---- cmusfm scrobble bridge ------------------------------------------------

def cmusfm(status, track=None):
    """Fire cmusfm the way cmus does as status_display_program."""
    args = ["cmusfm", "status", status]
    if track:
        args += [
            "file", track["url"],
            "artist", track["artist"],
            "album", track["album"],
            "title", track["title"],
            "duration", str(track["duration"]),
        ]
    subprocess.run(args, check=False)


# ---- mpv IPC ---------------------------------------------------------------

class IPC:
    """One mpv IPC connection. Matches request_id and skips async events —
    mpv interleaves event messages on the socket, so first-reply-wins is wrong.
    """

    def __init__(self, proc):
        self.buf = b""
        self.rid = 0
        for _ in range(50):
            if proc.poll() is not None:
                raise RuntimeError("mpv exited")
            try:
                self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                self.sock.connect(MPV_SOCK)
                self.sock.settimeout(2)
                return
            except OSError:
                time.sleep(0.1)
        raise RuntimeError("mpv IPC socket never appeared")

    def cmd(self, command):
        self.rid += 1
        rid = self.rid
        try:
            self.sock.sendall((json.dumps({"command": command, "request_id": rid}) + "\n").encode())
            while True:
                while b"\n" not in self.buf:
                    chunk = self.sock.recv(4096)
                    if not chunk:
                        return None
                    self.buf += chunk
                line, self.buf = self.buf.split(b"\n", 1)
                if not line:
                    continue
                msg = json.loads(line)
                if msg.get("request_id") == rid:  # ignore events + stale replies
                    return msg.get("data") if msg.get("error") == "success" else None
        except (OSError, json.JSONDecodeError):
            return None


class Player:
    """One background mpv for the whole session, driven over IPC."""

    def __init__(self):
        self.by_url = {}
        self.current = None
        if os.path.exists(MPV_SOCK):
            os.remove(MPV_SOCK)
        self.proc = subprocess.Popen(
            ["mpv", "--idle=yes", "--no-video", "--no-terminal",
             "--input-ipc-server=" + MPV_SOCK],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.ipc = IPC(self.proc)
        threading.Thread(target=self._watch, daemon=True).start()

    def play(self, tracks, start=0):
        self.by_url = {t["url"]: t for t in tracks}
        self.ipc.cmd(["loadfile", tracks[0]["url"], "replace"])
        for t in tracks[1:]:
            self.ipc.cmd(["loadfile", t["url"], "append"])
        if start:
            self.ipc.cmd(["set_property", "playlist-pos", start])
        self.ipc.cmd(["set_property", "pause", False])

    def toggle_pause(self):
        self.ipc.cmd(["cycle", "pause"])

    def next(self):
        self.ipc.cmd(["playlist-next"])

    def prev(self):
        self.ipc.cmd(["playlist-prev"])

    def progress(self):
        """(pos_s, dur_s, paused, title) — title from current track."""
        pos = self.ipc.cmd(["get_property", "time-pos"]) or 0
        dur = self.ipc.cmd(["get_property", "duration"]) or 0
        paused = bool(self.ipc.cmd(["get_property", "pause"]))
        title = self.current["title"] if self.current else ""
        return pos, dur, paused, title

    def quit(self):
        self.ipc.cmd(["quit"])
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()

    def _watch(self):
        """Own IPC connection; fire cmusfm on track/pause change."""
        try:
            ipc = IPC(self.proc)
        except RuntimeError:
            return
        last_path = last_pause = None
        while self.proc.poll() is None:
            path = ipc.cmd(["get_property", "path"])
            pause = ipc.cmd(["get_property", "pause"])
            if path and path != last_path:
                self.current = self.by_url.get(path)
                if self.current:
                    cmusfm("playing", self.current)
                last_path, last_pause = path, False
            elif self.current and pause is not None and pause != last_pause:
                cmusfm("paused" if pause else "playing", self.current)
                last_pause = pause
            time.sleep(1)
        cmusfm("stopped", self.current)


# ---- minimal search CLI (TUI is the main app; this is a fallback) ----------

def main():
    import shutil
    import sys
    for tool in ("mpv", "cmusfm"):
        if not shutil.which(tool):
            sys.exit("missing required tool: " + tool)
    yt = YTMusic()
    player = Player()
    print("ymc search — enter query, pick album, then n/p/space/q. Ctrl-C quits.")
    try:
        while True:
            q = input("\nsearch album> ").strip()
            if not q:
                continue
            albums = search_albums(yt, q)[:10]
            for i, a in enumerate(albums, 1):
                print("  %2d. %s — %s" % (i, a["title"], artists_str(a)))
            sel = input("pick (n/enter=skip)> ").strip()
            if not sel.isdigit() or not 1 <= int(sel) <= len(albums):
                continue
            title, tracks, thumb = album_tracks(yt, albums[int(sel) - 1]["browseId"])
            record(title, tracks, thumb)
            player.play(tracks)
            print("playing %s — [n]ext [p]rev [space]pause [s]earch [q]uit" % title)
            while True:
                c = input("> ").strip().lower()
                if c == "q":
                    return
                if c == "s":
                    break
                if c == "n":
                    player.next()
                elif c == "p":
                    player.prev()
                elif c in ("", " ", "space"):
                    player.toggle_pause()
    except (EOFError, KeyboardInterrupt):
        pass
    finally:
        player.quit()


if __name__ == "__main__":
    main()
