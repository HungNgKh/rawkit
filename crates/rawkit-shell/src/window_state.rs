//! Where the window was, so it opens there again.
//!
//! # Why this is a file and not the catalog
//!
//! It describes *this machine*, not the photographs. A catalog carried to
//! another computer should not bring a window position with it — the second
//! machine's screen is a different shape, and on a smaller one the remembered
//! rectangle is off the edge. So it lives beside the application's own settings
//! and is thrown away when it does not parse.
//!
//! # Why nothing here refuses
//!
//! Every failure is a shrug. A missing file is a first run; an unreadable one is
//! a file somebody edited; a directory that cannot be created is a locked-down
//! machine. None of those is a reason not to open a window, so the whole module
//! returns `Option` and the caller falls back to the default size.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Logical units throughout.
///
/// Physical would be the obvious choice — `outer_position` reports physical —
/// and it is the wrong one: the number would mean something different the next
/// time the desktop's scale factor changed, and the window would open a quarter
/// of the way across the screen from where it was left.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Remembered {
    pub width: f64,
    pub height: f64,
    pub x: f64,
    pub y: f64,
    /// The divider, which is part of the same answer: reopening at the right
    /// size with the panel back at its default is only half of coming back to
    /// where you were.
    pub panel: f64,
    /// Restored *after* the size, because a maximised window's own size is the
    /// screen's and would overwrite the one to come back to when it unmaximises.
    #[serde(default)]
    pub maximised: bool,
}

fn path(app: &tauri::AppHandle) -> Option<PathBuf> {
    use tauri::Manager;
    let dir = app.path().app_config_dir().ok()?;
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("window.json"))
}

/// What was saved last time, if any of it can be read.
pub fn load(app: &tauri::AppHandle) -> Option<Remembered> {
    let text = std::fs::read_to_string(path(app)?).ok()?;
    let saved: Remembered = serde_json::from_str(&text).ok()?;
    // A rectangle with no area is not a window. This is the one check worth
    // making: the rest of the numbers can be odd without being unusable, and a
    // position off the edge of a screen the user no longer has is something the
    // window manager already corrects.
    (saved.width >= 1.0 && saved.height >= 1.0).then_some(saved)
}

/// Write it, or do not. There is no third outcome worth reporting.
pub fn save(app: &tauri::AppHandle, state: Remembered) {
    let Some(path) = path(app) else { return };
    if let Ok(text) = serde_json::to_string_pretty(&state) {
        let _ = std::fs::write(path, text);
    }
}

/// Read the window's current geometry, in the units [`Remembered`] stores.
///
/// Nothing at all while fullscreen: the window's size is then the screen's, and
/// saving it would reopen a window the size of the display and not fullscreen —
/// which is neither state the user was in. Keeping the previous value is the
/// better answer, and it costs a `None`.
pub fn of(window: &tauri::Window, panel: f64) -> Option<Remembered> {
    if window.is_fullscreen().unwrap_or(false) {
        return None;
    }
    let scale = window.scale_factor().ok()?;
    let size = window.inner_size().ok()?.to_logical::<f64>(scale);
    let at = window.outer_position().ok()?.to_logical::<f64>(scale);
    Some(Remembered {
        width: size.width,
        height: size.height,
        x: at.x,
        y: at.y,
        panel,
        maximised: window.is_maximized().unwrap_or(false),
    })
}
