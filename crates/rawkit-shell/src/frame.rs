//! How the window is divided: a bar across the top, a panel either side, a
//! status bar across the bottom, and the photograph in what is left.
//!
//! # Why the shell decides and the page follows
//!
//! On Linux the canvas is an X window of its own, placed over the page, so where
//! the photograph ends is not something a stylesheet can decide. If the page
//! drew a top bar two pixels taller than the gap the canvas left, the photograph
//! would cover its bottom edge; two pixels shorter, and a line of page shows
//! through. So every inset is a number here, the canvas is placed from it, and
//! the page is handed the same numbers to draw its bars at. One description of
//! the division, read by all three.
//!
//! # Why the heights are constant
//!
//! The top bar has two rows in both workspaces: the second holds the filter in
//! Library and the tool's options in Develop. A tool-options bar that appeared
//! only while a tool was in hand would resize the canvas as crop was picked up —
//! the photograph would jump by a row's height under the handles somebody was
//! about to grab, and the swapchain would be rebuilt on a keypress. Keeping the
//! row costs 36 pixels of photograph in Library and buys a canvas that changes
//! size only when the window or a panel does.

use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Mutex;

/// The top bar: a row for where you are and what you can do (40), and a row
/// for the options of what you are doing (36). Logical pixels.
pub const TOP: i32 = 76;
/// The status bar: one line of text with room for a small Stop button.
pub const BOTTOM: i32 = 28;
/// Navigation: catalog, folders, collections; presets and snapshots in Develop.
/// Wide enough for a collection name and its count, and no wider — it takes
/// from the photograph's long edge, which a landscape frame can spare.
pub const LEFT: i32 = 260;

/// What the divider may do to the right panel. The lower bound is where the
/// widest label — the noise-reduction pair — starts wrapping, measured rather
/// than guessed; the upper is where the photograph stops being the larger half
/// of a minimum-width window.
pub const PANEL_MIN: f64 = 280.0;
pub const PANEL_MAX: f64 = 640.0;
/// Where the right panel starts, the first time.
pub const PANEL_DEFAULT: i32 = 360;

/// The least photograph worth showing, in logical pixels. What gives way in a
/// window too narrow for everything: the panels, not the picture. The left one
/// goes first — navigation can be reached by key; the controls beside the
/// photograph are what the photograph is being looked at for.
pub const PHOTO_MIN: f64 = 320.0;

/// Each side's share of the window, in whole logical pixels. Zero is a panel
/// that is hidden, or that the window had no room for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Frame {
    pub top: i32,
    pub left: i32,
    pub right: i32,
    pub bottom: i32,
}

/// What somebody asked for, before the window has had its say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wanted {
    /// The right panel's width from the divider.
    pub panel: i32,
    pub left: bool,
    pub right: bool,
}

/// Divide a window of this logical size.
///
/// The panels yield to the photograph here, rather than by writing smaller
/// numbers back: the width somebody chose is theirs, and a window that narrows
/// and widens again should give it back rather than having quietly forgotten it.
pub fn divide(window: (f64, f64), wanted: Wanted) -> Frame {
    let room = window.0;
    let right = if wanted.right {
        (wanted.panel as f64).min((room - PHOTO_MIN).max(PANEL_MIN)) as i32
    } else {
        0
    };
    let left = if wanted.left && room - right as f64 - LEFT as f64 >= PHOTO_MIN {
        LEFT
    } else {
        0
    };
    Frame {
        top: TOP,
        left,
        right,
        bottom: BOTTOM,
    }
}

impl Frame {
    /// The photograph's rectangle in a window of this *logical* size, as
    /// `[x, y, width, height]`. Never empty: a surface of no size is an error.
    // Only the native canvas places a window and hears GTK's events; under a
    // cutout the page does both, from the same numbers. Tested everywhere.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn canvas(&self, window: (i32, i32)) -> [i32; 4] {
        [
            self.left,
            self.top,
            (window.0 - self.left - self.right).max(1),
            (window.1 - self.top - self.bottom).max(1),
        ]
    }

    /// The same rectangle in *physical* pixels, for the surface.
    ///
    /// The insets are scaled and the canvas is what remains, rather than the
    /// canvas being scaled: at a fractional factor, rounding each of five numbers
    /// on its own leaves a pixel gap or overlap somewhere, and it is the canvas
    /// that has to meet the bars exactly.
    pub fn canvas_physical(&self, window: (u32, u32), scale: f64) -> [u32; 4] {
        let at = |logical: i32| (logical as f64 * scale).round() as u32;
        let (left, top) = (at(self.left), at(self.top));
        [
            left,
            top,
            window.0.saturating_sub(left + at(self.right)).max(1),
            window.1.saturating_sub(top + at(self.bottom)).max(1),
        ]
    }

    /// Where a point in the window's logical coordinates falls on the canvas,
    /// in the canvas's physical pixels — or nowhere, if it is on a bar or a
    /// panel. The one question both pointer deliverers ask.
    // Only the native canvas places a window and hears GTK's events; under a
    // cutout the page does both, from the same numbers. Tested everywhere.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn to_canvas(self, window: (i32, i32), at: (f64, f64), scale: f64) -> Option<[f64; 2]> {
        let [x, y, width, height] = self.canvas(window);
        let (dx, dy) = (at.0 - x as f64, at.1 - y as f64);
        ((0.0..width as f64).contains(&dx) && (0.0..height as f64).contains(&dy))
            .then_some([dx * scale, dy * scale])
    }
}

