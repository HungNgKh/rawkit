//! Does a twenty-thousand-image grid scroll on JPEG previews, or does it need
//! GPU-resident compressed tiles?
//!
//! This is the open item in [[Catalog & Library]], and it is deliberately
//! answered by measurement rather than by choosing: *"decide by profiling a 20k
//! image grid scroll"*. BCn tiles are cheaper to hand to the GPU and cost a
//! transcode, a format decision and a second code path, so they are worth it only
//! if the simple thing does not keep up.
//!
//! # What is simulated
//!
//! A grid scrolling at speed, frame by frame at 60 Hz. Each frame works out
//! which cells are visible, fetches the ones that are not already resident —
//! **read the file, decode the JPEG, upload a texture** — and evicts the least
//! recently used. What is reported is the per-frame cost, because a scroll is
//! judged by its worst frame and not by its average.
//!
//! # What is not simulated
//!
//! Drawing the cells. That is a textured quad each and the canvas already draws
//! one per frame in a fraction of a millisecond; the question here is whether
//! the *supply* of thumbnails keeps up, which is the part that touches disk and
//! the JPEG decoder.
//!
//! A texture per thumbnail is also the naive arrangement — a real grid would
//! pack them into an atlas — so the upload figure here is an upper bound.
//!
//! `cargo test -p rawkit-cli --test grid_scroll -- --ignored --nocapture`

use rawkit_engine::{Gpu, PreviewBlit, Renderer};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// The library size the design names.
const IMAGES: usize = 20_000;
/// A physical-pixel canvas the size of this monitor's, which is where the
/// question actually gets asked.
const CANVAS: (u32, u32) = (2400, 1350);
/// A hard flick, in physical pixels per second. Faster than anyone sustains, so
/// the answer has room in it.
const SCROLL_SPEED: f64 = 3000.0;
const FRAMES: usize = 180;
const FRAME_BUDGET_MS: f64 = 16.6;

fn fixture() -> Option<PathBuf> {
    let dir = std::env::var("RAWKIT_FIXTURES")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("rawkit-fixtures")
        });
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("arw")))
}

/// One real preview, so the file size and the entropy are a photograph's rather
/// than a gradient's — a smooth synthetic image compresses several times smaller
/// and decodes faster, which would answer a different question.
fn real_preview(gpu: &Gpu, raw: &Path, longest_edge: u32) -> (Vec<u8>, u32, u32) {
    let decoded = rawkit_decode::decode_file(raw).expect("decode the fixture");
    let phase = rawkit_engine::BayerPhase::from_cfa(decoded.cfa).expect("bayer");
    let mosaic = rawkit_engine::normalise(&decoded);
    let frame = rawkit_engine::Frame {
        data: &mosaic,
        width: decoded.width,
        height: decoded.height,
        phase,
        as_shot_wb: [
            decoded.as_shot_neutral[0],
            decoded.as_shot_neutral[1],
            decoded.as_shot_neutral[2],
        ],
        clip_level: 1.0,
        profile: rawkit_engine::CameraProfile::from_color_matrix(rawkit_engine::profile::IDENTITY),
    };

    // Render a coarse pyramid level — the same choice the preview builder makes,
    // and it keeps this setup to a couple of seconds.
    let pyramid = rawkit_engine::Pyramid::build(&frame, rawkit_engine::render::DEFAULT_TILE);
    let mut level = 0;
    while level < pyramid.levels()
        && (decoded.width.max(decoded.height) >> (level + 1)) >= longest_edge
    {
        level += 1;
    }
    let (data, width, height) = pyramid.level(level).expect("level");
    let small = rawkit_engine::Frame {
        data,
        width,
        height,
        ..frame
    };
    let renderer = Renderer::new(gpu);
    let rgba = renderer
        .run(
            gpu,
            &small,
            &rawkit_editstate::EditState::default(),
            rawkit_engine::Output::Display,
        )
        .expect("render")
        .pixels;

    // Area-averaged to the exact target. A local copy of what the preview
    // builder does, because this is a fixture generator and the alternative is
    // making an internal of the binary public for a benchmark's sake. Exactness
    // matters here: an integer step lands on 376 when asked for 256, and then
    // the thing being measured is not the size a grid actually reads.
    let scale = longest_edge as f64 / width.max(height) as f64;
    let (ow, oh) = (
        ((width as f64 * scale).round() as u32).max(1),
        ((height as f64 * scale).round() as u32).max(1),
    );
    let mut out = vec![0.0f32; (ow * oh * 4) as usize];
    for y in 0..oh {
        let y0 = (y as f64 * height as f64 / oh as f64).floor() as u32;
        let y1 =
            ((((y + 1) as f64 * height as f64 / oh as f64).ceil() as u32).min(height)).max(y0 + 1);
        for x in 0..ow {
            let x0 = (x as f64 * width as f64 / ow as f64).floor() as u32;
            let x1 = ((((x + 1) as f64 * width as f64 / ow as f64).ceil() as u32).min(width))
                .max(x0 + 1);
            let mut sum = [0.0f64; 4];
            let mut count = 0.0;
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let at = ((sy * width + sx) * 4) as usize;
                    for c in 0..4 {
                        sum[c] += rgba[at + c] as f64;
                    }
                    count += 1.0;
                }
            }
            let at = ((y * ow + x) * 4) as usize;
            for c in 0..4 {
                out[at + c] = (sum[c] / count) as f32;
            }
        }
    }
    let jpeg = rawkit_export::encode(&out, ow, oh, rawkit_export::Format::Jpeg { quality: 85 })
        .expect("encode");
    (jpeg, ow, oh)
}

