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
CONFIG = os.path.expanduser("~/.config/ymc")
HIST = os.path.join(CONFIG, "history.json")
AUTH = os.path.join(CONFIG, "browser.json")         # ytmusicapi browser-auth headers
LOCAL_MUSIC = os.path.expanduser("~/Music")
ART_CACHE = os.path.join(CONFIG, "art")
AUDIO_EXT = (".mp3", ".flac", ".m4a", ".opus", ".ogg", ".wav", ".aac", ".wma")

AUTHED = False  # set by get_yt(); read by the TUI to pick pane behavior


# ---- auth ------------------------------------------------------------------

def get_yt():
    """Authed YTMusic if browser.json exists, else unauthenticated. Sets
    module-level AUTHED. Any load failure falls back to unauth (never raises).

    Browser auth (not OAuth): YouTube's youtubei API rejects generic Google
    Cloud OAuth tokens with HTTP 400, so history/recs need real website
    session headers, which is what ytmusicapi's browser auth provides."""
    global AUTHED
    AUTHED = False
    if os.path.exists(AUTH):
        try:
            yt = YTMusic(AUTH)
            AUTHED = True
            return yt
        except Exception:
            pass  # corrupt/expired headers -> unauth, app still browses+plays
    return YTMusic()


def _headers_from_input(raw):
    """Normalize pasted request info into ytmusicapi's 'key: value\\n' format.
    Accepts three shapes DevTools produces: a curl command ('Copy as cURL'),
    clean 'key: value' lines, or Chrome's alternating name/value lines."""
    if "curl " in raw and " -H " in raw:
        # Copy as cURL: pull every -H 'k: v' / -H "k: v", plus -b/--cookie.
        pairs = _re.findall(r"-H\s+'([^']+)'", raw) + _re.findall(r'-H\s+"([^"]+)"', raw)
        cookie = _re.findall(r"(?:-b|--cookie)\s+'([^']+)'", raw) + \
                 _re.findall(r'(?:-b|--cookie)\s+"([^"]+)"', raw)
        if cookie and not any(p.lower().startswith("cookie:") for p in pairs):
            pairs.append("cookie: " + cookie[0])
        return "\n".join(pairs)
    lines = [l for l in raw.split("\n") if l.strip()]
    if lines and not any(": " in l for l in lines[:4]):
        # alternating name / value lines -> pair them up
        lines = ["%s: %s" % (k, v) for k, v in zip(lines[0::2], lines[1::2])]
    return "\n".join(lines)


def setup_auth(source=None):
    """Save YouTube Music browser-auth headers so plays record to history and
    recommendations load (chmod 600 — they contain your cookies).

    Reads the request from (in order): a file path arg, else the macOS
    clipboard (pbpaste), else stdin. Clipboard avoids the fragile terminal
    paste of a huge multi-line cURL."""
    import subprocess
    import sys
    from ytmusicapi import setup
    os.makedirs(CONFIG, exist_ok=True)

    if source:
        with open(os.path.expanduser(source)) as f:
            raw = f.read()
    else:
        raw = ""
        try:  # macOS: read the copied cURL straight from the clipboard
            raw = subprocess.run(["pbpaste"], capture_output=True, text=True, timeout=5).stdout
        except Exception:
            raw = ""
        if not raw.strip():
            print("Paste request headers / cURL, then press Ctrl-D:")
            raw = sys.stdin.read()

    headers_raw = _headers_from_input(raw)
    if "cookie:" not in headers_raw.lower():
        raise SystemExit(
            "No 'cookie' header found in the input.\n"
            "In DevTools -> Network, right-click a /browse POST -> Copy -> "
            "Copy as cURL, then run `msm auth` again (it reads your clipboard).")
    setup(filepath=AUTH, headers_raw=headers_raw)
    os.chmod(AUTH, 0o600)
    if is_logged_in():
        print("auth saved to %s and VERIFIED logged in. run `msm` to play." % AUTH)
    else:
        print("\nWARNING: headers saved but YouTube treats them as LOGGED OUT.\n"
              "Recopy the request headers from a fresh, logged-in (non-incognito)\n"
              "music.youtube.com tab and run `msm auth` again promptly (session\n"
              "tokens rotate). History recording won't work until this verifies.")


def is_logged_in():
    """True if the saved browser.json is an authenticated YouTube session.
    Checks the LOGGED_IN flag on the music.youtube.com home page (the youtubei
    API silently downgrades unauthenticated requests, so this is the reliable
    signal)."""
    try:
        import requests
        ck = json.load(open(AUTH))
        r = requests.get("https://music.youtube.com/",
                         headers={"cookie": ck["cookie"], "user-agent": ck.get("user-agent", "")},
                         timeout=10)
        return '"LOGGED_IN":true' in r.text
    except Exception:
        return False


# ---- YouTube Music ---------------------------------------------------------

import re as _re
_PLAYCOUNT = _re.compile(r"^[\d.,]+\s*[KMB]?\s*(plays|views)$", _re.I)
# content-type words get_home() prepends into the artists list as a stray token
_TYPEWORD = {"song", "video", "album", "single", "ep", "playlist",
             "artist", "episode", "podcast"}


def _real_artist(name):
    return bool(name) and name.lower() not in _TYPEWORD and not _PLAYCOUNT.match(name)


def artists_str(item):
    # get_home() song items pad the artists list with a type word ("Song") and a
    # "<N> plays" token; keep only genuine artist names.
    return ", ".join(a["name"] for a in (item.get("artists") or []) if _real_artist(a["name"]))