/// The division in force, as the last layout decided it.
///
/// A static because four places need it and none can hold it: the GTK
/// size-allocate handler that places the canvas, the pointer handlers, the
/// render loop that configures the surface, and the page's poll.
static NOW: Mutex<Frame> = Mutex::new(Frame {
    top: TOP,
    left: LEFT,
    right: PANEL_DEFAULT,
    bottom: BOTTOM,
});

/// Whether each panel is wanted. F7 and F8, remembered with the window.
static LEFT_WANTED: AtomicBool = AtomicBool::new(true);
static RIGHT_WANTED: AtomicBool = AtomicBool::new(true);

pub fn current() -> Frame {
    *NOW.lock().expect("frame lock")
}

pub fn settle(frame: Frame) {
    *NOW.lock().expect("frame lock") = frame;
}

pub fn wanted(panel: i32) -> Wanted {
    Wanted {
        panel,
        left: LEFT_WANTED.load(Relaxed),
        right: RIGHT_WANTED.load(Relaxed),
    }
}

pub fn want(left: bool, right: bool) {
    LEFT_WANTED.store(left, Relaxed);
    RIGHT_WANTED.store(right, Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOTH: Wanted = Wanted {
        panel: 360,
        left: true,
        right: true,
    };

    #[test]
    fn a_wide_window_has_all_four_sides() {
        let frame = divide((1920.0, 1080.0), BOTH);
        assert_eq!((frame.left, frame.right), (LEFT, 360));
        assert_eq!(frame.canvas((1920, 1080)), [260, 76, 1300, 976]);
    }

    #[test]
    fn a_narrowing_window_gives_up_the_left_panel_before_the_photograph() {
        // 260 + 360 + 320 = 940: one pixel less and navigation goes.
        assert_eq!(divide((940.0, 800.0), BOTH).left, LEFT);
        let narrow = divide((939.0, 800.0), BOTH);
        assert_eq!((narrow.left, narrow.right), (0, 360));
        // And then the right panel shrinks, but never below its minimum.
        let narrower = divide((720.0, 480.0), BOTH);
        assert_eq!((narrower.left, narrower.right), (0, 360));
        let narrowest = divide((500.0, 480.0), BOTH);
        assert_eq!(narrowest.right, PANEL_MIN as i32);
    }

    #[test]
    fn a_hidden_panel_gives_its_width_to_the_photograph() {
        let frame = divide(
            (1920.0, 1080.0),
            Wanted {
                left: false,
                right: false,
                ..BOTH
            },
        );
        assert_eq!(frame.canvas((1920, 1080)), [0, 76, 1920, 976]);
    }

    #[test]
    fn the_physical_canvas_meets_the_bars_exactly() {
        let frame = divide((1920.0, 1080.0), BOTH);
        // Twice the pixels at twice the scale, and nothing lost to rounding.
        assert_eq!(
            frame.canvas_physical((3840, 2160), 2.0),
            [520, 152, 2600, 1952]
        );
        // At a fractional scale the canvas is what the scaled bars leave.
        let [x, y, w, h] = frame.canvas_physical((2400, 1350), 1.25);
        assert_eq!((x, y), (325, 95));
        assert_eq!(x + w + (360.0f64 * 1.25).round() as u32, 2400);
        assert_eq!(y + h + (28.0f64 * 1.25).round() as u32, 1350);
    }

    #[test]
    fn a_pointer_is_on_the_canvas_only_between_the_bars() {
        let frame = divide((1920.0, 1080.0), BOTH);
        let window = (1920, 1080);
        assert_eq!(
            frame.to_canvas(window, (260.0, 76.0), 2.0),
            Some([0.0, 0.0])
        );
        assert_eq!(
            frame.to_canvas(window, (300.5, 100.0), 2.0),
            Some([81.0, 48.0])
        );
        // The left panel, the top bar, the right panel, the status bar.
        assert_eq!(frame.to_canvas(window, (259.9, 500.0), 2.0), None);
        assert_eq!(frame.to_canvas(window, (800.0, 75.9), 2.0), None);
        assert_eq!(frame.to_canvas(window, (1560.0, 500.0), 2.0), None);
        assert_eq!(frame.to_canvas(window, (800.0, 1052.0), 2.0), None);
    }
}
