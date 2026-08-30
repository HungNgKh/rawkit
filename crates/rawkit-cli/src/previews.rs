//! Building a library's previews.
//!
//! # Why this is a command and not a background thread
//!
//! It needs a GPU, and the shell's GPU belongs to the render loop on the thread
//! the compositor drives. A CLI run gets its own adapter and its own device with
//! nothing to contend with, which is also why it can be interrupted and resumed
//! by simply running it again — the catalog knows what is already current.
//!
//! # Rendering from the pyramid, not from the full frame
//!
//! A whole-image render of a 24 MP file takes about 1.4 seconds; a 20 000-image
//! library would be eight hours. But a 2560-pixel preview does not need 24
//! megapixels of input. The pyramid already reduces the *mosaic* while keeping
//! its Bayer phase, so rendering the coarsest level that still exceeds the
//! largest wanted size is the same picture for a quarter of the work — the same
//! choice, and the same code, that the canvas makes when you zoom out.
//!
//! What that costs is stated plainly in [`Catalog & Library`]: averaging softens
//! the edge of a blown highlight, so reconstruction can fail to fire where a
//! clipped sample averaged with unclipped neighbours. Previews are looked at,
//! not delivered; an export is always level 0 and never averages.

use anyhow::{Context, Result};
use rawkit_catalog::db::Catalog;
use rawkit_catalog::previews::{self, Level, Preview};
use rawkit_editstate::EditState;
use rawkit_engine::{render::DEFAULT_TILE, BayerPhase, Frame, Gpu, Output, Pyramid, Renderer};
use std::path::Path;

/// JPEG quality for previews. Lower than an export's 92 on purpose: these are
/// looked at on screen at a size that hides the difference, and a library's
/// worth of them is measured in gigabytes.
const PREVIEW_QUALITY: u8 = 85;

/// What a build did.
#[derive(Debug, Default)]
pub struct BuildReport {
    pub images: usize,
    pub written: usize,
    pub bytes: u64,
    /// Files that could not be rendered — not a RAW, unplugged, unsupported
    /// sensor. Counted and carried, never fatal: one bad file must not stop a
    /// library.
    pub failed: Vec<(String, String)>,
}

/// How many photographs to work on at once when the caller does not say.
///
/// Low on purpose, and the constraint is memory rather than cores. One
/// photograph in flight holds its mosaic, its pyramid, the render and the
/// largest resample — roughly 300 MB for a 24 MP frame — so a machine with
/// sixteen cores and sixteen gigabytes would spend all of them on buffers. Four
/// is also close to where this stops helping: about 100 ms of each 630 is on the
/// GPU, which does not divide.
pub fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().min(4))
        .unwrap_or(1)
}

