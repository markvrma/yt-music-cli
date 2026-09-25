//! Album art as 256-color half-blocks. Port of tui.py's art section; image
//! decode via ffmpeg (native JPEG planes or rgb24) instead of Pillow, then BOX resize,
//! Color enhance, point and FASTOCTREE quantize re-done Pillow-exact.

use std::collections::HashMap;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{LazyLock, Mutex};

/// (fg, bg) xterm-256 index per cell; rows x cols. Each cell is a "▀":
/// fg = top pixel, bg = bottom pixel. Cached per (path, cols, rows).
pub type Grid = Vec<Vec<(u8, u8)>>;

// ponytail: no ART_PAIR0 / pair budget / step-down loop. ncurses had a finite
// color-pair table and overrunning it rendered uncolored half-blocks, so the
// palette stepped 32 -> 16 -> 8 until the (fg,bg) combos fit. crossterm writes
// AnsiValue fg/bg per cell, there is no table, so 32 colors once.
const ART_COLORS: u32 = 32; // adaptive palette size
const GREY_GATE: i32 = 18; // max channel spread that may still snap to the grey ramp
const ART_SAT: f32 = 1.35; // pre-quantize saturation boost, see art_grid
const ART_GAMMA: f64 = 0.85; // <1: lifts shadows toward the light side of the gradient

static LIFT: LazyLock<[u8; 256]> = LazyLock::new(|| {
    let mut t = [0u8; 256];
    for (v, o) in t.iter_mut().enumerate() {
        *o = (255.0 * (v as f64 / 255.0).powf(ART_GAMMA)).round() as u8;
    }
    t
});

type Key = (PathBuf, usize, usize);
static GRID_CACHE: LazyLock<Mutex<HashMap<Key, Grid>>> = LazyLock::new(Default::default);

// xterm-256: the 6x6x6 cube levels are NOT evenly spaced, plus a 24-step grey ramp
const CUBE: [i32; 6] = [0, 95, 135, 175, 215, 255];

/// Nearest xterm-256 index (cube + grey ramp gated by GREY_GATE).
///
/// The old `//51` assumed evenly spaced cube levels and never reached the grey
/// ramp, so blacks came out as colored confetti.
///
/// Only near-neutral pixels may take the ramp, or it desaturates saturated
/// covers to mush. The gate is an absolute spread, not a saturation ratio: a
/// ratio scales with brightness, so highlights keep hue while shadows of the
/// same hue lose it.
///
/// The gate is tight (18, was 30) because the ramp is the only fine gradation
/// in the palette, so it wins on Euclidean distance for any muted dark color:
/// dark blues and greens came out grey/black. A hued cube color one step up the
/// 0->95 cliff reads truer than a grey of the right brightness.
pub fn xterm256(r: u8, g: u8, b: u8) -> u8 {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    let d = |c: (i32, i32, i32)| (c.0 - r).pow(2) + (c.1 - g).pow(2) + (c.2 - b).pow(2);
    let cube = (0..216).map(|j| (CUBE[j / 36], CUBE[j / 6 % 6], CUBE[j % 6]));
    let grey = (0..24).map(|i| (8 + 10 * i, 8 + 10 * i, 8 + 10 * i));
    let with_grey = r.max(g).max(b) - r.min(g).min(b) <= GREY_GATE;
    let pool = cube.chain(grey.take(if with_grey { 24 } else { 0 }));
    // min_by_key keeps the first of equal minimums, same as Python's min()
    let (i, _) = pool.enumerate().min_by_key(|&(_, c)| d(c)).unwrap();
    16 + i as u8 // cube 0..215 -> 16..231, grey ramp 216..239 -> 232..255
}

