# msm — YouTube Music + local library terminal player

Browse YouTube Music and your local `~/Music` library in the terminal, play
via `mpv`, scrobble each track through `cmusfm` (same protocol cmus uses as
its status_display_program). mpv runs in the background driven over its JSON
IPC socket, so the TUI owns the terminal.

## System requirements (not pip-installable — install separately)

- `mpv`, `yt-dlp`, `cmusfm`, `ffmpeg`/`ffprobe` on PATH (ffprobe reads local tags; ffmpeg extracts embedded art)
- cmusfm configured for last.fm (its daemon must be running — it is whenever
  cmus has run with `status_display_program=cmusfm`)

On macOS: `brew install mpv yt-dlp ffmpeg cmus cmusfm`

## Install

```sh
pip install msm-player            # from PyPI (once published)
pip install git+https://github.com/markverma/msm-player   # straight from git
pip install .                     # from a local clone
pipx install .                    # isolated + on PATH globally (recommended for CLI use)
```

Then run the TUI from anywhere:

```sh
msm
```

(`python -m msm` also works.)

## Develop

```sh
python3 -m venv .venv
.venv/bin/pip install -e .        # editable install; `msm` entry point in .venv/bin
.venv/bin/python test_ymc.py      # self-checks
```

## Build a distributable

```sh
pip install build twine
python -m build                   # -> dist/*.whl and dist/*.tar.gz
twine upload dist/*               # publish to PyPI
```

Two screens:

Bordered panes, purple/grey theme.

**browse** — five panes:
- **Now Playing** (left, tall): tracklist of the opened album — `enter` plays a track, `f` plays the album from start
- **Local ~/Music** (middle, tall): your local albums (one per subfolder) — `enter` opens its tracklist into Now Playing, `f` opens + plays
- **Last 5** (top-right): recently played albums — `enter` opens into Now Playing, `f` opens + plays
- **cover** (bottom-right): pixelated album art of the Now Playing album (square, 256-color half-block; local art from folder `cover.jpg`/`folder.jpg` or embedded tag)
- progress bar (bottom, full width)

Opening an album (from Local or Last 5) loads its tracklist into **Now Playing** and moves focus there.

**search** — press `/`:
- type a query, `enter` runs it; results tagged `[album]`/`[song]`/`[playlist]` with artist
- `j`/`k` pick; `enter` loads into Now Playing (no play); `f` loads + plays; `Esc` back

Keys: `h`/`l` switch pane (Now Playing ↔ Local ↔ Last 5) · `j`/`k` move · `enter` open/play ·
`f` play from start · `space` pause · `n`/`p` next/prev · `/` search · `q` quit.

Local albums use `ffprobe` for tags (title/artist/album/duration) and play straight from disk.
Play history persists to `~/.config/ymc/history.json` (last 5 albums).

## Plain search CLI (fallback)

```sh
python -m msm.ymc
```