/// Render every preview the catalog is missing or holds a stale copy of.
///
/// # Where the concurrency is, and where it deliberately is not
///
/// Photographs are independent, so each one is decoded, rendered, resampled and
/// encoded on its own thread. **The catalog is not touched from any of them.**
/// Results come back down a channel and this thread writes them, one image at a
/// time, as they arrive — which keeps SQLite single-threaded and keeps a build
/// resumable: interrupt it and everything already finished is already recorded.
///
/// The GPU is shared. `Renderer::run` takes `&self` and allocates its own
/// buffers per call, so concurrent renders do not collide; the device serialises
/// them, which is why more threads stop helping once the GPU is the slowest part.
pub fn build(
    catalog: &Catalog,
    levels: &[Level],
    jobs: usize,
    mut progress: impl FnMut(usize, usize, &str),
) -> Result<BuildReport> {
    let dir =
        previews::directory(catalog).context("an in-memory catalog has nowhere to put previews")?;
    let outstanding = previews::outstanding(catalog, levels)?;
    let mut report = BuildReport {
        images: outstanding.len(),
        ..BuildReport::default()
    };
    if outstanding.is_empty() {
        return Ok(report);
    }

    // One device for the whole run. Creating an adapter per image would cost
    // more than the rendering does.
    let gpu = Gpu::new()?;
    let renderer = Renderer::with_tile_size(&gpu, DEFAULT_TILE);

    // Interleaved per-stage timings from several threads are unreadable and
    // would be worse than none, so asking for them asks for one worker.
    let timing = std::env::var_os("RAWKIT_TIME_PREVIEWS").is_some();
    let workers = if timing { 1 } else { jobs.max(1) }.min(outstanding.len());

    let next = std::sync::atomic::AtomicUsize::new(0);
    let (sender, receiver) = std::sync::mpsc::channel();

    std::thread::scope(|scope| -> Result<()> {
        for _ in 0..workers {
            let sender = sender.clone();
            let (next, outstanding, gpu, renderer, dir) =
                (&next, &outstanding, &gpu, &renderer, &dir);
            scope.spawn(move || loop {
                let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(wanted) = outstanding.get(index) else {
                    break;
                };
                let built = one(
                    gpu,
                    renderer,
                    dir,
                    Path::new(&wanted.path),
                    wanted.image_id,
                    &wanted.state,
                    &wanted.edit_state_hash,
                    &wanted.missing,
                );
                // A closed channel means this thread's work is no longer wanted.
                if sender.send((index, built)).is_err() {
                    break;
                }
            });
        }
        // Dropped so the loop below ends when the last worker finishes.
        drop(sender);

        for (done, (index, built)) in receiver.into_iter().enumerate() {
            let wanted = &outstanding[index];
            progress(done, report.images, &wanted.filename);
            match built {
                Ok(built) => {
                    for preview in &built {
                        previews::record(catalog, wanted.image_id, preview)?;
                        report.bytes += preview.bytes;
                    }
                    report.written += built.len();
                }
                Err(e) => report.failed.push((wanted.filename.clone(), e.to_string())),
            }
        }
        Ok(())
    })?;

    progress(report.images, report.images, "");
    Ok(report)
}

/// Render one photograph's previews, largest first so the smaller ones are made
/// by downsampling what is already in hand rather than by rendering again.
#[allow(clippy::too_many_arguments)]
fn one(
    gpu: &Gpu,
    renderer: &Renderer,
    dir: &Path,
    raw_path: &Path,
    image_id: i64,
    state: &EditState,
    edit_state_hash: &str,
    levels: &[Level],
) -> Result<Vec<Preview>> {
    // Where a build's time actually goes. Set `RAWKIT_TIME_PREVIEWS=1` before
    // optimising anything here — the tile work taught this project that the
    // obvious suspect and the measured one are rarely the same.
    let timing = std::env::var_os("RAWKIT_TIME_PREVIEWS").is_some();
    let mut clock = std::time::Instant::now();
    let lap = |what: &str, clock: &mut std::time::Instant| {
        if timing {
            eprintln!(
                "  {what:<12} {:>7.1} ms",
                clock.elapsed().as_secs_f64() * 1000.0
            );
            *clock = std::time::Instant::now();
        }
    };

    let raw = rawkit_decode::decode_file(raw_path)
        .with_context(|| format!("decoding {}", raw_path.display()))?;
    let phase = BayerPhase::from_cfa(raw.cfa)
        .with_context(|| format!("{:?} is not a Bayer sensor", raw.cfa))?;
    let profile = rawkit_engine::render::profile_for(&raw);
    lap("decode", &mut clock);
    let mosaic = rawkit_engine::normalise(&raw);
    lap("normalise", &mut clock);
    let frame = Frame {
        data: &mosaic,
        width: raw.width,
        height: raw.height,
        phase,
        as_shot_wb: [
            raw.as_shot_neutral[0],
            raw.as_shot_neutral[1],
            raw.as_shot_neutral[2],
        ],
        clip_level: 1.0,
        profile,
    };

    // The largest thing being asked for decides how much detail has to survive.
    let largest = levels
        .iter()
        .map(|l| l.longest_edge().unwrap_or(u32::MAX))
        .max()
        .unwrap_or(0);

    let pyramid = Pyramid::build(&frame, DEFAULT_TILE);
    lap("pyramid", &mut clock);
    let level = coarsest_level(raw.width.max(raw.height), largest, pyramid.levels());
    let (data, width, height) = pyramid
        .level(level)
        .context("the pyramid does not have the level it was just asked for")?;
    // A reduced mosaic is a mosaic: same Bayer phase, same everything else. So
    // the render path is the ordinary one, over a smaller frame.
    let reduced = Frame {
        data,
        width,
        height,
        ..frame
    };
    let developed = renderer.run(gpu, &reduced, state, Output::Display)?;
    lap("render", &mut clock);

    // Largest first, each one resampled from the previous rather than from the
    // full render. A thumbnail derived from a 2560-pixel preview reads a
    // thirtieth as many pixels as one derived from the 3012-pixel render, and
    // successive area averages are the same answer.
    let mut order: Vec<Level> = levels.to_vec();
    order.sort_by_key(|l| std::cmp::Reverse(l.longest_edge().unwrap_or(u32::MAX)));

    // The developed size, which a crop makes different from the reduced frame's.
    let mut source = (developed.pixels, developed.width, developed.height);
    let mut built = Vec::new();
    for wanted in order {
        let edge = wanted.longest_edge().unwrap_or(source.1.max(source.2));
        let (scaled, w, h) = resample(&source.0, source.1, source.2, edge);
        lap("resample", &mut clock);
        let bytes = rawkit_export::encode(
            &scaled,
            w,
            h,
            rawkit_export::Format::Jpeg {
                quality: PREVIEW_QUALITY,
            },
        )?;

        let relative = previews::relative_path(image_id, wanted, edit_state_hash);
        let file = dir.join(&relative);
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&file, &bytes).with_context(|| format!("writing {}", file.display()))?;
        lap("encode+write", &mut clock);

        built.push(Preview {
            level: wanted,
            path: relative,
            edit_state_hash: edit_state_hash.to_string(),
            width: w,
            height: h,
            bytes: bytes.len() as u64,
        });
        source = (scaled, w, h);
    }
    Ok(built)
}