/// Resize art to cols x 2*rows px, reduce to an adaptive palette, map to
/// xterm-256; (fg,bg) per cell. Each cell is a half-block: 1px wide, 2px tall ->
/// square when cols == 2*rows.
///
/// BOX (area average), not NEAREST: at this size NEAREST samples one source
/// pixel per cell, so detailed covers come out as noise.
///
/// Saturation boost + shadow lift before quantizing: the 6x6x6 cube has nothing
/// between 0 and 95 per channel, so a muted dark hue is numerically closest to
/// grey and covers came out grey/black. Pushing hue and brightness up first
/// keeps them on the colored side. Measured over the local covers: mean grey-ramp
/// share 38% -> 25%, saturation drift -3.8 -> +7.6, luminance +8.
///
/// FASTOCTREE, not MEDIANCUT: median cut weights the palette by pixel count, so
/// on a mostly-black cover a small vivid region (Daft Punk's gold helmet) gets
/// merged into the greys. Octree keeps distinct colors distinct.
pub fn art_grid(path: &Path, cols: usize, rows: usize) -> Result<Grid, String> {
    let key = (path.to_path_buf(), cols, rows);
    if let Some(g) = GRID_CACHE.lock().unwrap().get(&key) {
        return Ok(g.clone());
    }
    if cols == 0 || rows == 0 {
        return Err("empty art box".into());
    }
    let (w, h, px) = decode(path)?;
    let mut img = box_resize(&px, w, h, cols, rows * 2);
    enhance_color(&mut img);
    for v in img.iter_mut() {
        *v = LIFT[*v as usize];
    }
    let (pal, idx) = quantize_octree(&img, ART_COLORS);
    let xt: Vec<u8> = pal.iter().map(|c| xterm256(c[0], c[1], c[2])).collect();
    let grid: Grid = (0..rows)
        .map(|cy| {
            (0..cols)
                .map(|cx| {
                    (
                        xt[idx[cy * 2 * cols + cx]],
                        xt[idx[(cy * 2 + 1) * cols + cx]],
                    )
                })
                .collect()
        })
        .collect();
    GRID_CACHE.lock().unwrap().insert(key, grid.clone());
    Ok(grid)
}

/// Full-size RGB, (w, h, rgb24). Baseline JPEG (nearly every cover) comes out
/// of ffmpeg as its native Y/Cb/Cr planes and is turned into RGB here the way
/// libjpeg-turbo (Pillow's decoder) does it; swscale's conversion drifted
/// enough to move cells across palette buckets. What's left vs Pillow is the
/// IDCT (±1 on a few % of samples). Anything else goes through ffmpeg rgb24,
/// which drops alpha without compositing, same as Pillow's convert("RGB").
fn decode(path: &Path) -> Result<(usize, usize, Vec<u8>), String> {
    let bad = || format!("could not decode {}", path.display());
    let probe = Command::new("ffprobe")
        .args(["-v", "quiet", "-select_streams", "v:0", "-show_entries"])
        .args([
            "stream=codec_name,width,height,pix_fmt",
            "-of",
            "default=nw=1",
        ])
        .arg(path)
        .output()
        .map_err(|e| format!("ffprobe: {e}"))?;
    let info = String::from_utf8_lossy(&probe.stdout);
    let field = |k: &str| {
        info.lines()
            .find_map(|l| l.strip_prefix(k)?.strip_prefix('='))
            .unwrap_or("")
    };
    let (w, h): (usize, usize) = match (field("width").parse(), field("height").parse()) {
        (Ok(w), Ok(h)) if w > 0 && h > 0 => (w, h),
        _ => return Err(bad()),
    };
    let native = field("codec_name") == "mjpeg"
        && ["yuvj444p", "yuvj420p", "gray"].contains(&field("pix_fmt"));
    let fmt = if native { field("pix_fmt") } else { "rgb24" };
    let (cw, ch) = if fmt == "yuvj420p" {
        (w.div_ceil(2), h.div_ceil(2))
    } else {
        (w, h)
    };
    let want = match fmt {
        "gray" => w * h,
        "rgb24" => w * h * 3,
        _ => w * h + 2 * cw * ch,
    };
    // -noautorotate: Pillow's open() ignores EXIF orientation, so must we
    let out = Command::new("ffmpeg")
        .args(["-v", "quiet", "-noautorotate", "-i"])
        .arg(path)
        .args([
            "-frames:v",
            "1",
            "-sws_flags",
            "accurate_rnd+full_chroma_int+bitexact",
        ])
        .args(["-f", "rawvideo", "-pix_fmt", fmt, "-"])
        .output()
        .map_err(|e| format!("ffmpeg: {e}"))?;
    if !out.status.success() || out.stdout.len() != want {
        return Err(bad());
    }
    let raw = out.stdout;
    let rgb = match fmt {
        "rgb24" => raw,
        "gray" => raw.iter().flat_map(|&v| [v; 3]).collect(),
        _ => {
            let (y, c) = raw.split_at(w * h);
            let (cb, cr) = c.split_at(cw * ch);
            if fmt == "yuvj420p" {
                ycc_to_rgb(
                    y,
                    &h2v2_fancy(cb, cw, ch, w, h),
                    &h2v2_fancy(cr, cw, ch, w, h),
                )
            } else {
                ycc_to_rgb(y, cb, cr)
            }
        }
    };
    Ok((w, h, rgb))
}

