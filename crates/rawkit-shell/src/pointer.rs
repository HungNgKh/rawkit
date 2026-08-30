//! What a pointer means on the canvas, decided once.
//!
//! Two things deliver pointer events and they have nothing in common. On Linux
//! the canvas is its own X window and GTK hands it real events; under a
//! transparent cutout there is no such window, the webview owns every pixel, and
//! the page forwards what it receives over IPC.
//!
//! What must *not* differ is what those events mean. A drag pans, a wheel zooms
//! about the cursor, a click in the grid picks a cell, a drag in crop draws a
//! rectangle — and if that lived in two places, one of them would quietly grow a
//! different idea of which. So both front ends do the same small job — put the
//! event in canvas pixels — and hand it here.
//!
//! Canvas pixels, not logical ones: the session works in the surface's own
//! coordinates, and converting at each boundary keeps the conversion where the
//! scale factor is known rather than passing a scale around.

use crate::{in_crop, in_grid, Marquee, CANVAS_CLICK, CANVAS_MARQUEE, CANVAS_SCROLL};
use rawkit_session::{Command, Session};
use std::sync::{Arc, Mutex};

/// One pointer event, already in canvas pixels.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Pointer {
    Press {
        at: [f64; 2],
        double: bool,
    },
    Motion {
        at: [f64; 2],
    },
    Release,
    /// Wheel notches, positive downwards. A trackpad sends fractions.
    Scroll {
        at: [f64; 2],
        notches: f64,
    },
}

/// Where the pointer was when the button went down, and where it has reached.
///
/// A `Mutex` rather than a `Cell` because the two front ends are not on the same
/// thread: GTK's handlers run on the main loop and Tauri's commands do not.
static DRAG: Mutex<Option<[f64; 2]>> = Mutex::new(None);

/// How much one wheel notch zooms. 1.15 is about a sixth of a stop of scale —
/// small enough that a notch feels like a nudge rather than a jump.
const ZOOM_STEP: f64 = 1.15;

pub(crate) fn route(event: Pointer, session: &Arc<Mutex<Session>>) {
    match event {
        Pointer::Press { at, double } => {
            if in_grid() {
                // The grid works out *which cell* this is, because it is the
                // only place that knows the layout. Everything here does is say
                // where the pointer was.
                *CANVAS_CLICK.lock().expect("click lock") = Some((at, double));
            } else if in_crop() {
                // A new drag replaces whatever rectangle was there. Starting
                // from the old one would mean a crop could only ever shrink.
                *CANVAS_MARQUEE.lock().expect("marquee lock") =
                    Some(Marquee { start: at, end: at });
            }
            *DRAG.lock().expect("drag lock") = Some(at);
        }

        Pointer::Motion { at } => {
            let previous = *DRAG.lock().expect("drag lock");
            let Some(previous) = previous else { return };
            if in_crop() {
                if let Some(marquee) = CANVAS_MARQUEE.lock().expect("marquee lock").as_mut() {
                    marquee.end = at;
                }
                return;
            }
            // Dragging right moves the image right, which the session reads as
            // the centre moving left. A drag emits one of these per motion
            // event, far faster than the GPU draws; the session has no queue, so
            // the extra ones cost a lock and two floats each and only the last
            // is ever rendered.
            session.lock().expect("session lock").apply(Command::Pan {
                dx: at[0] - previous[0],
                dy: at[1] - previous[1],
            });
            *DRAG.lock().expect("drag lock") = Some(at);
        }

        // Letting go is the whole of it. In crop the rectangle stays on screen
        // until Enter takes it or Escape throws it away, so a drag that came out
        // wrong can be redrawn.
        Pointer::Release => *DRAG.lock().expect("drag lock") = None,

        Pointer::Scroll { at, notches } => {
            if in_grid() {
                // A grid scrolls; it does not zoom. Accumulated rather than
                // applied, for the same reason a click is: the layout lives
                // elsewhere.
                CANVAS_SCROLL
                    .fetch_add(notches.round() as i32, std::sync::atomic::Ordering::Relaxed);
                return;
            }
            let mut session = session.lock().expect("session lock");
            let scale = session.viewport().scale * ZOOM_STEP.powf(-notches);
            // Hold the image point under the cursor still, which is what makes
            // scroll-to-zoom feel like examining a print rather than driving a
            // camera.
            session.apply(Command::ZoomTo { scale, anchor: at });
        }
    }
}