def thumb_url(item):
    """Largest thumbnail URL (ytmusicapi lists them smallest-first)."""
    th = item.get("thumbnails") or []
    return th[-1]["url"] if th else ""


def _video_id(url):
    """videoId from a YT watch url; None for local file paths (no ?v=)."""
    from urllib.parse import parse_qs, urlparse
    return (parse_qs(urlparse(url).query).get("v") or [None])[0]


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


def like_track(yt, track):
    """Thumbs-up a track on YouTube Music. Returns True on success, False if
    not a YT track / unauthed / request fails."""
    if not (AUTHED and yt):
        return False
    vid = _video_id(track.get("url", "")) if track else None
    if not vid:
        return False
    try:
        yt.rate_song(vid, "LIKE")
        return True
    except Exception:
        return False


def record_history(yt, video_id, watched=30):
    """Register a play in YouTube Music history the way the web player does:
    a playback ping then a watchtime ping sharing one cpn. Watchtime is what
    makes the play stick. Requires an authed (browser) YTMusic + history not
    paused on the account. Raises on failure (caller swallows)."""
    import random
    _CPNA = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_"
    pt = yt.get_song(video_id)["playbackTracking"]
    cpn = "".join(random.choice(_CPNA) for _ in range(16))
    # length from the playback url so watchtime end never exceeds track length
    from urllib.parse import parse_qs, urlparse
    play_url = pt["videostatsPlaybackUrl"]["baseUrl"]
    length = int((parse_qs(urlparse(play_url).query).get("len") or [watched])[0])
    et = min(watched, length) if length else watched
    yt._send_get_request(play_url, {"ver": 2, "c": "WEB_REMIX", "cpn": cpn})
    yt._send_get_request(pt["videostatsWatchtimeUrl"]["baseUrl"],
                         {"ver": 2, "c": "WEB_REMIX", "cpn": cpn,
                          "st": "0", "et": str(et), "cmt": str(et)})


def get_recs(yt, limit=5):
    """Personalized YouTube Music home rows, flattened to `limit` playable
    items. Each is a lazy album dict (tracks=None) resolved on open via the
    stored raw item. Best-effort: any failure -> []."""
    try:
        rows = yt.get_home(limit=6)
    except Exception:
        return []
    out = []
    for row in rows:
        for item in row.get("contents") or []:
            if not (item.get("videoId") or item.get("browseId") or item.get("playlistId")):
                continue  # header/shelf/artist -> not playable here
            if not item.get("title"):
                continue
            out.append({"title": item["title"], "thumb": thumb_url(item),
                        "tracks": None, "rec": item})
            if len(out) >= limit:
                return out
    return out


def album_tracks(yt, browse_id):
    alb = yt.get_album(browse_id)
    fb = artists_str(alb)
    tracks = [_track(t, alb["title"], fb) for t in alb["tracks"] if t.get("videoId")]
    return alb["title"], tracks, thumb_url(alb)


def resolve_result(yt, r):
    """Search result OR get_home() item -> (title, [tracks], thumb).
    album/playlist expand; song = 1. Home items carry no resultType, so infer
    it from which id field is present (videoId before playlistId: song 'radio'
    items carry both)."""
    rt = r.get("resultType")
    if rt is None:
        if r.get("videoId"):
            rt = "song"
        elif str(r.get("browseId") or "").startswith("MPRE"):
            rt = "album"
        elif r.get("playlistId"):
            rt = "playlist"
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

    def __init__(self, yt=None):
        self.yt = yt          # authed YTMusic -> record plays to YT history
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
        """Own IPC connection; fire cmusfm on track/pause change and record to
        YouTube Music history once a track has played >=30s (like a scrobble)."""
        try:
            ipc = IPC(self.proc)
        except RuntimeError:
            return
        last_path = last_pause = None
        recorded = False  # YT history logged for the current track yet?
        while self.proc.poll() is None:
            path = ipc.cmd(["get_property", "path"])
            pause = ipc.cmd(["get_property", "pause"])
            if path and path != last_path:
                self.current = self.by_url.get(path)
                if self.current:
                    cmusfm("playing", self.current)
                last_path, last_pause, recorded = path, False, False
            elif self.current and pause is not None and pause != last_pause:
                cmusfm("paused" if pause else "playing", self.current)
                last_pause = pause
            if self.current and not recorded:
                pos = ipc.cmd(["get_property", "time-pos"]) or 0
                if pos >= 30:
                    self._record_yt(self.current)
                    recorded = True
            time.sleep(1)
        cmusfm("stopped", self.current)

    def _record_yt(self, track, watched=30):
        """Log a play to YouTube Music history (best-effort, off-thread so a
        slow/hung request can't stall the watch loop). Local files are skipped.

        Sends both the playback-start and watchtime pings with one shared cpn —
        the watchtime ping is what actually registers the play (add_history_item
        alone only fires playback, which YT often ignores)."""
        if not (AUTHED and self.yt):
            return
        vid = _video_id(track["url"])
        if not vid:
            return

        def go():
            try:
                record_history(self.yt, vid, watched)
            except Exception:
                pass  # region-locked / offline / token / history-paused -> skip

        threading.Thread(target=go, daemon=True).start()


# ---- minimal search CLI (TUI is the main app; this is a fallback) ----------

def main():
    import shutil
    import sys
    for tool in ("mpv", "cmusfm"):
        if not shutil.which(tool):
            sys.exit("missing required tool: " + tool)
    yt = get_yt()
    player = Player(yt)
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
