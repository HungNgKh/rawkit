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

use crate::{
    in_crop, in_grid, picking_wb, placing_mask, targeting, Aim, Marquee, MaskDrag, CANVAS_CLICK,
    CANVAS_MARQUEE, CANVAS_SCROLL, MASK_DRAG, TARGET_AIM, TARGET_RANGE_PX, WB_PICK,
};
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

/// Move the two bands the sampled colour lies between, in the proportion the
/// renderer will weight them by.
///
/// Both, and by those weights, because that is the entire point: pointing at a
/// lawn and dragging up moves Yellow by 0.69 of the gesture and Green by 0.31,
/// which is what makes the lawn receive all of it. Moving the nearest band alone
/// would give roughly two-thirds of the effect — the problem the tool exists to
/// remove, arrived at from the other side.
///
/// Deltas are from where the sliders were when the hand went down, so a slow
/// drag does not compound and a drag back to the start returns the values it
/// started from.
fn aim(at: [f64; 2], control: rawkit_editstate::BandControl, session: &Arc<Mutex<Session>>) {
    let aim = TARGET_AIM.lock().expect("aim lock");
    let Some(aim) = aim.as_ref() else { return };
    // Still waiting for the render loop to say what colour this is. A frame at
    // most, and moving in the meantime would adjust a band nobody aimed at.
    let Some((bands, start)) = aim.picked else {
        return;
    };
    // Up is more, which is the way every drag-adjust in every editor works and
    // the opposite of the y axis.
    let moved = (aim.at[1] - at[1]) / TARGET_RANGE_PX;

    let mut session = session.lock().expect("session lock");
    for (index, (band, weight)) in bands.iter().enumerate() {
        let value = (start[index] + moved as f32 * weight).clamp(-1.0, 1.0);
        session.apply(Command::SetHsl {
            band: *band,
            control,
            value,
        });
    }
}

/// How much one wheel notch zooms. 1.15 is about a sixth of a stop of scale —
/// small enough that a notch feels like a nudge rather than a jump.
const ZOOM_STEP: f64 = 1.15;

pub(crate) fn route(event: Pointer, session: &Arc<Mutex<Session>>) {
    match event {
        Pointer::Press { at, double } => {
            // A white-balance pick is a click rather than a drag: it takes the
            // press, resolves on the next frame, and does not start a pan.
            if picking_wb() && !in_grid() {
                *WB_PICK.lock().expect("pick lock") = Some(at);
                return;
            }
            // Placing a gradient takes the press for the same reason aiming
            // does: the drag draws the mask, so it must not also pan the
            // photograph the mask is being drawn on. A press with no motion
            // after it leaves the gradient where it was, which is what makes a
            // mis-click harmless.
            if placing_mask().is_some() && !in_grid() {
                *MASK_DRAG.lock().expect("mask drag lock") = Some(MaskDrag {
                    start: at,
                    now: at,
                    trail: vec![at],
                    fresh: true,
                });
                return;
            }
            // Aiming takes the press before anything else, and does not fall
            // through to `DRAG`: a targeted drag must not also pan the
            // photograph out from under the colour it is adjusting.
            if targeting().is_some() && !in_grid() {
                *TARGET_AIM.lock().expect("aim lock") = Some(Aim { at, picked: None });
                return;
            }
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
            // The far end follows the pointer. Where that lands on the sensor is
            // the render loop's question, not this one's.
            if let Some(drag) = MASK_DRAG.lock().expect("mask drag lock").as_mut() {
                drag.now = at;
                // Kept, not replaced: a brush paints along every point the hand
                // passed through, and a frame that arrives two motions late must
                // still get both of them.
                drag.trail.push(at);
                return;
            }
            if let Some(control) = targeting() {
                aim(at, control, session);
                return;
            }
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
        Pointer::Release => {
            *DRAG.lock().expect("drag lock") = None;
            *TARGET_AIM.lock().expect("aim lock") = None;
            // The gradient stays where the drag left it; only the drag ends.
            // Disarming here as well would make placing a second gradient need
            // a trip back to the panel between every attempt.
            *MASK_DRAG.lock().expect("mask drag lock") = None;
        }

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