/// Least-recently-used, the design's *"decoded LRU cache in RAM"*, except the
/// decoded copy lives on the GPU where it is about to be drawn from.
struct Cache {
    resident: HashMap<usize, (rawkit_engine::PreviewImage, u64)>,
    clock: u64,
    capacity: usize,
}

impl Cache {
    fn touch(&mut self, index: usize) -> bool {
        self.clock += 1;
        let clock = self.clock;
        match self.resident.get_mut(&index) {
            Some((_, last)) => {
                *last = clock;
                true
            }
            None => false,
        }
    }

    fn insert(&mut self, index: usize, image: rawkit_engine::PreviewImage) {
        self.clock += 1;
        self.resident.insert(index, (image, self.clock));
        while self.resident.len() > self.capacity {
            let oldest = self
                .resident
                .iter()
                .min_by_key(|(_, (_, last))| *last)
                .map(|(i, _)| *i)
                .expect("not empty");
            self.resident.remove(&oldest);
        }
    }
}

struct Outcome {
    label: &'static str,
    /// The cell edge in physical pixels, which is what sets the demand rate.
    cell: u32,
    pixels: (u32, u32),
    bytes_each: usize,
    loaded: usize,
    mean_ms: f64,
    worst_ms: f64,
    over_budget: usize,
    thumbs_per_second: f64,
    demand_per_second: f64,
    /// Which frame was the worst. If it is the first one, the cost is filling an
    /// empty grid rather than scrolling it, and the fix is a placeholder rather
    /// than a format.
    worst_frame: usize,
    /// What the cache holds when full, in megabytes of decoded RGBA.
    resident_mb: f64,
    /// Where a fetch's time goes. The split that decides the format question:
    /// compressed GPU tiles remove the **decode** and nothing else, so if decode
    /// is not the expensive part then the format is not the answer.
    read_ms: f64,
    decode_ms: f64,
    upload_ms: f64,
}

fn scroll(
    gpu: &Gpu,
    blit: &PreviewBlit,
    dir: &Path,
    label: &'static str,
    cell: u32,
    pixels: (u32, u32),
    bytes_each: usize,
) -> Outcome {
    let columns = (CANVAS.0 / cell).max(1) as usize;
    let rows_visible = (CANVAS.1 / cell) as usize + 2;
    // Twice what is on screen, so a reversal of direction is a cache hit.
    let capacity = columns * rows_visible * 2;
    let mut cache = Cache {
        resident: HashMap::new(),
        clock: 0,
        capacity,
    };

    let mut per_frame = Vec::with_capacity(FRAMES);
    let mut loaded = 0usize;
    let (mut read_ms, mut decode_ms, mut upload_ms) = (0.0f64, 0.0f64, 0.0f64);
    let started = Instant::now();

    for frame in 0..FRAMES {
        let began = Instant::now();
        let offset = SCROLL_SPEED * frame as f64 / 60.0;
        let first_row = (offset / cell as f64) as usize;

        for row in first_row..first_row + rows_visible {
            for column in 0..columns {
                let index = (row * columns + column) % IMAGES;
                if cache.touch(index) {
                    continue;
                }
                let path = dir.join(format!("{:02x}/{index}.jpg", index % 256));
                let at = Instant::now();
                let bytes = std::fs::read(&path).expect("a preview that was written");
                read_ms += at.elapsed().as_secs_f64() * 1000.0;

                let at = Instant::now();
                let (rgba, width, height) = rawkit_export::decode(&bytes).expect("decode");
                decode_ms += at.elapsed().as_secs_f64() * 1000.0;

                let at = Instant::now();
                let image = blit.upload(gpu, &rgba, width, height).expect("upload");
                upload_ms += at.elapsed().as_secs_f64() * 1000.0;

                cache.insert(index, image);
                loaded += 1;
            }
        }
        per_frame.push(began.elapsed().as_secs_f64() * 1000.0);
    }

    let elapsed = started.elapsed().as_secs_f64();
    let worst = per_frame.iter().cloned().fold(0.0f64, f64::max);
    Outcome {
        label,
        cell,
        pixels,
        bytes_each,
        loaded,
        mean_ms: per_frame.iter().sum::<f64>() / per_frame.len() as f64,
        worst_ms: worst,
        over_budget: per_frame.iter().filter(|ms| **ms > FRAME_BUDGET_MS).count(),
        thumbs_per_second: loaded as f64 / elapsed,
        worst_frame: per_frame
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(i, _)| i)
            .unwrap_or(0),
        resident_mb: capacity as f64 * (pixels.0 as f64 * pixels.1 as f64 * 4.0) / 1_048_576.0,
        // What a scroll at this speed asks for: one new row of cells every time
        // the view moves one cell height.
        demand_per_second: SCROLL_SPEED / cell as f64 * columns as f64,
        read_ms,
        decode_ms,
        upload_ms,
    }
}