/// libjpeg-turbo jdcolor.c ycc_rgb_convert (SCALEBITS 16 tables).
fn ycc_to_rgb(y: &[u8], cb: &[u8], cr: &[u8]) -> Vec<u8> {
    let fix = |x: f64| (x * 65536.0 + 0.5) as i32;
    let half = 1 << 15;
    let clamp = |v: i32| v.clamp(0, 255) as u8;
    let mut out = Vec::with_capacity(y.len() * 3);
    for i in 0..y.len() {
        let (yy, b, r) = (y[i] as i32, cb[i] as i32 - 128, cr[i] as i32 - 128);
        out.push(clamp(yy + ((fix(1.40200) * r + half) >> 16)));
        out.push(clamp(
            yy + ((-fix(0.34414) * b + half - fix(0.71414) * r) >> 16),
        ));
        out.push(clamp(yy + ((fix(1.77200) * b + half) >> 16)));
    }
    out
}

/// libjpeg-turbo jdsample.c h2v2_fancy_upsample: triangle filter, 3/4 nearer
/// + 1/4 further sample each way, edge rows replicated; cropped to w x h.
fn h2v2_fancy(p: &[u8], cw: usize, ch: usize, w: usize, h: usize) -> Vec<u8> {
    let at = |x: usize, y: usize| p[y * cw + x] as i32;
    let mut out = vec![0u8; w * h];
    for oy in 0..h {
        let y0 = oy / 2;
        // upper output row leans on the row above, lower on the row below
        let y1 = if oy % 2 == 0 {
            y0.saturating_sub(1)
        } else {
            (y0 + 1).min(ch - 1)
        };
        let col = |x: usize| at(x, y0) * 3 + at(x, y1);
        for ox in 0..w {
            let x = ox / 2;
            let this = col(x);
            let v = match (ox % 2, x) {
                (0, 0) => (this * 4 + 8) >> 4,
                (0, _) => (this * 3 + col(x - 1) + 8) >> 4,
                _ if x + 1 == cw => (this * 4 + 7) >> 4,
                _ => (this * 3 + col(x + 1) + 7) >> 4,
            };
            out[oy * w + ox] = v as u8;
        }
    }
    out
}

// ---- Pillow Resample.c, BOX filter (support 0.5), 8bpc ---------------------

const PRECISION_BITS: u32 = 32 - 8 - 2;

