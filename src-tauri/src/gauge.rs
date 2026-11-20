//! The mascot picture (embedded with its eyes removed, so moods can draw their own eyes),
//! resized for the taskbar widget and the tray icon.

/// 160×160 straight RGBA, made by tools/make_mascot.py.
const SRC: &[u8] = include_bytes!("../assets/mascot-160.rgba");
const SRC_N: usize = 160;

/// Eye boxes in the picture's 0–100 design space: (x0, y0, x1, y1).
pub const EYES: [(f32, f32, f32, f32); 2] = [(24.11, 39.02, 31.61, 46.96), (70.18, 39.02, 77.68, 46.96)];

fn weights(i: usize, n0: usize, n: usize) -> Vec<(usize, f32)> {
    let (a, b) = (i as f32 * n0 as f32 / n as f32, (i + 1) as f32 * n0 as f32 / n as f32);
    (a.floor() as usize..(b.ceil() as usize).min(n0))
        .filter_map(|j| {
            let w = b.min(j as f32 + 1.0) - a.max(j as f32);
            (w > 0.0).then_some((j, w))
        })
        .collect()
}

/// Area-weighted resize to `n`×`n`; premultiplied RGBA in 0..1.
fn resized(n: usize) -> Vec<[f32; 4]> {
    let src: Vec<[f32; 4]> = SRC
        .chunks_exact(4)
        .map(|p| {
            let a = p[3] as f32 / 255.0;
            [p[0] as f32 / 255.0 * a, p[1] as f32 / 255.0 * a, p[2] as f32 / 255.0 * a, a]
        })
        .collect();
    let wts: Vec<_> = (0..n).map(|i| weights(i, SRC_N, n)).collect();
    let mut tmp = vec![[0f32; 4]; SRC_N * n];
    for y in 0..SRC_N {
        for (x, wx) in wts.iter().enumerate() {
            let (mut acc, mut tot) = ([0f32; 4], 0f32);
            for &(j, w) in wx {
                let p = src[y * SRC_N + j];
                for k in 0..4 {
                    acc[k] += p[k] * w;
                }
                tot += w;
            }
            tmp[y * n + x] = acc.map(|v| v / tot);
        }
    }
    let mut out = vec![[0f32; 4]; n * n];
    for (y, wy) in wts.iter().enumerate() {
        for x in 0..n {
            let (mut acc, mut tot) = ([0f32; 4], 0f32);
            for &(j, w) in wy {
                let p = tmp[j * n + x];
                for k in 0..4 {
                    acc[k] += p[k] * w;
                }
                tot += w;
            }
            out[y * n + x] = acc.map(|v| v / tot);
        }
    }
    out
}

/// Premultiplied BGRA bytes for a Direct2D bitmap.
pub fn bgra_premultiplied(n: u32) -> Vec<u8> {
    resized(n as usize)
        .iter()
        .flat_map(|p| [p[2], p[1], p[0], p[3]].map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8))
        .collect()
}

/// Tray icon: the mascot with its eyes, plus a status dot (colour = how much is left).
pub fn tray_icon(n: u32, dot: Option<[u8; 3]>) -> Vec<u8> {
    let nn = n as usize;
    let mut px = resized(nn);
    let cell = 100.0 / n as f32;
    // Eyes: exact pixel coverage of each eye box, painted black.
    for y in 0..nn {
        for x in 0..nn {
            let (px0, py0) = (x as f32 * cell, y as f32 * cell);
            let cov: f32 = EYES
                .iter()
                .map(|&(x0, y0, x1, y1)| {
                    let w = (x1.min(px0 + cell) - x0.max(px0)).max(0.0);
                    let h = (y1.min(py0 + cell) - y0.max(py0)).max(0.0);
                    w * h / (cell * cell)
                })
                .sum::<f32>()
                .min(1.0);
            let p = &mut px[y * nn + x];
            *p = [p[0] * (1.0 - cov), p[1] * (1.0 - cov), p[2] * (1.0 - cov), cov + p[3] * (1.0 - cov)];
        }
    }
    // Status dot with a white ring, bottom-right.
    if let Some(c) = dot {
        let nf = n as f32;
        let (cx, cy, r) = (nf * 0.84, nf * 0.84, nf * 0.15);
        let ring = r + (nf / 16.0).max(1.0);
        const SS: usize = 4;
        for y in 0..nn {
            for x in 0..nn {
                let (mut in_dot, mut in_ring) = (0f32, 0f32);
                for sy in 0..SS {
                    for sx in 0..SS {
                        let fx = x as f32 + (sx as f32 + 0.5) / SS as f32 - cx;
                        let fy = y as f32 + (sy as f32 + 0.5) / SS as f32 - cy;
                        let d = (fx * fx + fy * fy).sqrt();
                        if d <= r {
                            in_dot += 1.0;
                        } else if d <= ring {
                            in_ring += 1.0;
                        }
                    }
                }
                let total = (SS * SS) as f32;
                let (a_dot, a_ring) = (in_dot / total, in_ring / total);
                let p = &mut px[y * nn + x];
                for (cov, col) in [(a_ring, [1.0, 1.0, 1.0]), (a_dot, [c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0])] {
                    if cov > 0.0 {
                        *p = [col[0] * cov + p[0] * (1.0 - cov), col[1] * cov + p[1] * (1.0 - cov), col[2] * cov + p[2] * (1.0 - cov), cov + p[3] * (1.0 - cov)];
                    }
                }
            }
        }
    }
    px.iter()
        .flat_map(|p| {
            let a = p[3].clamp(0.0, 1.0);
            let un = |v: f32| if a > 0.002 { (v / a).clamp(0.0, 1.0) } else { 0.0 };
            [un(p[0]), un(p[1]), un(p[2]), a].map(|v| (v * 255.0).round() as u8)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn resize_keeps_the_picture_opaque_in_the_middle() {
        let n = 32usize;
        let img = super::bgra_premultiplied(n as u32);
        assert_eq!(img.len(), n * n * 4);
        let mid = (n / 2 * n + n / 2) * 4;
        assert!(img[mid + 3] > 250, "centre of the sticker is opaque");
        assert_eq!(img[3], 0, "corner is transparent");
    }

    #[test]
    fn tray_icon_has_eyes_and_dot() {
        let n = 32u32;
        let icon = super::tray_icon(n, Some([0, 200, 100]));
        let at = |x: usize, y: usize| &icon[(y * n as usize + x) * 4..(y * n as usize + x) * 4 + 4];
        let eye = at((27.8 * 0.32) as usize, (43.0 * 0.32) as usize);
        assert!(eye[0] < 40 && eye[3] > 200, "eye pixel is dark: {eye:?}");
        let dot = at(27, 27);
        assert_eq!(&dot[..3], &[0, 200, 100]);
    }
}

#[cfg(test)]
mod dump {
    /// `cargo test dump_tray -- --ignored` writes tray icons to %TEMP% for a visual check.
    #[test]
    #[ignore]
    fn dump_tray() {
        for (n, dot) in [(16u32, Some([0x7B, 0xE3, 0xAE])), (32, Some([0xFF, 0x80, 0x95])), (24, None)] {
            let path = std::env::temp_dir().join(format!("clump-tray-{n}.rgba"));
            std::fs::write(path, super::tray_icon(n, dot)).unwrap();
        }
    }
}