/// Scale to an exact longest edge by averaging the area each output pixel covers.
///
/// The export path downsamples by an integer step, which is right for
/// `--max-dim` ("no larger than this") and wrong here. A 3012-pixel render asked
/// for 2560 has no integer step that fits: the next one down is 2, and the
/// result is **1506** — a preview 41% smaller than the size it is filed under,
/// which is then upscaled on screen and looks soft for a reason nothing records.
/// A 256 thumbnail came out 251 for the same reason.
///
/// Area averaging handles a fractional ratio directly and antialiases while it
/// is at it. Done on the linear values the renderer produced, never on encoded
/// ones — averaging after the transfer function darkens detailed areas, which
/// reads as a preview that does not match its own photograph.
fn resample(rgba: &[f32], width: u32, height: u32, longest_edge: u32) -> (Vec<f32>, u32, u32) {
    let source_longest = width.max(height);
    if longest_edge == 0 || source_longest <= longest_edge {
        return (rgba.to_vec(), width, height);
    }
    let scale = longest_edge as f64 / source_longest as f64;
    let out_w = ((width as f64 * scale).round() as u32).max(1);
    let out_h = ((height as f64 * scale).round() as u32).max(1);

    let mut out = vec![0.0f32; (out_w * out_h * 4) as usize];
    for y in 0..out_h {
        // The half-open source rows this output row covers. Clamped because the
        // last row's right edge lands exactly on the boundary.
        let y0 = (y as f64 * height as f64 / out_h as f64).floor() as u32;
        let y1 = (((y + 1) as f64 * height as f64 / out_h as f64).ceil() as u32).min(height);
        for x in 0..out_w {
            let x0 = (x as f64 * width as f64 / out_w as f64).floor() as u32;
            let x1 = (((x + 1) as f64 * width as f64 / out_w as f64).ceil() as u32).min(width);

            let mut sum = [0.0f64; 4];
            let mut count = 0.0f64;
            for sy in y0..y1.max(y0 + 1) {
                for sx in x0..x1.max(x0 + 1) {
                    let at = ((sy * width + sx) * 4) as usize;
                    for c in 0..4 {
                        sum[c] += rgba[at + c] as f64;
                    }
                    count += 1.0;
                }
            }
            let at = ((y * out_w + x) * 4) as usize;
            for c in 0..4 {
                out[at + c] = (sum[c] / count) as f32;
            }
        }
    }
    (out, out_w, out_h)
}

