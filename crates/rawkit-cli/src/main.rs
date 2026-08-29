//! `rawkit` — the headless face of the engine.
//!
//! The GUI is the product, but the CLI is what CI, the golden harness and the
//! (later) Python lab talk to. Anything the app can do without a window belongs
//! here first: it makes the behaviour scriptable and, more usefully, testable on
//! three operating systems without a display attached.

mod render;

use anyhow::Result;
use clap::{Parser, Subcommand};
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
        Command::Catalog { path } => {
            let catalog = rawkit_catalog::db::Catalog::open(&path)?;
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
