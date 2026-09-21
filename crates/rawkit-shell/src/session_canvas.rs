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
    render::DEFAULT_TILE, Canvas, Frame, Gpu, Output, Overlay, Presenter, PreviewBlit,
    PreviewImage, Pyramid, Renderer, TileBuffers,
};
use rawkit_session::{Session, TileId, Viewport};

/// Everything needed to turn a session's decisions into pixels.
pub struct CanvasRenderer {
    renderer: Renderer,
    presenter: Presenter,
    /// What shows round the photograph, kept here as well as in the presenter:
    /// a presenter rebuilt for a new format or a monitor profile starts from the
    /// default, and this is what it is told again.
    surround: [f32; 3],
    buffers: TileBuffers,
    canvas: Canvas,
    /// What the interface draws over the photograph, in a layer of its own.
    ///
    /// Always the same size as `canvas` — created with it, by the one method
    /// that is allowed to replace either — because the presenter samples both
    /// with a single set of coordinates. See [`rawkit_engine::Overlay`] for why
    /// it is not simply drawn into the canvas, which is what it used to be.
    overlay: Overlay,
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
        let (width, height) = (surface[0].max(1), surface[1].max(1));
        let canvas = renderer.create_canvas(gpu, width, height);
        let overlay = renderer.create_overlay(gpu, width, height);
        Self {
            presenter: Presenter::new(gpu, rawkit_engine::CANVAS_FORMAT),
            surround: rawkit_engine::present::DEFAULT_SURROUND,
            renderer,
            buffers,
            canvas,
            overlay,
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
        self.presenter.set_surround(gpu, self.surround);
    }

    /// Change what shows round the photograph, now and after any rebuild.
    pub fn set_surround(&mut self, gpu: &Gpu, linear: [f32; 3]) {
        self.surround = linear;
        self.presenter.set_surround(gpu, linear);
    }

