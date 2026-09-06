//! What a pointer means on the canvas, decided once.
//!
//! Two things deliver pointer events and they have nothing in common. On Linux
//! the canvas is its own X window and GTK hands it real events; under a
//! transparent cutout there is no such window, the webview owns every pixel, and
//! the page forwards what it receives over IPC.
//!
//! What must *not* differ is what those events mean. A drag pans, a wheel zooms
//! about the cursor, a click in the grid picks a cell, a drag in crop takes hold
//! of the rectangle on the photograph — and if that lived in two places, one of
//! them would quietly grow a different idea of which. So both front ends do the same small job — put the
//! event in canvas pixels — and hand it here.
//!
//! Canvas pixels, not logical ones: the session works in the surface's own
//! coordinates, and converting at each boundary keeps the conversion where the
//! scale factor is known rather than passing a scale around.

use crate::{
    in_crop, in_grid, picking_range, picking_wb, placing_mask, targeting, Aim, MaskDrag,
    CANVAS_CLICK, CANVAS_SCROLL, MASK_DRAG, TARGET_AIM, TARGET_RANGE_PX, WB_PICK,
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
            // Aiming a range mask is the same gesture on the same terms: one
            // press, resolved on the next frame, and it must not start a pan.
            if picking_range() && !in_grid() {
                *crate::RANGE_PICK.lock().expect("range pick lock") = Some(at);
                return;
            }
            // Crop is a mode, and while it is on the canvas belongs to it
            // entirely. Taken before the handle test rather than after: a
            // `MaskGrab` coming back from `handle_under` arms a mask drag, and a
            // press meant for a crop corner would silently reshape whichever
            // adjustment happened to be selected.
            if in_crop() && !in_grid() {
                let was = *crate::CROP_RECT.lock().expect("crop rect lock");
                if let (Some(was), Some(grab)) = (was, crate::crop_grab_at(at)) {
                    *crate::CROP_DRAG.lock().expect("crop drag lock") = Some(crate::CropDrag {
                        grab,
                        start: at,
                        now: at,
                        was,
                    });
                }
                // Nothing else, and no `DRAG`: a press on the dimmed part is not
                // a gesture, and panning underneath a crop would move the
                // photograph out from under the rectangle being drawn on it.
                return;
            }
            // Placing a gradient takes the press for the same reason aiming
            // does: the drag draws the mask, so it must not also pan the
            // photograph the mask is being drawn on. A press with no motion
            // after it leaves the gradient where it was, which is what makes a
            // mis-click harmless.
            // A handle takes the press before anything else, including a pan:
            // grabbing the shape and dragging the photograph out from under it
            // are the two things that must never be confused, and the handle is
            // the smaller and more deliberate target.
            if !in_grid() {
                if let Some(grab) = crate::handle_under(at) {
                    *crate::MASK_GRAB.lock().expect("mask grab lock") = Some(grab);
                    *MASK_DRAG.lock().expect("mask drag lock") = Some(MaskDrag {
                        start: at,
                        now: at,
                        trail: Vec::new(),
                        fresh: true,
                    });
                    return;
                }
            }
            if placing_mask().is_some() && !in_grid() {
                *MASK_DRAG.lock().expect("mask drag lock") = Some(MaskDrag {
                    start: at,
                    now: at,
                    trail: vec![at],
                    fresh: true,
                });
                return;
            }
            // The spot tool takes the press on the same terms: the drag either
            // places a blemish marker and sizes it, or moves one that is already
            // there. Panning underneath it would move the photograph out from
            // under the thing being covered.
            if crate::in_spot() && !in_grid() {
                *crate::SPOT_DRAG.lock().expect("spot drag lock") = Some(MaskDrag {
                    start: at,
                    now: at,
                    trail: Vec::new(),
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
            }
            *DRAG.lock().expect("drag lock") = Some(at);
        }

        Pointer::Motion { at } => {
            // The far end follows the pointer. Where that lands on the sensor is
            // the render loop's question, not this one's.
            if let Some(drag) = crate::CROP_DRAG.lock().expect("crop drag lock").as_mut() {
                drag.now = at;
                return;
            }
            if let Some(drag) = MASK_DRAG.lock().expect("mask drag lock").as_mut() {
                drag.now = at;
                // Kept, not replaced: a brush paints along every point the hand
                // passed through, and a frame that arrives two motions late must
                // still get both of them.
                drag.trail.push(at);
                return;
            }
            if let Some(drag) = crate::SPOT_DRAG.lock().expect("spot drag lock").as_mut() {
                drag.now = at;
                return;
            }
            if let Some(control) = targeting() {
                aim(at, control, session);
                return;
            }
            let previous = *DRAG.lock().expect("drag lock");
            let Some(previous) = previous else { return };
            // A grid press falls through to set `DRAG`, because a click there
            // still has to be recorded — but a drag across a contact sheet is
            // not a pan, and there was nothing here to say so. It panned the
            // *loupe's* viewport, invisibly: the grid is laid out in screen
            // pixels and nothing on it moves, so the only symptom was opening a
            // photograph later and finding it off-centre for no reason anyone
            // could see. Which reads as the renderer having lost the picture.
            if in_grid() {
                return;
            }
            // Crop never pans. The press above returned without arming one, so
            // this can only be a drag that began before the mode did.
            if in_crop() {
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
            *crate::CROP_DRAG.lock().expect("crop drag lock") = None;
            *DRAG.lock().expect("drag lock") = None;
            *TARGET_AIM.lock().expect("aim lock") = None;
            // A placement drag that actually travelled has placed the thing,
            // so the next press must not place it again.
            //
            // This used to stay armed on purpose — "placing a second gradient
            // would need a trip back to the panel between every attempt" — and
            // that reasoning was sound while a drag was the only way to shape a
            // mask. It is not any more: the shape has handles now, so a press
            // after the placement is meant to *adjust* it, or to pan the
            // photograph. Staying armed meant every such press instead threw the
            // mask away and drew a new one centred where you had pressed, which
            // is what "the position and size go weird while dragging" was.
            //
            // Only the shape's own drag disarms. A brush is painted by many
            // drags and must stay armed, which is why the render loop makes that
            // distinction rather than this does.
            let travelled = MASK_DRAG
                .lock()
                .expect("mask drag lock")
                .as_ref()
                .is_some_and(|d| (d.now[0] - d.start[0]).hypot(d.now[1] - d.start[1]) > 4.0);
            if travelled && crate::MASK_GRAB.lock().expect("mask grab lock").is_none() {
                crate::PLACED_BY_DRAG.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            *MASK_DRAG.lock().expect("mask drag lock") = None;
            *crate::MASK_GRAB.lock().expect("mask grab lock") = None;
            // A spot that was being sized now needs somewhere to borrow from,
            // and the search wants the radius the hand settled on rather than
            // the one it passed through. The render loop does it, because that
            // is where the mosaic is.
            let grab = crate::SPOT_GRAB.lock().expect("spot grab lock").take();
            if let Some(crate::SpotGrab::Size(index)) = grab {
                crate::SPOT_PROPOSE.store(index, std::sync::atomic::Ordering::Relaxed);
            }
            *crate::SPOT_DRAG.lock().expect("spot drag lock") = None;
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
