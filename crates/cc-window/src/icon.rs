//! The window icon: the same Tab-key brand mark as the tray icon (see
//! `cc-tray`), drawn at 64 px with 4x4 supersampling. The shapes are defined on
//! the unit square, so the picture is identical at any size.

use tao::window::Icon;

const SIZE: u32 = 64;

/// The window / taskbar icon, or `None` if it could not be built.
pub(crate) fn window_icon() -> Option<Icon> {
    match Icon::from_rgba(rgba(SIZE), SIZE, SIZE) {
        Ok(icon) => Some(icon),
        Err(e) => {
            log::warn!("cc-window: could not build the window icon: {e}");
            None
        }
    }
}

fn rgba(size: u32) -> Vec<u8> {
    const SAMPLES: u32 = 4;
    let total = (SAMPLES * SAMPLES) as f32;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let (mut sum, mut hits) = ([0.0f32; 3], 0u32);
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let u = (px as f32 + (sx as f32 + 0.5) / SAMPLES as f32) / size as f32;
                    let v = (py as f32 + (sy as f32 + 0.5) / SAMPLES as f32) / size as f32;
                    if let Some(c) = mark_at(u, v) {
                        for (s, channel) in sum.iter_mut().zip(c) {
                            *s += channel as f32;
                        }
                        hits += 1;
                    }
                }
            }
            if hits == 0 {
                out.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            }
            for s in sum {
                out.push((s / hits as f32).round() as u8);
            }
            out.push((hits as f32 / total * 255.0).round() as u8);
        }
    }
    out
}

/// Signal blue keycap, ink bottom edge, white glyph.
const BLUE: [u8; 3] = [0x3A, 0x7B, 0xD5];
const INK: [u8; 3] = [0x17, 0x19, 0x1C];
const WHITE: [u8; 3] = [0xFF, 0xFF, 0xFF];
/// Keycap geometry as fractions of the icon: transparent border, corner radius
/// (of the keycap width), ink edge height, glyph stroke width.
const MARGIN: f32 = 1.0 / 32.0;
const RADIUS: f32 = 0.22 * (1.0 - 2.0 * MARGIN);
const EDGE: f32 = 4.0 / 120.0;
const STROKE: f32 = 0.10;
/// Mid line of the glyph: the centre of the keycap face.
const GLYPH_Y: f32 = MARGIN + (1.0 - 2.0 * MARGIN - EDGE) / 2.0;
/// The tab glyph as round-capped segments on the reference SVG's 120-unit
/// grid (x, y relative to the mid line): bar, arrowhead (two strokes), stop bar.
const GLYPH: [([f32; 2], [f32; 2]); 4] = [
    ([30.0, 0.0], [78.0, 0.0]),
    ([64.0, -18.0], [84.0, 0.0]),
    ([84.0, 0.0], [64.0, 18.0]),
    ([92.0, -20.0], [92.0, 20.0]),
];

/// The mark's colour at (u, v) on the unit square, or `None` where transparent.
fn mark_at(u: f32, v: f32) -> Option<[u8; 3]> {
    if in_keycap(u, v, MARGIN) {
        return Some(if on_glyph(u, v) { WHITE } else { BLUE });
    }
    in_keycap(u, v, MARGIN + EDGE).then_some(INK)
}

/// Rounded square whose top edge is at `top`; the face and the edge below it
/// are the same shape, shifted by `EDGE`.
fn in_keycap(u: f32, v: f32, top: f32) -> bool {
    let (lo, hi) = (MARGIN, 1.0 - MARGIN);
    let bottom = top + (1.0 - 2.0 * MARGIN - EDGE);
    if !(lo..=hi).contains(&u) || !(top..=bottom).contains(&v) {
        return false;
    }
    let dx = u - u.clamp(lo + RADIUS, hi - RADIUS);
    let dy = v - v.clamp(top + RADIUS, bottom - RADIUS);
    dx * dx + dy * dy <= RADIUS * RADIUS
}

/// Within half a stroke of any glyph segment.
fn on_glyph(u: f32, v: f32) -> bool {
    const GRID: f32 = 120.0;
    GLYPH.iter().any(|&([ax, ay], [bx, by])| {
        let (ax, ay) = (ax / GRID, GLYPH_Y + ay / GRID);
        let (bx, by) = (bx / GRID, GLYPH_Y + by / GRID);
        let (dx, dy) = (bx - ax, by - ay);
        let t = (((u - ax) * dx + (v - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
        let (ex, ey) = (ax + t * dx - u, ay + t * dy - v);
        ex * ex + ey * ey <= (STROKE / 2.0) * (STROKE / 2.0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_has_expected_size_and_content() {
        let rgba = rgba(SIZE);
        let n = SIZE as usize;
        assert_eq!(rgba.len(), n * n * 4);
        let px = |x: usize, y: usize| &rgba[(y * n + x) * 4..(y * n + x) * 4 + 4];
        assert_eq!(px(0, 0)[3], 0, "corner is transparent");
        assert_eq!(px(32, 6), &[0x3A, 0x7B, 0xD5, 255], "keycap face is opaque signal blue");
        assert_eq!(px(8, 32)[..3], [0x3A, 0x7B, 0xD5], "left of the glyph shows the keycap");
        assert_eq!(px(24, 31), &[255, 255, 255, 255], "the tab arrow's bar is white");
        assert_eq!(px(49, 24), &[255, 255, 255, 255], "the stop bar is white");
        assert_eq!(px(32, 61), &[0x17, 0x19, 0x1C, 255], "keycap bottom edge is ink");
        assert!(window_icon().is_some());
    }
}