/// The coarsest pyramid level whose longest edge still covers `wanted`.
///
/// Reducing below the target would render a preview smaller than it claims to
/// be, so this stops one level early — the preview is downsampled from
/// *more* detail than it needs, never from less.
fn coarsest_level(longest_edge: u32, wanted: u32, available: u8) -> u8 {
    let mut level = 0;
    while level < available && (longest_edge >> (level + 1)) >= wanted {
        level += 1;
    }
    level
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preview_is_rendered_from_more_detail_than_it_needs() {
        // 6024 wide. A 2560 preview can come from level 1 (3012) but not from
        // level 2 (1506), which would be an upscale wearing a preview's name.
        assert_eq!(coarsest_level(6024, 2560, 5), 1);
        assert_eq!(coarsest_level(6024, 1024, 5), 2); // 1506 covers it, 753 does not
        assert_eq!(coarsest_level(6024, 256, 5), 4); // 376 covers it, 188 does not
    }

    #[test]
    fn a_pyramid_that_does_not_go_that_deep_is_not_pretended_into_existence() {
        // The engine stops reducing once a level fits in one tile, so asking for
        // a very small preview of a small image must not run off the end.
        assert_eq!(coarsest_level(6024, 16, 2), 2);
        assert_eq!(coarsest_level(512, 256, 0), 0);
    }

    #[test]
    fn a_preview_is_exactly_the_size_it_is_filed_under() {
        // The bug this replaced: an integer step could only reach 1506 from
        // 3012 when asked for 2560, and the row said "standard" either way.
        let (_, w, h) = resample(&vec![0.5; (3012 * 2012 * 4) as usize], 3012, 2012, 2560);
        assert_eq!((w, h), (2560, 1710));

        let (_, w, h) = resample(&vec![0.5; (3012 * 2012 * 4) as usize], 3012, 2012, 256);
        assert_eq!((w, h), (256, 171));
    }

    #[test]
    fn a_portrait_frame_keeps_its_shape() {
        // "Longest edge" has to mean the same thing whichever way up the camera
        // was, or a grid of mixed orientations has two different thumbnail sizes.
        let (_, w, h) = resample(&vec![0.5; (2000 * 3000 * 4) as usize], 2000, 3000, 300);
        assert_eq!((w, h), (200, 300));
    }

    #[test]
    fn resampling_averages_rather_than_picking() {
        // A checkerboard has to come out mid-grey. Nearest-neighbour would
        // return one of the two source values, which is what aliasing looks
        // like on a grid of thumbnails.
        let (w, h) = (4u32, 4u32);
        let mut src = vec![0.0f32; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let value = if (x + y) % 2 == 0 { 1.0 } else { 0.0 };
                let at = ((y * w + x) * 4) as usize;
                src[at..at + 4].copy_from_slice(&[value, value, value, 1.0]);
            }
        }
        let (out, ow, oh) = resample(&src, w, h, 2);
        assert_eq!((ow, oh), (2, 2));
        assert!(
            out.chunks(4).all(|p| (p[0] - 0.5).abs() < 1e-6),
            "expected mid-grey everywhere, got {out:?}"
        );
    }

    #[test]
    fn an_image_smaller_than_the_target_is_left_alone() {
        // Never upscale. A preview larger than its source is bytes spent on
        // nothing, and it would claim detail that was never recorded.
        let src = vec![0.25f32; (100 * 80 * 4) as usize];
        let (out, w, h) = resample(&src, 100, 80, 256);
        assert_eq!((w, h), (100, 80));
        assert_eq!(out.len(), src.len());
    }

    #[test]
    fn a_full_size_request_uses_the_original() {
        // `OneToOne` has no longest edge, so nothing may be reduced away.
        assert_eq!(coarsest_level(6024, u32::MAX, 5), 0);
    }
}