#[test]
#[ignore = "requires a GPU adapter and a RAW fixture"]
fn a_twenty_thousand_image_grid_scrolls_on_jpeg_previews() {
    let Ok(gpu) = Gpu::new() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let Some(raw) = fixture() else {
        panic!("no .ARW fixture; set RAWKIT_FIXTURES");
    };
    let blit = PreviewBlit::new(&gpu);

    let root = std::env::temp_dir().join(format!("rawkit-grid-{}", std::process::id()));
    let mut outcomes = Vec::new();

    // Two densities, because the answer depends on which one a grid uses and the
    // design does not say. A contact sheet of 256-pixel cells, and the large
    // cells a 2x display needs if you want six across.
    for (label, cell, edge) in [("thumb", 256u32, 256u32), ("small", 400, 1024)] {
        let dir = root.join(label);
        let (jpeg, width, height) = real_preview(&gpu, &raw, edge);
        println!(
            "{label:<6}: {width}x{height} previews, {} KB each, writing {IMAGES}...",
            jpeg.len() / 1024
        );
        // Distinct files so nothing is served from one page-cache entry, and
        // sharded the way the catalog shards them.
        for index in 0..IMAGES {
            let shard = dir.join(format!("{:02x}", index % 256));
            std::fs::create_dir_all(&shard).expect("shard");
            std::fs::write(shard.join(format!("{index}.jpg")), &jpeg).expect("write");
        }
        outcomes.push(scroll(
            &gpu,
            &blit,
            &dir,
            label,
            cell,
            (width, height),
            jpeg.len(),
        ));
    }
    let _ = std::fs::remove_dir_all(&root);

    println!();
    println!(
        "{:<7} {:>7} {:>10} {:>8} {:>9} {:>9} {:>6} {:>9} {:>9}",
        "level",
        "cell px",
        "pixels",
        "KB each",
        "mean ms",
        "worst ms",
        "over",
        "supply/s",
        "demand/s"
    );
    for o in &outcomes {
        println!(
            "{:<7} {:>7} {:>10} {:>8} {:>9.2} {:>9.2} {:>6} {:>9.0} {:>9.0}",
            o.label,
            o.cell,
            format!("{}x{}", o.pixels.0, o.pixels.1),
            o.bytes_each / 1024,
            o.mean_ms,
            o.worst_ms,
            o.over_budget,
            o.thumbs_per_second,
            o.demand_per_second,
        );
    }

    println!();
    for o in &outcomes {
        println!(
            "{:<7} worst frame was #{} of {FRAMES}; cache holds {:.0} MB when full",
            o.label, o.worst_frame, o.resident_mb
        );
    }

    println!();
    println!(
        "{:<7} {:>10} {:>10} {:>10}   where a fetch goes, per thumbnail",
        "level", "read ms", "decode ms", "upload ms"
    );
    for o in &outcomes {
        let each = o.loaded as f64;
        println!(
            "{:<7} {:>10.3} {:>10.3} {:>10.3}",
            o.label,
            o.read_ms / each,
            o.decode_ms / each,
            o.upload_ms / each
        );
    }

    // No pass/fail threshold: this exists to produce the numbers a format
    // decision is made from, and a threshold would turn a measurement into an
    // opinion. It does assert that the scroll actually did some work — more than
    // a screenful of cells had to be fetched, or nothing was measured.
    for o in &outcomes {
        assert!(
            o.loaded > 100,
            "{} loaded only {} thumbnails; the scroll did not exercise anything",
            o.label,
            o.loaded
        );
    }
}
