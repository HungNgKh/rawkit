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
    render::DEFAULT_TILE, Canvas, Frame, Gpu, Output, Presenter, PreviewBlit, PreviewImage,
    Pyramid, Renderer, TileBuffers,
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
    /// The geometry the canvas was last drawn under, so a change to it can clear
    /// what the previous framing left behind.
    geometry: Option<rawkit_editstate::Geometry>,
    /// Where tiles land when the photograph is straightened.
    ///
    /// Tiles are scattered, which stays exact only while every output pixel
    /// falls on exactly one source pixel — so they go here, flat, and a second
    /// pass gathers from it at the angle. Absent while there is no angle, which
    /// is the common case and costs nothing.
    flat: Option<rawkit_engine::Canvas>,
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
            geometry: None,
            flat: None,
        }
    }

    /// Rebuild the presenter for a surface format. Separate from `new` because
    /// the surface is configured after the canvas exists.
    pub fn target(&mut self, gpu: &Gpu, format: wgpu::TextureFormat) {
        self.presenter = Presenter::new(gpu, format);
    }

    /// The same, but correcting for a monitor that is not sRGB.
    pub fn target_with_lut(
        &mut self,
        gpu: &Gpu,
        format: wgpu::TextureFormat,
        lut: &rawkit_export::display::DisplayLut,
    ) {
        self.presenter = Presenter::with_display_lut(gpu, format, lut.entries(), lut.grid());
    }

    /// Point the renderer at a different photograph.
    ///
    /// The buffers are reallocated rather than reused because their size follows
    /// the *profile* — a hue/saturation table's dimensions are a property of the
    /// file — so a second image can need a different shape even though tiles are
    /// always the same. Both caches are dropped: `shown` because the canvas now
    /// holds the previous photograph at the right coordinates, which is the worst
    /// kind of stale, and `uploaded` because the buffer it described is gone.
    pub fn reload(&mut self, gpu: &Gpu, frame: &Frame<'_>) {
        self.buffers = self.renderer.allocate(gpu, frame);
        self.shown = None;
        self.uploaded = None;
    }

    pub fn canvas(&self) -> &Canvas {
        &self.canvas
    }

    pub fn presenter(&self) -> &Presenter {
        &self.presenter
    }

    /// Size the canvas for the current view, and say which resolution level it
    /// is expressed in.
    ///
    /// Shared by both paths on purpose: a cached preview and a tile render put
    /// pixels in the *same* canvas at the *same* scale, so crossing between them
    /// does not rebuild it and does not change what the presenter sees.
    fn fit_canvas(&mut self, gpu: &Gpu, session: &Session, surface: [u32; 2]) -> u8 {
        let viewport = session.viewport();
        let level = viewport.level(session.max_level());
        let scale = viewport.scale * (1u32 << level) as f64;

        // A canvas only ever gets written where the photograph is, so when the
        // photograph gets smaller — a crop, a rotation into a narrower shape —
        // whatever was underneath stays on screen around it. Recreating clears
        // it, which is what the size branch below already relies on.
        let geometry = session.geometry();
        if self.geometry != Some(geometry) {
            self.geometry = Some(geometry);
            self.canvas = self.renderer.create_canvas(gpu, 1, 1);
            self.shown = None;
        }

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
        level
    }

    /// Forget what is on the canvas, so the next frame redraws all of it.
    ///
    /// Wanted by anything drawn *over* the tiles: the canvas is only written
    /// where a tile lands, so an overlay that moves would otherwise leave its
    /// previous position behind.
    pub fn invalidate(&mut self) {
        self.shown = None;
    }

    /// Size the canvas to the surface exactly, for a view that has no zoom.
    ///
    /// The grid is laid out in screen pixels, so the level-based sizing the
    /// loupe uses would make every cell a fractional scale away from what it
    /// asked for.
    pub fn fit_surface(&mut self, gpu: &Gpu, surface: [u32; 2]) {
        let wanted = [surface[0].max(1), surface[1].max(1)];
        if self.canvas.size() != wanted {
            self.canvas = self.renderer.create_canvas(gpu, wanted[0], wanted[1]);
        }
        // Whatever is in the canvas belongs to another view entirely.
        self.shown = None;
        self.uploaded = None;
    }

    /// Fill the canvas from a preview that was rendered earlier, instead of
    /// rendering tiles now.
    ///
    /// `image_size` is the photograph's full resolution, not the preview's: the
    /// viewport is expressed in image pixels, and the preview is a scaled copy of
    /// the same coordinate space. Using the preview's own size here would place
    /// the view by a factor of the reduction, which looks like a photograph that
    /// jumps when it finishes loading.
    pub fn show_preview(
        &mut self,
        gpu: &Gpu,
        blit: &PreviewBlit,
        image: &PreviewImage,
        session: &Session,
        image_size: [u32; 2],
        surface: [u32; 2],
    ) {
        let level = self.fit_canvas(gpu, session, surface);
        let viewport = session.viewport();
        let origin = viewport.image_at([0.0, 0.0]);
        let canvas = self.canvas.size();
        // One canvas pixel is 2^level image pixels, which is what makes the
        // canvas the same shape for both paths.
        let step = (1u32 << level) as f64;
        let (width, height) = (image_size[0] as f64, image_size[1] as f64);

        blit.draw(
            gpu,
            image,
            &self.canvas,
            [(origin[0] / width) as f32, (origin[1] / height) as f32],
            [
                (canvas[0] as f64 * step / width) as f32,
                (canvas[1] as f64 * step / height) as f32,
            ],
        );

        // The canvas now holds a preview, so nothing in it is a rendered tile.
        // Zooming past what the preview covers has to redraw everything.
        self.shown = None;
        self.uploaded = None;
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
        let level = self.fit_canvas(gpu, session, surface);

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

        // The viewport is measured in the *photograph*; tiles are addressed in
        // the sensor's frame, because the mosaic is never rotated — rotating it
        // would move the CFA phase. So each tile's corner is carried across the
        // geometry, and the same geometry tells the blit which way its own axes
        // now point.
        let geometry = session.geometry();
        let axes = geometry.axes();
        let level_size = pyramid
            .level(level)
            .map(|(_, w, h)| [w, h])
            .unwrap_or(session.image_size());

        // Straightening moves where tiles have to land. They go into a flat
        // buffer covering the *preimage* of the view — which a rotation swings
        // wider than the view itself — and a gather turns that into the canvas.
        let straight_origin = [origin[0].floor(), origin[1].floor()];
        let [canvas_w, canvas_h] = self.canvas.size();
        let (target, flat_origin) = if geometry.resamples() {
            let seen = [
                straight_origin[0],
                straight_origin[1],
                straight_origin[0] + canvas_w as f64,
                straight_origin[1] + canvas_h as f64,
            ];
            let flat = geometry.flat_rect(seen, level_size);
            // Room for the filter's own taps at every edge, for the same reason
            // the crop reserves it: a gather at the boundary would otherwise
            // read a clamped row instead of the photograph.
            let margin = 3.0;
            let corner = [(flat[0] - margin).floor(), (flat[1] - margin).floor()];
            let wanted = [
                ((flat[2] + margin - corner[0]).ceil() as u32).max(1),
                ((flat[3] + margin - corner[1]).ceil() as u32).max(1),
            ];
            if self.flat.as_ref().map(|c| c.size()) != Some(wanted) {
                self.flat = Some(self.renderer.create_canvas(gpu, wanted[0], wanted[1]));
            }
            (
                self.flat.as_ref().expect("just created"),
                [corner[0] as f32, corner[1] as f32],
            )
        } else {
            self.flat = None;
            (
                &self.canvas,
                [straight_origin[0] as f32, straight_origin[1] as f32],
            )
        };

        let mut drawn = 0;
        for tile in &tiles {
            let corner =
                geometry.flat_of([tile.x * DEFAULT_TILE, tile.y * DEFAULT_TILE], level_size);
            let dest = [
                (corner[0] - flat_origin[0] as i64) as i32,
                (corner[1] - flat_origin[1] as i64) as i32,
            ];
            self.renderer.draw_tile(
                gpu,
                &self.buffers,
                target,
                pyramid,
                level,
                tile.x,
                tile.y,
                dest,
                axes,
                Output::Display,
            )?;
            session.tile_rendered(*tile, job.generation);
            drawn += 1;
        }
        if geometry.resamples() {
            if let Some(flat) = &self.flat {
                self.renderer.straighten(
                    gpu,
                    flat,
                    &self.canvas,
                    &geometry,
                    rawkit_engine::StraightenView {
                        level_image: level_size,
                        straight_origin: [straight_origin[0] as f32, straight_origin[1] as f32],
                        flat_origin,
                    },
                );
            }
        }
        self.shown = Some(viewport);
        Ok(drawn)
    }
}
