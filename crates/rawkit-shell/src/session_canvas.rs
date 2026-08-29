//! The render loop: a [`Session`] decides, the engine draws, the canvas shows.
//!
//! This is the shell's half of the command bus. The session holds no pixels and
//! no GPU handle; everything here does. Between them the rule holds mechanically
//! rather than by intention — the page can move a slider and cannot touch a
//! frame.
//!
//! # The geometry, which is the only subtle part
//!
//! [`Viewport::level`] picks `floor(log2(1/scale))`, so a level pixel is between
//! half a screen pixel and one. The canvas is therefore sized in **level**
//! pixels — up to twice the surface — and the presenter scales it down. Sizing
//! it in screen pixels instead and drawing tiles 1:1 would show a crop at the
//! wrong magnification everywhere except exact powers of two.
//!
//! Tile positions come from the image, not the view: tile `(level, tx, ty)`
//! begins at `tx * TILE` in level pixels regardless of where the viewport is, so
//! its canvas destination is that minus the viewport's origin — routinely
//! negative, which is why destinations are signed.

use anyhow::Result;
use rawkit_editstate::EditState;
use rawkit_engine::{
    render::DEFAULT_TILE, Canvas, Frame, Gpu, Output, Presenter, Pyramid, Renderer, TileBuffers,
};
use rawkit_session::{Session, TileId, Viewport};

/// Everything needed to turn a session's decisions into pixels.
pub struct CanvasRenderer {
    renderer: Renderer,
    presenter: Presenter,
    buffers: TileBuffers,
    canvas: Canvas,
    /// The viewport the canvas currently shows. When this changes, every tile in
    /// the canvas is in the wrong place even if it is still fresh for the edit —
    /// so the canvas is redrawn wholesale rather than patched.
    ///
    /// That makes a pan cost a full redraw, which the session's tile freshness
    /// was designed to avoid. Fixing it properly means caching a texture per
    /// tile and compositing them per frame; a single flat canvas cannot express
    /// "these pixels are valid but somewhere else".
    shown: Option<Viewport>,
    /// The edit the uniform currently holds. Uploading it is cheap but not free,
    /// and it changes far less often than tiles are drawn.
    uploaded: Option<EditState>,
}

impl CanvasRenderer {
    pub fn new(gpu: &Gpu, frame: &Frame<'_>, surface: [u32; 2]) -> Self {
        let renderer = Renderer::new(gpu);
        let buffers = renderer.allocate(gpu, frame);
        let canvas = renderer.create_canvas(gpu, surface[0].max(1), surface[1].max(1));
        Self {
            presenter: Presenter::new(gpu, rawkit_engine::CANVAS_FORMAT),
            renderer,
            buffers,
            canvas,
            shown: None,
            uploaded: None,
        }
    }

    /// Rebuild the presenter for a surface format. Separate from `new` because
    /// the surface is configured after the canvas exists.
    pub fn target(&mut self, gpu: &Gpu, format: wgpu::TextureFormat) {
        self.presenter = Presenter::new(gpu, format);
    }

    pub fn canvas(&self) -> &Canvas {
        &self.canvas
    }

    pub fn presenter(&self) -> &Presenter {
        &self.presenter
    }

    /// Draw whatever the session says is missing. Returns how many tiles were
    /// drawn, which is the honest measure of what a frame cost.
    pub fn advance(
        &mut self,
        gpu: &Gpu,
        session: &mut Session,
        frame: &Frame<'_>,
        pyramid: &Pyramid<'_>,
        surface: [u32; 2],
    ) -> Result<usize> {
        let viewport = session.viewport();
        let level = viewport.level(session.max_level());
        let scale = viewport.scale * (1u32 << level) as f64;

        // Canvas in level pixels, so tiles land 1:1 in it and the presenter does
        // the fractional part.
        let wanted = [
            ((surface[0] as f64 / scale).ceil() as u32).max(1),
            ((surface[1] as f64 / scale).ceil() as u32).max(1),
        ];
        if self.canvas.size() != wanted {
            self.canvas = self.renderer.create_canvas(gpu, wanted[0], wanted[1]);
            self.shown = None;
        }

        let moved = self.shown != Some(viewport);
        let job = session.pending_work();
        let tiles: Vec<TileId> = if moved {
            // The view changed, so nothing already in the canvas is where it
            // should be. Fresh-for-the-edit is not the same as in-the-right-place.
            session.visible_tiles(level)
        } else {
            job.tiles.clone()
        };
        if tiles.is_empty() {
            self.shown = Some(viewport);
            return Ok(0);
        }

        if self.uploaded.as_ref() != Some(&job.state) {
            self.renderer
                .set_edit(gpu, &self.buffers, frame, &job.state)?;
            self.uploaded = Some(job.state.clone());
        }

        // The viewport's top-left in level pixels. Tiles are placed relative to
        // it, which is where the negative destinations come from.
        let origin = viewport.image_at([0.0, 0.0]);
        let divisor = (1u32 << level) as f64;
        let origin = [origin[0] / divisor, origin[1] / divisor];

        let span = DEFAULT_TILE as i32;
        let mut drawn = 0;
        for tile in &tiles {
            let dest = [
                (tile.x as i32 * span) - origin[0].floor() as i32,
                (tile.y as i32 * span) - origin[1].floor() as i32,
            ];
            self.renderer.draw_tile(
                gpu,
                &self.buffers,
                &self.canvas,
                pyramid,
                level,
                tile.x,
                tile.y,
                dest,
                Output::Display,
            )?;
            session.tile_rendered(*tile, job.generation);
            drawn += 1;
        }
        self.shown = Some(viewport);
        Ok(drawn)
    }
}
