//! `rawkit` — the headless face of the engine.
//!
//! The GUI is the product, but the CLI is what CI, the golden harness and the
//! (later) Python lab talk to. Anything the app can do without a window belongs
//! here first: it makes the behaviour scriptable and, more usefully, testable on
//! three operating systems without a display attached.

mod previews;
mod render;

use anyhow::Result;
use clap::{Parser, Subcommand};
use rawkit_catalog::previews::Level as PreviewLevel;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "rawkit", version, about = "RAW editor — headless tools")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the `EditState` JSON Schema.
    ///
    /// Shared artifact #1: serde defines it here, and everything else — the
    /// Python lab in v2, any external tooling — derives from this output rather
    /// than from a second hand-written copy that can drift.
    Schema,

    /// Print the render pipeline, stage by stage, with each stage's light
    /// domain. The order is a compile-time fact in `rawkit-engine`; this prints
    /// it so a human can check it against the design without reading Rust.
    Stages,

    /// Decode, demosaic, develop and write a colour-managed image.
    ///
    /// The whole chain on real sensor data: decode, RCD demosaic, white
    /// balance, highlight reconstruction, camera profile, exposure, tone map,
    /// output transform. A `.jpg` or `.png` carries an ICC profile; a `.ppm`
    /// cannot, and is for looking at intermediate results rather than for
    /// anything that leaves the machine.
    ///
    /// Colour is only as good as the profile: without `--profile` it uses the
    /// decoder's single-illuminant matrix, which is defensible and not accurate.
    Render {
        /// RAW file to read.
        input: PathBuf,
        /// Where to write the image. The extension picks the format.
        #[arg(short, long, default_value = "render.ppm")]
        output: PathBuf,
        /// Box-downsample so the longest edge is at most this many pixels.
        /// 0 writes full resolution, which for a 24 MP sensor is a 72 MB file.
        #[arg(long, default_value_t = 2000)]
        max_dim: u32,
        /// Tile edge in pixels. Exposed for benchmarking and for proving that
        /// the choice does not change the output.
        #[arg(long, default_value_t = rawkit_engine::render::DEFAULT_TILE)]
        tile: u32,
        /// A `.dcp` camera profile to render with, instead of the decoder's
        /// built-in single-illuminant matrix.
        ///
        /// Point this at a profile you already have; none are bundled, and none
        /// can be — Adobe's are not redistributable.
        #[arg(long)]
        profile: Option<PathBuf>,
        /// Exposure in stops, applied in scene-linear light before the tone map.
        ///
        /// The `signal` line this command prints says how far the frame sits
        /// from clipping, which is the number to set this from: a file peaking
        /// at -1.2 EV has that much room before anything blows.
        #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
        exposure: f32,
    },

    /// Report the GPU adapter this machine would render on.
    ///
    /// Deliberately trivial and deliberately present from day one: the v1
    /// invariant is that all three OSes render identically, so knowing exactly
    /// which backend and driver produced a given render is the first thing any
    /// cross-platform divergence report needs.
    Gpu,

    /// Open or create a catalog, and report what it is.
    ///
    /// Creating one applies every migration, takes a backup on close, and
    /// refuses the file outright if it fails SQLite's integrity check — so this
    /// is also how to check that a catalog is sound before trusting it.
    Catalog {
        /// The catalog file. Created if it does not exist.
        path: PathBuf,
        /// Index every supported file under this folder, and flag anything
        /// previously catalogued there that has gone.
        ///
        /// Records size, modification time, and the camera, lens and capture
        /// time from each file's header. It does not hash, which is what keeps
        /// an import bounded by reading headers rather than whole files — a
        /// two-hundredfold difference per photograph. See `--hash`.
        #[arg(long)]
        scan: Option<PathBuf>,
        /// Fill in the content hashes a scan left empty.
        ///
        /// This is the deferred cost, made explicit: it reads every catalogued
        /// file once. Minutes on an internal disk, considerably longer on an
        /// external one.
        #[arg(long)]
        hash: bool,
        /// Render the previews a grid and a filmstrip need, into a directory
        /// beside the catalog.
        ///
        /// Only what is missing or stale, so interrupting it and running it
        /// again picks up where it stopped. An edit makes every preview of that
        /// photograph stale, which is what keeps a grid honest.
        #[arg(long)]
        previews: bool,
        /// How many photographs to build previews for at once.
        ///
        /// Bounded by memory rather than by cores: one photograph in flight
        /// holds about 300 MB for a 24 MP frame. Above four this stops helping
        /// anyway, because the GPU share of the work does not divide.
        #[arg(long, default_value_t = previews::default_jobs())]
        jobs: usize,
        /// Delete preview files the catalog no longer refers to.
        ///
        /// Regenerating after an edit writes a new file and leaves the old one,
        /// so a library that is edited often accumulates orphans.
        #[arg(long)]
        sweep: bool,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Schema => {
            let schema = rawkit_editstate::EditState::json_schema();
            println!("{}", serde_json::to_string_pretty(&schema)?);
        }
        Command::Stages => {
            for (i, stage) in rawkit_engine::Stage::ALL.iter().enumerate() {
                let preview = if stage.runs_in_preview() {
                    "preview+export"
                } else {
                    "export only"
                };
                println!(
                    "{:>2}. {:<20} {:<16} {}",
                    i + 1,
                    format!("{stage:?}"),
                    format!("{:?}", stage.domain()),
                    preview
                );
            }
        }
        Command::Render {
            input,
            output,
            max_dim,
            tile,
            profile,
            exposure,
        } => render::render(&input, &output, max_dim, tile, profile.as_deref(), exposure)?,
        Command::Catalog {
            path,
            scan,
            hash,
            previews,
            jobs,
            sweep,
        } => {
            let mut catalog = rawkit_catalog::db::Catalog::open(&path)?;
            println!("path       : {}", path.display());
            println!(
                "schema     : v{} of v{}",
                catalog.version()?,
                rawkit_catalog::SCHEMA_VERSION
            );
            println!("journal    : {}", catalog.journal_mode());
            match catalog.backup_dir() {
                Some(dir) => {
                    let existing = std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0);
                    println!(
                        "backups    : {} ({existing} kept, {} max)",
                        dir.display(),
                        rawkit_catalog::backup::KEEP
                    );
                }
                None => println!("backups    : none"),
            }
            if let Some(root) = scan {
                let report = rawkit_catalog::scan::scan(&mut catalog, &root, file_metadata)?;
                println!(
                    "scanned    : {} added, {} updated, {} unchanged, {} now missing",
                    report.added, report.updated, report.unchanged, report.missing
                );
                if report.without_metadata > 0 {
                    println!(
                        "unreadable : {} file(s) gave no camera metadata",
                        report.without_metadata
                    );
                }
                if report.symlinks > 0 {
                    println!("symlinks   : {} not followed", report.symlinks);
                }
                for dir in &report.unreadable {
                    println!("unreadable : {}", dir.display());
                }
            }

            if hash {
                let mut last = usize::MAX;
                let (hashed, failed) =
                    rawkit_catalog::scan::hash_missing(&mut catalog, |done, total| {
                        // One line per ten percent: enough to show progress on a
                        // long run, quiet enough not to bury the result.
                        let step = (total / 10).max(1);
                        if total > 0 && done / step != last {
                            last = done / step;
                            eprint!("\rhashing    : {done}/{total}");
                        }
                    })?;
                if hashed + failed > 0 {
                    eprintln!();
                }
                println!(
                    "hashed     : {hashed} files{}",
                    match failed {
                        0 => String::new(),
                        n => format!(", {n} unreadable"),
                    }
                );
            }

            if previews {
                let mut last = usize::MAX;
                let report =
                    previews::build(&catalog, PreviewLevel::BULK, jobs, |done, total, name| {
                        // One line per image, rewritten in place: a build is long
                        // enough that silence reads as a hang.
                        if done != last {
                            last = done;
                            eprint!("\rpreviews   : {done}/{total} {name:<40}");
                        }
                    })?;
                if report.images > 0 {
                    eprintln!();
                }
                println!(
                    "previews   : {} written for {} image(s), {}",
                    report.written,
                    report.images,
                    human_bytes(report.bytes)
                );
                for (name, why) in &report.failed {
                    println!("            {name}: {why}");
                }
            }

            if sweep {
                match rawkit_catalog::previews::directory(&catalog) {
                    Some(dir) => {
                        let (removed, freed) = rawkit_catalog::previews::sweep(&catalog, &dir)?;
                        println!(
                            "swept      : {removed} orphaned preview(s), {} freed",
                            human_bytes(freed)
                        );
                    }
                    None => println!("swept      : nothing (this catalog is in memory)"),
                }
            }

            let (count, bytes) = rawkit_catalog::previews::tally(&catalog)?;
            if count > 0 {
                println!("previews   : {count} on disk, {}", human_bytes(bytes));
            }

            // Dropping the catalog is what writes the backup, so say so after.
            drop(catalog);
            println!("closed     : backup written");
        }
        Command::Gpu => {
            let gpu = rawkit_engine::Gpu::new()?;
            let info = &gpu.adapter_info;
            println!("adapter : {}", info.name);
            println!("backend : {:?}", info.backend);
            println!("type    : {:?}", info.device_type);
            println!("driver  : {} {}", info.driver, info.driver_info);
        }
    }
    Ok(())
}

/// The seam between the catalog and the decoder.
///
/// `rawkit-catalog` deliberately does not depend on `rawkit-decode`, so that the
/// library layer stays free of LibRaw and its CDDL obligations. It takes a
/// reader instead, and this is the one place the two are joined — a translation
/// small enough to read in full, which is the point of putting it here rather
/// than giving the catalog a dependency it would only use for six columns.
///
/// A failure is `None`, not an error: an `.ARW` that will not parse is a row
/// with empty camera columns, never a scan that stops halfway through a library.
fn file_metadata(path: &std::path::Path) -> Option<rawkit_catalog::scan::FileMetadata> {
    let found = rawkit_decode::read_metadata(path).ok()?;
    Some(rawkit_catalog::scan::FileMetadata {
        captured_at: found.captured_at,
        camera_make: Some(found.camera.make).filter(|s| !s.is_empty()),
        camera_model: Some(found.camera.model).filter(|s| !s.is_empty()),
        camera_serial: found.camera.serial,
        shutter_count: found.shutter_count.map(i64::from),
        lens: found.lens,
    })
}

/// Sizes in the units a person reads, because "17179869184" is not a size.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
