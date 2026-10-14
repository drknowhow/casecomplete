//! The window icon: the same blue rounded square with a white "C" as the tray
//! icon (see `cc-tray`), drawn at 64 px with 4x4 supersampling. All shapes are
//! defined on a 32-unit grid and scaled, so the picture is identical.

use tao::window::Icon;

const SIZE: u32 = 64;
/// The grid the shapes are defined on (the tray icon is 32 px).
const GRID: f32 = 32.0;

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
    const BLUE: [f32; 3] = [37.0, 99.0, 235.0];
    let total = (SAMPLES * SAMPLES) as f32;
    let to_grid = GRID / size as f32;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let (mut in_square, mut in_letter) = (0u32, 0u32);
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let x = (px as f32 + (sx as f32 + 0.5) / SAMPLES as f32) * to_grid;
                    let y = (py as f32 + (sy as f32 + 0.5) / SAMPLES as f32) * to_grid;
                    if in_rounded_square(x, y) {
                        in_square += 1;
                        if in_letter_c(x, y) {
                            in_letter += 1;
                        }
                    }
                }
            }
            if in_square == 0 {
                out.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            }
            let white = in_letter as f32 / in_square as f32;
            for channel in BLUE {
                out.push((channel * (1.0 - white) + 255.0 * white).round() as u8);
            }
            out.push((in_square as f32 / total * 255.0).round() as u8);
        }
    }
    out
}

/// Square inset by one grid unit with a corner radius of 7.
fn in_rounded_square(x: f32, y: f32) -> bool {
    const LO: f32 = 1.0;
    const HI: f32 = GRID - 1.0;
    const R: f32 = 7.0;
    if !(LO..=HI).contains(&x) || !(LO..=HI).contains(&y) {
        return false;
    }
    let dx = x - x.clamp(LO + R, HI - R);
    let dy = y - y.clamp(LO + R, HI - R);
    dx * dx + dy * dy <= R * R
}

/// A ring (radius 5..10 around the centre) open towards the right.
fn in_letter_c(x: f32, y: f32) -> bool {
    const CENTER: f32 = GRID / 2.0;
    const OPENING_HALF_ANGLE: f32 = 0.7; // radians, about 40 degrees
    let (dx, dy) = (x - CENTER, y - CENTER);
    let dist = (dx * dx + dy * dy).sqrt();
    (5.0..=10.0).contains(&dist) && dy.atan2(dx).abs() > OPENING_HALF_ANGLE
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
        assert_eq!(px(32, 6), &[37, 99, 235, 255], "background is opaque blue");
        assert_eq!(px(12, 32), &[255, 255, 255, 255], "left stroke of the C is white");
        assert_eq!(px(52, 32)[..3], [37, 99, 235], "opening of the C shows the background");
        assert!(window_icon().is_some());
    }
}
