//! `rawkit` — the headless face of the engine.
//!
//! The GUI is the product, but the CLI is what CI, the golden harness and the
//! (later) Python lab talk to. Anything the app can do without a window belongs
//! here first: it makes the behaviour scriptable and, more usefully, testable on
//! three operating systems without a display attached.

mod export;
mod previews;
mod render;

use anyhow::{bail, Result};
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

    /// Copy photographs off a card into the library, verifying every one.
    ///
    /// Files land in `destination/YEAR/YEAR-MM-DD/`, from the camera's own
    /// clock, under the names the camera gave them. Each is hashed as it is
    /// read and again after it lands, and only then given its real name — a
    /// card reader that drops a byte does not say so, and the damage otherwise
    /// surfaces years later in one frame nobody opened in the meantime.
    ///
    /// It copies. The card is the only other copy of these photographs until
    /// this finishes, so emptying it stays a decision you make afterwards with
    /// the files in front of you.
    Ingest {
        /// The catalog to add them to. Created if it does not exist.
        catalog: PathBuf,
        /// The card, or any folder to read from. Never written to.
        #[arg(long)]
        from: PathBuf,
        /// Where the library lives.
        #[arg(long)]
        into: PathBuf,
    },

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
        #[command(flatten)]
        edit: EditFlags,
    },

    /// Render the edits you made, into files you can send someone.
    ///
    /// The loop `render` never closed: that one takes a RAW and renders it as
    /// shot, because it has never heard of a catalog. This reads each image's
    /// stored `EditState` and renders with it, always at full resolution.
    Export {
        /// The catalog to read.
        catalog: PathBuf,
        /// Where to write. Created if it does not exist, and never inside the
        /// catalog's own previews or backups — those get swept.
        #[arg(long)]
        to: PathBuf,
        /// Only what survived the cull.
        #[arg(long, conflicts_with_all = ["rated", "image", "all"])]
        picks: bool,
        /// Only images with at least this many stars.
        #[arg(long, conflicts_with_all = ["picks", "image", "all"])]
        rated: Option<u8>,
        /// One image, by its catalog id.
        #[arg(long, conflicts_with_all = ["picks", "rated", "all"])]
        image: Option<i64>,
        /// Everything in the library.
        #[arg(long, conflicts_with_all = ["picks", "rated", "image"])]
        all: bool,
        /// Longest edge in pixels. 0 writes full resolution, which is what an
        /// export usually wants.
        #[arg(long, default_value_t = 0)]
        max_dim: u32,
        /// Replace files that are already there. Off by default: an export is
        /// the one thing here that writes outside the catalog's own directory.
        #[arg(long)]
        overwrite: bool,
        /// How many to render at once.
        #[arg(long, default_value_t = previews::default_jobs())]
        jobs: usize,
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
        Command::Ingest {
            catalog,
            from,
            into,
        } => {
            let mut catalog = rawkit_catalog::db::Catalog::open(&catalog)?;
            let mut last = String::new();
            let report = rawkit_catalog::ingest::ingest(
                &mut catalog,
                &from,
                &into,
                file_metadata,
                |done, total, name| {
                    if !name.is_empty() && name != last {
                        eprint!("\rcopying    : {done}/{total} {name}   ");
                        last = name.to_string();
                    }
                },
            )?;
            eprintln!();
            eprintln!(
                "copied     : {} new, {} already there, {} renamed",
                report.copied, report.already_there, report.renamed
            );
            for directory in &report.unreadable {
                eprintln!("unreadable : {}", directory.display());
            }
            for (path, why) in &report.failed {
                eprintln!("FAILED     : {} — {why}", path.display());
            }
            if let Some(scanned) = &report.scanned {
                eprintln!(
                    "catalogued : {} added, {} unchanged",
                    scanned.added, scanned.unchanged
                );
            }
            if !report.failed.is_empty() {
                bail!(
                    "{} file(s) did not arrive intact; the card still has them",
                    report.failed.len()
                );
            }
        }

        Command::Render {
            input,
            output,
            max_dim,
            tile,
            profile,
            edit,
        } => render::render(
            &input,
            &output,
            max_dim,
            tile,
            profile.as_deref(),
            &edit.state()?,
        )?,
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
        Command::Export {
            catalog,
            to,
            picks,
            rated,
            image,
            all,
            max_dim,
            overwrite,
            jobs,
        } => {
            let selection = match (picks, rated, image, all) {
                (true, _, _, _) => export::Selection::Picks,
                (_, Some(stars), _, _) => export::Selection::Rated(stars),
                (_, _, Some(id), _) => export::Selection::Image(id),
                (_, _, _, true) => export::Selection::All,
                _ => {
                    anyhow::bail!("say what to export: --picks, --rated <n>, --image <id> or --all")
                }
            };
            let catalog = rawkit_catalog::db::Catalog::open(&catalog)?;
            export::check_destination(&catalog, &to)?;

            let mut last = usize::MAX;
            let report = export::export(
                &catalog,
                selection,
                &to,
                max_dim,
                overwrite,
                jobs,
                |done, total, name| {
                    if done != last {
                        last = done;
                        eprint!("\rexporting  : {done}/{total} {name:<40}");
                    }
                },
            )?;
            if report.written + report.skipped > 0 {
                eprintln!();
            }
            println!(
                "exported   : {} file(s), {} to {}",
                report.written,
                human_bytes(report.bytes),
                to.display()
            );
            if report.skipped > 0 {
                println!(
                    "skipped    : {} already there (use --overwrite)",
                    report.skipped
                );
            }
            for (name, why) in &report.failed {
                println!("            {name}: {why}");
            }
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

/// The edit, as command-line flags.
///
/// Grouped rather than passed as eight arguments, which is what clippy objected
/// to and it was right: these are one thing — a description of how to develop
/// the frame — and they belong together for the same reason `EditState` does.
#[derive(clap::Args, Debug)]
pub struct EditFlags {
    /// Exposure in stops, applied in scene-linear light before the tone map.
    ///
    /// The `signal` line this command prints says how far the frame sits
    /// from clipping, which is the number to set this from: a file peaking
    /// at -1.2 EV has that much room before anything blows.
    #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
    exposure: f32,
    /// Contrast, about middle grey. -1 to 1.
    ///
    /// This and the four below are display-referred: they shape what the
    /// tone map produced, rather than how much light there was. Whites and
    /// blacks are the two that clip.
    #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
    contrast: f32,
    /// Highlights. Negative recovers, -1 to 1.
    #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
    highlights: f32,
    /// Shadows. Positive opens them, -1 to 1.
    #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
    shadows: f32,
    /// White point. Positive blows the brightest values to white, -1 to 1.
    #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
    whites: f32,
    /// Black point. Negative crushes the darkest values to black, -1 to 1.
    #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
    blacks: f32,
    /// Rotate in 90-degree steps clockwise: 0, 90, 180 or 270.
    ///
    /// Applied before the crop, so a crop is always read in the frame you
    /// are looking at.
    #[arg(long, default_value_t = 0)]
    rotate: u32,
    /// Keep only this rectangle, as fractions of the rotated frame:
    /// `left,top,right,bottom`, each from 0 to 1.
    ///
    /// Fractions rather than pixels because the same numbers have to be
    /// right for a full-resolution export and for a thumbnail.
    #[arg(long, value_name = "L,T,R,B")]
    crop: Option<String>,
    /// Straighten, in degrees clockwise. -15 to 15.
    ///
    /// The crop pulls in far enough that no empty corner is left, so a
    /// straightened frame always comes out smaller than an unstraightened one.
    /// This is the only operation in the pipeline that resamples.
    #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
    straighten: f32,
}

impl EditFlags {
    /// The edit these flags describe.
    ///
    /// Parsed and refused here, before anything reads a file: a mistyped
    /// rotation should not cost a two-second decode of a 24 MP frame before it
    /// is reported.
    pub fn state(&self) -> anyhow::Result<rawkit_editstate::EditState> {
        use anyhow::{anyhow, bail};
        use rawkit_editstate::{EditState, Orientation, Tone};

        if !self.exposure.is_finite() {
            bail!("exposure must be a finite number of stops");
        }
        for (name, value) in [
            ("contrast", self.contrast),
            ("highlights", self.highlights),
            ("shadows", self.shadows),
            ("whites", self.whites),
            ("blacks", self.blacks),
        ] {
            if !value.is_finite() || !(-1.0..=1.0).contains(&value) {
                bail!("{name} runs from -1 to 1; got {value}");
            }
        }
        let orientation = match self.rotate {
            0 => Orientation::AsShot,
            90 => Orientation::Rotate90Cw,
            180 => Orientation::Rotate180,
            270 => Orientation::Rotate270Cw,
            other => bail!("--rotate takes 0, 90, 180 or 270; got {other}"),
        };
        let crop = rawkit_editstate::Crop {
            angle_deg: self.straighten,
            ..match self.crop.as_deref() {
                Some(text) => parse_crop(text)?,
                None => rawkit_editstate::Crop::default(),
            }
        };
        crop.validate()
            .map_err(|e| anyhow!("{e}; see --crop and --straighten"))?;

        Ok(EditState {
            tone: Tone {
                exposure_ev: self.exposure,
                contrast: self.contrast,
                highlights: self.highlights,
                shadows: self.shadows,
                whites: self.whites,
                blacks: self.blacks,
            },
            orientation,
            crop,
            ..Default::default()
        })
    }
}

/// `left,top,right,bottom`, each a fraction of the rotated frame.
fn parse_crop(text: &str) -> anyhow::Result<rawkit_editstate::Crop> {
    use anyhow::{anyhow, Context};
    let parts: Vec<&str> = text.split(',').map(str::trim).collect();
    let [left, top, right, bottom] = <[&str; 4]>::try_from(parts.as_slice())
        .map_err(|_| anyhow!("--crop takes four fractions, `left,top,right,bottom`"))?;
    let number = |name: &str, text: &str| -> anyhow::Result<f32> {
        text.parse::<f32>()
            .with_context(|| format!("--crop {name} is `{text}`, which is not a number"))
    };
    Ok(rawkit_editstate::Crop {
        left: number("left", left)?,
        top: number("top", top)?,
        right: number("right", right)?,
        bottom: number("bottom", bottom)?,
        ..rawkit_editstate::Crop::default()
    })
}