/// (xmin, fixed-point weights) per output sample; precompute_coeffs +
/// normalize_coeffs_8bpc.
fn coeffs(in_size: usize, out_size: usize) -> Vec<(usize, Vec<i32>)> {
    let scale = in_size as f64 / out_size as f64;
    let filterscale = scale.max(1.0);
    let support = 0.5 * filterscale;
    (0..out_size)
        .map(|xx| {
            let center = (xx as f64 + 0.5) * scale;
            let ss = 1.0 / filterscale;
            let xmin = ((center - support + 0.5) as i64).max(0) as usize;
            let xmax = ((center + support + 0.5) as i64).min(in_size as i64) as usize - xmin;
            let k: Vec<f64> = (0..xmax)
                .map(|x| {
                    let t = (x as f64 + xmin as f64 - center + 0.5) * ss;
                    if t > -0.5 && t <= 0.5 {
                        1.0
                    } else {
                        0.0
                    }
                })
                .collect();
            let ww: f64 = k.iter().sum();
            let k = k
                .iter()
                .map(|&w| {
                    let w = if ww != 0.0 { w / ww } else { w };
                    (0.5 + w * (1u32 << PRECISION_BITS) as f64) as i32 // weights are >= 0
                })
                .collect();
            (xmin, k)
        })
        .collect()
}

fn clip8(ss: i32) -> u8 {
    (ss >> PRECISION_BITS).clamp(0, 255) as u8
}

/// Pillow's two-pass resize: horizontal into u8, then vertical. A pass whose
/// size doesn't change is skipped (it would be the identity anyway).
fn box_resize(px: &[u8], w: usize, h: usize, ow: usize, oh: usize) -> Vec<u8> {
    let mut cur = px.to_vec();
    let mut cw = w;
    let kv = coeffs(h, oh);
    let (y0, y1) = (kv[0].0, kv[oh - 1].0 + kv[oh - 1].1.len());
    if ow != w {
        let kh = coeffs(w, ow);
        let mut tmp = vec![0u8; ow * (y1 - y0) * 3];
        for yy in 0..y1 - y0 {
            let row = &px[(yy + y0) * w * 3..];
            for (xx, (xmin, k)) in kh.iter().enumerate() {
                for c in 0..3 {
                    let mut ss = 1i32 << (PRECISION_BITS - 1);
                    for (x, &kx) in k.iter().enumerate() {
                        ss = ss.wrapping_add(row[(x + xmin) * 3 + c] as i32 * kx);
                    }
                    tmp[(yy * ow + xx) * 3 + c] = clip8(ss);
                }
            }
        }
        cur = tmp;
        cw = ow;
    }
    if oh != h {
        // the horizontal pass only kept rows y0..y1
        let shift = if ow != w { y0 } else { 0 };
        let mut out = vec![0u8; cw * oh * 3];
        for (yy, (ymin, k)) in kv.iter().enumerate() {
            for xx in 0..cw {
                for c in 0..3 {
                    let mut ss = 1i32 << (PRECISION_BITS - 1);
                    for (y, &ky) in k.iter().enumerate() {
                        ss = ss
                            .wrapping_add(cur[((y + ymin - shift) * cw + xx) * 3 + c] as i32 * ky);
                    }
                    out[(yy * cw + xx) * 3 + c] = clip8(ss);
                }
            }
        }
        cur = out;
    }
    cur
}

// ---- ImageEnhance.Color(img).enhance(ART_SAT) ------------------------------

/// Image.blend(img.convert("L").convert("RGB"), img, 1.35): the extrapolating
/// branch of Blend.c, float math, truncate, clip.
fn enhance_color(img: &mut [u8]) {
    for p in img.chunks_exact_mut(3) {
        // Convert.c rgb2l: L24 fixed point
        let l = ((p[0] as u32 * 19595 + p[1] as u32 * 38470 + p[2] as u32 * 7471 + 0x8000) >> 16)
            as i32;
        for v in p.iter_mut() {
            // clang contracts `in1 + alpha * (in2 - in1)` into one fused
            // multiply-add on arm64; mul_add keeps the same rounding.
            let t = ART_SAT.mul_add((*v as i32 - l) as f32, l as f32);
            *v = if t <= 0.0 {
                0
            } else if t >= 255.0 {
                255
            } else {
                t as u8
            };
        }
    }
}

