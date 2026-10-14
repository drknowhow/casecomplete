//! Pure window geometry (no Win32 calls, so it is unit-testable anywhere).
//!
//! `Area` and the numbers it carries are physical (device) pixels; the
//! `*_LOGICAL` constants are device-independent pixels that get multiplied by
//! the monitor's scale factor.

/// A rectangle in physical pixels, with Win32 `RECT` semantics (`right` and
/// `bottom` are exclusive).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Area {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Area {
    pub(crate) fn width(&self) -> i32 {
        self.right - self.left
    }

    pub(crate) fn height(&self) -> i32 {
        self.bottom - self.top
    }
}

/// Default client size of the dashboard window.
pub(crate) const DASHBOARD_LOGICAL: (f64, f64) = (1280.0, 820.0);
/// Client size of the flyout.
pub(crate) const FLYOUT_LOGICAL: (f64, f64) = (380.0, 620.0);
/// Gap between the flyout and the work-area edges.
pub(crate) const FLYOUT_MARGIN_LOGICAL: f64 = 12.0;

/// Where the flyout goes: the bottom-right corner of `work`, `FLYOUT_MARGIN_LOGICAL`
/// away from the edges. The size is shrunk if the work area is too small.
pub(crate) fn flyout_rect(work: Area, scale: f64) -> Area {
    let margin = (FLYOUT_MARGIN_LOGICAL * scale).round() as i32;
    let width = ((FLYOUT_LOGICAL.0 * scale).round() as i32)
        .min(work.width() - 2 * margin)
        .max(1);
    let height = ((FLYOUT_LOGICAL.1 * scale).round() as i32)
        .min(work.height() - 2 * margin)
        .max(1);
    let right = work.right - margin;
    let bottom = work.bottom - margin;
    Area {
        left: right - width,
        top: bottom - height,
        right,
        bottom,
    }
}

/// Dashboard client size in logical pixels: the default, reduced on screens
/// that cannot hold it together with the title bar and some slack.
pub(crate) fn dashboard_logical_size(work: Area, scale: f64) -> (f64, f64) {
    let max_width = (f64::from(work.width()) / scale - 16.0).max(320.0);
    let max_height = (f64::from(work.height()) / scale - 48.0).max(240.0);
    (
        DASHBOARD_LOGICAL.0.min(max_width),
        DASHBOARD_LOGICAL.1.min(max_height),
    )
}

/// Top-left corner that centres a `width` x `height` window in `work`.
pub(crate) fn centered(work: Area, width: i32, height: i32) -> (i32, i32) {
    (
        work.left + ((work.width() - width) / 2).max(0),
        work.top + ((work.height() - height) / 2).max(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_HD: Area = Area {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1040, // 1080 minus a 40 px taskbar
    };

    #[test]
    fn flyout_sits_bottom_right_above_the_taskbar() {
        let r = flyout_rect(FULL_HD, 1.0);
        assert_eq!((r.width(), r.height()), (380, 620));
        assert_eq!((r.right, r.bottom), (1920 - 12, 1040 - 12));
        assert_eq!((r.left, r.top), (1528, 408));
    }

    #[test]
    fn flyout_scales_with_dpi() {
        let work = Area {
            left: 0,
            top: 0,
            right: 3840,
            bottom: 2080,
        };
        let r = flyout_rect(work, 2.0);
        assert_eq!((r.width(), r.height()), (760, 1240));
        assert_eq!((r.right, r.bottom), (3840 - 24, 2080 - 24));
    }

    #[test]
    fn flyout_follows_a_taskbar_on_another_edge() {
        // Taskbar docked on the left (60 px) and primary monitor at the origin.
        let work = Area {
            left: 60,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let r = flyout_rect(work, 1.0);
        assert_eq!((r.right, r.bottom), (1908, 1068));
        assert!(r.left >= work.left && r.top >= work.top);
    }

    #[test]
    fn flyout_shrinks_on_a_tiny_work_area() {
        let work = Area {
            left: 0,
            top: 0,
            right: 300,
            bottom: 500,
        };
        let r = flyout_rect(work, 1.0);
        assert!(r.left >= work.left && r.top >= work.top);
        assert_eq!((r.right, r.bottom), (288, 488));
        assert_eq!((r.width(), r.height()), (276, 476));
    }

    #[test]
    fn dashboard_keeps_its_default_size_when_it_fits() {
        assert_eq!(dashboard_logical_size(FULL_HD, 1.0), (1280.0, 820.0));
    }

    #[test]
    fn dashboard_shrinks_on_a_small_screen() {
        let laptop = Area {
            left: 0,
            top: 0,
            right: 1366,
            bottom: 728,
        };
        let (w, h) = dashboard_logical_size(laptop, 1.0);
        assert_eq!(w, 1280.0);
        assert_eq!(h, 680.0);
    }

    #[test]
    fn centering() {
        assert_eq!(centered(FULL_HD, 1280, 820), (320, 110));
        let offset = Area {
            left: 100,
            top: 50,
            right: 500,
            bottom: 450,
        };
        assert_eq!(centered(offset, 600, 600), (100, 50), "never starts left of / above the work area");
    }
}