    /// The same, but correcting for a monitor that is not sRGB.
    pub fn target_with_lut(
        &mut self,
        gpu: &Gpu,
        format: wgpu::TextureFormat,
        lut: &rawkit_export::display::DisplayLut,
    ) {
        self.presenter = Presenter::with_display_lut(gpu, format, lut.entries(), lut.grid());
        self.presenter.set_surround(gpu, self.surround);
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

    /// The whole photograph, developed at its coarsest resolution.
    ///
    /// Deliberately not read back from the canvas, which holds whatever the
    /// viewport is showing: a histogram that changed shape when you panned would
    /// be describing the window rather than the photograph, and one taken at the
    /// current zoom would change its own answer as you worked.
    ///
    /// The coarsest pyramid level is one tile by construction — that is what
    /// "bottoms out when a tile covers the image" means — so this costs about
    /// one tile, and it goes through [`Renderer::run`], which is the export's
    /// own path. Crop, orientation and straightening are therefore applied by
    /// the same code that applies them to a file, rather than by a second
    /// implementation that could disagree with it about what is in the frame.
    pub fn survey(
        &self,
        gpu: &Gpu,
        frame: &Frame<'_>,
        pyramid: &Pyramid<'_>,
        state: &EditState,
    ) -> Result<rawkit_engine::Rendered> {
        // `levels()` is the coarsest level, not how many there are — reading it
        // as a count surveys one octave too fine, which is four times the pixels
        // and, measured, four times the cost.
        let (data, width, height) = pyramid
            .level(pyramid.levels())
            .ok_or_else(|| anyhow::anyhow!("a pyramid with no levels"))?;
        // A reduced mosaic is a mosaic: same phase, same profile, same clip
        // level. Only the pixel count changes.
        let level = Frame {
            data,
            width,
            height,
            phase: frame.phase,
            as_shot_wb: frame.as_shot_wb,
            clip_level: frame.clip_level,
            profile: frame.profile.clone(),
            recorded_orientation: frame.recorded_orientation,
        };
        Ok(self.renderer.run(gpu, &level, state, Output::Display)?)
    }

    /// Measure the lens's lateral chromatic aberration on this frame.
    ///
    /// Full resolution and not the pyramid, which is the one thing this does
    /// differently from [`CanvasRenderer::survey`] beside it. A survey wants a
    /// histogram, and a histogram of a reduced frame is the same histogram; this
    /// wants a hundredth of a pixel, and reducing the frame halves that while
    /// leaving the sensor noise where it was. So it costs a whole render, which
    /// is why it happens when somebody presses a button rather than on open.
    pub fn measure_lens(&self, gpu: &Gpu, frame: &Frame<'_>) -> Result<rawkit_editstate::Lens> {
        Ok(rawkit_engine::aberration::measure(
            gpu,
            &self.renderer,
            frame,
        )?)
    }

    /// Paint where one adjustment reaches, over the canvas.
    ///
    /// `straight_origin` is where the canvas's top-left pixel sits in the
    /// straightened photograph — the same number the straighten gather is given,
    /// and the render loop is the only place that has it.
    pub fn overlay_mask(
        &self,
        gpu: &Gpu,
        geometry: &rawkit_editstate::Geometry,
        image: [u32; 2],
        view: rawkit_engine::MaskOverlay,
    ) {
        self.renderer
            .overlay_mask(gpu, &self.buffers, &self.overlay, geometry, image, view);
    }

    pub fn canvas(&self) -> &Canvas {
        &self.canvas
    }

    /// The layer everything drawn *over* the photograph goes into.
    pub fn overlay(&self) -> &Overlay {
        &self.overlay
    }

    /// Empty the overlay, once at the top of a frame.
    ///
    /// This is what replaced the bookkeeping. Nothing has to work out whether an
    /// outline has moved since last time, because last time is gone: the layer
    /// is cleared and whatever is still true is drawn again, at the cost of one
    /// screen-sized write and no tile work at all.
    pub fn clear_overlay(&self, gpu: &Gpu) {
        self.overlay.clear(gpu);
    }

    /// Replace the canvas, and the overlay with it.
    ///
    /// The only place either is created after construction. Separately would be
    /// two chances to resize one and forget the other, which the presenter
    /// refuses — correctly, and at the worst possible moment.
    fn resize(&mut self, gpu: &Gpu, width: u32, height: u32) {
        self.canvas = self.renderer.create_canvas(gpu, width, height);
        self.overlay = self.renderer.create_overlay(gpu, width, height);
        self.shown = None;
    }

    /// A canvas of its own for something drawn beside the photograph — the
    /// filmstrip — from the same renderer, so it presents through the same
    /// output transform.
    pub fn create_canvas(&self, gpu: &Gpu, width: u32, height: u32) -> Canvas {
        self.renderer
            .create_canvas(gpu, width.max(1), height.max(1))
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
        // The session's, not this crate's arithmetic: the tile job is chosen at
        // the same level, and a canvas sized from a second computation of it is
        // one refactor away from disagreeing with the tiles that go into it.
        let level = session.level();
        let scale = viewport.scale * (1u32 << level) as f64;

        // A canvas only ever gets written where the photograph is, so when the
        // photograph gets smaller — a crop, a rotation into a narrower shape —
        // whatever was underneath stays on screen around it. Recreating clears
        // it, which is what the size branch below already relies on.
        let geometry = session.geometry();
        if self.geometry != Some(geometry) {
            self.geometry = Some(geometry);
            self.resize(gpu, 1, 1);
        }

        // Canvas in level pixels, so tiles land 1:1 in it and the presenter does
        // the fractional part.
        //
        // Clamped to what the device will actually allocate. The session refuses
        // to zoom out past fit, which is what keeps this sane for an ordinary
        // frame — but "ordinary" is doing work in that sentence: a large enough
        // sensor in a small enough window reaches the limit at fit itself, and
        // asking for a texture wider than the device allows is not a bad frame,
        // it is a dead process. A clamped canvas merely shows the photograph
        // slightly softer than it could be.
        let largest = gpu.device.limits().max_texture_dimension_2d;
        let wanted = [
            ((surface[0] as f64 / scale).ceil() as u32).clamp(1, largest),
            ((surface[1] as f64 / scale).ceil() as u32).clamp(1, largest),
        ];
        if self.canvas.size() != wanted {
            self.resize(gpu, wanted[0], wanted[1]);
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
            self.resize(gpu, wanted[0], wanted[1]);
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