// ---- Pillow QuantOctree.c (RGB, no alpha) ----------------------------------

/// Same layout as Pillow's struct _ColorBucket, so the platform qsort below
/// sees the same element size.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Bucket {
    count: u32,
    r: u64,
    g: u64,
    b: u64,
    a: u64,
}

struct Cube {
    bits: [u32; 3],
    buckets: Vec<Bucket>,
}

impl Cube {
    fn new(bits: [u32; 3]) -> Cube {
        Cube {
            bits,
            buckets: vec![Bucket::default(); 1 << (bits[0] + bits[1] + bits[2])],
        }
    }
    fn pos(&self, r: u32, g: u32, b: u32) -> usize {
        (r << (self.bits[1] + self.bits[2]) | g << self.bits[2] | b) as usize
    }
    fn offset(&self, p: [u8; 3]) -> usize {
        let s = |i: usize| p[i] as u32 >> (8 - self.bits[i]);
        self.pos(s(0), s(1), s(2))
    }
    fn used(&self) -> usize {
        self.buckets.iter().filter(|b| b.count > 0).count()
    }
    /// copy_color_cube: expand or shrink to `bits`.
    fn copy(&self, bits: [u32; 3]) -> Cube {
        let mut res = Cube::new(bits);
        let mut src_reduce = [0; 3];
        let mut dst_reduce = [0; 3];
        let mut width = [0; 3];
        for i in 0..3 {
            if self.bits[i] > bits[i] {
                dst_reduce[i] = self.bits[i] - bits[i];
                width[i] = 1 << self.bits[i];
            } else {
                src_reduce[i] = bits[i] - self.bits[i];
                width[i] = 1 << bits[i];
            }
        }
        for r in 0..width[0] {
            for g in 0..width[1] {
                for b in 0..width[2] {
                    let s = self.buckets
                        [self.pos(r >> src_reduce[0], g >> src_reduce[1], b >> src_reduce[2])];
                    let d = res.pos(r >> dst_reduce[0], g >> dst_reduce[1], b >> dst_reduce[2]);
                    let d = &mut res.buckets[d];
                    d.count = d.count.wrapping_add(s.count);
                    d.r = d.r.wrapping_add(s.r);
                    d.g = d.g.wrapping_add(s.g);
                    d.b = d.b.wrapping_add(s.b);
                }
            }
        }
        res
    }
}

/// avg_color_from_color_bucket: float division, truncate.
fn avg(b: &Bucket) -> [u8; 3] {
    let n = b.count as f32;
    if n == 0.0 {
        return [0; 3];
    }
    let c = |s: u64| ((s as f32 / n) as i32).clamp(0, 255) as u8;
    [c(b.r), c(b.g), c(b.b)]
}

extern "C" fn cmp_count(a: *const c_void, b: *const c_void) -> i32 {
    // SAFETY: qsort hands back pointers into the Bucket slice it was given.
    let (a, b) = unsafe { (&*(a as *const Bucket), &*(b as *const Bucket)) };
    b.count.wrapping_sub(a.count) as i32
}

extern "C" {
    fn qsort(
        base: *mut c_void,
        n: usize,
        size: usize,
        cmp: extern "C" fn(*const c_void, *const c_void) -> i32,
    );
}

/// create_sorted_color_palette. Pillow uses the libc qsort, which is not
/// stable: equal-count buckets land in its order, and that decides which fine
/// colors make the cut. Calling the same qsort keeps palettes identical.
fn sorted(c: &Cube) -> Vec<Bucket> {
    let mut v = c.buckets.clone();
    // SAFETY: v is a live, contiguous Vec<Bucket>; cmp_count only reads.
    unsafe {
        qsort(
            v.as_mut_ptr().cast(),
            v.len(),
            size_of::<Bucket>(),
            cmp_count,
        )
    };
    v
}

fn subtract(cube: &mut Cube, buckets: &[Bucket]) {
    for s in buckets.iter().filter(|s| s.count != 0) {
        let off = cube.offset(avg(s));
        let m = &mut cube.buckets[off];
        m.count = m.count.wrapping_sub(s.count);
        m.r = m.r.wrapping_sub(s.r);
        m.g = m.g.wrapping_sub(s.g);
        m.b = m.b.wrapping_sub(s.b);
    }
}

/// add_lookup_buckets: palette index stored in `count`; walks backwards so the
/// lowest index wins a shared bucket.
fn add_lookup(cube: &mut Cube, palette: &[Bucket], range: std::ops::Range<usize>) {
    for i in range.rev() {
        let off = cube.offset(avg(&palette[i]));
        cube.buckets[off].count = i as u32;
    }
}

const FINE: [u32; 3] = [4, 4, 4];
const COARSE: [u32; 3] = [2, 2, 2];

/// quantize_octree: (palette of n colors, palette index per pixel).
fn quantize_octree(img: &[u8], n: u32) -> (Vec<[u8; 3]>, Vec<usize>) {
    let n = n as usize;
    let pixels: Vec<[u8; 3]> = img.chunks_exact(3).map(|p| [p[0], p[1], p[2]]).collect();
    let mut fine = Cube::new(FINE);
    for &p in &pixels {
        let off = fine.offset(p);
        let b = &mut fine.buckets[off];
        b.count += 1;
        b.r += p[0] as u64;
        b.g += p[1] as u64;
        b.b += p[2] as u64;
    }
    let mut coarse = fine.copy(COARSE);
    let mut n_coarse = coarse.used().min(n);
    let mut n_fine = n - n_coarse;
    let fine_pal = sorted(&fine);
    subtract(&mut coarse, &fine_pal[..n_fine]);
    // did the subtraction clear one or more coarse buckets? then the freed
    // slots go to fine colors
    while n_coarse > coarse.used() {
        let done = n_fine;
        n_coarse = coarse.used();
        n_fine = n - n_coarse;
        subtract(&mut coarse, &fine_pal[done..n_fine]);
    }
    let mut palette = sorted(&coarse)[..n_coarse].to_vec();
    palette.extend_from_slice(&fine_pal[..n_fine]);

    let mut coarse_lookup = Cube::new(COARSE);
    add_lookup(&mut coarse_lookup, &palette, 0..n_coarse);
    let mut lookup = coarse_lookup.copy(FINE);
    add_lookup(&mut lookup, &palette, n_coarse..n_coarse + n_fine);

    let idx = pixels
        .iter()
        .map(|&p| lookup.buckets[lookup.offset(p)].count as usize)
        .collect();
    (palette.iter().map(avg).collect(), idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xterm256_grey_gate() {
        // neutral -> ramp; same brightness past the gate -> cube
        assert_eq!(xterm256(64, 64, 64), 232 + 6); // 8 + 10*6 = 68
        assert!(xterm256(64, 64, 64 + GREY_GATE as u8) >= 232);
        assert!(xterm256(64, 64, 64 + GREY_GATE as u8 + 1) < 232);
        assert_eq!(xterm256(0, 0, 0), 16); // pure black is the cube corner, not the ramp
        assert_eq!(xterm256(255, 255, 255), 231);
    }

    #[test]
    fn xterm256_cube_is_not_evenly_spaced() {
        // levels 0,95,135,175,215,255: 60 is closer to 95 than to 0, 114 to 95
        assert_eq!(xterm256(60, 0, 0), 16 + 36);
        assert_eq!(xterm256(114, 0, 200), 16 + 36 + 4);
        assert_eq!(xterm256(255, 135, 0), 16 + 5 * 36 + 2 * 6);
    }

    fn solid_png(dir: &Path, name: &str, rgb: (u8, u8, u8)) -> PathBuf {
        let path = dir.join(format!("{name}.png"));
        let color = format!("color=0x{:02x}{:02x}{:02x}:s=90x72", rgb.0, rgb.1, rgb.2);
        let ok = Command::new("ffmpeg")
            .args([
                "-v",
                "quiet",
                "-y",
                "-f",
                "lavfi",
                "-i",
                &color,
                "-frames:v",
                "1",
            ])
            .arg(&path)
            .status()
            .unwrap()
            .success();
        assert!(ok, "ffmpeg could not write {name}");
        path
    }

    /// Dark blue and dark green must not snap to the grey ramp. The ramp is the
    /// only fine gradation in xterm-256, so it wins on distance for any muted
    /// dark color and covers rendered grey/black. Neutrals must still take the ramp.
    #[test]
    fn test_dark_hues_keep_their_color() {
        let dir = std::env::temp_dir().join(format!("msm-art-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, col, grey) in [
            ("navy", (25, 25, 60), false),
            ("green", (28, 52, 33), false),
            ("grey", (64, 64, 64), true),
        ] {
            let path = solid_png(&dir, name, col);
            let fg = art_grid(&path, 45, 18).unwrap()[0][0].0;
            assert_eq!(fg >= 232, grey, "{name} {fg}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn undecodable_is_err() {
        let path = std::env::temp_dir().join(format!("msm-art-bad-{}.jpg", std::process::id()));
        std::fs::write(&path, b"not an image").unwrap();
        assert!(art_grid(&path, 20, 8).is_err());
        std::fs::remove_file(&path).ok();
    }

    /// Per-cell match against Pillow's _art_grid on the real cover cache.
    /// `cargo test art -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn matches_pillow_on_real_covers() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let py = [
            root.join(".venv/bin/python"),
            root.join("../../../.venv/bin/python"),
        ]
        .into_iter()
        .find(|p| p.exists())
        .expect("no .venv python with Pillow");
        // MSM_ART_DIR: e.g. covers pre-decoded by Pillow to .ppm, to split
        // decoder drift (ffmpeg vs libjpeg) from pipeline drift
        let art = std::env::var_os("MSM_ART_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(crate::art_cache);
        let mut covers: Vec<PathBuf> = std::fs::read_dir(&art)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        covers.sort();
        let covers: Vec<PathBuf> = covers.into_iter().step_by(5).collect();
        for (cols, rows) in [(43, 18), (20, 8), (36, 18)] {
            let script = format!(
                "import sys, json\nfrom msm import tui\nprint(json.dumps([tui._art_grid(p, {cols}, {rows}) for p in sys.argv[1:]]))"
            );
            let out = Command::new(&py)
                .current_dir(root)
                .arg("-c")
                .arg(&script)
                .args(&covers)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let want: Vec<Vec<Vec<(u8, u8)>>> = serde_json::from_slice(&out.stdout).unwrap();
            let (mut fg, mut bg, mut n, mut exact) = (0, 0, 0, 0);
            for (path, w) in covers.iter().zip(&want) {
                let g = art_grid(path, cols, rows).unwrap();
                let (mut f1, mut b1) = (0, 0);
                for (gr, wr) in g.iter().zip(w) {
                    for (a, b) in gr.iter().zip(wr) {
                        f1 += (a.0 == b.0) as usize;
                        b1 += (a.1 == b.1) as usize;
                    }
                }
                exact += (f1 + b1 == 2 * cols * rows) as usize;
                fg += f1;
                bg += b1;
                n += cols * rows;
            }
            println!(
                "{cols}x{rows}: {} covers, fg {:.2}% bg {:.2}%, {} covers exact",
                covers.len(),
                100.0 * fg as f64 / n as f64,
                100.0 * bg as f64 / n as f64,
                exact
            );
        }
    }
}
